//! 「ヘイクロード」の依頼(テンプレートの補完と文章の手直し)の流れ。

use super::*;
use crate::core::assistant::{check_edit, edit_changes, EditCheck, ReplyKind, Target, MAX_TARGET_CHARS};

/// AI 訂正を待っている発話があるときに、依頼を送らずに待つ上限。訂正は発話ごとに `CORRECTION_WAIT`(外部 API が
/// あればその分を足す)で確定するので、ふつうはその前に送る。呼びかけの待ち(`CALL_WAIT`、15 秒)と同じ長さ。
const REQUEST_CORRECTION_LIMIT: Duration = CALL_WAIT;

/// 声も処理中の発話も見えなくなってから、まだ声があるとみなす長さ。声と処理中の発話のどちらも見えない
/// 隙間をまとめて覆う: マイクを止めると声の印はすぐ消えるが、録音役は尾(600 ms)を録り終えてから発話を
/// 処理中に数える。認識役は処理中の数を減らしてから認識文を送る。送る(切れる)のが最長 1 秒遅れるだけ。
const CALL_QUIET_GRACE: Duration = Duration::from_secs(1);

/// 聞き取りの時間が来たあと、声が続いていても待つ上限。止まらない雑音で送れなくならないよう打ち切る
/// (時間が来たときに話していた発話は、上限で切る長さの 25 秒まで続きうる)。
const LISTEN_OVERRUN_LIMIT: Duration = Duration::from_secs(30);

/// 聞き取りを早く終える語。発話全体(前後の句読点・空白を除く)がこの語なら、溜めた文をすぐ送る。
/// 文末の「以上」は見ない: 「短径1センチ以上」「3個以上」のように "or more" の意味で依頼に入る。
const REQUEST_END_WORDS: [&str; 2] = ["以上", "以上です"];

/// 呼びかけだけを聞いたあとの、依頼の待ち(話し始めるまで)と聞き取り(話し始めてから設定の秒数)。
pub(super) struct CallWait {
    /// 話し始めを待つ期限(呼びかけから `CALL_WAIT`)。
    deadline: Instant,
    /// 聞き取る長さ(呼びかけの時点の設定)。
    listen_for: Duration,
    /// 聞き取りを始めた時刻: 見張りが声か処理中の発話を初めて見た時刻(見ないまま発話が届けばその時刻)。
    /// 話し始めを待っている間は None。
    listen_from: Option<Instant>,
    /// 最後に声か処理中の発話を見た時刻。
    last_voice: Option<Instant>,
    /// 聞き取った依頼の文(発話ごと)。
    parts: Vec<String>,
    /// 聞き取りの間に届いた発話の数(最初の発話だけ、命令語などなら待ちをやめて普通に働かせる)。
    heard: usize,
}

impl CallWait {
    fn new(now: Instant, listen_for: Duration) -> Self {
        CallWait {
            deadline: now + CALL_WAIT,
            listen_for,
            listen_from: None,
            last_voice: None,
            parts: Vec::new(),
            heard: 0,
        }
    }

    /// 見張りが見た、声か処理中の発話があるかを控える(最後に見た時刻は戻さない)。
    /// 初めて見たら聞き取りを始め、true を返す。
    fn note(&mut self, voice: bool, now: Instant) -> bool {
        if !voice {
            return false;
        }
        self.last_voice = Some(self.last_voice.map_or(now, |seen| seen.max(now)));
        self.start_listening(now)
    }

    /// 聞き取りを始める(始めていなければ)。始めたら true。
    fn start_listening(&mut self, now: Instant) -> bool {
        if self.listen_from.is_some() {
            return false;
        }
        self.listen_from = Some(now);
        true
    }

    /// 聞き取りが終わる時刻。話し始めを待っている間は None。
    fn listen_end(&self) -> Option<Instant> {
        self.listen_from.map(|from| from + self.listen_for)
    }

    /// 声か処理中の発話がある: いまの状態 `voice`、または最後に見てから `CALL_QUIET_GRACE` たっていない。
    fn hearing(&self, voice: bool, now: Instant) -> bool {
        voice || self.last_voice.is_some_and(|seen| now < seen + CALL_QUIET_GRACE)
    }

    /// 待ちが切れたか: 話し始めないまま期限を過ぎ、声も処理中の発話も無い。
    fn lapsed(&self, now: Instant, voice: bool) -> bool {
        self.listen_from.is_none() && now >= self.deadline && !self.hearing(voice, now)
    }

