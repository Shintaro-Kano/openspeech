//! 転送の許可、転送の依頼と結果、転送履歴。

use super::*;

/// 転送の口と途中の状態、転送の待ちに置いた発話。
pub(super) struct TransferState {
    /// None の間は他のアプリへ何も送らない(試験用)。
    pub(super) jobs: Option<Sender<TransferJob>>,
    pub(super) tx: Sender<TransferDone>,
    pub(super) rx: Receiver<TransferDone>,
    /// 音声の「転送」で未確定の発話を待っているとき。
    pub(super) voice: Option<VoiceTransfer>,
    /// エディターの文を転送している途中の数。
    pub(super) in_flight: usize,
    /// 転送を始めた時刻(途中のとき)。
    pub(super) began: Option<Instant>,
    /// 転送の処理役が返らないので、発話の受け取りに戻った。
    pub(super) stuck: bool,
    /// エディターの先頭だけを送っている途中(後ろに「転送」のあとに話した文が残っている)。
    pub(super) sending_head: bool,
    /// 発話の待ち行列。発話はすべてここを通り、転送の待ちと途中は置いたままにする
    /// (先に入れると、転送した分と一緒に送ったり、消す分と合わなくなったりする)。
    pub(super) held: Vec<HeldUtterance>,
}

impl TransferState {
    /// エディターの文を転送している途中か(処理役が返らず待つのをやめた分は数えない)。
    /// 訂正・外部認識・補完の当て方と発話の受け取りがこれを見て止まる。
    pub(super) fn sending(&self) -> bool {
        self.in_flight > 0 && !self.stuck
    }

    /// 音声の転送の待ちか、エディターの文の転送の途中か。
    pub(super) fn busy(&self) -> bool {
        self.voice.is_some() || self.sending()
    }

    pub(super) fn new() -> Self {
        let (tx, rx) = mpsc::channel();
        TransferState {
            jobs: None,
            tx,
            rx,
            voice: None,
            in_flight: 0,
            began: None,
            stuck: false,
            sending_head: false,
            held: Vec::new(),
        }
    }
}

impl VoiceApp {
    // ---- アクセシビリティ(転送の許可) ----

    /// 転送に必要なアクセシビリティの許可があるかを確かめる(間隔を置いて)。
    pub fn check_accessibility(&mut self, force: bool) {
        let due = force
            || self
                .accessibility_checked_at
                .is_none_or(|at| Instant::now() >= at + ACCESSIBILITY_CHECK_INTERVAL);
        if !due {
            return;
        }
        let before = self.accessibility_ok;
        let first_check = self.accessibility_checked_at.is_none();
        self.accessibility_ok = transfer::accessibility_trusted();
        self.accessibility_checked_at = Some(Instant::now());
        if before != self.accessibility_ok {
            diag::log(format!("アクセシビリティ許可: {}", self.accessibility_ok));
            if self.accessibility_ok {
                self.windows.accessibility_help = false;
                // 起動時に許可があるのは普通のことなので知らせない。
                if !first_check {
                    self.notify("転送の許可を確認しました");
                }
            }
        }
    }

    /// Mac の確認画面を出し、システム設定のアクセシビリティも開く。
    pub fn ask_accessibility(&mut self) {
        transfer::request_accessibility();
        if let Err(message) = transfer::open_accessibility_settings() {
            self.notify_error(message);
        }
    }

    // ---- 転送 ----

    pub(super) fn finish_transfer(&mut self, done: TransferDone) {
        if done.editor_snapshot.is_some() {
            self.transfers.in_flight = self.transfers.in_flight.saturating_sub(1);
        }
        match done.result {
            Ok(delivery) => {
                self.last_delivery = Some(delivery);
                match done.warning {
                    Some(warning) => {
                        self.notify_error(format!("{}({})", warning.message(), delivery.label()));
                    }
                    None => self.notify(format!("転送しました({})", delivery.label())),
                }
                if let Some(snapshot) = done.editor_snapshot {
                    self.clear_sent_text(&snapshot);
                }
            }
            Err(error) => {
                if error == TransferError::NotTrusted {
                    self.accessibility_ok = false;
                    self.windows.accessibility_help = true;
                }
                // 送れなかった文はエディターに残る。続きの「転送」を働かせると、一緒に送ってしまう。
                if done.editor_snapshot.is_some() {
                    self.transfers.voice = None;
                    self.drop_spoken_transfers();
                    for held in &mut self.transfers.held {
                        held.transfer_cancelled = true;
                    }
                }
                self.notify_error(error.to_string());
            }
        }
        if self.transfers.in_flight == 0 {
            self.transfers.stuck = false;
            self.transfers.sending_head = false;
            self.transfers.began = None;
        }
    }

