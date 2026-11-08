//! AI 訂正の待ちと確定、学習。

use super::*;

/// API の答えを待っている発話の、訂正の依頼の控え(発話の番号, 学習で当てた置き換え, 依頼)。
pub(super) type AfterRemote = (u64, Vec<(String, String)>, CorrectionRequest);

/// AI 訂正の待ち。話した順に確定するための控えと、処理役への口。
pub(super) struct CorrectionQueue {
    pub(super) jobs: Option<Sender<AiJob>>,
    pub(super) tx: Sender<CorrectionDone>,
    pub(super) rx: Receiver<CorrectionDone>,
    /// AI の答えがまだ戻っていない依頼の数(画面の印に使う)。
    pub(super) pending: usize,
    /// 確定を待っている発話(話した順)。
    pub(super) unconfirmed: VecDeque<Unconfirmed>,
    /// 上限を過ぎて戻った訂正。送っている途中なら文を書き換えないよう、終わってから当てる。
    pub(super) late: Vec<CorrectionDone>,
    /// 外部の音声認識 API の処理役への口と、答えの受け口。
    pub(super) remote_jobs: Option<Sender<RemoteJob>>,
    pub(super) remote_tx: Sender<RemoteDone>,
    pub(super) remote_rx: Receiver<RemoteDone>,
    /// API の答えを待っている発話の、訂正の依頼の控え。
    pub(super) after_remote: Vec<AfterRemote>,
    /// 転送の途中に届いた API の答え。送った文を書き換えないよう、送り終えてから当てる。
    pub(super) held_remote: Vec<RemoteDone>,
}

impl CorrectionQueue {
    pub(super) fn new() -> Self {
        let (tx, rx) = mpsc::channel();
        let (remote_tx, remote_rx) = mpsc::channel();
        CorrectionQueue {
            jobs: None,
            tx,
            rx,
            pending: 0,
            unconfirmed: VecDeque::new(),
            late: Vec::new(),
            remote_jobs: None,
            remote_tx,
            remote_rx,
            after_remote: Vec::new(),
            held_remote: Vec::new(),
        }
    }
}

impl VoiceApp {
    /// AI の答えがまだ戻っていない訂正の数(画面の印と試験が読む)。
    pub fn pending_corrections(&self) -> usize {
        self.corrections.pending
    }

    /// 訂正役を作り直す(設定・API キー・辞書・学習が変わったとき)。
    pub fn rebuild_corrector(&mut self) {
        self.corrector = make_corrector(&self.store, &self.global, &self.data);
        // 補完役も同じ設定と辞書(テンプレート一覧)に依るので、一緒に作り直す。
        self.rebuild_assistant();
    }

    /// 外部の音声認識 API を作り直す(設定・鍵が変わったとき)。
    pub fn rebuild_remote(&mut self) {
        self.remote = make_remote(&self.store, &self.global);
    }

    /// 外部の音声認識 API に送る。送れたら true(答えが戻ってから訂正を依頼する。`receive_remote`)。
    /// 声でない短い文の発話は送らない(門番)。
    pub(super) fn enqueue_remote(
        &mut self,
        id: u64,
        haas_text: &str,
        audio: Option<&Arc<Vec<f32>>>,
        applied: Vec<(String, String)>,
        request: CorrectionRequest,
    ) -> bool {
        let (Some(provider), Some(jobs), Some(audio)) =
            (self.remote.clone(), self.corrections.remote_jobs.as_ref(), audio)
        else {
            return false;
        };
        if !remote_asr::worth_sending(haas_text) {
            return false;
        }
        let words = if self.global.remote.send_glossary {
            remote_asr::hint_words(&self.data.dictionary)
        } else {
            Vec::new()
        };
        let job = RemoteJob {
            id,
            haas_text: request.text.clone(),
            audio: audio.clone(),
            provider,
            hints: remote_asr::Hints {
                words,
                language: remote_asr::LANGUAGE,
            },
            deadline: Instant::now() + self.remote_timeout(),
        };
        if jobs.send(job).is_err() {
            return false;
        }
        self.corrections.pending += 1;
        self.corrections.after_remote.push((id, applied, request));
        true
    }