    /// 聞き取りを終えて送るときか: 時間が来ていて、声も処理中の発話も無い。続いていればその発話が届くまで
    /// 待つが、時間から `LISTEN_OVERRUN_LIMIT` で打ち切る。
    fn due(&self, now: Instant, voice: bool) -> bool {
        self.listen_end().is_some_and(|end| {
            now >= end && (!self.hearing(voice, now) || now >= end + LISTEN_OVERRUN_LIMIT)
        })
    }
}

/// 発話全体が聞き取りを終える語か(前後の句読点・空白を除いて「以上」「以上です」と一致)。
fn is_end_word(text: &str) -> bool {
    let body = text.trim_matches(|c: char| c.is_whitespace() || "、。,.，．!?！？…・".contains(c));
    REQUEST_END_WORDS.contains(&body)
}

/// 状態の文字に出す秒数: 切り上げで、0 は出さない(見張りと描画は別々に時刻を取るので、時間が来る直前のフレームでも 1)。
fn seconds_up(left: Duration) -> u64 {
    u64::try_from(left.as_millis().div_ceil(1000))
        .unwrap_or(u64::MAX)
        .max(1)
}

/// 送った依頼の控え(答えを当てるときに見る)。
pub(super) struct SentRequest {
    /// 依頼の文(手直しの左右の門で見る)。
    request: String,
    /// 添えた対象の文章の範囲(字の位置)と文。エディターが空で添えなかったら None。
    target: Option<(usize, usize, String)>,
}

/// 直前の手直し(Esc で戻すための控え)。
pub(super) struct LastEdit {
    /// 手直しの答えを入れた発話の番号。
    id: u64,
    start: usize,
    /// 当てた文。
    applied: String,
    /// 置き換える前の文。
    original: String,
}

/// 「ヘイクロード」の補完役と、依頼の待ちと答えの控え。
pub(super) struct RequestState {
    /// 「ヘイ、クロード」のテンプレート補完役。Claude に接続していなければ None。
    pub(super) assistant: Option<Arc<dyn Assistant>>,
    pub(super) jobs: Option<Sender<AssistantJob>>,
    pub(super) tx: Sender<AssistantDone>,
    pub(super) rx: Receiver<AssistantDone>,
    /// 呼びかけだけを聞いて、依頼の話し始めを待っているか、依頼を聞き取っている間。
    pub(super) call_waiting: Option<CallWait>,
    /// 依頼を Claude に送った時刻(答えを待っている間)。
    pub(super) began: Option<Instant>,
    /// 直前に補完した発話(番号と本文)。手付かずで残っていれば、次の依頼で言い直しとして置き換える。
    pub(super) last_completion: Option<(u64, String)>,
    /// 転送の途中に戻った補完の答え(送り終えてから当てる)。
    pub(super) held: Option<AssistantDone>,
    /// Esc で取りやめた依頼の数。その分の答えは捨てる。
    pub(super) discarded: u32,
    /// 答えを待っている依頼の控え。
    pub(super) sent: Option<SentRequest>,
    /// 直前の手直し。直後の Esc で戻す。別の入力が入ったら捨てる。
    pub(super) last_edit: Option<LastEdit>,
    /// AI 訂正を待っている発話があるので、送らずに控えている依頼の文と、控えた時刻。訂正が終わるか
    /// `REQUEST_CORRECTION_LIMIT` たったら送る(対象の文章を訂正後の文にするため)。
    pub(super) pending: Option<(String, Instant)>,
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
            sent: None,
            last_edit: None,
            pending: None,
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

    /// 補完役に渡している指示文(試験で、依頼の指示が届いたかを見る)。使えないときは None。
    pub fn assistant_instructions(&self) -> Option<String> {
        self.requests.assistant.as_ref().map(|a| a.instructions().to_string())
    }

    /// 呼びかけだけを聞いて、依頼の話し始めを待っているか、依頼を聞き取っているか。
    pub fn call_waiting(&self) -> bool {
        self.requests.call_waiting.is_some()
    }

    /// 依頼を聞き取っている途中か(話し始めてから送るまで)。
    pub fn request_listening(&self) -> bool {
        self.requests
            .call_waiting
            .as_ref()
            .is_some_and(|wait| wait.listen_from.is_some())
    }

    /// 依頼を Claude に送っている途中か。
    pub fn completing(&self) -> bool {
        self.requests.began.is_some()
    }

