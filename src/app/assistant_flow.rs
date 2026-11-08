//! 「ヘイクロード」のテンプレート補完の流れ。

use super::*;

/// 「ヘイクロード」の補完役と、依頼の待ちと答えの控え。
pub(super) struct RequestState {
    /// 「ヘイ、クロード」のテンプレート補完役。Claude に接続していなければ None。
    pub(super) assistant: Option<Arc<dyn Assistant>>,
    pub(super) jobs: Option<Sender<AssistantJob>>,
    pub(super) tx: Sender<AssistantDone>,
    pub(super) rx: Receiver<AssistantDone>,
    /// 呼びかけだけを聞いて、次の発話を依頼として待っている期限。
    pub(super) call_waiting: Option<Instant>,
    /// 依頼を Claude に送った時刻(答えを待っている間)。
    pub(super) began: Option<Instant>,
    /// 直前に補完した発話(番号と本文)。手付かずで残っていれば、次の依頼で言い直しとして置き換える。
    pub(super) last_completion: Option<(u64, String)>,
    /// 転送の途中に戻った補完の答え(送り終えてから当てる)。
    pub(super) held: Option<AssistantDone>,
    /// Esc で取りやめた依頼の数。その分の答えは捨てる。
    pub(super) discarded: u32,
}

impl RequestState {
    pub(super) fn new() -> Self {
        let (tx, rx) = mpsc::channel();
        RequestState {
            assistant: None,
            jobs: None,
            tx,
            rx,
            call_waiting: None,
            began: None,
            last_completion: None,
            held: None,
            discarded: 0,
        }
    }
}

impl VoiceApp {
    // ---- 「ヘイ、クロード」のテンプレート補完(単位 B) ----

    /// 補完役を用意する(設定の接続方法で)。辞書が変わるとテンプレート一覧も変わるので、訂正役と一緒に作り直す。
    pub fn rebuild_assistant(&mut self) {
        self.requests.assistant = make_assistant(&self.store, &self.global, &self.data);
        if self.requests.assistant.is_some() {
            self.start_assistant_worker();
        }
    }

    /// 補完役を差す(試験は偽物を渡す)。
    pub fn enable_assistant(&mut self, assistant: Arc<dyn Assistant>) {
        self.requests.assistant = Some(assistant);
        self.start_assistant_worker();
    }

    pub(super) fn start_assistant_worker(&mut self) {
        // 処理役は一度だけ作る(補完役の差し替えでは作り直さない。依頼ごとに補完役を持たせている)。
        if self.requests.jobs.is_none() {
            self.requests.jobs = Some(spawn_assistant_worker(
                self.requests.tx.clone(),
                self.ctx.clone(),
            ));
        }
    }

    /// 補完役の名前。使えないときは None。
    pub fn assistant_name(&self) -> Option<String> {
        self.requests.assistant.as_ref().map(|a| a.name())
    }

    /// 呼びかけだけを聞いて、依頼を待っているか。
    pub fn call_waiting(&self) -> bool {
        self.requests.call_waiting.is_some()
    }

    /// 依頼を Claude に送っている途中か。
    pub fn completing(&self) -> bool {
        self.requests.began.is_some()
    }

    /// 呼びかけ語の判定(判定 1 の前)。依頼として扱ったら true。
    /// 呼びかけに続けて言った依頼はすぐ送る。呼びかけだけなら次の発話(15 秒以内)を依頼にする。
    /// 頭のフィラー(「えー、ヘイクロード」)は整形で落ちるので、整形後の文でも見る。
    pub(super) fn take_call(&mut self, recognition: &mut Recognition) -> bool {
        let call_words = &self.data.settings.call_words;
        let mut call = match_call(&recognition.text, call_words);
        if matches!(call, Call::None) {
            let filtered =
                apply_prefilters(&recognition.text, &self.data.settings.filters, &self.data.dictionary);
            call = match_call(&filtered, call_words);
        }
        match call {
            Call::WithRequest(rest) => {
                self.requests.call_waiting = None;
                // 依頼が「〜テンプレート」だけなら Claude に頼むまでもない。呼びかけ語を除いた文にして、
                // キーワードの呼び出しと同じ経路(転送の取りやめ後の文末への挿入なども同じ)で本文を入れる。
                if self.template_called(&rest).is_some() {
                    diag::log("呼びかけ: 依頼がテンプレートの呼び出しなので、そのまま入れる");
                    recognition.text = rest;
                    recognition.unsure.clear();
                    return false;
                }
                self.begin_request(&rest);
                true
            }
            Call::Alone => {
                self.requests.call_waiting = None;
                if self.request_blocked() {
                    return true;
                }
                diag::log("呼びかけ: 次の発話を依頼として待つ");
                self.requests.call_waiting = Some(Instant::now() + CALL_WAIT);
                self.notify("Claude への依頼をどうぞ");
                true
            }
            Call::None => {
                let Some(deadline) = self.requests.call_waiting.take() else {
                    return false;
                };
                if Instant::now() >= deadline {
                    return false;
                }
                // 待っている間の命令語・記号の読み・「〜テンプレート」(文末の命令語が続いてもよい)は依頼ではない。
                // 待ちをやめて、普通の経路で働かせる。
                if self.is_not_a_request(&recognition.text) {
                    diag::log("呼びかけ: 命令語・記号・テンプレートの呼び出しが来たので待ちをやめる");
                    return false;
                }
                self.begin_request(&recognition.text);
                true
            }
        }
    }