    /// 転送の処理役が返らなくても、発話を置いたままにしない。時刻を受けるのは、試験で上限まで進めるため。
    pub fn watch_transfer(&mut self, now: Instant) {
        if self
            .transfers.began
            .is_some_and(|at| now >= at + TRANSFER_STUCK)
        {
            diag::log("転送が終わらないため、待つのをやめる");
            self.transfers.stuck = true;
            self.transfers.began = None;
            self.notify_error("転送が終わりません。転送先のアプリを確かめてください");
        }
    }

    /// 転送した分だけエディターから消す。転送中に話した続きは残す。
    pub fn clear_sent_text(&mut self, sent: &str) {
        if self.editor.text == sent {
            self.editor.delete_all();
        } else if self.editor.text.starts_with(sent) {
            self.editor.remove_head(sent.chars().count());
        } else {
            self.notify("転送中に文章が変わったため、エディターの内容は残しました");
        }
        self.selection_dirty = true;
    }

    /// エディターを空にする。文字が残っていれば設定に従って確認する。
    pub fn request_close_editor(&mut self) {
        if self.editor.is_empty() || !self.data.settings.editor.confirm_close {
            self.editor.delete_all();
            self.selection_dirty = true;
        } else {
            self.windows.confirm_close = true;
        }
    }

    // ---- 転送 ----

    pub fn current_target(&self) -> Option<Target> {
        self.tracker.current()
    }

    /// ⌘Enter と「転送」ボタン。画面を見て押すので待たずに送る。送っている途中は二重に送らない。
    pub fn transfer(&mut self) {
        if self.transfer_sending() {
            return;
        }
        self.transfers.voice = None;
        self.drop_spoken_transfers();
        let target = self.current_target();
        self.transfer_to(target, false);
    }

    /// Esc と「取り消し」。音声の転送を待っていれば取りやめ、次に呼びかけの待ちと送った依頼を取りやめ、
    /// どれも無ければ直前の発話を消す。送っている途中は文を変えない(消す分と合わなくなる)。
    pub fn take_back(&mut self) {
        if self.transfer_sending() {
            return;
        }
        if self.cancel_voice_transfer("転送を取りやめました") || self.cancel_request() {
            return;
        }
        self.editor.delete_last_utterance();
        self.selection_dirty = true;
    }

    /// 利用者が手で動いたので、訂正のあとで「転送」と分かっても働かせない(手で直している途中の文が送られる)。
    pub(super) fn drop_spoken_transfers(&mut self) {
        for unconfirmed in &mut self.corrections.unconfirmed {
            unconfirmed.transfer_cancelled = true;
        }
    }

    /// 待っている音声の転送を取りやめる。待っていたら true。
    /// 置いてある発話の「転送」も働かせない(解放されると、取りやめた文が送られる)。
    pub(super) fn cancel_voice_transfer(&mut self, message: &str) -> bool {
        self.drop_spoken_transfers();
        let waiting = self.transfers.voice.take().is_some();
        if waiting {
            for held in &mut self.transfers.held {
                held.transfer_cancelled = true;
            }
            diag::log(message);
            self.notify(message);
        }
        waiting
    }

    /// 画面で手入力された結果を取り込む。音声の転送を待っているなら、書きかけを送らないよう取りやめる。
    pub fn sync_editor_from_input(&mut self, text: &str, selection: (usize, usize)) {
        if text != self.editor.text {
            self.cancel_voice_transfer("手で直したので、転送を取りやめました");
        }
        self.editor.sync_from_input(text, selection);
    }