    /// 外部の音声認識 API の答えを待つ上限。
    pub(super) fn remote_timeout(&self) -> Duration {
        Duration::from_secs(u64::from(self.global.remote.timeout_secs.clamp(3, 60)))
    }

    /// 外部の音声認識 API の答えを受け取る。使える文なら表示を差し替えて、その文で訂正を依頼する。
    /// 失敗・時間切れ・空・長すぎる文(長さの門番)なら Haas の文で進む。命令語は Haas の文で判定済みなので、
    /// API の文の文末の命令語は働かせない(訂正役がいるときは、訂正の答えの判定 2 に任せる)。
    pub(super) fn receive_remote(&mut self, done: RemoteDone) {
        // 送っている途中は文を書き換えない(送った文とエディターが食い違う)。送り終えてから当てる。
        if self.transfer_sending() {
            self.corrections.held_remote.push(done);
            return;
        }
        self.corrections.pending = self.corrections.pending.saturating_sub(1);
        let Some(index) = self.corrections.after_remote.iter().position(|(id, ..)| *id == done.id) else {
            // 上限を過ぎて、Haas の文で訂正に回した後(`watch_corrections`)。
            diag::log("外部 API の答えが上限より遅れたので使わない");
            return;
        };
        let (id, mut applied, mut request) = self.corrections.after_remote.remove(index);
        if !self.corrections.unconfirmed.iter().any(|u| u.id == id) {
            diag::log("外部 API の答えが上限より遅れたので使わない");
            return;
        }
        match done.result {
            Ok(answer) => {
                if let Some((text, pairs)) = self.usable_remote_text(id, &done.haas_text, &answer) {
                    if text != request.text && self.editor.intact_start(id).is_some() {
                        let changed = changed_ranges(&request.text, &text);
                        if self.editor.apply_correction(id, &request.text, &text, &changed) {
                            self.selection_dirty = true;
                            request.unsure = disabled_ranges(&text, &self.data.dictionary);
                            request.text = text;
                            applied = pairs;
                            if let Some(utterance) = self.editor.utterances.iter_mut().find(|u| u.id == id) {
                                utterance.applied = applied.clone();
                            }
                        }
                    }
                }
            }
            // 設定を直さないと通らない誤り(鍵の誤りなど)なら、直すまで API に送らない(毎回音声を送らない)。
            Err(error @ (RemoteError::Permanent(_) | RemoteError::NoKey)) => {
                diag::log("外部 API が使えないので、設定が変わるまで Haas だけで動く");
                self.remote = None;
                self.notify_error(format!(
                    "外部 API: {error}。設定の「認識」を直すまで、Haas だけで動きます"
                ));
            }
            Err(error) => {
                diag::log("外部 API が答えないので Haas の文で進む");
                self.notify_error(format!("外部 API: {error}(Haas の文で進めました)"));
            }
        }
        self.correct_after_remote(id, applied, request);
    }

    /// API の答えから、表示を差し替えてよい文と、それに当てた学習の組を作る。使えなければ None(Haas の文で進む)。
    /// 改行は除き、整形と学習の置き換えを Haas の文と同じく当てる。命令語は Haas の文で判定済みなので、
    /// Haas が命令語を働かせた発話は、API の文の文末からその命令語を外せたときだけ使う(外せなければ、
    /// 命令語が文に残るか二度働くので Haas の文で進む)。訂正役がいないときも文末の命令語は働かせない。
    fn usable_remote_text(&mut self, id: u64, haas_text: &str, answer: &str) -> Option<(String, Vec<(String, String)>)> {
        let answer = answer.replace(['\r', '\n'], "");
        let mut text = apply_prefilters(&answer, &self.data.settings.filters, &self.data.dictionary);
        let spoken_command = self.editor.utterances.iter().any(|u| u.id == id && u.spoken_command);
        let commands = &self.data.settings.command_words;
        match match_after_correction(&text, commands) {
            Match::Command { prefix, .. } if spoken_command || self.corrector.is_none() => {
                if !spoken_command {
                    let heard: String = text.chars().skip(prefix.chars().count()).collect();
                    self.notify(format!(
                        "外部 API は文末を「{}」と聞きましたが、命令としては働かせていません",
                        heard.trim()
                    ));
                }
                text = prefix;
            }
            _ if spoken_command => {
                diag::log("外部 API の文の文末に命令語が無いので Haas の文で進む");
                return None;
            }
            _ => {}
        }
        if text.trim().is_empty() {
            diag::log("外部 API の文が空なので Haas の文で進む");
            return None;
        }
        if remote_asr::too_long(haas_text, &text) || remote_asr::too_short(haas_text, &text) {
            diag::log(format!(
                "外部 API の文が{}({} 字、Haas は {} 字)ので Haas の文で進む",
                if remote_asr::too_long(haas_text, &text) { "長すぎる" } else { "短すぎる" },
                text.chars().count(),
                haas_text.chars().count()
            ));
            return None;
        }
        // 学習した置き換え(手直しで覚えたものを含む)を、Haas の文と同じく当てる。
        let learning = &self.data.settings.learning;
        if learning.enabled && learning.auto_apply {
            let applied = self.data.memory.apply_with_pairs(&text, learning.min_count);
            return Some((applied.text, applied.pairs));
        }
        Some((text, Vec::new()))
    }