    /// 話し始めを待っている間の、期限までの残り秒数(切り上げ、最小 1)。待っていなければ None。
    pub fn call_wait_seconds_left(&self, now: Instant) -> Option<u64> {
        self.requests
            .call_waiting
            .as_ref()
            .filter(|wait| wait.listen_from.is_none())
            .map(|wait| seconds_up(wait.deadline.saturating_duration_since(now)))
    }

    /// 聞き取りの残り秒数(切り上げ、最小 1)。聞き取っていない、または時間が来て声を待っている間は None。
    pub fn listen_seconds_left(&self, now: Instant) -> Option<u64> {
        self.requests
            .call_waiting
            .as_ref()
            .and_then(CallWait::listen_end)
            .filter(|end| now < *end)
            .map(|end| seconds_up(end - now))
    }

    /// 依頼を Claude に送ってからの経過。送っていなければ None。
    pub fn completing_for(&self, now: Instant) -> Option<Duration> {
        self.requests
            .began
            .map(|began| now.saturating_duration_since(began))
    }

    /// 声がしているか、まだエディターに届いていない発話があるか(文字にしている途中、送っている間に置いてある)。
    fn voice_in_flight(&self) -> bool {
        let live = &self.session.live;
        live.speaking.load(std::sync::atomic::Ordering::Relaxed) || live.pending() > 0 || !self.transfers.held.is_empty()
    }

    /// 依頼の待ちが切れたか: 話し始めないまま期限を過ぎ、声も処理中の発話も無い。毎フレームの見張り
    /// (`watch_call`)も、待ちの間に届いた発話(`take_call`)もこれで決める。見張りが声を見た時点で聞き取りに
    /// 移るので、話し始めが期限内なら話し終わりが過ぎても切らない。見張りが一度も動かないまま期限後に発話が
    /// 届くと(描画が止まっていたあとの最初のフレーム)、期限内に話し始めた依頼でも口述になる(以前と同じ)。
    fn call_expired(&self, now: Instant) -> bool {
        self.requests
            .call_waiting
            .as_ref()
            .is_some_and(|wait| wait.lapsed(now, self.voice_in_flight()))
    }

    /// 発話が呼びかけか。頭のフィラー(「えー、ヘイクロード」)は整形で落ちるので、当たらなければ整形後の文でも見る。
    fn call_in(&self, text: &str) -> Call {
        let call_words = &self.data.settings.call_words;
        match match_call(text, call_words) {
            Call::None => {
                let filtered = apply_prefilters(text, &self.data.settings.filters, &self.data.dictionary);
                match_call(&filtered, call_words)
            }
            call => call,
        }
    }