    /// 音声の「転送」。話した文が確定するのを待ってから送る
    /// (文末の「転送」は訂正を依頼した直後に働くので、待たないと認識したままの文が黙って送られる)。
    /// 待ちの長さは発話の上限(`CORRECTION_WAIT`)の残りで決まる。
    pub(super) fn transfer_after_corrections(&mut self, target: Option<Target>) {
        // 先に頼んだ補完(「ヘイクロード…」→「転送」)も、戻ってから一緒に送る。
        if self.editor.awaiting_correction() || self.completing() {
            self.transfers.voice = Some(VoiceTransfer { target });
        } else {
            self.transfer_to(target, true);
        }
    }

    /// 未確定の発話が無くなったら、待っていた音声の転送を送る。先頭だけ送っている途中なら、終わるまで待つ。
    pub(super) fn finish_voice_transfer(&mut self) {
        // 「転送」より先に頼んだ補完は、戻ってから一緒に送る(答えの上限は API 15 秒・Claude Code 25 秒)。
        if self.editor.awaiting_correction() || self.transfer_sending() || self.completing() {
            return;
        }
        if let Some(waiting) = self.transfers.voice.take() {
            self.transfer_to(waiting.target, true);
        }
    }

    /// 訂正のあとで分かった「転送」(`id` の発話の、位置 `end` までが話した文)。
    /// 後ろの発話は「転送」のあとに話した分なので送らない(次の報告を話し始めていることがある)。
    /// 待っている音声の転送があれば、それは後の発話のものなので、先に話した分だけをここで切り分けて送る。
    pub(super) fn transfer_corrected(&mut self, id: u64, end: usize, spoken: Option<Unconfirmed>) {
        let next = self.next_utterance_start(id);
        let spoken_end = self
            .editor
            .utterances
            .iter()
            .filter(|utterance| utterance.id < id)
            .map(|utterance| utterance.end)
            .fold(end, usize::max);
        let refused = match &spoken {
            // 話してから時間がたっているので送らない。手直しの途中の文が出たり、転送先の画面が変わっていたりする。
            None if next.is_some() => Some(
                "訂正が遅れたので「転送」は働かせませんでした。後の文と混ざるので、確かめて手で送ってください",
            ),
            None => Some("訂正が遅れたので「転送」は働かせませんでした。送るときはもう一度言ってください"),
            Some(spoken) if spoken.transfer_cancelled => {
                Some("取りやめや手の操作のあとなので「転送」は働かせませんでした")
            }
            // 文の並びが話した順と違うと切り分けられない。後の文を混ぜて送るより、送らない。
            Some(_) if next.is_some_and(|start| start < spoken_end) => Some(
                "文の並びが話した順と違うので「転送」は働かせませんでした。確かめて手で送ってください",
            ),
            Some(_) => None,
        };
        if let Some(message) = refused {
            self.refuse_transfer(message, next);
            return;
        }
        let Some(spoken) = spoken else {
            return;
        };
        match next {
            Some(start) => self.send_head(start, spoken.target, true),
            // 同じ文を送る「転送」が待っていても、送り終えれば文が無いので二重にならない。
            None => self.transfer_to(spoken.target, true),
        }
    }

    /// 発話 `id` のあとに話した発話のうち、いちばん手前の始まりの位置。
    pub(super) fn next_utterance_start(&self, id: u64) -> Option<usize> {
        self.editor
            .utterances
            .iter()
            .filter(|utterance| utterance.id > id)
            .map(|utterance| utterance.start)
            .min()
    }

    /// 訂正のあとの「転送」を働かせないと決めた。`next` は後の発話の位置。
    pub(super) fn refuse_transfer(&mut self, message: &str, next: Option<usize>) {
        match (next, self.transfers.voice.is_some()) {
            // 待っている転送はこの文だけを送るので、そのまま任せる。
            (None, true) => {}
            // 待っている転送は後の文と一緒に送ってしまうので止める。
            (Some(_), true) => {
                self.cancel_voice_transfer(message);
            }
            (_, false) => self.notify(message),
        }
    }