    /// API の段のあとの訂正を依頼する。手直し・削除された発話は依頼しない(確定は `watch_corrections`)。
    /// 訂正役がいなければ、この文で確定する。
    pub(super) fn correct_after_remote(&mut self, id: u64, applied: Vec<(String, String)>, request: CorrectionRequest) {
        if let Some(unconfirmed) = self.corrections.unconfirmed.iter_mut().find(|u| u.id == id) {
            unconfirmed.deadline = Instant::now() + CORRECTION_WAIT;
        }
        if self.editor.intact_start(id).is_none() {
            return;
        }
        let text = request.text.clone();
        if !self.enqueue_correction(id, applied.clone(), request) {
            if let Some(unconfirmed) = self.corrections.unconfirmed.iter_mut().find(|u| u.id == id) {
                unconfirmed.done = Some(CorrectionDone {
                    id,
                    applied,
                    original: text.clone(),
                    result: Ok(text),
                });
            }
        }
    }

    /// 訂正役の名前。使えないときは None。
    pub fn corrector_name(&self) -> Option<String> {
        self.corrector.as_ref().map(|c| c.name())
    }

    /// 訂正を依頼する。処理役がいなければ false(そのときは呼び元がその場で確定する)。
    pub(super) fn enqueue_correction(
        &mut self,
        id: u64,
        applied: Vec<(String, String)>,
        request: CorrectionRequest,
    ) -> bool {
        let (Some(corrector), Some(jobs)) = (self.corrector.clone(), self.corrections.jobs.as_ref()) else {
            return false;
        };
        let job = AiJob {
            id,
            request: AiRequest::Correction {
                applied,
                request,
                corrector,
            },
        };
        if jobs.send(job).is_err() {
            return false;
        }
        self.corrections.pending += 1;
        true
    }

    /// 戻った訂正を受け取る。確定は `watch_corrections` が話した順に行う。
    pub(super) fn receive_correction(&mut self, done: CorrectionDone) {
        self.corrections.pending = self.corrections.pending.saturating_sub(1);
        match self.corrections.unconfirmed.iter_mut().find(|u| u.id == done.id) {
            Some(unconfirmed) => unconfirmed.done = Some(done),
            None => self.corrections.late.push(done),
        }
    }