    /// 呼びかけ語の判定(判定 1 の前)。依頼として扱ったら true。
    /// 呼びかけに続けて言った依頼はすぐ送る。呼びかけだけなら `CALL_WAIT` のうちに話し始めるのを待ち、
    /// 話し始めてから設定の秒数(`request_listen`)の間に届いた発話をまとめて 1 つの依頼にする(`listen_to`)。
    pub(super) fn take_call(&mut self, recognition: &mut Recognition) -> bool {
        if self.requests.call_waiting.is_some() {
            if let Some(taken) = self.listen_to(recognition) {
                return taken;
            }
        }
        match self.call_in(&recognition.text) {
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
                diag::log("呼びかけ: 依頼の話し始めを待つ");
                self.requests.call_waiting =
                    Some(CallWait::new(Instant::now(), self.data.settings.request_listen()));
                self.notify("Claude への依頼をどうぞ");
                true
            }
            Call::None => false,
        }
    }

    /// 呼びかけのあとに届いた発話を聞き取る。依頼として溜めたら Some(true)、待ちをやめて普通の経路で
    /// 働かせるなら Some(false)、呼びかけ語の判定から始めるなら None(待ちが切れていた、最初の発話が呼びかけ直し)。
    /// 聞き取りの間は命令語・記号の読み・「〜テンプレート」も依頼の一部として溜める(働かせない)。発話全体が
    /// 「以上」「以上です」なら、溜めた文をすぐ送る(文末の「以上」は依頼の一部として溜める)。
    fn listen_to(&mut self, recognition: &Recognition) -> Option<bool> {
        let now = Instant::now();
        if self.call_expired(now) {
            self.requests.call_waiting = None;
            return None;
        }
        let call = self.call_in(&recognition.text);
        let first = self.requests.call_waiting.as_ref()?.heard == 0;
        if first {
            if !matches!(call, Call::None) {
                return None;
            }
            // 最初の発話が命令語・記号の読み・「〜テンプレート」(文末の命令語が続いてもよい)なら依頼ではない。
            // 待ちをやめて、普通の経路で働かせる。
            if self.is_not_a_request(&recognition.text) {
                diag::log("呼びかけ: 命令語・記号・テンプレートの呼び出しが来たので待ちをやめる");
                self.requests.call_waiting = None;
                return Some(false);
            }
        }
        // 聞き取りの途中で呼びかけ語から言い直したら、語のあとの文だけを溜める。
        let spoken = match call {
            Call::WithRequest(rest) => rest,
            Call::Alone => String::new(),
            Call::None => recognition.text.clone(),
        };
        let end = is_end_word(&spoken);
        let part = if end { String::new() } else { spoken };
        let wait = self.requests.call_waiting.as_mut()?;
        if wait.start_listening(now) {
            diag::log("呼びかけ: 声を見ないまま発話が届いたので、ここから聞き取る");
        }
        wait.heard += 1;
        let part = part.trim();
        if !part.is_empty() {
            wait.parts.push(part.to_string());
        }
        let count = wait.parts.len();
        let heard = wait.parts.join(" ");
        if !heard.is_empty() {
            self.live_caption = format!("依頼: {heard}");
        }
        diag::log(format!(
            "呼びかけ: 依頼を聞き取った({count} 件{})",
            if end { "。「以上」で送る" } else { "" }
        ));
        if end {
            self.finish_listening();
        }
        Some(true)
    }

    /// 聞き取りを終え、溜めた文を空白 1 つでつないで 1 つの依頼として送る。空なら口述に戻ったと知らせる。
    fn finish_listening(&mut self) {
        let Some(wait) = self.requests.call_waiting.take() else {
            return;
        };
        if self.live_caption.starts_with("依頼: ") {
            self.live_caption.clear();
        }
        let text = wait.parts.join(" ");
        if text.trim().is_empty() {
            diag::log("呼びかけ: 聞き取りの間に依頼の文が無かった");
            self.notify("依頼がなかったので、口述に戻りました");
            return;
        }
        diag::log(format!("呼びかけ: 聞き取った {} 件をまとめて送る", wait.parts.len()));
        self.begin_request(&text);
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

    /// 依頼を受けられない理由を知らせる(Claude に接続していない、テンプレートも手直しの対象の文章も無い、
    /// 前の依頼の処理中)。受けられなければ true。呼びかけだけのときも見るので、待ってから次の発話を食べて断ることはない。
    pub(super) fn request_blocked(&mut self) -> bool {
        if self.requests.assistant.is_none() {
            diag::log("呼びかけ: Claude に接続していない");
            self.notify_error(
                "設定の「AI 訂正」で Claude に接続すると、「ヘイクロード」で依頼できます",
            );
            return true;
        }
        // テンプレートが無くても、エディターに文があれば手直しを頼める。
        if self.data.dictionary.templates.is_empty() && self.editor.is_empty() {
            diag::log("呼びかけ: テンプレートが無い");
            self.notify_error(
                "テンプレートが登録されていません。「辞書」の「テンプレート」か、辞書ファイル(dictionary.json)の templates に登録してください",
            );
            return true;
        }
        if self.completing() || self.requests.pending.is_some() {
            self.notify("前の依頼を処理しています。終わってからもう一度どうぞ");
            return true;
        }
        false
    }

    /// 依頼の待ちと聞き取りを見張る(毎フレーム)。声か処理中の発話を見たら控え、待ちの間なら聞き取りを始める。
    /// 待ちが切れたら(`call_expired`)口述に戻って知らせ、聞き取りの時間が来たら溜めた文を送る。
    /// 時刻を受けるのは、試験で時間を進めるため。
    pub fn watch_call(&mut self, now: Instant) {
        self.release_pending_request(now);
        let voice = self.voice_in_flight();
        let Some(wait) = self.requests.call_waiting.as_mut() else {
            return;
        };
        if wait.note(voice, now) {
            diag::log(format!("呼びかけ: 話し始めたので {} 秒聞き取る", wait.listen_for.as_secs()));
        }
        let due = wait.due(now, voice);
        if self.call_expired(now) {
            self.requests.call_waiting = None;
            diag::log("呼びかけ: 依頼がないまま期限を過ぎた");
            self.notify("依頼がなかったので、口述に戻りました");
        } else if due {
            diag::log("呼びかけ: 聞き取りの時間が来た");
            self.finish_listening();
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
        // 対象の文章は訂正後の文にする(認識したままの文で送ると、訂正が先に戻って手直しが当たらない)。
        // 音声の「転送」が訂正を待つのと同じく、訂正が終わってから送る(`release_pending_request`)。
        if self.editor.awaiting_correction() {
            diag::log("依頼: AI 訂正を待ってから送る");
            self.live_caption = format!("依頼: {text}(訂正を待っています)");
            self.requests.pending = Some((text, Instant::now()));
            return;
        }
        self.send_request(text);
    }

    /// 控えている依頼を、訂正が終わったか上限が過ぎたら送る(毎フレーム、`watch_call` から)。
    fn release_pending_request(&mut self, now: Instant) {
        let Some((_, since)) = &self.requests.pending else {
            return;
        };
        let waited = now.saturating_duration_since(*since);
        if self.editor.awaiting_correction() && waited < REQUEST_CORRECTION_LIMIT {
            return;
        }
        let Some((text, _)) = self.requests.pending.take() else {
            return;
        };
        if self.editor.awaiting_correction() {
            diag::log("依頼: 訂正の待ちの上限を過ぎたので、今の文で送る");
        }
        self.send_request(text);
    }

    /// 依頼に添える対象の文章: 選択範囲(空白だけでなければ)、無ければ全文。長ければ先頭 `MAX_TARGET_CHARS` 字。
    /// エディターが空なら None。範囲(字の位置)と一緒に返す。
    fn request_target(&self) -> Option<(usize, usize, Target)> {
        let (a, b) = self.editor.selection;
        let (from, to) = (a.min(b), a.max(b));
        let selected = from < to && !self.editor.slice(from, to).trim().is_empty();
        let (start, end) = if selected { (from, to) } else { (0, self.editor.len()) };
        let cut = start + (end - start).min(MAX_TARGET_CHARS);
        let text = self.editor.slice(start, cut);
        if text.trim().is_empty() {
            return None;
        }
        let target = Target {
            text,
            selected,
            truncated: cut < end,
        };
        Some((start, cut, target))
    }

    /// 依頼を Claude に送る。直前に補完した文が手付かずで残っていれば添える(言い直し用)。対象の文章(選択範囲か
    /// 全文)も添え、答えが手直しだったときに当てる範囲を控える。
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
        let target = self.request_target();
        diag::log(format!(
            "依頼 {} 字を送る(直前の補完{}、対象 {})",
            text.chars().count(),
            if previous.is_some() { "あり" } else { "なし" },
            target.as_ref().map_or("なし".to_string(), |(_, _, t)| format!(
                "{}の {} 字{}",
                if t.selected { "選択範囲" } else { "全文" },
                t.text.chars().count(),
                if t.truncated { "(先頭だけ)" } else { "" }
            ))
        ));
        let sent = SentRequest {
            request: text.clone(),
            target: target.as_ref().map(|(start, end, t)| (*start, *end, t.text.clone())),
        };
        let request = AssistantRequest {
            request: text.clone(),
            previous,
            target: target.map(|(_, _, t)| t),
        };
        if jobs.send(AssistantJob { request, assistant }).is_err() {
            self.notify_error("Claude への依頼を送れませんでした");
            return;
        }
        self.requests.sent = Some(sent);
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
        let sent = self.requests.sent.take();
        let reply = match result {
            Ok(reply) => reply,
            Err(message) => {
                self.notify_error(message);
                return;
            }
        };
        if reply.kind == ReplyKind::Edit {
            if let Some(text) = reply.text.clone() {
                self.receive_edit(sent, &text, reply.reply.as_deref(), took);
                return;
            }
        }
        let Some(text) = reply.text.filter(|_| reply.kind == ReplyKind::Template) else {
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

    /// 手直しの答えを当てる。門を順に通し(対象が無い・その間に文章が変わった・左右・長さ・同じ文)、通れば
    /// 対象の範囲を 1 つの fixed 発話(学習の比較対象にしない)に置き換え、変わった字に色を付け、カーソルを末尾に置く。
    /// 直後の Esc で戻せる。
    fn receive_edit(&mut self, sent: Option<SentRequest>, text: &str, reply: Option<&str>, took: Duration) {
        let seconds = took.as_secs_f32();
        let Some(SentRequest { request, target: Some((start, end, target)) }) = sent else {
            diag::log("手直し: 対象の文章を添えていない");
            self.notify("対象の文章がありません");
            return;
        };
        if self.editor.slice(start, end) != target {
            diag::log("手直し: 送ったあとに文章が変わったので当てない");
            self.notify("文章が変わったので手直しを当てませんでした");
            return;
        }
        let applied = match check_edit(&request, &target, text) {
            EditCheck::Refused(why) => {
                diag::log(format!("手直し: 使わない({why})"));
                self.notify_error(why);
                return;
            }
            EditCheck::Unchanged => {
                diag::log(format!("手直し: 直すところが無い({seconds:.1} 秒)"));
                let mut message = "直すところはありませんでした".to_string();
                // 「直すところはありません」の返事は重ねない。
                if let Some(reply) = reply.filter(|r| !r.starts_with("直すところはありません")) {
                    message.push('。');
                    message.push_str(reply);
                }
                self.notify(message);
                return;
            }
            EditCheck::Apply(applied) => applied,
        };
        let changes = edit_changes(&target, &applied);
        let id = self.editor.replace_range_fixed(start, end, &applied);
        // 変わった字に AI 訂正と同じ色を付けて見せる(選択で見せると、次に打った 1 字で整えた文が置き換わる)。
        self.editor.mark_corrected(start, &changed_ranges(&target, &applied));
        self.selection_dirty = true;
        self.requests.last_edit = Some(LastEdit {
            id,
            start,
            applied: applied.clone(),
            original: target.clone(),
        });
        // 言い直しの控えは補完のためのもの。手直しでは触らない(置き換えた範囲にあれば、手付かずでなくなる)。
        diag::log(format!(
            "手直し: 対象 {} 字 → {} 字、変更 {changes} 箇所({seconds:.1} 秒)",
            target.chars().count(),
            applied.chars().count()
        ));
        let mut message = format!("文章を整えました(変更 {changes} 箇所、{seconds:.1} 秒)");
        if let Some(reply) = reply {
            message.push_str("。指摘: ");
            message.push_str(reply);
        }
        self.after_insert();
        self.notify(message);
    }

    /// 直前の手直しを戻す(Esc)。整えた文が手付かずで、そのあとに別の発話が無いときだけ、置き換える前の文に戻す。
    /// 戻したら true。戻せないときは控えを捨てる(手直しした文は Esc では消さない。`take_back`)。
    pub(super) fn undo_edit(&mut self) -> bool {
        let Some(edit) = self.requests.last_edit.take() else {
            return false;
        };
        let last = self.editor.utterances.last().is_some_and(|u| u.id == edit.id);
        if !last || self.editor.intact_start(edit.id) != Some(edit.start) {
            diag::log("手直し: 戻す控えを捨てた(そのあとに入力があった)");
            return false;
        }
        let end = edit.start + edit.applied.chars().count();
        self.editor.replace_range_fixed(edit.start, end, &edit.original);
        self.selection_dirty = true;
        diag::log("手直し: Esc で戻した");
        self.notify("手直しを戻しました");
        true
    }

    /// Esc で消す最後の発話が手直しの文か(範囲の全体なので消さない)。
    pub(super) fn last_is_from_edit(&self) -> bool {
        self.editor.utterances.last().is_some_and(|u| u.from_edit)
    }

    /// 呼びかけの待ちと聞き取り(溜めた文も捨てる)と、送った依頼を取りやめる(Esc)。送ってしまった依頼の
    /// 答えは、戻っても捨てる。取りやめるものがあれば true。
    pub(super) fn cancel_request(&mut self) -> bool {
        if self.requests.call_waiting.take().is_some() {
            diag::log("呼びかけ: Esc で待ち・聞き取りをやめた");
            if self.live_caption.starts_with("依頼: ") {
                self.live_caption.clear();
            }
            self.notify("Claude への依頼を取りやめました");
            return true;
        }
        if self.requests.pending.take().is_some() {
            diag::log("依頼: Esc で、訂正を待っていた依頼を取りやめた");
            if self.live_caption.starts_with("依頼: ") {
                self.live_caption.clear();
            }
            self.notify("Claude への依頼を取りやめました");
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
        self.requests.sent = None;
        if self.live_caption.starts_with("依頼: ") {
            self.live_caption.clear();
        }
        diag::log("依頼: Esc で取りやめた");
        self.notify("Claude への依頼を取りやめました");
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 話し始めないまま 15 秒たてば切れる。声を見たら聞き取りに移り、15 秒の期限では切れない。
    #[test]
    fn the_wait_lapses_without_voice_and_turns_into_listening_with_it() {
        let start = Instant::now();
        let listen = Duration::from_secs(30);
        let wait = CallWait::new(start, listen);
        assert!(!wait.lapsed(start + Duration::from_secs(14), false));
        assert!(wait.lapsed(start + CALL_WAIT, false), "声を一度も見ていなければ 15 秒で切れる");
        assert!(!wait.lapsed(start + CALL_WAIT, true), "いま声がしていれば切れない");
        assert!(!wait.due(start + Duration::from_secs(60), false), "聞き取りを始めていなければ送らない");

        let mut wait = CallWait::new(start, listen);
        let spoke = start + Duration::from_secs(14);
        assert!(wait.note(true, spoke), "初めて声を見たら聞き取りを始める");
        assert!(!wait.note(true, spoke + Duration::from_secs(1)), "始めるのは一度だけ");
        assert_eq!(wait.listen_end(), Some(spoke + listen));
        assert!(!wait.lapsed(start + Duration::from_secs(60), false), "聞き取りの間は待ちの期限で切れない");
    }

    /// 聞き取りの時間が来ても、声か処理中の発話があるか、最後に見てから 1 秒たっていなければ送らない
    /// (届いた認識文の時点では処理中の数から抜けているので、最後に見た時刻で見る)。続いても 30 秒で打ち切る。
    #[test]
    fn listening_ends_after_its_time_and_a_quiet_second() {
        let ms = Duration::from_millis;
        let start = Instant::now();
        let mut wait = CallWait::new(start, Duration::from_secs(30));
        wait.start_listening(start);
        let end = start + Duration::from_secs(30);
        assert!(!wait.due(end - ms(1), false));
        assert!(wait.due(end, false), "声を見ていなければ時間で送る");
        assert!(!wait.due(end, true), "いま声がしていれば待つ");

        // 時間の直前まで話し、話し終えると同時にマイクを止めた(押している間だけ録る使い方)。止めると声の印は
        // すぐ消え、録音役が尾 600 ms を録り終えてから発話を処理中に数える。認識役は数を減らしてから認識文を送る。
        let stopped = end - ms(100);
        wait.note(true, stopped);
        assert!(!wait.due(stopped + ms(600), false), "尾を録っている間は送らない");
        let counted = stopped + ms(650);
        wait.note(true, counted);
        assert!(!wait.due(counted + ms(30), false), "数が減ってから認識文が届くまでの間も送らない");
        assert!(!wait.due(counted + ms(900), false));
        assert!(wait.due(counted + ms(1100), false), "1 秒静かなら送る");
        wait.note(true, start);
        assert!(wait.due(counted + ms(1100), false), "前の時刻を控えても、最後に見た時刻は戻らない");
        assert!(!wait.due(end + LISTEN_OVERRUN_LIMIT - ms(1), true));
        assert!(wait.due(end + LISTEN_OVERRUN_LIMIT, true), "声が続いても打ち切る");
    }

    /// 終える語は発話全体が「以上」「以上です」のときだけ。文末の「以上」("or more")は依頼の一部。
    #[test]
    fn only_a_whole_utterance_of_ijou_ends_the_listening() {
        for text in ["以上", "以上です", "以上。", "以上です。", " 以上 ", "、以上です!"] {
            assert!(is_end_word(text), "{text}");
        }
        for text in [
            "短径1センチ以上",
            "サイズは1センチ以上です",
            "5.5かける4.5、以上です",
            "3個以上",
            "以上の所見を認めます",
            "以上でお願いします",
            "異常なし",
            "",
        ] {
            assert!(!is_end_word(text), "{text}");
        }
    }

    #[test]
    fn the_seconds_shown_are_rounded_up_and_never_zero() {
        assert_eq!(seconds_up(Duration::from_secs(30)), 30);
        assert_eq!(seconds_up(Duration::from_millis(29_001)), 30);
        assert_eq!(seconds_up(Duration::from_millis(1)), 1);
        assert_eq!(seconds_up(Duration::ZERO), 1);
    }
}