    /// 発話が「キーワード+テンプレート」ならその本文。整形前の文と、不要語を除いた文の両方で見る。
    pub(super) fn template_called(&self, spoken: &str) -> Option<String> {
        let dictionary = &self.data.dictionary;
        dictionary
            .find_template(spoken)
            .or_else(|| {
                let filtered = apply_prefilters(spoken, &self.data.settings.filters, dictionary);
                dictionary.find_template(&filtered)
            })
            .map(|template| template.body.clone())
    }

    /// 待っている間に来た発話が依頼ではないか: 命令語だけ(「転送」「改行」)、記号の読みだけ、
    /// 「〜テンプレート」(「胸部CTテンプレート 改行」のように文末の命令語が続いてもよい)。
    pub(super) fn is_not_a_request(&self, text: &str) -> bool {
        match match_utterance(text, &self.data.settings.command_words, &self.data.dictionary.symbols) {
            Match::Command { prefix, .. } => prefix.is_empty() || self.template_called(&prefix).is_some(),
            Match::Symbol(_) => true,
            Match::Text => self.template_called(text).is_some(),
        }
    }

    /// 依頼を受けられない理由を知らせる(Claude に接続していない、テンプレートが無い、前の依頼の処理中)。
    /// 受けられなければ true。呼びかけだけのときも見るので、待ってから次の発話を食べて断ることはない。
    pub(super) fn request_blocked(&mut self) -> bool {
        if self.requests.assistant.is_none() {
            diag::log("呼びかけ: Claude に接続していない");
            self.notify_error(
                "設定の「AI 訂正」で Claude に接続すると、「ヘイクロード」で依頼できます",
            );
            return true;
        }
        if self.data.dictionary.templates.is_empty() {
            diag::log("呼びかけ: テンプレートが無い");
            self.notify_error(
                "テンプレートが登録されていません。「辞書」の「テンプレート」か、辞書ファイル(dictionary.json)の templates に登録してください",
            );
            return true;
        }
        if self.completing() {
            self.notify("前の依頼を処理しています。終わってからもう一度どうぞ");
            return true;
        }
        false
    }

    /// 依頼を待つ期限が過ぎたら黙って口述に戻り、知らせる。時刻を受けるのは、試験で期限まで進めるため。
    pub fn watch_call(&mut self, now: Instant) {
        if self.requests.call_waiting.is_some_and(|deadline| now >= deadline) {
            self.requests.call_waiting = None;
            diag::log("呼びかけ: 依頼がないまま期限を過ぎた");
            self.notify("依頼がなかったので、口述に戻りました");
        }
    }

    /// 依頼を受ける。Claude に接続していない、テンプレートが無いときは知らせだけ。
    /// 依頼は AI 訂正・テンプレート照合・学習には通さない。
    pub(super) fn begin_request(&mut self, spoken: &str) {
        if self.request_blocked() {
            return;
        }
        let text = apply_prefilters(spoken, &self.data.settings.filters, &self.data.dictionary);
        if text.trim().is_empty() {
            self.notify("依頼が聞き取れませんでした");
            return;
        }
        let count = self.data.dictionary.templates.len();
        if count > MAX_TEMPLATES {
            self.notify(format!(
                "テンプレートが {count} 件あるので、先頭 {MAX_TEMPLATES} 件だけ Claude に送ります"
            ));
        }
        self.send_request(text);
    }