    /// 発話を話した順に確定する。先頭の答えが戻るか上限を過ぎるまで、後ろの発話は仮表示のまま待つ。
    /// 送っている途中は文を書き換えない(消す分と合わなくなる)。時刻を受けるのは、試験で上限まで進めるため。
    pub fn watch_corrections(&mut self, now: Instant) {
        if self.transfer_sending() {
            return;
        }
        for done in std::mem::take(&mut self.corrections.late) {
            self.finish_correction(done, None);
        }
        for done in std::mem::take(&mut self.corrections.held_remote) {
            self.receive_remote(done);
        }
        // 「転送」が働いたら、残りはその転送が終わってから確定する。
        while !self.transfer_sending() {
            let Some(next) = self.corrections.unconfirmed.front() else {
                return;
            };
            let touched = self.editor.intact_start(next.id).is_none();
            // API の答えを待ったまま上限を過ぎたら、Haas の文で訂正に回す(列で待った分、訂正まで落とさない)。
            if next.done.is_none() && !touched && now >= next.deadline {
                let id = next.id;
                if let Some(index) = self.corrections.after_remote.iter().position(|(waiting, ..)| *waiting == id) {
                    let (_, applied, request) = self.corrections.after_remote.remove(index);
                    diag::log("外部 API の答えが上限までに来ないので Haas の文で進む");
                    self.correct_after_remote(id, applied, request);
                    if let Some(front) = self.corrections.unconfirmed.front_mut() {
                        front.deadline = now + CORRECTION_WAIT;
                    }
                    return;
                }
            }
            let Some(next) = self.corrections.unconfirmed.front() else {
                return;
            };
            if next.done.is_none() && !touched && now < next.deadline {
                return;
            }
            let Some(mut next) = self.corrections.unconfirmed.pop_front() else {
                return;
            };
            match next.done.take() {
                Some(done) => self.finish_correction(done, Some(next)),
                // 手直しや削除の済んだ発話は、もう訂正を待たない。
                None if touched => self.editor.set_correction(next.id, CorrectionState::Settled),
                None => {
                    diag::log("AI 訂正の上限を過ぎたので認識文のまま確定");
                    self.editor.set_correction(next.id, CorrectionState::Failed);
                }
            }
        }
    }

    /// 戻った訂正を当てる。訂正文の文末の命令語と全体一致のテンプレートもここで判定する(判定 2)。
    /// `spoken` は上限の前に戻った発話の控えで、上限を過ぎて戻った分は None。
    pub(super) fn finish_correction(&mut self, done: CorrectionDone, spoken: Option<Unconfirmed>) {
        let CorrectionDone {
            id,
            applied,
            original,
            result,
        } = done;
        // 先に手直し・削除・転送された発話の答えは、失敗も含めて捨てる。
        let Some(start) = self.editor.intact_start(id) else {
            self.editor.set_correction(id, CorrectionState::Settled);
            return;
        };
        let corrected = match result {
            Ok(corrected) => corrected,
            Err(message) => {
                self.editor.set_correction(id, CorrectionState::Failed);
                self.notify_error(format!("訂正できませんでした: {message}"));
                return;
            }
        };
        self.editor.set_correction(id, CorrectionState::Settled);
        if corrected != original && !accept_correction(&original, &corrected) {
            return;
        }
        let shrunk =
            original.chars().count() > corrected.chars().count() + MATCH_SHRINK_LIMIT;
        let commands = &self.data.settings.command_words;
        let (sentence, action, count) = match match_after_correction(&corrected, commands) {
            Match::Command {
                prefix,
                action,
                count,
            } => (prefix, Some(action), count),
            _ => (corrected, None, 0),
        };
        let template = self.data.dictionary.find_template(&sentence).cloned();
        if shrunk && (action.is_some() || template.is_some()) {
            let message = "AI の答えが元の文よりかなり短いので、命令として扱わず認識した文を残しました";
            if action == Some(CommandAction::Transfer) {
                self.refuse_transfer(message, self.next_utterance_start(id));
            } else {
                self.notify(message);
            }
            return;
        }
        if action.is_none() && template.is_none() {
            if sentence != original {
                let changed = changed_ranges(&original, &sentence);
                self.editor.apply_correction(id, &original, &sentence, &changed);
                self.selection_dirty = true;
                self.learn(&original, &sentence, false, &applied);
                self.data.memory.corrected += 1;
            }
            return;
        }
        // 文末の語やキーワードへの直しは誤認識の組ではないので覚えない
        // (「開業→改行」を覚えると、次から一般の文の「開業」が書き換わる)。
        let end = match &template {
            Some(template) => {
                // 呼ぶ語は書かない(話した語なので)。
                diag::log("訂正後に一致: テンプレート");
                self.editor.replace_with_template(id, &template.body);
                start + template.body.chars().count()
            }
            None => {
                let changed = changed_ranges(&original, &sentence);
                self.editor.apply_correction(id, &original, &sentence, &changed);
                start + sentence.chars().count()
            }
        };
        self.selection_dirty = true;
        if let Some(action) = action {
            diag::log(format!("訂正後に一致: {}", action.label()));
            self.editor.set_spoken_command(id);
            match action {
                CommandAction::NewLine => self.editor.insert_at(end, &"\n".repeat(count)),
                CommandAction::Transfer => self.transfer_corrected(id, end, spoken),
            }
        }
    }