    /// `spoken` は音声の転送か。画面を見ていないので、AI 訂正を通っていない文を送ったら知らせる。
    pub(super) fn transfer_to(&mut self, target: Option<Target>, spoken: bool) {
        self.send_head(self.editor.len(), target, spoken);
    }

    /// エディターの先頭から位置 `end` までを送る。
    pub(super) fn send_head(&mut self, end: usize, target: Option<Target>, spoken: bool) {
        if end == 0 {
            return;
        }
        // 返っていない転送の後ろに並べると、回復したときに同じ文が二重に届く。
        if self.transfers.in_flight > 0 {
            self.notify_error("前の転送が終わっていません。転送先のアプリを確かめてください");
            return;
        }
        let warning = if spoken { self.send_warning(end) } else { None };
        self.learn_manual_edits();
        self.transfers.sending_head = end < self.editor.len();
        let text = self.editor.slice(0, end);
        self.send_text(text, target, warning);
    }

    /// 先頭から `end` までを音声で送るときに伝えること。
    pub(super) fn send_warning(&self, end: usize) -> Option<SendWarning> {
        self.editor
            .has_uncorrected(end)
            .then_some(SendWarning::Uncorrected)
    }

    pub fn retransfer(&mut self) {
        if self.transfers.in_flight > 0 {
            self.notify_error("前の転送が終わっていません。転送先のアプリを確かめてください");
            return;
        }
        match self.history.latest().map(|e| e.text.clone()) {
            Some(text) => {
                let target = self.current_target();
                self.send_raw(text, None, target, None);
            }
            None => self.notify("再転送できる履歴がありません"),
        }
    }

    pub(super) fn send_text(&mut self, text: String, target: Option<Target>, warning: Option<SendWarning>) {
        let filtered = apply_transfer_filters(&text, &self.data.settings.filters);
        self.record_history(&filtered);
        self.send_raw(filtered, Some(text), target, warning);
    }

    /// 転送を順番待ちに入れる。`editor_snapshot` があれば成功後にその分を消す。
    pub fn send_raw(
        &mut self,
        text: String,
        editor_snapshot: Option<String>,
        target: Option<Target>,
        warning: Option<SendWarning>,
    ) {
        if let Some(target) = &target {
            // 自分が前面にいるときは、入力先へ前面を譲る宣言をしてから渡す(主スレッドでしか呼べない)。
            transfer::yield_activation(target.pid);
        }
        let from_editor = editor_snapshot.is_some();
        let settings = &self.data.settings.transfer;
        let job = TransferJob {
            text,
            target,
            method: settings.method,
            restore_clipboard: settings.restore_clipboard,
            enter_after: settings.enter_after,
            editor_snapshot,
            warning,
        };
        let sent = self
            .transfers.jobs
            .as_ref()
            .is_some_and(|jobs| jobs.send(job).is_ok());
        if sent && from_editor {
            self.transfers.in_flight += 1;
            self.transfers.began.get_or_insert_with(Instant::now);
        } else if !sent && self.transfers.jobs.is_some() {
            self.notify_error("転送の処理が止まっています。アプリを開き直してください");
        }
    }

    pub(super) fn record_history(&mut self, text: &str) {
        let entry = history::new_entry(text);
        if self.data.settings.save_history {
            let folder = self.store.history_dir();
            if let Err(error) = history::append_log(&folder, &entry) {
                self.notify_error(format!("転送履歴を保存できません: {error}"));
            }
            history::prune(&folder, entry.date);
        }
        self.history.push(entry);
    }

    /// 転送履歴の日別ファイルと、画面の一覧をすべて消す(設定の「履歴とログ」)。
    /// 消せなかったファイルがあっても画面の一覧は消し、残った分を読み直す。
    pub fn clear_history(&mut self) {
        let folder = self.store.history_dir();
        let result = history::clear(&folder);
        self.history = History::load(&folder, self.data.settings.history_count);
        match result {
            Ok(removed) => {
                diag::log(format!("転送履歴を消した({removed} ファイル)"));
                self.notify(format!("転送履歴を消しました({removed} ファイル)"));
            }
            Err(message) => self.notify_error(message),
        }
    }
}