    /// 依頼を Claude に送る。直前に補完した文が手付かずで残っていれば添える(言い直し用)。
    pub(super) fn send_request(&mut self, text: String) {
        let (Some(assistant), Some(jobs)) = (self.requests.assistant.clone(), self.requests.jobs.as_ref())
        else {
            self.notify_error(
                "設定の「AI 訂正」で Claude に接続すると、「ヘイクロード」で依頼できます",
            );
            return;
        };
        let previous = self
            .requests.last_completion
            .as_ref()
            .filter(|(id, _)| self.editor.intact_start(*id).is_some())
            .map(|(_, body)| body.clone());
        diag::log(format!(
            "依頼 {} 字を送る(直前の補完{})",
            text.chars().count(),
            if previous.is_some() { "あり" } else { "なし" }
        ));
        let request = AssistantRequest {
            request: text.clone(),
            previous,
        };
        if jobs.send(AssistantJob { request, assistant }).is_err() {
            self.notify_error("Claude への依頼を送れませんでした");
            return;
        }
        self.requests.began = Some(Instant::now());
        self.live_caption = format!("依頼: {text}");
    }

    /// 戻った答えを当てる。本文はテンプレートと同じ経路で 1 発話として入れる(AI 訂正・学習は通さず、Esc で消せる)。
    /// Claude が言い直しと判断し(`replace`)、直前に補完した文が手付かずなら、新しく足さずにその文を置き換える。
    /// 本文が無ければ返事を知らせるだけ。
    pub(super) fn receive_completion(&mut self, done: AssistantDone) {
        self.requests.began = None;
        if self.live_caption.starts_with("依頼: ") {
            self.live_caption.clear();
        }
        let AssistantDone { result, took } = done;
        let reply = match result {
            Ok(reply) => reply,
            Err(message) => {
                self.notify_error(message);
                return;
            }
        };
        let Some(text) = reply.text else {
            let why = reply
                .reply
                .unwrap_or_else(|| "合うテンプレートがありません".to_string());
            // Claude の返事は依頼の文を引くことがあるので、字数だけ残す。
            diag::log(format!("補完: 該当なし(返事 {} 字)", why.chars().count()));
            self.notify(why);
            return;
        };
        let template = reply.template.unwrap_or_else(|| "テンプレート".to_string());
        let replaced = self
            .requests.last_completion
            .as_ref()
            .filter(|_| reply.replace)
            .map(|(id, _)| *id)
            .filter(|id| self.editor.intact_start(*id).is_some());
        let id = match replaced {
            Some(id) => {
                self.editor.replace_with_template(id, &text);
                self.selection_dirty = true;
                Some(id)
            }
            None => {
                // 答えは遅れて届くので、その間に選んだ範囲を上書きしない(選択はカーソルに畳む)。
                let (a, b) = self.editor.selection;
                let end = a.max(b);
                self.editor.selection = (end, end);
                self.insert_template(&text, None);
                self.editor.utterances.last().map(|u| u.id)
            }
        };
        self.requests.last_completion = id.map(|id| (id, text));
        let seconds = took.as_secs_f32();
        diag::log(format!(
            "補完: テンプレート({} 字)を{}({seconds:.1} 秒)",
            template.chars().count(),
            if replaced.is_some() { "置き換えた" } else { "入れた" }
        ));
        self.notify(if replaced.is_some() {
            format!("テンプレート「{template}」を補完し直しました({seconds:.1} 秒)")
        } else {
            format!("テンプレート「{template}」を補完しました({seconds:.1} 秒)")
        });
    }

    /// 呼びかけの待ちと、送った依頼を取りやめる(Esc)。送ってしまった依頼の答えは、戻っても捨てる。
    /// 取りやめるものがあれば true。
    pub(super) fn cancel_request(&mut self) -> bool {
        if self.requests.call_waiting.take().is_some() {
            diag::log("呼びかけ: Esc で待ちをやめた");
            self.notify("依頼の待ちを取りやめました");
            return true;
        }
        if !self.completing() && self.requests.held.is_none() {
            return false;
        }
        if self.requests.held.take().is_some() {
            self.requests.began = None;
        } else if self.requests.began.take().is_some() {
            self.requests.discarded += 1;
        }
        if self.live_caption.starts_with("依頼: ") {
            self.live_caption.clear();
        }
        diag::log("依頼: Esc で取りやめた");
        self.notify("Claude への依頼を取りやめました");
        true
    }
}