    /// 「誤認識 → 正しい語」を覚える。覚えたら訂正役の例と認識エンジンの手がかりも更新する。
    /// 認識したままの文と直した文の差分を覚える。`applied` はその文に学習で当てていた置き換え。
    pub(super) fn learn(&mut self, raw: &str, fixed: &str, manual: bool, applied: &[(String, String)]) {
        if !self.data.settings.learning.enabled {
            return;
        }
        let keys = |words: Vec<&str>| -> Vec<String> {
            let keys = words.into_iter().map(template_key);
            keys.filter(|key| !key.is_empty()).collect()
        };
        let command_words = command_word_list(&self.data);
        let command_keys = keys(command_words.iter().map(String::as_str).collect());
        // 左右が変わる文は話し間違いの訂正を含むので、文ごと覚えない(断片にすると right と left の差が見えない)。
        let learned = if changes_laterality(raw, fixed) {
            0
        } else {
            self.data.memory.learn_with(raw, fixed, manual, &|from, to| {
                let to_key = template_key(to);
                // 所見の意味を変える直しは誤認識の修正ではない。覚えると次から自動で書き換わる。
                !changes_meaning(from, to)
                    // 学習で当てた置き換えを元に戻しただけの差分は、逆向きに覚えるのではなく、その組を弱める。
                    && !applied.iter().any(|(f, t)| f == to && t == from)
                    // 命令語への直しを覚えると、AI の判断なしに訂正文の判定が働く。組はふつう語の断片なので断片でも止める。
                    // テンプレートのキーワードは普通の語(検査名)で、「テンプレート」が続かなければ判定は働かないので覚える
                    // (聞き違いを覚えれば、次からは「〜テンプレート」が AI なしで通る)。
                    && !command_keys.iter().any(|key| {
                        to_key.contains(key.as_str())
                            || (!to_key.is_empty() && key.contains(to_key.as_str()))
                    })
            })
        };
        let unlearned = self.unlearn_reverted(applied, raw, fixed, manual);
        if learned > 0 || unlearned > 0 {
            diag::log(format!(
                "学習 {learned} 組、弱めた組 {unlearned}({})",
                if manual { "手直し" } else { "AI 訂正" }
            ));
            self.after_memory_change();
        }
    }

    /// 学習で当てた置き換えのうち、AI や手直しが元の語に戻したものの重みを下げる。戻した数を返す。
    pub(super) fn unlearn_reverted(
        &mut self,
        applied: &[(String, String)],
        before: &str,
        after: &str,
        manual: bool,
    ) -> usize {
        let mut reverted = 0;
        for (from, to) in applied {
            let restored = after.contains(from.as_str()) && !before.contains(from.as_str());
            if restored && self.data.memory.unlearn(from, to, manual) {
                // 語そのものは書かない(医療の語が残る。動作の記録は字数だけ)。
                diag::log(format!(
                    "学習した置き換えを 1 組弱めた({} 字 → {} 字が元に戻された)",
                    from.chars().count(),
                    to.chars().count()
                ));
                reverted += 1;
            }
        }
        reverted
    }

    /// 転送の直前に、機械(AI 訂正後、または認識したまま)の文と手直し後の文を比べて覚える(手直しは重みが大きい)。
    /// 比べる相手は AI 訂正後の文。認識したままの文と比べると、AI の訂正まで手直しとして数えてしまう。
    pub(super) fn learn_manual_edits(&mut self) {
        if !self.data.settings.learning.enabled {
            return;
        }
        let length = self.editor.len();
        let edits: Vec<ManualEdit> = self
            .editor
            .utterances
            .iter()
            .filter(|u| !u.fixed && u.start < u.end && u.end <= length)
            .filter_map(|u| {
                let current = self.editor.slice(u.start, u.end);
                (current != u.machine).then(|| (u.machine.clone(), current, u.applied.clone()))
            })
            .collect();
        for (machine, fixed, applied) in edits {
            self.learn(&machine, &fixed, true, &applied);
        }
    }
}
