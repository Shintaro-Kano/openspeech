//! 発話が入ってからエディターに届くまでの流れを、マイク無しで試す。

use openspeech::app::{recognize_options, MicState, VoiceApp};
use openspeech::core::assistant::{Assistant, AssistantReply, AssistantRequest, ReplyKind, Target};
use openspeech::core::audio::MicSource;
use openspeech::core::corrector::{CorrectionRequest, Corrector};
use openspeech::core::dictionary::{Dictionary, Module, Template};
use openspeech::core::editor_model::{CorrectionState, SpanKind};
use openspeech::core::recognizer::{Recognition, RecognizeOptions, Recognizer};
use openspeech::core::remote_asr::{Hints, RemoteError, RemoteProvider};
use openspeech::core::settings::RecognizerKind;
use openspeech::core::session::Session;
use openspeech::core::settings::CorrectorBackend;
use openspeech::core::speechcpp;
use openspeech::core::store::Store;
use openspeech::core::template_library::{parse_index, TEMPLATE_LIBRARY_INDEX, TEMPLATE_SHARE_FORM};
use openspeech::core::transfer::{Delivery, TransferError};
use openspeech::core::user_data::UserData;
use openspeech::ui::library::{LibraryLoad, LibraryView};
use openspeech::ui::{theme, DictTab, LibraryWindow};
use openspeech::app::IMPORT_WHILE_EDITING;
use openspeech::core::voice_command::CommandAction;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

fn app(tag: &str) -> VoiceApp {
    let home = std::env::temp_dir().join(format!("openspeech-it-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    VoiceApp::with_store(egui::Context::default(), Store::at(&home), None)
}

fn say(app: &mut VoiceApp, text: &str) {
    let recognition = Recognition {
        text: text.to_string(),
        unsure: Vec::new(),
    };
    app.handle_utterance(recognition, None);
}

/// 音声コマンドの言葉を登録する(既定は空)。
fn register(app: &mut VoiceApp, action: CommandAction, words: &[&str]) {
    let row = app
        .data
        .settings
        .command_words
        .iter_mut()
        .find(|c| c.action == action)
        .unwrap();
    row.words = words.iter().map(|w| w.to_string()).collect();
}

#[test]
fn dictation_session_flows_through_filters_and_commands() {
    let mut app = app("flow");
    register(&mut app, CommandAction::NewLine, &["改行"]);

    // 文章は整形されて入る。
    say(&mut app, "えーと、2026年6月4日に胸部CTを撮影しました。");
    assert_eq!(app.editor.text, "令和8年6月4日に胸部 CT を撮影しました。");

    // 文末の「改行」はコマンドになる。
    say(&mut app, "所見はありません 改行");
    assert!(app.editor.text.ends_with("所見はありません\n"));

    // 音声コマンド単独。
    say(&mut app, "改行");
    assert!(app.editor.text.ends_with("所見はありません\n\n"));

    // 記号の読み(辞書ファイルの既定の記号)。
    app.editor.delete_all();
    say(&mut app, "矢印");
    assert_eq!(app.editor.text, "→");
    say(&mut app, "やじるし");
    assert_eq!(app.editor.text, "→→");
    say(&mut app, "かっこ");
    say(&mut app, "まる");
    assert_eq!(app.editor.text, "→→（。");
    // 直前が同じ句読点なら重ねない(認識結果の文末の「。」のあとの「まる」)。
    say(&mut app, "まる");
    assert_eq!(app.editor.text, "→→（。");
}

/// 「改行」「転送」は漢字でもかなでも、登録しなくても働く。外した操作の言葉は文章として入る。
#[test]
fn the_command_names_work_without_registration() {
    let mut app = app("no-words");
    app.data.settings.save_history = false;
    say(&mut app, "胸部単純CT 改行改行改行");
    assert_eq!(app.editor.text, "胸部単純 CT\n\n\n");
    say(&mut app, "前立腺MRI かいぎょうかいぎょう");
    assert_eq!(app.editor.text, "胸部単純 CT\n\n\n前立腺 MRI\n\n");
    say(&mut app, "改行");
    assert_eq!(app.editor.text, "胸部単純 CT\n\n\n前立腺 MRI\n\n\n");
    for spoken in ["すべて削除", "音声認識停止"] {
        say(&mut app, spoken);
    }
    assert!(app.editor.text.ends_with("すべて削除音声認識停止"));
    say(&mut app, "てんそう");
    assert_eq!(
        app.history.latest().unwrap().text,
        "胸部単純 CT\n\n\n前立腺 MRI\n\n\nすべて削除音声認識停止"
    );
    say(&mut app, "以上です。転送");
    assert!(app.history.latest().unwrap().text.ends_with("すべて削除音声認識停止以上です。"));
}

/// 文末の「転送」は、その文を入れてから送る。
#[test]
fn a_trailing_transfer_sends_the_last_sentence() {
    let mut app = app("trailing-transfer");
    app.data.settings.save_history = false;
    register(&mut app, CommandAction::Transfer, &["送信"]);
    say(&mut app, "所見です。");
    say(&mut app, "以上です 送信");
    assert_eq!(app.history.latest().unwrap().text, "所見です。以上です");
}

/// 試験から答えを渡すまで返らない訂正役。`slow` を指定すると、その語を含む文だけを待たせ、
/// ほかの文は `fix` の組の置き換えだけをして返す。待つ依頼は 1 本の受け取り口に並び、先に並んだ依頼が
/// 先の答えを受け取る(`asked` は口の先頭に着いた依頼の数)。
struct GatedCorrector {
    answers: Mutex<Receiver<Result<String, String>>>,
    slow: Option<&'static str>,
    fix: (&'static str, &'static str),
    asked: Arc<AtomicUsize>,
}

/// 訂正の答えを渡す口。
struct Answers {
    sender: Sender<Result<String, String>>,
    asked: Arc<AtomicUsize>,
}

impl Answers {
    fn send(&self, answer: Result<String, String>) -> Result<(), mpsc::SendError<Result<String, String>>> {
        self.sender.send(answer)
    }

    /// `count` 件目の依頼が受け取り口の先頭に着くまで待つ。訂正の処理役は 3 本あるので、続けて話すと
    /// 後の依頼が先に並ぶことがある。答えの順を発話の順にそろえたい試験は、1 件目を話したら `wait_asked(1)`
    /// で待ってから次を話す(2 件目は 1 件目に答えるまで先頭に着かないので、2 件目では待たない)。
    fn wait_asked(&self, count: usize) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while self.asked.load(Ordering::SeqCst) < count {
            assert!(Instant::now() < deadline, "訂正の依頼が {count} 件目まで届かない");
            std::thread::sleep(Duration::from_millis(2));
        }
    }
}

impl Corrector for GatedCorrector {
    fn name(&self) -> String {
        "偽の訂正役".to_string()
    }

    fn correct(&self, request: &CorrectionRequest) -> Result<String, String> {
        if self.slow.is_some_and(|word| !request.text.contains(word)) {
            return Ok(request.text.replace(self.fix.0, self.fix.1));
        }
        let answers = self.answers.lock().unwrap();
        self.asked.fetch_add(1, Ordering::SeqCst);
        answers.recv().unwrap()
    }
}

/// 転送を止めておく門。
type TransferGate = Receiver<Result<(), TransferError>>;

/// 他のアプリへ送らない転送。送った文を覚える。門を閉じると、試験が開けるまで終わらない。
#[derive(Clone, Default)]
struct FakeTransfer {
    sent: Arc<Mutex<Vec<String>>>,
    gate: Arc<Mutex<Option<TransferGate>>>,
}

impl FakeTransfer {
    fn sent(&self) -> Vec<String> {
        self.sent.lock().unwrap().clone()
    }

    /// 転送を止める門を閉じ、開ける口を返す。
    fn close_gate(&self) -> Sender<Result<(), TransferError>> {
        let (open, gate) = mpsc::channel();
        *self.gate.lock().unwrap() = Some(gate);
        open
    }
}

/// 偽物をつないだアプリと、訂正の答えを渡す口と、偽の転送。
type WiredApp = (VoiceApp, Answers, FakeTransfer);

/// 訂正と転送の処理役を、偽物で動かす。
fn wired_app(tag: &str) -> WiredApp {
    wired_app_slow_on(tag, None)
}

fn wired_app_slow_on(tag: &str, slow: Option<&'static str>) -> WiredApp {
    wired_app_fixing(tag, slow, ("", ""))
}

fn wired_app_fixing(
    tag: &str,
    slow: Option<&'static str>,
    fix: (&'static str, &'static str),
) -> WiredApp {
    let mut app = app(tag);
    app.data.settings.save_history = false;
    register(&mut app, CommandAction::Transfer, &["送信"]);
    let (sender, answers) = mpsc::channel();
    let asked = Arc::new(AtomicUsize::new(0));
    let answer = Answers {
        sender,
        asked: asked.clone(),
    };
    app.corrector = Some(Arc::new(GatedCorrector {
        answers: Mutex::new(answers),
        slow,
        fix,
        asked,
    }));
    app.enable_corrections();
    let fake = FakeTransfer::default();
    let sender = fake.clone();
    app.enable_transfer(Arc::new(move |request| {
        sender.sent.lock().unwrap().push(request.text.to_string());
        match sender.gate.lock().unwrap().as_ref() {
            Some(gate) => gate.recv().unwrap().map(|()| Delivery::Inserted),
            None => Ok(Delivery::Inserted),
        }
    }));
    (app, answer, fake)
}

/// 文末の「転送」は、その文の AI 訂正が戻ってから、直した文を送る。
#[test]
fn a_spoken_transfer_sends_the_corrected_text() {
    let (mut app, answer, fake) = wired_app("transfer-corrected");
    say(&mut app, "右配上葉に結節 送信");
    assert_eq!(app.pending_corrections(), 1);
    app.poll_events();
    assert!(fake.sent().is_empty(), "訂正が戻るまで送らない");
    answer.send(Ok("右肺上葉に結節".to_string())).unwrap();
    pump_until(&mut app, "送って消える", |app| app.editor.is_empty());
    assert_eq!(fake.sent(), vec!["右肺上葉に結節"]);
    assert!(notice_of(&app).starts_with("転送しました"));
}

/// 待っている間の発話は、送った文の後に話した順で入る。言いよどみや「転送」の言い直しでは送らない。
#[test]
fn speech_during_the_wait_follows_the_sent_text() {
    let (mut app, answer, fake) = wired_app("transfer-held");
    say(&mut app, "一文目。送信");
    say(&mut app, "えーと");
    say(&mut app, "二文目。");
    say(&mut app, "三文目。");
    app.poll_events();
    assert!(fake.sent().is_empty());
    assert_eq!(app.editor.text, "一文目。");
    answer.send(Ok("一文目です。".to_string())).unwrap();
    pump_until(&mut app, "続きが入る", |app| app.editor.text.contains("三文目"));
    assert_eq!(fake.sent(), vec!["一文目です。"]);
    assert_eq!(app.editor.text, "二文目。三文目。");
    answer.send(Ok("二文目。".to_string())).unwrap();
    answer.send(Ok("三文目。".to_string())).unwrap();
}

/// 待っている間に「転送」を言い直しても、同じ文を二重に送らない。
#[test]
fn saying_transfer_twice_sends_once() {
    let (mut app, answer, fake) = wired_app("transfer-twice");
    say(&mut app, "一文目 送信");
    say(&mut app, "送信");
    say(&mut app, "送信");
    assert_eq!(app.held_utterance_count(), 0, "待っている間の言い直しは置かない");
    answer.send(Ok("一文目".to_string())).unwrap();
    pump_until(&mut app, "送って消える", |app| {
        app.editor.is_empty() && app.pending_corrections() == 0
    });
    for _ in 0..5 {
        app.poll_events();
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(fake.sent(), vec!["一文目"]);
}

/// 発話の上限を過ぎたら認識したまま確定して送り、そのことを画面で知らせる。
#[test]
fn the_wait_limit_sends_the_text_and_says_so() {
    let (mut app, answer, fake) = wired_app("transfer-limit");
    say(&mut app, "右配上葉 送信");
    app.poll_events();
    assert!(fake.sent().is_empty());
    app.watch_corrections(Instant::now() + Duration::from_secs(5));
    app.poll_events();
    assert!(app.transfer_waiting(), "上限までは待つ");
    assert!(app.editor.awaiting_correction());
    app.watch_corrections(Instant::now() + Duration::from_secs(7));
    pump_until(&mut app, "送って消える", |app| app.editor.is_empty());
    assert_eq!(fake.sent(), vec!["右配上葉"]);
    assert!(notice_of(&app).contains("AI 訂正を通っていない文を含めて転送しました"));
    answer.send(Ok("右肺上葉".to_string())).unwrap();
    pump_until(&mut app, "訂正の戻り", |app| app.pending_corrections() == 0);
    assert!(app.editor.is_empty(), "送ったあとの訂正はエディターに戻らない");
    assert!(app.data.memory.pairs.is_empty(), "画面に入らなかった訂正は覚えない");
}

/// 訂正に失敗した文を音声で送ったら、そのことを知らせる。
#[test]
fn a_failed_correction_is_reported_when_the_text_is_sent() {
    let (mut app, answer, fake) = wired_app("transfer-error");
    say(&mut app, "右配上葉 送信");
    answer.send(Err("通信できません".to_string())).unwrap();
    pump_until(&mut app, "送って消える", |app| app.editor.is_empty());
    assert_eq!(fake.sent(), vec!["右配上葉"]);
    assert!(notice_of(&app).contains("AI 訂正を通っていない文を含めて転送しました"));
}

/// 前の報告の訂正が遅れていても、いま送る文とは関係ないので待たず、警告も出さない。
#[test]
fn a_late_correction_of_an_earlier_report_does_not_hold_the_next_one() {
    let (mut app, answer, fake) = wired_app_slow_on("transfer-earlier", Some("遅い"));
    say(&mut app, "遅い文");
    app.transfer();
    pump_until(&mut app, "前の報告が消える", |app| app.editor.is_empty());
    assert_eq!(app.pending_corrections(), 1);
    say(&mut app, "次の報告 送信");
    pump_until(&mut app, "次の報告が消える", |app| app.editor.is_empty());
    assert_eq!(fake.sent(), vec!["遅い文", "次の報告"]);
    assert!(notice_of(&app).starts_with("転送しました"));
    answer.send(Ok("遅い文".to_string())).unwrap();
    pump_until(&mut app, "訂正の戻り", |app| app.pending_corrections() == 0);
    assert!(app.editor.is_empty());
}

/// Esc で消した発話の訂正は待たない。
#[test]
fn a_deleted_utterance_is_not_waited_for() {
    let (mut app, answer, fake) = wired_app_slow_on("transfer-deleted", Some("遅い"));
    say(&mut app, "残す文。");
    pump_until(&mut app, "訂正の戻り", |app| app.pending_corrections() == 0);
    say(&mut app, "遅い文");
    app.take_back();
    say(&mut app, "送信");
    pump_until(&mut app, "送って消える", |app| app.editor.is_empty());
    assert_eq!(fake.sent(), vec!["残す文。"]);
    answer.send(Ok("遅い文".to_string())).unwrap();
}

/// 待っている間に手で打ち始めたら、書きかけを送らない。
#[test]
fn typing_during_the_wait_cancels_the_transfer() {
    let (mut app, answer, fake) = wired_app("transfer-typing");
    say(&mut app, "右配上葉 送信");
    assert!(app.transfer_waiting());
    app.sync_editor_from_input("右肺配上葉", (2, 2));
    assert!(!app.transfer_waiting());
    assert_eq!(notice_of(&app), "手で直したので、転送を取りやめました");
    answer.send(Ok("右肺上葉".to_string())).unwrap();
    pump_until(&mut app, "訂正の戻り", |app| app.pending_corrections() == 0);
    assert!(fake.sent().is_empty());
    assert_eq!(app.editor.text, "右肺配上葉");
}

/// 「転送」を言い直してから Esc で取りやめても、言い直しの分で待ちに戻らない。
#[test]
fn esc_stays_cancelled_after_a_repeated_transfer_word() {
    let (mut app, answer, fake) = wired_app("transfer-esc-repeat");
    say(&mut app, "一文目 送信");
    say(&mut app, "送信");
    app.take_back();
    app.poll_events();
    assert!(!app.transfer_waiting());
    answer.send(Ok("一文目".to_string())).unwrap();
    pump_until(&mut app, "訂正の戻り", |app| app.pending_corrections() == 0);
    assert!(fake.sent().is_empty());
}

/// 取りやめたら、置いてある発話の「転送」も働かない。文は残る。
#[test]
fn a_cancelled_transfer_stays_cancelled_for_held_transfer_words() {
    for (extra, expected) in [("えーと", "一文目"), ("二文目 送信", "一文目二文目")] {
        let (mut app, answer, fake) = wired_app("transfer-held-cancel");
        say(&mut app, "一文目 送信");
        say(&mut app, extra);
        say(&mut app, "送信");
        app.take_back();
        app.poll_events();
        assert!(!app.transfer_waiting(), "{extra}");
        assert_eq!(app.editor.text, expected);
        for _ in 0..3 {
            let _ = answer.send(Ok("一文目".to_string()));
        }
        pump_until(&mut app, "訂正の戻り", |app| app.pending_corrections() == 0);
        for _ in 0..5 {
            app.poll_events();
        }
        assert!(fake.sent().is_empty(), "{extra}: {:?}", fake.sent());
    }
}

/// 手で直して取りやめたら、置いてあった発話は書きかけの語の中ではなく文末へ入り、カーソルは動かない。
#[test]
fn held_speech_goes_to_the_end_after_typing_cancels_the_wait() {
    let (mut app, answer, fake) = wired_app("transfer-typing-held");
    say(&mut app, "右配上葉に結節 送信");
    say(&mut app, "二文目 送信");
    app.sync_editor_from_input("右肺配上葉に結節", (2, 2));
    app.poll_events();
    assert_eq!(app.editor.text, "右肺配上葉に結節二文目");
    assert_eq!(app.editor.selection, (2, 2));
    for _ in 0..2 {
        answer.send(Ok("直した文".to_string())).unwrap();
    }
    pump_until(&mut app, "訂正の戻り", |app| app.pending_corrections() == 0);
    assert!(fake.sent().is_empty());
}

/// 語を選んだまま Esc を押しても、置いてあった発話が選択を上書きしない。
#[test]
fn held_speech_does_not_overwrite_a_selection_after_esc() {
    let (mut app, answer, _fake) = wired_app("transfer-esc-selection");
    say(&mut app, "右肺に結節 送信");
    say(&mut app, "二文目。");
    app.editor.selection = (0, 2);
    app.take_back();
    app.poll_events();
    assert_eq!(app.editor.text, "右肺に結節二文目。");
    assert_eq!(app.editor.selection, (0, 2));
    for text in ["右肺に結節", "二文目。"] {
        answer.send(Ok(text.to_string())).unwrap();
    }
}

/// 送っている途中の ⌘Enter と Esc は、同じ文を送り直さず、文も変えない。
#[test]
fn keys_during_a_transfer_change_nothing() {
    let (mut app, _answer, fake) = wired_app_slow_on("transfer-keys", Some("遅い"));
    let open = fake.close_gate();
    say(&mut app, "一文目");
    pump_until(&mut app, "訂正の戻り", |app| app.pending_corrections() == 0);
    app.transfer();
    app.transfer();
    app.take_back();
    assert_eq!(app.editor.text, "一文目");
    open.send(Ok(())).unwrap();
    pump_until(&mut app, "送って消える", |app| app.editor.is_empty());
    assert_eq!(fake.sent(), vec!["一文目"]);
}

/// 転送の処理役が返らなくても、発話を置いたままにせず、知らせる。
#[test]
fn a_stuck_transfer_stops_holding_speech() {
    let (mut app, _answer, fake) = wired_app_slow_on("transfer-stuck", Some("遅い"));
    let open = fake.close_gate();
    say(&mut app, "一文目");
    pump_until(&mut app, "訂正の戻り", |app| app.pending_corrections() == 0);
    app.transfer();
    say(&mut app, "続き");
    assert_eq!(app.held_utterance_count(), 1);
    app.watch_transfer(Instant::now() + Duration::from_secs(29));
    assert_eq!(app.held_utterance_count(), 1);
    app.watch_transfer(Instant::now() + Duration::from_secs(31));
    assert!(notice_of(&app).contains("転送が終わりません"));
    app.poll_events();
    assert_eq!(app.editor.text, "一文目続き");

    // 返っていない転送の後ろには並べない(回復したときに二重に届く)。
    app.transfer();
    assert!(notice_of(&app).contains("前の転送が終わっていません"));
    open.send(Ok(())).unwrap();
    pump_until(&mut app, "送った分が消える", |app| app.editor.text == "続き");
    assert_eq!(fake.sent(), vec!["一文目"]);

    // 回復したら普通に送れる。
    app.transfer();
    open.send(Ok(())).unwrap();
    pump_until(&mut app, "送って消える", |app| app.editor.is_empty());
    assert_eq!(fake.sent(), vec!["一文目", "続き"]);
}

/// 待っている間に話した順は、そのあとの発話でも崩れない。
#[test]
fn speech_right_after_a_cancelled_wait_keeps_its_order() {
    let (mut app, answer, _fake) = wired_app("transfer-order");
    say(&mut app, "一文目 送信");
    say(&mut app, "二文目。");
    app.take_back();
    say(&mut app, "三文目。");
    assert_eq!(app.editor.text, "一文目二文目。三文目。");
    for text in ["一文目", "二文目。", "三文目。"] {
        answer.send(Ok(text.to_string())).unwrap();
    }
}

/// ⌘Enter は待っている転送より優先し、その場で送る。
#[test]
fn the_send_key_does_not_wait_for_corrections() {
    let (mut app, answer, fake) = wired_app("transfer-key");
    say(&mut app, "一文目 送信");
    app.transfer();
    pump_until(&mut app, "送って消える", |app| app.editor.is_empty());
    assert_eq!(fake.sent(), vec!["一文目"]);
    assert!(notice_of(&app).starts_with("転送しました"), "手で送るときは警告しない");
    answer.send(Ok("一文目".to_string())).unwrap();
    pump_until(&mut app, "訂正の戻り", |app| app.pending_corrections() == 0);
    assert_eq!(fake.sent().len(), 1);
}

/// 転送の途中に戻った訂正で送った文を書き換えない(書き換えると消す分と合わず、次の転送で二重に送る)。
#[test]
fn a_correction_during_a_transfer_does_not_keep_the_sent_text() {
    let (mut app, answer, fake) = wired_app("transfer-inflight");
    let open = fake.close_gate();
    say(&mut app, "右配上葉");
    app.transfer();
    say(&mut app, "続きの文");
    answer.send(Ok("右肺上葉".to_string())).unwrap();
    let limit = Instant::now() + Duration::from_secs(5);
    while app.pending_corrections() > 0 && Instant::now() < limit && fake.sent().is_empty() {
        app.poll_events();
    }
    std::thread::sleep(Duration::from_millis(50));
    app.poll_events();
    assert_eq!(app.editor.text, "右配上葉", "途中は書き換えない");
    open.send(Ok(())).unwrap();
    pump_until(&mut app, "続きが入る", |app| app.editor.text == "続きの文");
    assert_eq!(fake.sent(), vec!["右配上葉"]);
    answer.send(Ok("続きの文".to_string())).unwrap();
    pump_until(&mut app, "訂正の戻り", |app| app.pending_corrections() == 0);
    assert_eq!(app.editor.text, "続きの文");
}

/// 転送に失敗して文が残るなら、途中に戻った訂正は当てる。
#[test]
fn a_failed_transfer_still_applies_the_correction() {
    let (mut app, answer, fake) = wired_app("transfer-failed");
    let open = fake.close_gate();
    say(&mut app, "右配上葉");
    app.transfer();
    answer.send(Ok("右肺上葉".to_string())).unwrap();
    std::thread::sleep(Duration::from_millis(50));
    app.poll_events();
    assert_eq!(app.editor.text, "右配上葉");
    open.send(Err(TransferError::NoTarget)).unwrap();
    pump_until(&mut app, "訂正が当たる", |app| app.editor.text == "右肺上葉");
}

/// Esc は待っている転送を取りやめる。文は消えず、後の発話は入る。
#[test]
fn esc_cancels_a_waiting_transfer() {
    let (mut app, answer, fake) = wired_app("transfer-cancel");
    say(&mut app, "一文目 送信");
    answer.wait_asked(1);
    say(&mut app, "二文目");
    app.take_back();
    assert_eq!(notice_of(&app), "転送を取りやめました");
    app.poll_events();
    assert_eq!(app.editor.text, "一文目二文目");
    answer.send(Ok("一文目".to_string())).unwrap();
    answer.send(Ok("二文目".to_string())).unwrap();
    pump_until(&mut app, "訂正の戻り", |app| app.pending_corrections() == 0);
    assert!(fake.sent().is_empty());
    app.take_back();
    assert_eq!(app.editor.text, "一文目");
}

/// 転送の途中に手で打った続きがあっても、転送した分を消したあとの入力位置は変わらない。
#[test]
fn clearing_the_sent_text_keeps_the_cursor() {
    let mut app = app("sent-cursor");
    say(&mut app, "送った文。");
    let sent = app.editor.text.clone();
    say(&mut app, "続き");
    app.clear_sent_text(&sent);
    say(&mut app, "の文");
    assert_eq!(app.editor.text, "続きの文");
}

#[test]
fn pure_zero_audio_counts_as_no_signal() {
    use openspeech::core::session::has_signal;
    assert!(!has_signal(&[0.0; 800]));
    assert!(!has_signal(&[]));
    assert!(has_signal(&[0.0, 0.0001, 0.0]));
}

#[test]
fn registered_words_and_disabled_words_change_results() {
    let mut app = app("dict");
    app.data.dictionary = Dictionary::default();
    app.data.dictionary.add_word("腺癌", "せんがん").unwrap();
    app.data.dictionary.disable_word("不要語");
    say(&mut app, "せんがん");
    assert_eq!(app.editor.text, "腺癌");
    // 無効単語は消さずに印を付ける。
    say(&mut app, "の疑い不要語");
    assert_eq!(app.editor.text, "腺癌の疑い不要語");
    assert_eq!(app.editor.spans.len(), 1);
}

#[test]
fn only_the_sent_text_leaves_the_editor() {
    let mut app = app("sent");
    say(&mut app, "転送した文。");
    let sent = app.editor.text.clone();
    say(&mut app, "転送中の続き");
    app.clear_sent_text(&sent);
    assert_eq!(app.editor.text, "転送中の続き");
}

#[test]
fn changed_text_stays_when_it_no_longer_matches() {
    let mut app = app("changed");
    say(&mut app, "あとで直した文");
    app.clear_sent_text("別の文");
    assert_eq!(app.editor.text, "あとで直した文");
}

/// 手直しは転送のときに覚え、次の発話では AI を待たずに当たる。
#[test]
fn manual_edits_are_learned_at_transfer_and_applied_next_time() {
    let mut app = app("learn");
    app.data.settings.save_history = false;
    say(&mut app, "右配上葉に結節を認めます。");
    assert_eq!(app.editor.text, "右配上葉に結節を認めます。");
    // 画面での手直しに相当する。
    app.editor
        .sync_from_input("右肺上葉に結節を認めます。", (13, 13));
    app.transfer();
    assert!(
        app.data
            .memory
            .pairs
            .iter()
            .any(|p| p.from == "右配上" && p.to == "右肺上" && p.manual == 1),
        "{:?}",
        app.data.memory.pairs
    );
    // 転送先が無いので文章は残る。次の発話では学習した置き換えが先に当たる。
    app.editor.delete_all();
    say(&mut app, "右配上葉に腫瘤");
    assert_eq!(app.editor.text, "右肺上葉に腫瘤");
    assert!(app
        .editor
        .spans
        .iter()
        .any(|s| s.kind == SpanKind::Corrected));
    assert_eq!(app.data.memory.utterances, 2);
}

/// 右を左に直したのは話し間違いの訂正なので覚えない(覚えると次から「右」が「左」になる)。
#[test]
fn swapping_left_and_right_is_not_learned() {
    let mut app = app("swap");
    app.data.settings.save_history = false;
    say(&mut app, "右肺上葉に結節");
    app.editor.sync_from_input("左肺上葉に結節", (7, 7));
    app.transfer();
    assert!(
        app.data.memory.pairs.is_empty(),
        "{:?}",
        app.data.memory.pairs
    );
    // 入れ替えと誤認識が隣り合って 1 つの塊になっても覚えない(「右配→左肺」を覚えると次から左右が変わる)。
    app.editor.delete_all();
    say(&mut app, "右配上葉に結節");
    app.editor.sync_from_input("左肺上葉に結節", (7, 7));
    app.transfer();
    assert!(
        app.data.memory.pairs.is_empty(),
        "{:?}",
        app.data.memory.pairs
    );
    // 同じ発話の中の誤認識の直しは覚える。
    app.editor.delete_all();
    say(&mut app, "右配上葉に結節");
    app.editor.sync_from_input("右肺上葉に結節", (7, 7));
    app.transfer();
    assert_eq!(app.data.memory.pairs.len(), 1);
}

/// 学習で当てた置き換えを手で元に戻したら、逆向きに覚えるのではなく、その組を忘れる。
#[test]
fn reverting_an_applied_replacement_unlearns_it() {
    let mut app = app("unlearn");
    app.data.settings.save_history = false;
    assert!(app.data.memory.learn_pair("右配上", "右肺上", true));
    say(&mut app, "右配上葉に腫瘤");
    assert_eq!(app.editor.text, "右肺上葉に腫瘤");
    app.editor.sync_from_input("右配上葉に腫瘤", (7, 7));
    app.transfer();
    assert!(
        app.data.memory.pairs.is_empty(),
        "{:?}",
        app.data.memory.pairs
    );
}

#[test]
fn learning_can_be_turned_off() {
    let mut app = app("nolearn");
    app.data.settings.save_history = false;
    app.data.settings.learning.enabled = false;
    say(&mut app, "右配上葉に結節");
    app.editor.sync_from_input("右肺上葉に結節", (7, 7));
    app.transfer();
    assert!(app.data.memory.pairs.is_empty());
}

fn with_template(app: &mut VoiceApp) {
    app.data.settings.save_history = false;
    app.data
        .dictionary
        .save_template(None, "胸部正常", "肺野に異常なし。\n胸水なし。")
        .unwrap();
}

/// キーワードに続けて「テンプレート」と話すと登録した文章に置き換わる。キーワードだけ、文の中では普通の語のまま。
#[test]
fn a_keyword_with_the_suffix_expands_to_the_template() {
    let mut app = app("template");
    with_template(&mut app);
    say(&mut app, "胸部正常テンプレート。");
    assert_eq!(app.editor.text, "肺野に異常なし。\n胸水なし。");
    app.editor.delete_all();
    say(&mut app, "胸部正常のテンプレ");
    assert_eq!(app.editor.text, "肺野に異常なし。\n胸水なし。");
    app.editor.delete_all();
    say(&mut app, "胸部正常。");
    assert_eq!(app.editor.text, "胸部正常。", "キーワードだけは口述");
    app.editor.delete_all();
    say(&mut app, "前回は胸部正常テンプレートでした");
    assert_eq!(app.editor.text, "前回は胸部正常テンプレートでした");
}

/// 「えー」などの言いよどみが付いても発動し、文末の改行コマンドとも組み合わせられる。
#[test]
fn templates_survive_fillers_and_a_trailing_command() {
    let mut app = app("template-filler");
    with_template(&mut app);
    register(&mut app, CommandAction::NewLine, &["改行"]);
    say(&mut app, "えーと、胸部正常テンプレート");
    assert_eq!(app.editor.text, "肺野に異常なし。\n胸水なし。");
    app.editor.delete_all();
    say(&mut app, "胸部正常テンプレート 改行");
    assert_eq!(app.editor.text, "肺野に異常なし。\n胸水なし。\n");
}

/// 命令語と同じキーワードでも、命令語だけなら命令、「〜テンプレート」ならテンプレート。
#[test]
fn a_command_word_can_also_name_a_template() {
    let mut app = app("template-command");
    app.data.dictionary.save_template(None, "改行", "本文").unwrap();
    register(&mut app, CommandAction::NewLine, &["改行"]);
    say(&mut app, "文章");
    say(&mut app, "改行");
    assert_eq!(app.editor.text, "文章\n");
    say(&mut app, "改行テンプレート");
    assert_eq!(app.editor.text, "文章\n本文");
}

/// 手を入れても内容の変更なので学習しない。Esc で 1 発話として消せる。
#[test]
fn templates_are_not_learned_and_can_be_taken_back() {
    let mut app = app("template-learn");
    with_template(&mut app);
    say(&mut app, "胸部正常テンプレート");
    app.editor
        .sync_from_input("肺野に結節あり。\n胸水なし。", (0, 0));
    app.transfer();
    assert!(
        app.data.memory.pairs.is_empty(),
        "{:?}",
        app.data.memory.pairs
    );
    app.editor.delete_all();
    say(&mut app, "所見。");
    say(&mut app, "胸部正常テンプレート");
    assert!(app.editor.delete_last_utterance());
    assert_eq!(app.editor.text, "所見。");
}

/// 発話の一部を選んでキーワードを話しても、元の所見を「異常なし」へ直す組を覚えない
/// (覚えると、次から陽性の所見が自動で「異常なし」に置き換わる)。
#[test]
fn a_template_over_a_selection_is_not_learned_as_a_correction() {
    let mut app = app("template-selection");
    app.data.settings.save_history = false;
    app.data.dictionary.save_template(None, "陰性", "異常なし。").unwrap();
    say(&mut app, "右肺に結節影あり");
    app.editor.selection = (3, 8);
    say(&mut app, "陰性テンプレート");
    assert_eq!(app.editor.text, "右肺に異常なし。");
    app.transfer();
    assert!(
        app.data.memory.pairs.is_empty(),
        "{:?}",
        app.data.memory.pairs
    );
    app.editor.delete_all();
    say(&mut app, "左肺に結節影あり");
    assert_eq!(app.editor.text, "左肺に結節影あり");
}

/// 以前に覚えた組でも、左右を入れ替えるものは起動時に捨てる。
#[test]
fn learned_pairs_that_flip_laterality_are_dropped_at_startup() {
    let home = std::env::temp_dir().join(format!("openspeech-it-pairs-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    let store = Store::at(&home);
    let mut data = UserData::default();
    assert!(data.memory.learn_pair("左腎", "右腎", true));
    assert!(data.memory.learn_pair("あり", "なし", true));
    assert!(data.memory.learn_pair("右配", "右肺", true));
    store.save_user(&data).unwrap();
    let mut app = VoiceApp::with_store(egui::Context::default(), store, None);
    let pairs: Vec<&str> = app.data.memory.pairs.iter().map(|p| p.to.as_str()).collect();
    assert_eq!(pairs, vec!["右肺"]);
    say(&mut app, "左腎に嚢胞");
    assert_eq!(app.editor.text, "左腎に嚢胞");
}

/// 読めない設定があったことを画面で知らせる。
#[test]
fn unreadable_settings_are_reported_on_screen() {
    let home = std::env::temp_dir().join(format!("openspeech-it-note-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(home.join("user.json"), b"{broken").unwrap();
    let app = VoiceApp::with_store(egui::Context::default(), Store::at(&home), None);
    assert!(app.windows.startup_warning.is_some());
}

/// 内容を変えた手直しを覚えると、次の発話が黙って書き換わる。
#[test]
fn edits_that_change_the_finding_are_not_learned() {
    for (spoken, edited, next) in [
        ("右肺に結節あり", "右肺に結節なし", "左腎に嚢胞あり"),
        ("結節を認めます", "結節を認めません", "嚢胞を認めます"),
        ("腫瘤は増大", "腫瘤は縮小", "結節は増大"),
        ("所見は陽性", "所見は陰性", "検査は陽性"),
        ("悪性の疑い", "良性の疑い", "悪性の可能性"),
        ("右肺上葉", "右肺下葉", "左肺上葉に腫瘤"),
        ("right lung nodule", "left lung nodule", "right kidney"),
    ] {
        let mut app = app("meaning");
        app.data.settings.save_history = false;
        app.data.settings.filters.english_style = 1;
        say(&mut app, spoken);
        let length = edited.chars().count();
        app.editor.sync_from_input(edited, (length, length));
        app.transfer();
        assert!(
            app.data.memory.pairs.is_empty(),
            "{edited}: {:?}",
            app.data.memory.pairs
        );
        app.editor.delete_all();
        say(&mut app, next);
        assert_eq!(app.editor.text, next);
    }
}

/// 話し直しで所見を変えても同じ。
#[test]
fn speaking_over_a_finding_is_not_learned() {
    let mut app = app("respeak");
    app.data.settings.save_history = false;
    say(&mut app, "右肺に結節影あり");
    app.editor.selection = (3, 8);
    say(&mut app, "異常なし。");
    app.transfer();
    assert!(
        app.data.memory.pairs.is_empty(),
        "{:?}",
        app.data.memory.pairs
    );
}

/// 直接入力・分野・一時停止・古い音声コマンドが残る user.json を開いても設定は飛ばない。
#[test]
fn a_user_file_from_before_the_simplification_keeps_its_settings() {
    let home = std::env::temp_dir().join(format!("openspeech-it-oldjson-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();
    let old = r#"{"settings":{
        "input_mode":"Direct",
        "direct":{"edit":false,"auto_accept":true,"accept_seconds":3},
        "editor":{"font_size":28.0},
        "record":{"shortcut":"CapsLock","hold":true,"start_paused":true},
        "engine":{"sensitivity":0.55,"speed_vs_accuracy":0.1,"profile":"General",
            "mark_unsure":false,"noise_margin":2,"post_silence_ms":800,"live_preview":false},
        "transfer":{"method":"Paste","restore_clipboard":false,"ai_wait_ms":7000,"enter_after":true},
        "voice_commands":[
            {"name":"改行","readings":["かいぎょー"],"surfaces":["改行"],"enabled":true,"action":"NewLine"},
            {"name":"２行改行","readings":[],"surfaces":[],"enabled":true,"action":"NewLine2"},
            {"name":"音声認識停止","readings":[],"surfaces":[],"enabled":false,"action":"StopRecognition"},
            {"name":"外部","readings":[],"surfaces":[],"enabled":false,
             "action":{"External":{"program":"x","wait":true}}}],
        "history_count":50},
        "dictionary":{"words":[{"word":"腺癌","reading":"せんがん"}]},
        "memory":{"pairs":[{"from":"右配上","to":"右肺上","count":0,"manual":1,"last":"2026-11-05"}]}}"#;
    std::fs::write(home.join("user.json"), old).unwrap();
    let mut app = VoiceApp::with_store(egui::Context::default(), Store::at(&home), None);
    assert!(app.windows.startup_warning.is_none());
    assert_eq!(app.data.settings.editor.font_size, 28.0);
    assert_eq!(app.data.settings.engine.post_silence_ms, 800);
    assert!(app.data.settings.record.hold);
    assert!(app.data.settings.transfer.enter_after);
    assert_eq!(app.data.settings.history_count, 50);
    assert_eq!(app.data.dictionary.words[0].word, "腺癌");
    assert_eq!(app.data.memory.pairs.len(), 1);
    assert!(app.data.settings.command_words.iter().all(|c| c.words.is_empty()));
    // 古い一覧は引き継がないが、名前の「改行」は登録なしで働く。次の保存で新しい形になる。
    say(&mut app, "改行");
    assert_eq!(app.editor.text, "\n");
    app.save_data();
    let saved = std::fs::read_to_string(home.join("user.json")).unwrap();
    assert!(saved.contains("command_words"));
    assert!(!saved.contains("voice_commands"));
    let reread = Store::at(&home).load_user();
    assert_eq!(reread.settings, app.data.settings);
}

const IPHONE: &str = "テストのiPhoneのマイク";
const MAC: &str = "テストのMacのマイク";

#[derive(Default)]
struct FakeState {
    names: Vec<String>,
    /// 開く処理に入った数(開く処理の待ちの前に数える)。
    entered: usize,
    /// 開かれたデバイス(古い順)。開く処理が終わった時点で入る。
    opened: Vec<Option<String>>,
    /// いま開いている数。
    open_now: usize,
    /// 同時に開いていた数の最大。
    open_peak: usize,
    /// 流した音を受け取る先(いま開いているデバイス)。
    sender: Option<Sender<Vec<f32>>>,
    /// 開く処理にかかる時間。
    open_delay: Duration,
    /// 閉じる処理にかかる時間。
    close_delay: Duration,
    /// 開くのに失敗させる回数。
    failures: usize,
}

/// 偽の録音デバイス。開かれた順を覚え、試験から音を流せる。
#[derive(Clone, Default)]
struct FakeMic(Arc<Mutex<FakeState>>);

/// 落とすとデバイスが閉じる。
struct FakeCapture(FakeMic);

impl Drop for FakeCapture {
    fn drop(&mut self) {
        let delay = self.0 .0.lock().unwrap().close_delay;
        std::thread::sleep(delay);
        self.0 .0.lock().unwrap().open_now -= 1;
    }
}

impl MicSource for FakeMic {
    fn device_names(&self) -> Vec<String> {
        self.0.lock().unwrap().names.clone()
    }

    fn default_device(&self) -> Option<String> {
        Some(MAC.to_string())
    }

    fn open(
        &self,
        device: Option<String>,
        samples: Sender<Vec<f32>>,
    ) -> Result<Box<dyn Send>, String> {
        let delay = {
            let mut state = self.0.lock().unwrap();
            state.entered += 1;
            state.open_delay
        };
        std::thread::sleep(delay);
        let mut state = self.0.lock().unwrap();
        state.opened.push(device);
        if state.failures > 0 {
            state.failures -= 1;
            return Err("開けませんでした".to_string());
        }
        state.open_now += 1;
        state.open_peak = state.open_peak.max(state.open_now);
        state.sender = Some(samples);
        Ok(Box::new(FakeCapture(self.clone())))
    }
}

impl FakeMic {
    fn opened(&self) -> Vec<Option<String>> {
        self.0.lock().unwrap().opened.clone()
    }

    fn entered(&self) -> usize {
        self.0.lock().unwrap().entered
    }

    fn push(&self, samples: Vec<f32>) {
        let state = self.0.lock().unwrap();
        state.sender.as_ref().unwrap().send(samples).unwrap();
    }
}

/// iPhone を優先する設定で、偽のデバイスをつないだアプリ。
fn iphone_app(tag: &str) -> (VoiceApp, FakeMic) {
    let mut app = app(tag);
    let fake = FakeMic::default();
    fake.0.lock().unwrap().names = vec![MAC.to_string(), IPHONE.to_string()];
    app.session.set_source(Arc::new(fake.clone()));
    app.engine_name = Some("偽のエンジン".to_string());
    app.global.prefer_iphone = true;
    (app, fake)
}

/// 条件が満たされるまで、画面の更新と同じ処理を回す。
/// 上限は混んだ機械(試験を並べて走らせる CI)でも切れない長さ。満たされればすぐ戻る。
fn pump_until(app: &mut VoiceApp, what: &str, done: impl Fn(&VoiceApp) -> bool) {
    let limit = Instant::now() + Duration::from_secs(15);
    while !done(app) {
        assert!(Instant::now() < limit, "待ちきれない: {what}");
        app.poll_events();
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn notice_of(app: &VoiceApp) -> String {
    app.notice.as_ref().map_or(String::new(), |n| n.text.clone())
}

/// iPhone を開いて音が届いた状態にする。
fn connect_iphone(app: &mut VoiceApp, fake: &FakeMic) {
    app.start_mic();
    let opened = fake.clone();
    pump_until(app, "iPhone が開く", move |_| opened.opened().len() == 1);
    fake.push(vec![0.1; 1600]);
    pump_until(app, "音が届く", |app| {
        app.session.live.has_signal() && app.iphone_wait_seconds().is_none()
    });
}

/// 録音中に連係が切れて 0 だけの音が続いたら、閉じて開き直す。
#[test]
fn zeros_after_sound_reopen_the_iphone() {
    let (mut app, fake) = iphone_app("link-reopen");
    connect_iphone(&mut app, &fake);
    assert_eq!(app.global.last_iphone.as_deref(), Some(IPHONE));

    fake.push(vec![0.0; 16_000]);
    std::thread::sleep(Duration::from_millis(100));
    app.poll_events();
    assert_eq!(fake.opened().len(), 1, "1 秒では開き直さない");

    fake.push(vec![0.0; 16_000 * 2]);
    let opened = fake.clone();
    pump_until(&mut app, "開き直す", move |_| opened.opened().len() == 2);
    assert_eq!(fake.opened()[1].as_deref(), Some(IPHONE));
    assert_eq!(notice_of(&app), "iPhone の接続を張り直しています");
    assert_eq!(app.mic, MicState::On);
    assert_eq!(fake.0.lock().unwrap().open_peak, 1, "二重に開かない");

    // 音が戻れば待ちは解ける。
    pump_until(&mut app, "開いた知らせ", |app| {
        app.iphone_wait_seconds().is_some() && app.session.live.silence().is_zero()
    });
    fake.push(vec![0.1; 1600]);
    pump_until(&mut app, "音が戻る", |app| app.iphone_wait_seconds().is_none());
    assert!(app.using_iphone());
    app.stop_mic();
}

/// 接続の途中で 0 だけの音が続くのは普通なので、開き直さず待つ。8 秒では諦めない。
#[test]
fn zeros_while_connecting_are_waited_out() {
    let (mut app, fake) = iphone_app("link-connect");
    app.start_mic();
    let opened = fake.clone();
    pump_until(&mut app, "iPhone が開く", move |_| opened.opened().len() == 1);
    fake.push(vec![0.0; 16_000 * 5]);
    pump_until(&mut app, "0 だけの音を読む", |app| {
        app.session.live.silence() >= Duration::from_secs(5)
    });
    app.watch_mic(Instant::now() + Duration::from_secs(9));
    assert_eq!(fake.opened().len(), 1);
    assert_eq!(app.mic, MicState::On);
    assert!(!app.windows.iphone_help);
    assert!(app.iphone_wait_seconds().is_some());
    app.stop_mic();
}

/// 開いたまま音が来なければ途中で開き直し、待ちの上限で Mac のマイクへ切り替える。
#[test]
fn the_wait_limit_switches_to_the_mac_microphone() {
    let (mut app, fake) = iphone_app("link-limit");
    app.start_mic();
    pump_until(&mut app, "待ち時間の表示", |app| {
        app.iphone_wait_seconds().is_some()
    });
    let opened = fake.clone();
    pump_until(&mut app, "iPhone が開く", move |_| opened.opened().len() == 1);
    pump_until(&mut app, "開いた知らせ", |app| {
        app.session.live.silence().is_zero() && app.session.is_recording()
    });
    std::thread::sleep(Duration::from_millis(50));
    app.poll_events();

    app.watch_mic(Instant::now() + Duration::from_secs(11));
    let opened = fake.clone();
    pump_until(&mut app, "途中の開き直し", move |_| opened.opened().len() == 2);
    assert_eq!(fake.opened()[1].as_deref(), Some(IPHONE));

    app.watch_mic(Instant::now() + Duration::from_secs(31));
    let opened = fake.clone();
    pump_until(&mut app, "Mac へ切り替え", move |_| opened.opened().len() == 3);
    assert_eq!(fake.opened()[2], None, "OS の既定のマイクを開く");
    assert!(!app.using_iphone());
    assert_eq!(app.mic, MicState::On);
    assert!(notice_of(&app).contains("Mac のマイクに切り替えました"));
    assert!(!app.windows.iphone_help, "案内の窓は開かない");
    assert_eq!(app.iphone_wait_seconds(), None);
    fake.push(vec![0.1; 1600]);

    // iPhone は一覧に残っていても、この録音の間は戻さない。
    app.scan_for_iphone(Instant::now() + Duration::from_secs(10));
    assert_eq!(fake.opened().len(), 3);
    // 一覧から消えて戻ったら、試し直す。
    fake.0.lock().unwrap().names = vec![MAC.to_string()];
    app.scan_for_iphone(Instant::now() + Duration::from_secs(10));
    fake.0.lock().unwrap().names = vec![MAC.to_string(), IPHONE.to_string()];
    app.scan_for_iphone(Instant::now() + Duration::from_secs(20));
    assert!(app.using_iphone());
    app.stop_mic();
}

/// 開くのに時間がかかったときは連係を張っている途中なので、上限まで開き直さない。
#[test]
fn a_slow_open_is_left_alone_until_the_limit() {
    let (mut app, fake) = iphone_app("link-slow");
    fake.0.lock().unwrap().open_delay = Duration::from_millis(1100);
    app.start_mic();
    let opened = fake.clone();
    pump_until(&mut app, "iPhone が開く", move |_| opened.opened().len() == 1);
    std::thread::sleep(Duration::from_millis(50));
    app.poll_events();
    app.watch_mic(Instant::now() + Duration::from_secs(20));
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(fake.opened().len(), 1);
    fake.0.lock().unwrap().open_delay = Duration::ZERO;
    app.watch_mic(Instant::now() + Duration::from_secs(31));
    let opened = fake.clone();
    pump_until(&mut app, "Mac へ切り替え", move |_| opened.opened().len() == 2);
    assert_eq!(fake.opened()[1], None);
    app.stop_mic();
}

/// 開く処理が長くても画面は止まらず、途中で止めたら音は流れない。
#[test]
fn opening_does_not_block_and_can_be_cancelled() {
    let (mut app, fake) = iphone_app("link-async");
    // 開くのに 1 秒かかる。呼んだ側はその半分も待たない(余裕は混んだ CI のため)。
    fake.0.lock().unwrap().open_delay = Duration::from_millis(1000);
    let began = Instant::now();
    app.start_mic();
    assert!(began.elapsed() < Duration::from_millis(500), "{:?}", began.elapsed());
    assert_eq!(app.mic, MicState::On);
    assert_eq!(app.iphone_wait_seconds(), Some(0));
    // 開く処理の途中で止める(処理に入る前に止めると、そもそも開かない)。
    let entered = fake.clone();
    pump_until(&mut app, "開く処理に入る", move |_| entered.entered() == 1);
    app.stop_mic();
    assert_eq!(app.mic, MicState::Off);
    let opened = fake.clone();
    pump_until(&mut app, "開く処理の終わり", move |_| opened.opened().len() == 1);
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(fake.0.lock().unwrap().open_now, 0, "止めたあとに開いた分はすぐ閉じる");
    assert_eq!(app.mic, MicState::Off);
}

/// 録り直しは前の録音が閉じ切ってから開く。
#[test]
fn restarting_waits_for_the_previous_capture_to_close() {
    let (mut app, fake) = iphone_app("link-restart");
    fake.0.lock().unwrap().close_delay = Duration::from_millis(100);
    connect_iphone(&mut app, &fake);
    app.restart_mic_if_on();
    assert_eq!(app.mic, MicState::On);
    let opened = fake.clone();
    pump_until(&mut app, "開き直す", move |_| opened.opened().len() == 2);
    assert_eq!(fake.0.lock().unwrap().open_peak, 1);
    app.stop_mic();
}

/// iPhone が開けないときは待ちの間に開き直す。Mac のマイクが開けなければ止めて知らせる。
#[test]
fn a_failed_open_retries_on_the_iphone_and_stops_on_the_mac() {
    let (mut app, fake) = iphone_app("link-fail");
    fake.0.lock().unwrap().failures = 1;
    app.start_mic();
    let opened = fake.clone();
    pump_until(&mut app, "開く処理の終わり", move |_| opened.opened().len() == 1);
    std::thread::sleep(Duration::from_millis(50));
    app.poll_events();
    assert_eq!(app.mic, MicState::On);
    app.watch_mic(Instant::now() + Duration::from_secs(11));
    let opened = fake.clone();
    pump_until(&mut app, "開き直す", move |_| opened.opened().len() == 2);
    app.stop_mic();

    let (mut app, fake) = iphone_app("mac-fail");
    app.global.input_device = None;
    app.global.prefer_iphone = false;
    fake.0.lock().unwrap().failures = 1;
    app.start_mic();
    pump_until(&mut app, "止まる", |app| app.mic == MicState::Off);
    assert_eq!(notice_of(&app), "開けませんでした");
}

/// 録音デバイスに選んだ iPhone が見えないときは、待たずに既定のマイクを使う。
#[test]
fn a_chosen_iphone_that_is_gone_is_not_waited_for() {
    let (mut app, fake) = iphone_app("link-pinned");
    app.global.prefer_iphone = false;
    app.global.input_device = Some("見えないiPhoneのマイク".to_string());
    app.start_mic();
    let opened = fake.clone();
    pump_until(&mut app, "開く", move |_| opened.opened().len() == 1);
    assert_eq!(fake.opened()[0], None);
    assert_eq!(app.iphone_wait_seconds(), None);
    app.stop_mic();
}

/// 2 台見えたら、最後に音が届いた方を選ぶ。
#[test]
fn the_iphone_that_last_worked_is_preferred() {
    let (mut app, fake) = iphone_app("link-two");
    let other = "別のiPhoneのマイク";
    fake.0.lock().unwrap().names = vec![other.to_string(), IPHONE.to_string()];
    app.global.last_iphone = Some(IPHONE.to_string());
    app.start_mic();
    let opened = fake.clone();
    pump_until(&mut app, "iPhone が開く", move |_| opened.opened().len() == 1);
    assert_eq!(fake.opened()[0].as_deref(), Some(IPHONE));
    app.stop_mic();
}

/// 本物の認識エンジン(Haas)。モデルか推論ライブラリが無ければ失敗にする(`#[ignore]` の試験からだけ呼ぶ)。
fn real_engine() -> speechcpp::SpeechCpp {
    let (Some(library), Some(model)) = (speechcpp::find_library(), speechcpp::find_model()) else {
        panic!("Haas のモデルか推論ライブラリが無い");
    };
    speechcpp::SpeechCpp::new(&library, &model).unwrap()
}

/// マイクの代わりに音声ファイルを、切り出しから認識まで通す。
#[test]
#[ignore = "モデルが要る"]
fn audio_flows_from_segmenter_to_the_real_engine_when_available() {
    use openspeech::core::segmenter::{Segmenter, SegmenterConfig};
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/sample-ja.wav");
    assert!(path.is_file(), "試験音声が無い");
    let samples: Vec<f32> = hound::WavReader::open(&path)
        .unwrap()
        .samples::<i16>()
        .map(|s| s.unwrap() as f32 / i16::MAX as f32)
        .collect();
    let mut segmenter = Segmenter::new(SegmenterConfig::default(), 0.5);
    let mut utterances = Vec::new();
    for chunk in samples.chunks(800) {
        utterances.extend(segmenter.push(chunk));
    }
    utterances.extend(segmenter.push(&vec![0.0; 16_000]));
    assert_eq!(utterances.len(), 1, "発話は 1 つのはず");
    assert!(
        !segmenter.is_too_loud(),
        "普通の音声を音割れと判定しないこと"
    );
    let mut engine = real_engine();
    let options = RecognizeOptions {
        words: Vec::new(),
        readings: std::collections::HashMap::new(),
    };
    let result = engine.recognize(&utterances[0], &options).unwrap();
    assert!(result.text.contains("結節"), "{}", result.text);
}

/// 実際の認識エンジンを通す。
#[test]
#[ignore = "モデルが要る"]
fn real_engine_recognizes_a_sample_when_available() {
    let sample = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/sample-ja.wav");
    assert!(sample.is_file(), "試験音声が無い");
    let mut engine = real_engine();
    let options = RecognizeOptions {
        words: Vec::new(),
        readings: std::collections::HashMap::new(),
    };
    let result = engine.recognize_file(&sample, &options).unwrap();
    assert!(!result.text.is_empty());
    assert!(result.text.contains("結節"), "{}", result.text);
}

/// Haas は句読点付きで出す。文末の「。」があっても音声コマンドとテンプレートは効き、英字の小文字も同じ語として扱う。
#[test]
fn text_with_the_punctuation_haas_writes_still_matches_commands_and_templates() {
    let mut app = app("haas-style");
    register(&mut app, CommandAction::NewLine, &["改行"]);
    app.data
        .dictionary
        .save_template(None, "前立腺MRI", "前立腺に異常なし")
        .unwrap();

    say(&mut app, "右肺上葉に5ミリの結節を認めます。");
    assert_eq!(app.editor.text, "右肺上葉に5ミリの結節を認めます。");
    say(&mut app, "改行。");
    assert!(app.editor.text.ends_with("認めます。\n"), "{:?}", app.editor.text);
    say(&mut app, "所見はありません、改行。");
    assert!(app.editor.text.ends_with("所見はありません\n"), "{:?}", app.editor.text);
    say(&mut app, "前立腺mri テンプレート。");
    assert!(app.editor.text.ends_with("前立腺に異常なし"), "{:?}", app.editor.text);
    say(&mut app, "まる。");
    assert!(app.editor.text.ends_with("前立腺に異常なし。"), "{:?}", app.editor.text);
}

/// 渡された語を控える偽の認識エンジン。
struct Spy {
    words: Arc<Mutex<Vec<String>>>,
}

impl Recognizer for Spy {
    fn name(&self) -> String {
        "spy".to_string()
    }
    fn recognize(
        &mut self,
        _samples: &[f32],
        options: &RecognizeOptions,
    ) -> Result<Recognition, String> {
        *self.words.lock().unwrap() = options.words.clone();
        Ok(Recognition::default())
    }
    fn recognize_file(
        &mut self,
        _path: &Path,
        options: &RecognizeOptions,
    ) -> Result<Recognition, String> {
        self.recognize(&[], options)
    }
}

/// 登録単語・コマンドの言葉・テンプレートのキーワードは、認識エンジンまで届く。
/// (Haas は英字を含む語を keywords にしない。それは `speechcpp` の試験で確かめる。)
#[test]
fn registered_words_reach_the_engine() {
    let mut app = app("engine-words");
    let words = Arc::new(Mutex::new(Vec::new()));
    let spy = Spy {
        words: words.clone(),
    };
    let options = Arc::new(Mutex::new(recognize_options(&app.data)));
    app.session = Session::new(Some(Box::new(spy)), options, Arc::new(|| {}));
    app.data.dictionary.add_word("肺上葉", "はいじょうよう").unwrap();
    app.data
        .dictionary
        .save_template(None, "前立腺MRI", "本文")
        .unwrap();
    register(&mut app, CommandAction::NewLine, &["次の行"]);
    app.refresh_engine_options();
    app.session.recognize_file(PathBuf::from("無い.wav"));
    let began = Instant::now();
    while words.lock().unwrap().is_empty() && began.elapsed() < Duration::from_secs(2) {
        std::thread::sleep(Duration::from_millis(5));
    }
    let words = words.lock().unwrap();
    for word in ["肺上葉", "前立腺MRI", "テンプレート", "次の行", "胸水"] {
        assert!(words.iter().any(|w| w == word), "{word}: {words:?}");
    }
}

/// Metal はモデルを持ったまま終わると abort するので、終了処理はモデルを解放してから戻る。
/// 解放した後は推論ライブラリの鍵を返さない(終了の直前に呼ぶ前提)ので、同じプロセスの他の試験が声の判定を
/// 読もうとすると止まってしまう。子プロセスで確かめる。
#[test]
fn shutdown_closes_the_speech_library() {
    const CHILD: &str = "OPENSPEECH_TEST_SHUTDOWN_CHILD";
    const DONE: &str = "shutdown-closed";
    if std::env::var_os(CHILD).is_some() {
        let mut app = app("engine-close");
        app.shutdown();
        assert!(speechcpp::is_closed());
        println!("{DONE}");
        return;
    }
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "--nocapture", "shutdown_closes_the_speech_library"])
        .env(CHILD, "1")
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert!(
        String::from_utf8_lossy(&output.stdout).contains(DONE),
        "子が最後まで走っていない"
    );
}

/// 不要語を除くと句読点しか残らない発話は入れない(句読点を出すエンジンで「えー。」が「。」になっていた)。
#[test]
fn an_utterance_with_only_a_filler_and_a_mark_leaves_nothing() {
    let mut app = app("filler-marks");
    for spoken in ["えー。", "えーと。", "えー?"] {
        say(&mut app, spoken);
        assert_eq!(app.editor.text, "", "{spoken}");
    }
    assert_eq!(app.pending_corrections(), 0);
    // エンジンが記号そのものを書いた発話は、不要語を除いたのではないので今までどおり入る。
    say(&mut app, "。");
    assert_eq!(app.editor.text, "。");
}

/// 補足のファイルが変わると、次に見たときに訂正役を 1 回だけ作り直す。
/// 設定で訂正を切ってあるので、作り直すと偽の訂正役が消えて None になる。
#[test]
fn a_changed_guidance_rebuilds_the_corrector_once() {
    let (mut app, _answer, _transfer) = wired_app("guidance-watch");
    app.global.corrector.enabled = false;
    let fake = app.corrector.clone();
    let start = Instant::now();
    let after = |seconds: u64| start + Duration::from_secs(seconds);

    app.watch_files(after(0));
    assert!(app.corrector.is_some(), "変わっていなければ作り直さない");
    assert_eq!(app.guidance_chars, Some(0));

    let path = app.store.ensure_guidance().unwrap();
    std::fs::write(&path, "単位は mm\n").unwrap();
    app.watch_files(after(1));
    assert!(app.corrector.is_some(), "2 秒たつまでは見ない");
    app.watch_files(after(2));
    assert!(app.corrector.is_none(), "変わったので作り直す");
    assert_eq!(app.guidance_chars, Some(6), "前後の空白は数えない");

    app.corrector = fake.clone();
    app.watch_files(after(4));
    assert!(app.corrector.is_some(), "同じ更新時刻では 2 度作り直さない");

    // 中身が同じでも、更新時刻が戻れば変化として扱う(控えから戻したときなど)。
    let file = std::fs::File::options().write(true).open(&path).unwrap();
    file.set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000))
        .unwrap();
    app.watch_files(after(6));
    assert!(app.corrector.is_none(), "更新時刻が戻っても作り直す");

    app.corrector = fake;
    std::fs::remove_file(&path).unwrap();
    app.watch_files(after(8));
    assert!(app.corrector.is_none(), "消したときも作り直す");
    assert_eq!(app.guidance_chars, Some(0));
}

/// 保存した補足が、毎フレームの処理を通って訂正役の指示文まで届く。読めないファイルは字数を None にする。
#[test]
fn a_saved_guidance_reaches_the_corrector() {
    let mut app = app("guidance-wired");
    app.store.save_api_key("k").unwrap();
    app.global.corrector.enabled = true;
    app.global.corrector.backend = CorrectorBackend::ApiKey;
    app.rebuild_corrector();
    let instructions = |app: &VoiceApp| app.corrector.as_ref().unwrap().instructions().to_string();
    assert!(instructions(&app).contains("してはいけないこと"), "ファイルが無ければ既定の指示");
    assert!(!instructions(&app).contains("試験の指示です"));

    let path = app.store.ensure_guidance().unwrap();
    std::fs::write(&path, "試験の指示です").unwrap();
    app.poll_events();
    assert!(instructions(&app).contains("直し方の指示:"));
    assert!(instructions(&app).contains("試験の指示です"));
    assert!(!instructions(&app).contains("してはいけないこと"), "利用者の指示が既定に置き換わる");

    // Claude Code の接続も同じ文を受け取る。入っていない機械では訂正役が作れないので見ない。
    app.global.corrector.backend = CorrectorBackend::ClaudeCode;
    app.rebuild_corrector();
    if app.corrector.is_some() {
        assert!(instructions(&app).contains("試験の指示です"));
    }
    app.global.corrector.backend = CorrectorBackend::ApiKey;
    app.rebuild_corrector();

    std::fs::write(&path, [0x82, 0xa0, 0xff]).unwrap();
    app.watch_files(Instant::now() + Duration::from_secs(2));
    assert_eq!(app.guidance_chars, None);
    assert!(!instructions(&app).contains("試験の指示です"), "読めなければ既定の指示で続ける");
    assert!(instructions(&app).contains("してはいけないこと"));
}

// ---- 辞書ファイルの見張り(単位 K) ----

/// 辞書ファイルを外で編集すると読み直し、読みが認識エンジンの語に、テンプレートが判定に届く。
/// 画面からの変更は書くだけで、自分の書き込みを外の更新と取り違えない。
#[test]
fn an_edited_dictionary_file_is_reloaded_and_own_writes_are_not() {
    let mut app = app("dict-watch");
    let path = app.store.dictionary_path();
    assert!(path.is_file(), "起動時に既定の辞書ファイルを作る");
    let start = Instant::now();
    let after = |seconds: u64| start + Duration::from_secs(seconds);
    app.watch_files(after(0));
    assert!(app.data.dictionary.words.is_empty());

    std::fs::write(
        &path,
        r#"{"words": [{"word": "肺気腫", "reading": "はいきしゅ"}],
            "symbols": [{"readings": ["ぷらす"], "text": "＋"}],
            "templates": [{"keywords": ["胸部正常"], "body": ["異常なし。", "以上。"], "memo": "自由項目"}]}"#,
    )
    .unwrap();
    app.watch_files(after(1));
    assert!(app.data.dictionary.words.is_empty(), "2 秒たつまでは見ない");
    app.watch_files(after(2));
    assert_eq!(app.data.dictionary.words[0].word, "肺気腫");
    let options = recognize_options(&app.data);
    assert!(options.words.contains(&"肺気腫".to_string()));
    assert_eq!(options.readings["肺気腫"], "はいきしゅ");
    say(&mut app, "ぷらす");
    say(&mut app, "胸部正常テンプレート");
    assert_eq!(app.editor.text, "＋異常なし。\n以上。");

    // 画面から足した語は書かれ、次の見張りで読み直さず、控えも作らない。
    app.data.dictionary.add_word("腎嚢胞", "じんのうほう").unwrap();
    app.save_data();
    let written = std::fs::read_to_string(&path).unwrap();
    assert!(written.contains("腎嚢胞") && written.contains("自由項目"), "{written}");
    app.watch_files(after(4));
    assert_eq!(app.data.dictionary.words.len(), 2);
    assert!(notice_of(&app).is_empty(), "{}", notice_of(&app));
    assert!(conflict_copies(&app).is_empty());
}

/// 読めないファイル(編集の途中)は読み直さず、書き換えもしない。画面で辞書を変えたら、その中身を残してから書く。
#[test]
fn a_broken_dictionary_file_is_kept_and_set_aside_before_a_ui_write() {
    let mut app = app("dict-broken");
    let path = app.store.dictionary_path();
    let start = Instant::now();
    std::fs::write(&path, "{not json").unwrap();
    app.watch_files(start + Duration::from_secs(2));
    assert_eq!(app.data.dictionary, Dictionary::default(), "今の辞書のまま");
    assert_eq!(std::fs::read(&path).unwrap(), b"{not json", "触らない");
    assert!(notice_of(&app).contains("読めません"), "{}", notice_of(&app));

    app.data.dictionary.add_word("肺気腫", "はいきしゅ").unwrap();
    app.save_data();
    let kept = conflict_copies(&app);
    assert_eq!(kept.len(), 1);
    assert_eq!(std::fs::read(&kept[0]).unwrap(), b"{not json", "編集中の中身を残す");
    assert!(std::fs::read_to_string(&path).unwrap().contains("肺気腫"));
    assert!(notice_of(&app).contains("残して"), "{}", notice_of(&app));
}

/// 外で変えた直後(見張りが読む前)に画面で辞書を変えても、外の編集は控えに残る。消したファイルは作り直す。
#[test]
fn a_manual_edit_just_before_a_ui_change_is_kept_aside() {
    let mut app = app("dict-race");
    let path = app.store.dictionary_path();
    let start = Instant::now();
    std::fs::write(&path, r#"{"words": [{"word": "手で足した語"}]}"#).unwrap();
    app.data.dictionary.add_word("画面で足した語", "").unwrap();
    app.save_data();
    let kept = conflict_copies(&app);
    assert_eq!(kept.len(), 1);
    assert!(std::fs::read_to_string(&kept[0]).unwrap().contains("手で足した語"));
    let written = std::fs::read_to_string(&path).unwrap();
    assert!(written.contains("画面で足した語") && !written.contains("手で足した語"), "{written}");
    app.watch_files(start + Duration::from_secs(2));
    assert_eq!(app.data.dictionary.words.len(), 1, "自分の書き込みは読み直さない");

    std::fs::remove_file(&path).unwrap();
    app.watch_files(start + Duration::from_secs(4));
    assert!(path.is_file(), "消されたら今の辞書で作り直す");
    assert!(std::fs::read_to_string(&path).unwrap().contains("画面で足した語"));
    assert_eq!(conflict_copies(&app).len(), 1, "作り直しは控えを増やさない");
}

fn conflict_copies(app: &VoiceApp) -> Vec<PathBuf> {
    std::fs::read_dir(&app.store.root)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with("dictionary.conflict-"))
        })
        .collect()
}

/// 起動の前からあるファイルは、起動時に読んだので変化ではない。
#[test]
fn guidance_saved_before_the_start_is_read_once() {
    let home = std::env::temp_dir().join(format!("openspeech-it-guidance-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    let store = Store::at(&home);
    std::fs::write(store.ensure_guidance().unwrap(), "分野は放射線").unwrap();
    let mut app = VoiceApp::with_store(egui::Context::default(), store, None);
    assert_eq!(app.guidance_chars, Some(6));
    let (_answer, answers) = mpsc::channel();
    app.corrector = Some(Arc::new(GatedCorrector {
        answers: Mutex::new(answers),
        slow: None,
        fix: ("", ""),
        asked: Arc::default(),
    }));
    app.watch_files(Instant::now());
    assert!(app.corrector.is_some());
}

// ---- 訂正を確定の前に置く流れ(単位 F) ----

fn state_of(app: &VoiceApp, index: usize) -> CorrectionState {
    app.editor.utterances[index].correction
}

/// 上限まで時計を進めたことにして、画面の更新と同じ処理を 1 回回す。
fn past_the_limit(app: &mut VoiceApp) {
    app.watch_corrections(Instant::now() + Duration::from_secs(7));
    app.poll_events();
}

/// 認識文で一致した命令語・記号・テンプレートはその場で働き、AI を呼ばない。
#[test]
fn matches_in_the_recognized_text_never_reach_the_ai() {
    let (mut app, _answer, _fake) = wired_app("f-first");
    register(&mut app, CommandAction::NewLine, &["改行"]);
    with_template(&mut app);
    for spoken in ["改行", "矢印。", "胸部正常テンプレート。", "改行"] {
        say(&mut app, spoken);
    }
    assert_eq!(app.pending_corrections(), 0);
    assert_eq!(app.editor.text, "\n→肺野に異常なし。\n胸水なし。\n");
    assert!(!app.editor.awaiting_correction());
}

/// 認識文で外れた文末の命令語は、AI が直した文で効く。語を除いた文で確定し、言葉の直しは覚えない。
#[test]
fn a_trailing_word_the_engine_misheard_works_after_correction() {
    let (mut app, answer, _fake) = wired_app("f-newline");
    register(&mut app, CommandAction::NewLine, &["改行"]);
    say(&mut app, "所見なし、開業。");
    assert_eq!(app.pending_corrections(), 1);
    assert_eq!(app.editor.text, "所見なし、開業。", "戻るまでは仮表示");
    assert_eq!(app.editor.provisional_ranges(), vec![(0, 8)]);
    answer.send(Ok("所見なし、改行。".to_string())).unwrap();
    pump_until(&mut app, "確定", |app| !app.editor.awaiting_correction());
    assert_eq!(app.editor.text, "所見なし\n");
    assert_eq!(state_of(&app, 0), CorrectionState::Settled);
    assert_eq!(app.editor.utterances[0].end, 4, "改行は発話の外");
    assert!(app.data.memory.pairs.is_empty(), "{:?}", app.data.memory.pairs);
    say(&mut app, "次の行。");
    assert_eq!(app.editor.text, "所見なし\n次の行。", "続きは改行の後ろへ入る");
    answer.send(Ok("次の行。".to_string())).unwrap();
}

/// 発話全体が命令語だったと分かったら、文は残らない。
#[test]
fn a_whole_utterance_can_turn_out_to_be_a_command() {
    let (mut app, answer, _fake) = wired_app("f-whole");
    register(&mut app, CommandAction::NewLine, &["改行"]);
    say(&mut app, "開業。");
    assert_eq!(app.pending_corrections(), 1);
    answer.send(Ok("改行。".to_string())).unwrap();
    pump_until(&mut app, "確定", |app| !app.editor.awaiting_correction());
    assert_eq!(app.editor.text, "\n");
    assert!(app.editor.utterances.is_empty());
    assert_eq!(app.editor.cursor(), 1);
}

/// 記号の読みは話した時点の判定だけ。訂正で記号の読みになっても文として入る。
#[test]
fn symbols_do_not_work_after_correction() {
    let (mut app, answer, _fake) = wired_app("f-symbol-late");
    say(&mut app, "矢じり");
    answer.send(Ok("矢印".to_string())).unwrap();
    pump_until(&mut app, "確定", |app| !app.editor.awaiting_correction());
    assert_eq!(app.editor.text, "矢印");
}

/// 後ろに別の発話があっても、改行は話した発話の直後に入り、カーソルは続きの入力位置のまま。
#[test]
fn a_late_line_end_goes_after_its_own_utterance() {
    let (mut app, answer, _fake) = wired_app_slow_on("f-position", Some("開業"));
    register(&mut app, CommandAction::NewLine, &["改行"]);
    say(&mut app, "一文目、開業。");
    say(&mut app, "二文目。");
    pump_until(&mut app, "後の文の戻り", |app| app.pending_corrections() == 1);
    app.selection_dirty = false;
    answer.send(Ok("一文目、改行。".to_string())).unwrap();
    pump_until(&mut app, "確定", |app| !app.editor.awaiting_correction());
    assert_eq!(app.editor.text, "一文目\n二文目。");
    assert_eq!(app.editor.cursor(), app.editor.len());
    assert!(app.selection_dirty, "動いたカーソルを画面へ反映する");
}

/// 訂正文全体がキーワードならテンプレートの文に置き換える。学習せず、Esc で 1 発話として消せる。
#[test]
fn a_keyword_the_engine_misheard_expands_after_correction() {
    let (mut app, answer, _fake) = wired_app("f-template");
    with_template(&mut app);
    say(&mut app, "今部正常天ぷらと。");
    assert_eq!(app.editor.text, "今部正常天ぷらと。");
    answer.send(Ok("胸部正常テンプレート。".to_string())).unwrap();
    pump_until(&mut app, "確定", |app| !app.editor.awaiting_correction());
    assert_eq!(app.editor.text, "肺野に異常なし。\n胸水なし。");
    assert!(app.data.memory.pairs.is_empty(), "{:?}", app.data.memory.pairs);
    assert!(app.editor.utterances[0].fixed, "手を入れても覚えない");
    app.take_back();
    assert!(app.editor.is_empty());
}

/// 文末の命令語とテンプレートは、訂正のあとでも判定 1 と同じに組み合わさる。
#[test]
fn a_keyword_and_a_trailing_word_work_together_after_correction() {
    let (mut app, answer, _fake) = wired_app("f-template-newline");
    with_template(&mut app);
    register(&mut app, CommandAction::NewLine, &["改行"]);
    say(&mut app, "今部正常天ぷらと、開業。");
    answer.send(Ok("胸部正常テンプレート、改行。".to_string())).unwrap();
    pump_until(&mut app, "確定", |app| !app.editor.awaiting_correction());
    assert_eq!(app.editor.text, "肺野に異常なし。\n胸水なし。\n");
    assert_eq!(app.editor.cursor(), app.editor.len());
}

/// 後ろに文があっても、改行は本文の直後に入る(本文の字数で位置を数える)。
#[test]
fn a_late_template_keeps_the_next_utterance_after_it() {
    let (mut app, answer, _fake) = wired_app_slow_on("f-template-position", Some("今部"));
    with_template(&mut app);
    register(&mut app, CommandAction::NewLine, &["改行"]);
    say(&mut app, "今部正常天ぷらと、開業。");
    say(&mut app, "二文目。");
    pump_until(&mut app, "後の文の戻り", |app| app.pending_corrections() == 1);
    answer.send(Ok("胸部正常テンプレート、改行。".to_string())).unwrap();
    pump_until(&mut app, "確定", |app| !app.editor.awaiting_correction());
    assert_eq!(app.editor.text, "肺野に異常なし。\n胸水なし。\n二文目。");
}

/// AI が文を語に置き換えた答えでは、命令もテンプレートも働かせない。文は消えず、送らない。
#[test]
fn an_answer_that_replaces_the_sentence_with_a_word_does_not_fire() {
    let (mut app, answer, fake) = wired_app("f-shrunk");
    with_template(&mut app);
    say(&mut app, "前の文。");
    answer.send(Ok("前の文。".to_string())).unwrap();
    pump_until(&mut app, "確定", |app| !app.editor.awaiting_correction());
    for (spoken, answered) in [
        ("所見は電です。", "送信"),
        ("胸部は本当に正常と言ってよいでしょう。", "胸部正常テンプレート"),
    ] {
        say(&mut app, spoken);
        answer.send(Ok(answered.to_string())).unwrap();
        pump_until(&mut app, "確定", |app| !app.editor.awaiting_correction());
    }
    for _ in 0..5 {
        app.poll_events();
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(fake.sent().is_empty(), "{:?}", fake.sent());
    assert_eq!(app.editor.text, "前の文。所見は電です。胸部は本当に正常と言ってよいでしょう。");
    assert!(notice_of(&app).contains("認識した文を残しました"), "{}", notice_of(&app));
}

/// 句読点の分の縮みまでは判定する。3 字以上の縮みでは判定しない。
#[test]
fn a_small_shrink_still_counts_as_a_match() {
    for (answered, expected) in [("所見なし改行", "所見なし\n"), ("所見、改行", "所見なし、開業。")] {
        let (mut app, answer, _fake) = wired_app("f-shrink-limit");
        register(&mut app, CommandAction::NewLine, &["改行"]);
        say(&mut app, "所見なし、開業。");
        answer.send(Ok(answered.to_string())).unwrap();
        pump_until(&mut app, "確定", |app| !app.editor.awaiting_correction());
        assert_eq!(app.editor.text, expected);
    }
}

/// 命令語への直しは文の途中でも覚えない。同じ文のほかの直しは覚え、テンプレートのキーワードへの直しも
/// 覚える(キーワードは検査名で、「テンプレート」が続かなければ判定は働かない)。
#[test]
fn fixes_into_command_words_are_not_learned() {
    let (mut app, answer, _fake) = wired_app("f-learn-word");
    register(&mut app, CommandAction::NewLine, &["改行"]);
    app.data.dictionary.save_template(None, "陰性", "本文").unwrap();
    say(&mut app, "右配上葉のここで開業して結果は隠世です");
    answer
        .send(Ok("右肺上葉のここで改行して結果は陰性です".to_string()))
        .unwrap();
    pump_until(&mut app, "確定", |app| !app.editor.awaiting_correction());
    assert_eq!(app.editor.text, "右肺上葉のここで改行して結果は陰性です");
    let pairs: Vec<&str> = app.data.memory.pairs.iter().map(|p| p.to.as_str()).collect();
    assert_eq!(pairs, vec!["右肺上", "陰性"]);
}

/// 組は語の断片のことがある。命令語の断片への直しと、語を含む長い組も覚えない。キーワードの断片は覚える。
#[test]
fn fragments_of_command_words_are_not_learned() {
    let (mut app, answer, _fake) = wired_app("f-learn-fragment");
    register(&mut app, CommandAction::NewLine, &["ここで改行"]);
    register(&mut app, CommandAction::Transfer, &["段落"]);
    app.data.dictionary.save_template(None, "胸部正常", "本文").unwrap();
    for (spoken, answered) in [
        ("ここで開業して続ける", "ここで改行して続ける"),
        ("結果は談楽らです", "結果は段落だです"),
    ] {
        say(&mut app, spoken);
        answer.send(Ok(answered.to_string())).unwrap();
        pump_until(&mut app, "確定", |app| !app.editor.awaiting_correction());
    }
    assert!(app.data.memory.pairs.is_empty(), "{:?}", app.data.memory.pairs);
    say(&mut app, "強舞の結果");
    answer.send(Ok("胸部の結果".to_string())).unwrap();
    pump_until(&mut app, "確定", |app| !app.editor.awaiting_correction());
    let pairs: Vec<&str> = app.data.memory.pairs.iter().map(|p| p.to.as_str()).collect();
    assert_eq!(pairs, vec!["胸部"], "キーワードの断片は医療語なので覚える");
}

/// AI が左右を変えた答えは、確定の入口でも捨てる。
#[test]
fn an_answer_that_flips_laterality_is_not_used() {
    let (mut app, answer, _fake) = wired_app("f-laterality");
    say(&mut app, "右肺上葉に結節");
    answer.send(Ok("左肺上葉に結節".to_string())).unwrap();
    pump_until(&mut app, "確定", |app| !app.editor.awaiting_correction());
    assert_eq!(app.editor.text, "右肺上葉に結節");
    assert!(app.data.memory.pairs.is_empty(), "{:?}", app.data.memory.pairs);
}

/// AI の訂正は手直しと数えない(機械の文を更新しないと、転送のときに手直しとして重く覚える)。
#[test]
fn an_applied_correction_is_not_learned_again_as_a_hand_edit() {
    let (mut app, answer, _fake) = wired_app("f-machine");
    say(&mut app, "右配上葉");
    answer.send(Ok("右肺上葉".to_string())).unwrap();
    pump_until(&mut app, "確定", |app| !app.editor.awaiting_correction());
    app.transfer();
    pump_until(&mut app, "送って消える", |app| app.editor.is_empty());
    assert!(app.data.memory.pairs.iter().all(|p| p.manual == 0), "{:?}", app.data.memory.pairs);
}

/// 上限を過ぎたら認識文で確定し、手直しの前に遅れて届いた訂正は当てて覚える。
#[test]
fn the_limit_settles_the_text_and_a_late_correction_still_lands() {
    let (mut app, answer, _fake) = wired_app("f-late");
    say(&mut app, "右配上葉");
    assert_eq!(state_of(&app, 0), CorrectionState::Waiting);
    app.watch_corrections(Instant::now() + Duration::from_secs(5));
    assert_eq!(state_of(&app, 0), CorrectionState::Waiting, "上限までは待つ");
    past_the_limit(&mut app);
    assert_eq!(state_of(&app, 0), CorrectionState::Failed);
    assert!(app.editor.provisional_ranges().is_empty());
    assert_eq!(app.editor.text, "右配上葉");
    answer.send(Ok("右肺上葉".to_string())).unwrap();
    pump_until(&mut app, "遅れて当たる", |app| app.editor.text == "右肺上葉");
    assert_eq!(state_of(&app, 0), CorrectionState::Settled);
    assert!(
        app.data.memory.pairs.iter().any(|p| p.to == "右肺上"),
        "{:?}",
        app.data.memory.pairs
    );
}

/// 手直しのあとに遅れて届いた訂正は捨て、覚えない。
#[test]
fn a_late_correction_after_a_hand_edit_is_dropped() {
    let (mut app, answer, _fake) = wired_app("f-late-edit");
    say(&mut app, "右配上葉");
    past_the_limit(&mut app);
    app.sync_editor_from_input("右配の上葉", (3, 3));
    answer.send(Ok("右肺上葉".to_string())).unwrap();
    pump_until(&mut app, "訂正の戻り", |app| app.pending_corrections() == 0);
    assert_eq!(app.editor.text, "右配の上葉");
    assert!(app.data.memory.pairs.is_empty(), "{:?}", app.data.memory.pairs);
}

/// 訂正の遅れは送る文を変えない。上限のあとに届いた「転送」は働かせず、語だけ除いて知らせる。
#[test]
fn a_transfer_word_in_a_late_correction_does_not_send() {
    let (mut app, answer, fake) = wired_app("f-late-transfer");
    say(&mut app, "所見なし、電送。");
    past_the_limit(&mut app);
    answer.send(Ok("所見なし、送信。".to_string())).unwrap();
    pump_until(&mut app, "遅れて当たる", |app| app.editor.text == "所見なし");
    assert!(fake.sent().is_empty());
    assert!(notice_of(&app).contains("もう一度言ってください"), "{}", notice_of(&app));
}

/// 後の文があるときに言い直しを勧めると、後の文まで送ってしまう。手で送るよう知らせる。
#[test]
fn a_late_transfer_word_with_a_later_sentence_asks_for_a_manual_send() {
    let (mut app, answer, fake) = wired_app_slow_on("f-late-transfer-next", Some("電送"));
    say(&mut app, "報告一の文、電送。");
    say(&mut app, "報告二の文。");
    pump_until(&mut app, "後の文の戻り", |app| app.pending_corrections() == 1);
    past_the_limit(&mut app);
    answer.send(Ok("報告一の文、送信。".to_string())).unwrap();
    pump_until(&mut app, "遅れて当たる", |app| app.editor.text == "報告一の文報告二の文。");
    assert!(fake.sent().is_empty());
    assert!(notice_of(&app).contains("手で送ってください"), "{}", notice_of(&app));
}

/// 後の発話の確定(判定 2 と学習)は前の発話を待つ。仮表示はすぐ出る。
#[test]
fn a_later_utterance_settles_after_the_earlier_one() {
    let (mut app, answer, _fake) = wired_app_fixing("f-order", Some("遅い"), ("右配", "右肺"));
    say(&mut app, "遅い文。");
    say(&mut app, "右配上葉");
    assert_eq!(app.editor.text, "遅い文。右配上葉", "後の発話も仮表示は出る");
    pump_until(&mut app, "後の文の戻り", |app| app.pending_corrections() == 1);
    for _ in 0..3 {
        app.poll_events();
    }
    assert_eq!(app.editor.text, "遅い文。右配上葉", "前が未確定の間は当てない");
    assert_eq!(state_of(&app, 1), CorrectionState::Waiting);
    assert!(app.data.memory.pairs.is_empty(), "学習も話した順");
    answer.send(Ok("遅い文です。".to_string())).unwrap();
    pump_until(&mut app, "順に確定", |app| !app.editor.awaiting_correction());
    assert_eq!(app.editor.text, "遅い文です。右肺上葉");
    assert!(
        app.data.memory.pairs.iter().any(|p| p.to == "右肺上"),
        "{:?}",
        app.data.memory.pairs
    );
}

/// 前の発話が上限で確定したら、後の発話も進む。
#[test]
fn the_limit_of_the_earlier_utterance_releases_the_later_one() {
    let (mut app, answer, _fake) = wired_app_fixing("f-order-limit", Some("遅い"), ("右配", "右肺"));
    say(&mut app, "遅い文。");
    say(&mut app, "右配上葉");
    pump_until(&mut app, "後の文の戻り", |app| app.pending_corrections() == 1);
    past_the_limit(&mut app);
    assert_eq!(app.editor.text, "遅い文。右肺上葉");
    assert_eq!(state_of(&app, 0), CorrectionState::Failed);
    assert_eq!(state_of(&app, 1), CorrectionState::Settled);
    answer.send(Ok("遅い文です。".to_string())).unwrap();
    pump_until(&mut app, "遅れて当たる", |app| app.editor.text == "遅い文です。右肺上葉");
}

/// 手で直した発話は、もう訂正を待たない。後の発話を止めない。
#[test]
fn an_edited_utterance_does_not_hold_up_the_next_one() {
    let (mut app, answer, _fake) = wired_app_fixing("f-order-edit", Some("遅い"), ("右配", "右肺"));
    say(&mut app, "遅い文。");
    say(&mut app, "右配上葉");
    pump_until(&mut app, "後の文の戻り", |app| app.pending_corrections() == 1);
    app.sync_editor_from_input("直した文。右配上葉", (5, 5));
    pump_until(&mut app, "確定", |app| !app.editor.awaiting_correction());
    assert_eq!(app.editor.text, "直した文。右肺上葉");
    assert_eq!(state_of(&app, 0), CorrectionState::Settled, "手で直した文は待たない");
    answer.send(Err("遅れて失敗".to_string())).unwrap();
    pump_until(&mut app, "訂正の戻り", |app| app.pending_corrections() == 0);
    app.poll_events();
    assert_eq!(app.editor.text, "直した文。右肺上葉");
    assert_eq!(state_of(&app, 0), CorrectionState::Settled, "直した文の失敗は関係ない");
    assert!(!notice_of(&app).contains("訂正できませんでした"));
}

/// 認識文で一致した「転送」も、前の発話の未確定を待ち、上限で出る。
#[test]
fn a_spoken_transfer_waits_for_earlier_utterances_up_to_the_limit() {
    let (mut app, answer, fake) = wired_app("f-transfer-limit");
    say(&mut app, "一文目。");
    say(&mut app, "送信");
    assert!(app.transfer_waiting());
    app.watch_corrections(Instant::now() + Duration::from_secs(5));
    app.poll_events();
    assert!(app.transfer_waiting(), "上限までは待つ");
    past_the_limit(&mut app);
    pump_until(&mut app, "送って消える", |app| app.editor.is_empty());
    assert_eq!(fake.sent(), vec!["一文目。"]);
    assert!(notice_of(&app).contains("AI 訂正を通っていない文を含めて転送しました"));
    answer.send(Ok("一文目です。".to_string())).unwrap();
    pump_until(&mut app, "訂正の戻り", |app| app.pending_corrections() == 0);
    assert!(app.editor.is_empty());
}

/// 訂正のあとで分かった「転送」は、話した文までを送る。そのあとに話した文は送らず、消さない。
#[test]
fn a_misheard_transfer_word_sends_only_what_came_before_it() {
    let (mut app, answer, fake) = wired_app_fixing("f-transfer", Some("電送"), ("二文", "次の文"));
    say(&mut app, "所見なし、電送。");
    say(&mut app, "二文目。");
    assert_eq!(app.editor.text, "所見なし、電送。二文目。");
    pump_until(&mut app, "後の文の戻り", |app| app.pending_corrections() == 1);
    answer.send(Ok("所見なし、送信。".to_string())).unwrap();
    pump_until(&mut app, "送って続きが残る", |app| {
        !app.editor.awaiting_correction() && app.editor.text == "次の文目。"
    });
    assert_eq!(fake.sent(), vec!["所見なし"]);
    assert!(notice_of(&app).starts_with("転送しました"), "{}", notice_of(&app));
}

/// 聞き違えた「転送」のあとに次の報告と「転送」を話しても、2 つの報告は別々に送る。
#[test]
fn two_reports_stay_apart_when_the_first_transfer_word_was_misheard() {
    for (second, third, expected) in [
        ("報告二の文。", Some("送信"), "報告二の文。"),
        ("報告二の文 送信", None, "報告二の文"),
    ] {
        let (mut app, answer, fake) = wired_app_slow_on("f-two-reports", Some("電送"));
        say(&mut app, "報告一の文、電送。");
        say(&mut app, second);
        if let Some(word) = third {
            say(&mut app, word);
        }
        assert!(app.transfer_waiting(), "{second}");
        pump_until(&mut app, "後の文の戻り", |app| app.pending_corrections() == 1);
        answer.send(Ok("報告一の文、送信。".to_string())).unwrap();
        pump_until(&mut app, "両方送って消える", |app| {
            app.editor.is_empty() && !app.transfer_waiting()
        });
        assert_eq!(fake.sent(), vec!["報告一の文", expected], "{second}");
    }
}

/// 先頭だけ送っている途中の「転送」は、残りの文のものなので聞き流さない。
#[test]
fn a_transfer_word_during_a_partial_send_is_for_the_rest() {
    let (mut app, answer, fake) = wired_app_slow_on("f-part-repeat", Some("電送"));
    let open = fake.close_gate();
    say(&mut app, "報告一の文、電送。");
    say(&mut app, "報告二の文。");
    pump_until(&mut app, "後の文の戻り", |app| app.pending_corrections() == 1);
    answer.send(Ok("報告一の文、送信。".to_string())).unwrap();
    let sending = fake.clone();
    pump_until(&mut app, "先頭を送り始める", move |_| !sending.sent().is_empty());
    say(&mut app, "送信");
    assert_eq!(app.held_utterance_count(), 1);
    open.send(Ok(())).unwrap();
    let sent = fake.clone();
    pump_until(&mut app, "残りも送り始める", move |_| sent.sent().len() == 2);
    open.send(Ok(())).unwrap();
    pump_until(&mut app, "送って消える", |app| app.editor.is_empty());
    assert_eq!(fake.sent(), vec!["報告一の文", "報告二の文。"]);
}

/// 先頭の転送に失敗したら文は全部残り、待っている「転送」で続きと一緒に送らない。
#[test]
fn a_failed_partial_send_does_not_send_the_rest_with_it() {
    let (mut app, answer, fake) = wired_app_slow_on("f-part-failed", Some("電送"));
    let open = fake.close_gate();
    say(&mut app, "報告一の文、電送。");
    say(&mut app, "報告二の文。");
    say(&mut app, "送信");
    pump_until(&mut app, "後の文の戻り", |app| app.pending_corrections() == 1);
    answer.send(Ok("報告一の文、送信。".to_string())).unwrap();
    let sending = fake.clone();
    pump_until(&mut app, "先頭を送り始める", move |_| !sending.sent().is_empty());
    open.send(Err(TransferError::NoTarget)).unwrap();
    pump_until(&mut app, "待ちが解ける", |app| !app.transfer_waiting());
    for _ in 0..5 {
        app.poll_events();
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(fake.sent(), vec!["報告一の文"]);
    assert_eq!(app.editor.text, "報告一の文報告二の文。");
}

/// 転送の処理役が返らなくても、後の発話の確定は止まったままにならない。
#[test]
fn a_stuck_partial_send_still_settles_the_rest() {
    let (mut app, answer, fake) = wired_app_slow_on("f-part-stuck", Some("電送"));
    let open = fake.close_gate();
    say(&mut app, "報告一の文、電送。");
    say(&mut app, "報告二の文。");
    pump_until(&mut app, "後の文の戻り", |app| app.pending_corrections() == 1);
    answer.send(Ok("報告一の文、送信。".to_string())).unwrap();
    let sending = fake.clone();
    pump_until(&mut app, "先頭を送り始める", move |_| !sending.sent().is_empty());
    assert!(app.editor.awaiting_correction(), "送っている間は確定しない");
    app.watch_transfer(Instant::now() + Duration::from_secs(31));
    pump_until(&mut app, "確定", |app| !app.editor.awaiting_correction());
    open.send(Ok(())).unwrap();
    pump_until(&mut app, "送った分が消える", |app| app.editor.text == "報告二の文。");
    assert_eq!(fake.sent(), vec!["報告一の文"]);
}

/// 後から話した文が手前にあると切り分けられない。後の文を混ぜて送らず、知らせる。
#[test]
fn a_later_sentence_in_front_stops_a_transfer_found_after_correction() {
    let (mut app, answer, fake) = wired_app_slow_on("f-front", Some("電送"));
    say(&mut app, "報告一の文、電送。");
    app.editor.move_start();
    say(&mut app, "あとの文。");
    pump_until(&mut app, "後の文の戻り", |app| app.pending_corrections() == 1);
    answer.send(Ok("報告一の文、送信。".to_string())).unwrap();
    pump_until(&mut app, "確定", |app| !app.editor.awaiting_correction());
    for _ in 0..5 {
        app.poll_events();
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(fake.sent().is_empty(), "{:?}", fake.sent());
    assert_eq!(app.editor.text, "あとの文。報告一の文");
    assert!(notice_of(&app).contains("手で送ってください"), "{}", notice_of(&app));
}

/// 前に話した文が切る位置より後ろにあるときも同じ(先に話した文を残して送らない)。
#[test]
fn an_earlier_sentence_behind_the_cut_stops_the_transfer() {
    let (mut app, answer, fake) = wired_app_slow_on("f-behind", Some("電送"));
    say(&mut app, "前の文。");
    app.editor.move_start();
    say(&mut app, "途中の文、電送。");
    say(&mut app, "続きの文。");
    pump_until(&mut app, "ほかの文の戻り", |app| app.pending_corrections() == 1);
    answer.send(Ok("途中の文、送信。".to_string())).unwrap();
    pump_until(&mut app, "確定", |app| !app.editor.awaiting_correction());
    for _ in 0..5 {
        app.poll_events();
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(fake.sent().is_empty(), "{:?}", fake.sent());
    assert_eq!(app.editor.text, "途中の文続きの文。前の文。");
}

/// ⌘Enter に失敗して文が残っても、あとから分かった「転送」で送り直さない。
#[test]
fn a_failed_send_key_is_not_retried_by_a_late_transfer_word() {
    let (mut app, answer, fake) = wired_app("f-key-failed");
    let open = fake.close_gate();
    say(&mut app, "所見なし、電送。");
    app.transfer();
    open.send(Err(TransferError::NoTarget)).unwrap();
    pump_until(&mut app, "転送の終わり", |app| notice_of(app).contains("転送先"));
    answer.send(Ok("所見なし、送信。".to_string())).unwrap();
    pump_until(&mut app, "確定", |app| !app.editor.awaiting_correction());
    for _ in 0..5 {
        app.poll_events();
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(fake.sent().len(), 1, "{:?}", fake.sent());
    assert_eq!(app.editor.text, "所見なし");
}

/// 取りやめのあとに置いてあった発話は、訂正のあとで「転送」と分かっても送らない。文末の語の前の文も同じ。
#[test]
fn a_held_utterance_after_a_cancel_does_not_send_after_correction() {
    for (held, text) in [
        ("二文目、電送。", "一文目二文目、電送。"),
        ("二文目、電送 送信", "一文目二文目、電送"),
    ] {
        let (mut app, answer, fake) = wired_app_fixing("f-held-cancel", Some("一文目"), ("電送", "送信"));
        say(&mut app, "一文目 送信");
        say(&mut app, held);
        assert_eq!(app.held_utterance_count(), 1);
        app.take_back();
        app.poll_events();
        assert_eq!(app.editor.text, text);
        answer.send(Ok("一文目".to_string())).unwrap();
        pump_until(&mut app, "確定", |app| !app.editor.awaiting_correction());
        for _ in 0..5 {
            app.poll_events();
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(fake.sent().is_empty(), "{held}: {:?}", fake.sent());
        assert!(!app.editor.text.contains("送信"), "{held}: {}", app.editor.text);
    }
}

/// 上限のあとの転送の途中に遅れて届いた訂正は、送っている文を書き換えない(消す分と合わなくなる)。
#[test]
fn a_late_correction_during_the_send_does_not_change_the_sent_text() {
    let (mut app, answer, fake) = wired_app("f-late-sending");
    let open = fake.close_gate();
    say(&mut app, "右配上葉 送信");
    past_the_limit(&mut app);
    let sending = fake.clone();
    pump_until(&mut app, "送り始める", move |_| !sending.sent().is_empty());
    answer.send(Ok("右肺上葉".to_string())).unwrap();
    pump_until(&mut app, "訂正の戻り", |app| app.pending_corrections() == 0);
    for _ in 0..3 {
        app.poll_events();
    }
    assert_eq!(app.editor.text, "右配上葉", "送っている間は書き換えない");
    open.send(Ok(())).unwrap();
    pump_until(&mut app, "送って消える", |app| app.editor.is_empty());
    assert_eq!(fake.sent(), vec!["右配上葉"]);
}

/// 先頭の転送に失敗したら、未確定の発話の訂正で分かった「転送」も働かせない。
#[test]
fn a_failed_partial_send_stops_a_transfer_found_later() {
    let (mut app, answer, fake) = wired_app_fixing("f-part-failed-next", Some("電送"), ("電信", "送信"));
    let open = fake.close_gate();
    say(&mut app, "報告一の文、電送。");
    say(&mut app, "報告二の文、電信。");
    pump_until(&mut app, "後の文の戻り", |app| app.pending_corrections() == 1);
    answer.send(Ok("報告一の文、送信。".to_string())).unwrap();
    let sending = fake.clone();
    pump_until(&mut app, "先頭を送り始める", move |_| !sending.sent().is_empty());
    open.send(Err(TransferError::NoTarget)).unwrap();
    pump_until(&mut app, "確定", |app| !app.editor.awaiting_correction());
    for _ in 0..5 {
        app.poll_events();
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(fake.sent(), vec!["報告一の文"]);
    assert_eq!(app.editor.text, "報告一の文報告二の文");
    assert!(notice_of(&app).contains("働かせませんでした"), "{}", notice_of(&app));
}

/// 先頭の転送に失敗したら、置いてあった発話の「転送」も働かせない。
#[test]
fn a_failed_partial_send_stops_a_held_transfer_word() {
    let (mut app, answer, fake) = wired_app_slow_on("f-part-failed-held", Some("電送"));
    let open = fake.close_gate();
    say(&mut app, "報告一の文、電送。");
    say(&mut app, "報告二の文。");
    pump_until(&mut app, "後の文の戻り", |app| app.pending_corrections() == 1);
    answer.send(Ok("報告一の文、送信。".to_string())).unwrap();
    let sending = fake.clone();
    pump_until(&mut app, "先頭を送り始める", move |_| !sending.sent().is_empty());
    say(&mut app, "報告三の文 送信");
    assert_eq!(app.held_utterance_count(), 1);
    open.send(Err(TransferError::NoTarget)).unwrap();
    pump_until(&mut app, "続きが入る", |app| app.held_utterance_count() == 0);
    pump_until(&mut app, "確定", |app| !app.editor.awaiting_correction());
    for _ in 0..5 {
        app.poll_events();
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(fake.sent(), vec!["報告一の文"]);
    assert_eq!(app.editor.text, "報告一の文報告二の文。報告三の文");
}

/// 後ろが確定済みのテンプレートだけでも、先頭を送り終えてから待っている転送を送る。
#[test]
fn a_waiting_transfer_waits_for_the_partial_send() {
    let (mut app, answer, fake) = wired_app("f-part-template");
    with_template(&mut app);
    say(&mut app, "報告一の文、電送。");
    say(&mut app, "胸部正常テンプレート");
    say(&mut app, "送信");
    assert!(app.transfer_waiting());
    answer.send(Ok("報告一の文、送信。".to_string())).unwrap();
    pump_until(&mut app, "両方送って消える", |app| {
        app.editor.is_empty() && !app.transfer_waiting()
    });
    assert_eq!(fake.sent(), vec!["報告一の文", "肺野に異常なし。\n胸水なし。"]);
}

/// 後ろが確定済みのテンプレートだけでも、部分送信の間の「転送」は残りの文のもの。
#[test]
fn a_transfer_word_during_a_partial_send_also_covers_a_template() {
    let (mut app, answer, fake) = wired_app("f-part-repeat-template");
    with_template(&mut app);
    let open = fake.close_gate();
    say(&mut app, "報告一の文、電送。");
    say(&mut app, "胸部正常テンプレート");
    answer.send(Ok("報告一の文、送信。".to_string())).unwrap();
    let sending = fake.clone();
    pump_until(&mut app, "先頭を送り始める", move |_| !sending.sent().is_empty());
    say(&mut app, "送信");
    assert_eq!(app.held_utterance_count(), 1);
    open.send(Ok(())).unwrap();
    let sent = fake.clone();
    pump_until(&mut app, "残りも送り始める", move |_| sent.sent().len() == 2);
    open.send(Ok(())).unwrap();
    pump_until(&mut app, "送って消える", |app| app.editor.is_empty());
}

/// 直前の文章の再転送が失敗しても、エディターの文のための待ちは取りやめない。
#[test]
fn a_failed_retransfer_does_not_cancel_a_waiting_transfer() {
    let (mut app, answer, fake) = wired_app("f-retransfer");
    say(&mut app, "一文目");
    answer.send(Ok("一文目".to_string())).unwrap();
    pump_until(&mut app, "確定", |app| !app.editor.awaiting_correction());
    app.transfer();
    pump_until(&mut app, "送って消える", |app| app.editor.is_empty());
    let open = fake.close_gate();
    say(&mut app, "二文目 送信");
    assert!(app.transfer_waiting());
    app.retransfer();
    open.send(Err(TransferError::NoTarget)).unwrap();
    pump_until(&mut app, "再転送の失敗", |app| notice_of(app).contains("転送先"));
    assert!(app.transfer_waiting());
    answer.send(Ok("二文目".to_string())).unwrap();
    let sent = fake.clone();
    pump_until(&mut app, "二文目を送り始める", move |_| sent.sent().len() == 3);
    open.send(Ok(())).unwrap();
    pump_until(&mut app, "送って消える", |app| app.editor.is_empty());
    assert_eq!(fake.sent(), vec!["一文目", "一文目", "二文目"]);
}

/// 「転送」を止めたら、後の発話の待っている転送が切れ目を無視して全部を送ることもない。
#[test]
fn a_refused_transfer_word_also_stops_the_waiting_one() {
    // 上限のあとに語が分かった。
    let (mut app, answer, fake) = wired_app_slow_on("f-refused-late", Some("電送"));
    say(&mut app, "報告一の文、電送。");
    past_the_limit(&mut app);
    say(&mut app, "報告二の文 送信");
    assert!(app.transfer_waiting());
    answer.send(Ok("報告一の文、送信。".to_string())).unwrap();
    pump_until(&mut app, "待ちが解ける", |app| !app.transfer_waiting());
    assert!(notice_of(&app).contains("手で送ってください"), "{}", notice_of(&app));
    settle_and_check_nothing_was_sent(&mut app, &fake, "報告一の文報告二の文");

    // 文の並びが話した順と違う。
    let (mut app, answer, fake) = wired_app_slow_on("f-refused-order", Some("電送"));
    say(&mut app, "報告一の文、電送。");
    app.editor.move_start();
    say(&mut app, "あとの文 送信");
    assert!(app.transfer_waiting());
    pump_until(&mut app, "後の文の戻り", |app| app.pending_corrections() == 1);
    answer.send(Ok("報告一の文、送信。".to_string())).unwrap();
    pump_until(&mut app, "待ちが解ける", |app| !app.transfer_waiting());
    assert!(notice_of(&app).contains("手で送ってください"), "{}", notice_of(&app));
    settle_and_check_nothing_was_sent(&mut app, &fake, "あとの文報告一の文");

    // Esc のあと。
    let (mut app, answer, fake) = wired_app_slow_on("f-refused-esc", Some("電送"));
    say(&mut app, "報告一の文、電送。");
    say(&mut app, "消す文。");
    pump_until(&mut app, "後の文の戻り", |app| app.pending_corrections() == 1);
    app.take_back();
    say(&mut app, "報告二の文 送信");
    assert!(app.transfer_waiting());
    pump_until(&mut app, "後の文の戻り", |app| app.pending_corrections() == 1);
    answer.send(Ok("報告一の文、送信。".to_string())).unwrap();
    pump_until(&mut app, "待ちが解ける", |app| !app.transfer_waiting());
    assert!(notice_of(&app).contains("働かせませんでした"), "{}", notice_of(&app));
    settle_and_check_nothing_was_sent(&mut app, &fake, "報告一の文報告二の文");
}

/// 止めた語の後ろに文が無ければ、待っている転送はその文だけを送るので任せる。誤った知らせも出さない。
#[test]
fn a_refused_word_leaves_a_waiting_transfer_of_the_same_text() {
    let (mut app, answer, fake) = wired_app_slow_on("f-refused-same", Some("電送"));
    say(&mut app, "報告一の文、電送。");
    say(&mut app, "消す文。");
    pump_until(&mut app, "後の文の戻り", |app| app.pending_corrections() == 1);
    app.take_back();
    say(&mut app, "送信");
    assert!(app.transfer_waiting());
    answer.send(Ok("報告一の文、送信。".to_string())).unwrap();
    pump_until(&mut app, "送って消える", |app| app.editor.is_empty());
    assert_eq!(fake.sent(), vec!["報告一の文"]);
    assert!(!notice_of(&app).contains("働かせませんでした"), "{}", notice_of(&app));
}

/// 待っている転送を止めたら、置いてあった発話の「転送」も働かせない。文は順に残る。
#[test]
fn a_refused_word_also_stops_transfer_words_held_behind_it() {
    let (mut app, answer, fake) = wired_app_slow_on("f-refused-held", Some("電送"));
    say(&mut app, "報告一の文、電送。");
    past_the_limit(&mut app);
    say(&mut app, "報告二の文 送信");
    say(&mut app, "報告三の文 送信");
    assert_eq!(app.held_utterance_count(), 1);
    answer.send(Ok("報告一の文、送信。".to_string())).unwrap();
    // 報告一の遅れた答えが、報告二の答えと同じ受け取りに入るのを待つ(報告二の答えは先に届いている)。
    // 別々に受け取ると、報告二の確定で先に転送が働く(処理役の起き方しだいで、この試験の形にならない)。
    // 答えがアプリに届いたことを試験から見る口は無いので、決まった時間だけ待つ(混んだ機械では揺れうる)。
    std::thread::sleep(Duration::from_millis(50));
    pump_until(&mut app, "待ちが解ける", |app| !app.transfer_waiting());
    pump_until(&mut app, "続きが入る", |app| app.held_utterance_count() == 0);
    settle_and_check_nothing_was_sent(&mut app, &fake, "報告一の文報告二の文報告三の文");
}

/// 語の答えが縮んでいて信用できないときも、後の発話の待っている転送で一緒に送らない。
#[test]
fn a_shrunk_transfer_word_also_stops_the_waiting_one() {
    let (mut app, answer, fake) = wired_app_slow_on("f-refused-shrunk", Some("電送"));
    say(&mut app, "所見なし所見なし、電送。");
    say(&mut app, "報告二の文 送信");
    assert!(app.transfer_waiting());
    pump_until(&mut app, "後の文の戻り", |app| app.pending_corrections() == 1);
    answer.send(Ok("所見なし、送信。".to_string())).unwrap();
    pump_until(&mut app, "待ちが解ける", |app| !app.transfer_waiting());
    assert!(notice_of(&app).contains("認識した文を残しました"), "{}", notice_of(&app));
    settle_and_check_nothing_was_sent(&mut app, &fake, "所見なし所見なし、電送。報告二の文");
}

fn settle_and_check_nothing_was_sent(app: &mut VoiceApp, fake: &FakeTransfer, text: &str) {
    pump_until(app, "確定", |app| !app.editor.awaiting_correction());
    for _ in 0..5 {
        app.poll_events();
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(fake.sent().is_empty(), "{:?}", fake.sent());
    assert_eq!(app.editor.text, text);
}

/// 手で触ったあとは、訂正のあとで「転送」と分かっても送らない。文は語を除いて残る。
#[test]
fn touching_the_text_stops_a_transfer_found_after_correction() {
    let touch: [fn(&mut VoiceApp); 2] = [
        |app| app.sync_editor_from_input("手で足した。所見なし、電送。消す文。", (6, 6)),
        |app| app.take_back(),
    ];
    for (index, action) in touch.into_iter().enumerate() {
        let (mut app, answer, fake) = wired_app_slow_on("f-transfer-touch", Some("電送"));
        say(&mut app, "所見なし、電送。");
        say(&mut app, "消す文。");
        pump_until(&mut app, "後の文の戻り", |app| app.pending_corrections() == 1);
        action(&mut app);
        answer.send(Ok("所見なし、送信。".to_string())).unwrap();
        pump_until(&mut app, "確定", |app| !app.editor.awaiting_correction());
        for _ in 0..5 {
            app.poll_events();
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(fake.sent().is_empty(), "{index}: {:?}", fake.sent());
        assert!(app.editor.text.contains("所見なし"), "{index}: {}", app.editor.text);
        assert!(!app.editor.text.contains("送信"), "{index}: {}", app.editor.text);
        assert!(notice_of(&app).contains("働かせませんでした"), "{index}: {}", notice_of(&app));
    }
}

/// 「転送」が 2 回分かっても、同じ文は 1 回だけ送る。
#[test]
fn a_transfer_found_after_correction_does_not_double_a_waiting_one() {
    let (mut app, answer, fake) = wired_app("f-transfer-twice");
    say(&mut app, "所見なし、電送。");
    say(&mut app, "送信");
    assert!(app.transfer_waiting());
    answer.send(Ok("所見なし、送信。".to_string())).unwrap();
    pump_until(&mut app, "送って消える", |app| app.editor.is_empty());
    for _ in 0..5 {
        app.poll_events();
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(fake.sent(), vec!["所見なし"]);
}

/// 訂正役が無ければ、学習した置き換えまでで確定する。判定 2 は無い。
#[test]
fn without_a_corrector_text_settles_at_once() {
    let mut app = app("f-no-ai");
    register(&mut app, CommandAction::NewLine, &["改行"]);
    assert!(app.data.memory.learn_pair("右配上", "右肺上", true));
    say(&mut app, "右配上葉。");
    say(&mut app, "開業。");
    assert_eq!(app.editor.text, "右肺上葉。開業。");
    assert_eq!(app.pending_corrections(), 0);
    assert_eq!(state_of(&app, 0), CorrectionState::Settled);
    assert!(app.editor.provisional_ranges().is_empty());
    app.poll_events();
    assert_eq!(app.editor.text, "右肺上葉。開業。");
}

/// 訂正役が失敗を返したら、その発話は認識文のまま確定し、後の発話を止めない。
#[test]
fn a_failed_correction_settles_at_once_and_releases_the_next_one() {
    let (mut app, answer, _fake) = wired_app_fixing("f-error", Some("遅い"), ("右配", "右肺"));
    say(&mut app, "遅い文。");
    say(&mut app, "右配上葉");
    pump_until(&mut app, "後の文の戻り", |app| app.pending_corrections() == 1);
    answer.send(Err("通信できません".to_string())).unwrap();
    pump_until(&mut app, "確定", |app| !app.editor.awaiting_correction());
    assert_eq!(app.editor.text, "遅い文。右肺上葉");
    assert_eq!(state_of(&app, 0), CorrectionState::Failed);
    assert!(notice_of(&app).contains("訂正できませんでした"));
}

/// 命令語の名前と読み・登録語・テンプレートのキーワードは、訂正役の指示文に「そのまま残す語」として入る。
#[test]
fn command_words_and_keywords_reach_the_instructions() {
    let mut app = app("f-protected");
    app.store.save_api_key("k").unwrap();
    app.global.corrector.enabled = true;
    app.global.corrector.backend = CorrectorBackend::ApiKey;
    app.rebuild_corrector();
    let instructions = |app: &VoiceApp| app.corrector.as_ref().unwrap().instructions().to_string();
    assert!(instructions(&app).contains("そのまま残す"));
    for name in ["改行", "かいぎょう", "転送", "てんそう"] {
        assert!(instructions(&app).contains(&format!("- {name}\n")), "{name}");
    }
    assert!(!instructions(&app).contains("- 次の行\n"));
    assert!(!instructions(&app).contains("- テンプレート\n"), "テンプレートが無ければ入れない");
    register(&mut app, CommandAction::NewLine, &["次の行"]);
    app.data.dictionary.save_template(None, "胸部正常", "本文").unwrap();
    app.refresh_engine_options();
    assert!(instructions(&app).contains("そのまま残す"));
    assert!(instructions(&app).contains("- 次の行\n"));
    assert!(instructions(&app).contains("- 胸部正常\n"));
    assert!(instructions(&app).contains("- テンプレート\n"), "呼ぶときの「テンプレート」も残す");
}

// ---- 「ヘイ、クロード」のテンプレート補完(単位 B) ----

/// 試験から答えを渡すまで返らない補完役。受けた依頼を覚える。
struct FakeAssistant {
    answers: Mutex<Receiver<Result<AssistantReply, String>>>,
    requests: Arc<Mutex<Vec<AssistantRequest>>>,
}

impl Assistant for FakeAssistant {
    fn name(&self) -> String {
        "偽の補完役".to_string()
    }

    fn complete(&self, request: &AssistantRequest) -> Result<AssistantReply, String> {
        self.requests.lock().unwrap().push(request.clone());
        self.answers.lock().unwrap().recv().unwrap()
    }
}

/// 試験の側の口。
struct AssistantLine {
    answer: Sender<Result<AssistantReply, String>>,
    requests: Arc<Mutex<Vec<AssistantRequest>>>,
}

impl AssistantLine {
    fn requests(&self) -> Vec<AssistantRequest> {
        self.requests.lock().unwrap().clone()
    }

    /// 答えを渡して、当たるまで回す。
    fn answer(&self, app: &mut VoiceApp, result: Result<AssistantReply, String>) {
        self.answer.send(result).unwrap();
        pump_until(app, "補完の答え", |app| !app.completing());
    }
}

fn completed(template: &str, text: &str) -> Result<AssistantReply, String> {
    Ok(AssistantReply {
        kind: ReplyKind::Template,
        template: Some(template.to_string()),
        text: Some(text.to_string()),
        reply: None,
        replace: false,
    })
}

/// 言い直しの答え(直前の補完を置き換える)。
fn revised(template: &str, text: &str) -> Result<AssistantReply, String> {
    Ok(AssistantReply {
        replace: true,
        ..completed(template, text).unwrap()
    })
}

fn enable_fake_assistant(app: &mut VoiceApp) -> AssistantLine {
    let (answer, answers) = mpsc::channel();
    let requests = Arc::new(Mutex::new(Vec::new()));
    app.enable_assistant(Arc::new(FakeAssistant {
        answers: Mutex::new(answers),
        requests: requests.clone(),
    }));
    AssistantLine { answer, requests }
}

fn add_template(app: &mut VoiceApp, keywords: &[&str], body: &str) {
    app.data.dictionary.templates.push(Template {
        keywords: keywords.iter().map(|k| k.to_string()).collect(),
        body: body.to_string(),
        ..Template::default()
    });
}

const PROSTATE: &str = "前立腺MRI\n前立腺のサイズは5.5×4.5×5.0cmと計測され、推定64mLで腫大しています。";
const CHEST: &str = "胸部単純CT\n肺に明らかな異常陰影を認めません。";

/// 「ヘイクロード、…」の依頼は本文に入らず Claude へ行き、補完した文がテンプレートとして 1 発話で入る。
/// AI 訂正には出さず、学習の対象にせず、Esc で消える。
#[test]
fn a_call_with_a_request_is_completed_from_a_template() {
    let mut app = app("call-request");
    app.data.settings.save_history = false;
    add_template(&mut app, &["前立腺MRI"], "前立腺MRI\n前立腺のサイズは〔　〕×〔　〕×〔　〕cm。");
    let line = enable_fake_assistant(&mut app);
    // 認識エンジンの出力そのまま(句読点なし、合成音 2026-11-07)。
    say(&mut app, "ヘイクロード前立腺 MRIのテンプレートサイズは5.5かける4.5かける5.0");
    assert!(app.editor.is_empty(), "依頼は本文に入れない");
    assert!(app.completing());
    pump_until(&mut app, "依頼が届く", |_| line.requests().len() == 1);
    let request = line.requests().remove(0);
    assert!(request.request.contains("前立腺"), "{}", request.request);
    assert!(request.request.contains("5.5"), "{}", request.request);
    assert!(!request.request.contains("ヘイ"), "{}", request.request);
    assert_eq!(request.previous, None);
    line.answer(&mut app, completed("前立腺MRI", PROSTATE));
    assert_eq!(app.editor.text, PROSTATE);
    assert!(
        notice_of(&app).starts_with("テンプレート「前立腺MRI」を補完しました"),
        "{}",
        notice_of(&app)
    );
    assert_eq!(app.pending_corrections(), 0, "AI 訂正には出さない");
    let id = app.editor.utterances.last().unwrap().id;
    assert!(app.editor.is_fixed(id), "学習の対象にしない");
    assert!(!app.completing());
    app.take_back();
    assert!(app.editor.is_empty(), "Esc で 1 発話として消える");
}

/// 呼びかけだけなら次の発話が依頼になる。言い直しは直前の補完を置き換え、手を入れたあとは足す。
#[test]
fn a_bare_call_takes_the_next_utterance_as_the_request() {
    let mut app = app("call-alone");
    add_template(&mut app, &["胸部CT"], CHEST);
    let line = enable_fake_assistant(&mut app);
    say(&mut app, "へいクロード。");
    assert!(app.call_waiting());
    assert_eq!(notice_of(&app), "Claude への依頼をどうぞ");
    assert!(app.editor.is_empty());
    say(&mut app, "胸部CTで異常なしの定型文");
    assert!(app.request_listening(), "話し始めてから聞き取りの時間が来るまで溜める");
    assert!(app.editor.is_empty(), "依頼は本文に入れない");
    app.watch_call(Instant::now() + Duration::from_secs(31));
    assert!(!app.call_waiting());
    pump_until(&mut app, "依頼が届く", |_| line.requests().len() == 1);
    assert!(line.requests()[0].request.contains("異常なしの定型文"), "{}", line.requests()[0].request);
    line.answer(&mut app, completed("胸部CT", CHEST));
    assert_eq!(app.editor.text, CHEST);

    // 言い直し: Claude が言い直しと判断し、直前の補完が手付かずなら置き換える。
    say(&mut app, "ヘイクロード、腫瘤なしも足して");
    pump_until(&mut app, "言い直しが届く", |_| line.requests().len() == 2);
    assert_eq!(line.requests()[1].previous.as_deref(), Some(CHEST));
    let again = "胸部単純CT\n肺に明らかな異常陰影や腫瘤を認めません。";
    line.answer(&mut app, revised("胸部CT", again));
    assert_eq!(app.editor.text, again);
    assert!(
        notice_of(&app).starts_with("テンプレート「胸部CT」を補完し直しました"),
        "{}",
        notice_of(&app)
    );

    // 新しい依頼(Claude が replace を付けない)は、直前の補完が残っていても足す。後ろに続けた文はそのまま。
    let edited = format!("{again}追記。");
    let end = edited.chars().count();
    app.sync_editor_from_input(&edited, (end, end));
    say(&mut app, "ヘイクロード、前立腺のテンプレート");
    pump_until(&mut app, "次の依頼が届く", |_| line.requests().len() == 3);
    assert_eq!(line.requests()[2].previous.as_deref(), Some(again), "補完した文は手付かず");
    line.answer(&mut app, completed("前立腺MRI", "前立腺MRI"));
    assert_eq!(app.editor.text, format!("{again}追記。前立腺MRI"));

    // 補完した文そのものに手を入れたあとは、言い直しでも置き換えず、足す。
    let edited = format!("{again}追記。前立腺 MRI");
    let end = edited.chars().count();
    app.sync_editor_from_input(&edited, (end, end));
    say(&mut app, "ヘイクロード、サイズを足して");
    pump_until(&mut app, "次の依頼が届く", |_| line.requests().len() == 4);
    assert_eq!(line.requests()[3].previous, None, "手を入れた文は添えない");
    line.answer(&mut app, revised("前立腺MRI", "前立腺MRI 5cm"));
    assert_eq!(app.editor.text, format!("{again}追記。前立腺 MRI前立腺MRI 5cm"));
}

/// 呼びかけのあと 15 秒たてば口述に戻る。合うテンプレートが無ければ知らせだけで、失敗も知らせだけ。
#[test]
fn the_call_lapses_and_a_missing_template_only_notifies() {
    let mut app = app("call-lapse");
    add_template(&mut app, &["胸部CT"], CHEST);
    let line = enable_fake_assistant(&mut app);
    say(&mut app, "ヘイ、クロード");
    app.watch_call(Instant::now() + Duration::from_secs(14));
    assert!(app.call_waiting());
    app.watch_call(Instant::now() + Duration::from_secs(16));
    assert!(!app.call_waiting());
    assert_eq!(notice_of(&app), "依頼がなかったので、口述に戻りました");
    say(&mut app, "右肺上葉に結節を認めます");
    assert_eq!(app.editor.text, "右肺上葉に結節を認めます");
    assert!(line.requests().is_empty());

    say(&mut app, "ヘイクロード、頭部MRIの定型文");
    pump_until(&mut app, "依頼が届く", |_| line.requests().len() == 1);
    line.answer(
        &mut app,
        Ok(AssistantReply {
            kind: ReplyKind::None,
            template: None,
            text: None,
            reply: Some("頭部MRIのテンプレートがありません".to_string()),
            replace: false,
        }),
    );
    assert_eq!(app.editor.text, "右肺上葉に結節を認めます");
    assert_eq!(notice_of(&app), "頭部MRIのテンプレートがありません");

    say(&mut app, "ヘイクロード、胸部CTの定型文");
    pump_until(&mut app, "依頼が届く", |_| line.requests().len() == 2);
    line.answer(&mut app, Err("Claude Code の応答が遅いので待つのをやめました".to_string()));
    assert_eq!(app.editor.text, "右肺上葉に結節を認めます");
    assert_eq!(notice_of(&app), "Claude Code の応答が遅いので待つのをやめました");
    assert!(app.notice.as_ref().unwrap().is_error);
}

/// 呼びかけだけのあと、話し始めてから設定の秒数(既定 30)の間に届いた発話は、途切れてもまとめて
/// 1 つの依頼になり、時間が来たら送る。送るまでは本文に何も入らない。
#[test]
fn utterances_heard_after_a_bare_call_become_one_request_when_the_time_is_up() {
    let mut app = app("call-listen");
    add_template(&mut app, &["前立腺MRI"], PROSTATE);
    let line = enable_fake_assistant(&mut app);
    say(&mut app, "ヘイクロード");
    let called = Instant::now();
    assert_eq!(app.call_wait_seconds_left(called), Some(15));
    assert!(!app.request_listening());
    say(&mut app, "前立腺MRIのテンプレートで");
    let first = Instant::now();
    assert!(app.request_listening());
    assert_eq!(app.call_wait_seconds_left(first), None);
    assert_eq!(app.listen_seconds_left(first), Some(30));
    assert_eq!(app.live_caption, "依頼: 前立腺MRIのテンプレートで");
    // 間を置いて(待ちの 15 秒を過ぎて)続きを言っても、同じ依頼に溜める。
    app.watch_call(first + Duration::from_secs(20));
    assert!(app.request_listening());
    say(&mut app, "サイズは5.5かける4.5かける5.0");
    assert_eq!(app.live_caption, "依頼: 前立腺MRIのテンプレートで サイズは5.5かける4.5かける5.0");
    app.watch_call(first + Duration::from_secs(29));
    assert!(app.request_listening());
    assert!(app.editor.is_empty(), "依頼は本文に入れない");
    assert!(line.requests().is_empty(), "時間が来るまで送らない");

    app.watch_call(Instant::now() + Duration::from_secs(30));
    assert!(!app.call_waiting());
    pump_until(&mut app, "依頼が届く", |_| line.requests().len() == 1);
    let request = &line.requests()[0].request;
    assert!(request.contains("テンプレートで") && request.contains("5.5"), "{request}");
    line.answer(&mut app, completed("前立腺MRI", PROSTATE));
    assert_eq!(app.editor.text, PROSTATE);
}

/// 発話全体が「以上」「以上です」なら、溜めた文をすぐ送る。文に続けて言った「以上です」は終える語ではなく、
/// 依頼の一部として溜め、時間が来てから送る。
#[test]
fn saying_ijou_sends_the_request_at_once() {
    let mut app = app("call-ijou");
    add_template(&mut app, &["前立腺MRI"], PROSTATE);
    let line = enable_fake_assistant(&mut app);
    say(&mut app, "ヘイクロード");
    say(&mut app, "前立腺MRIのテンプレートでサイズは5.5かける4.5、以上です");
    assert!(app.request_listening(), "文に続けた「以上です」ではすぐ送らない");
    assert!(line.requests().is_empty());
    app.watch_call(Instant::now() + Duration::from_secs(31));
    pump_until(&mut app, "依頼が届く", |_| line.requests().len() == 1);
    let request = line.requests()[0].request.clone();
    assert!(request.contains("4.5") && request.contains("以上です"), "{request}");
    line.answer(&mut app, completed("前立腺MRI", PROSTATE));

    say(&mut app, "ヘイクロード");
    say(&mut app, "前立腺MRIのテンプレートで");
    say(&mut app, "以上");
    assert!(!app.call_waiting());
    pump_until(&mut app, "次の依頼が届く", |_| line.requests().len() == 2);
    assert!(line.requests()[1].request.contains("テンプレートで"), "{}", line.requests()[1].request);
    line.answer(&mut app, completed("前立腺MRI", PROSTATE));

    // 何も溜まらないまま「以上」なら送らず、口述に戻る。
    say(&mut app, "ヘイクロード");
    say(&mut app, "以上です");
    assert!(!app.call_waiting());
    assert_eq!(notice_of(&app), "依頼がなかったので、口述に戻りました");
    assert_eq!(line.requests().len(), 2);
}

/// 話し始めた時点(見張りが声を見た時点)から聞き取る。待ちの 15 秒は話し始めに掛ける。
/// 時間が来たとき声が続いていれば送らず、止んで認識文が届いたらそれも含めて送る(止んでから 1 秒待つ)。
#[test]
fn listening_starts_with_the_voice_and_waits_for_the_last_utterance() {
    let mut app = app("call-listen-voice");
    add_template(&mut app, &["胸部CT"], CHEST);
    let line = enable_fake_assistant(&mut app);
    say(&mut app, "ヘイクロード");
    let spoke = Instant::now() + Duration::from_secs(14);
    set_speaking(&app, true);
    app.watch_call(spoke);
    assert!(app.request_listening(), "声を見たら聞き取りを始める");
    assert_eq!(app.listen_seconds_left(spoke), Some(30));
    set_speaking(&app, false);
    app.watch_call(spoke + Duration::from_secs(5));
    assert!(app.call_waiting(), "話し始めていれば待ちの 15 秒では切れない");
    say(&mut app, "胸部CTで");

    let end = spoke + Duration::from_secs(30);
    set_speaking(&app, true);
    app.watch_call(end);
    assert!(app.request_listening(), "時間が来ても声が続いていれば送らない");
    assert_eq!(app.listen_seconds_left(end), None, "時間が来たら秒数は出さない");
    set_speaking(&app, false);
    say(&mut app, "異常なしの定型文");
    app.watch_call(end + Duration::from_millis(900));
    assert!(app.request_listening(), "声が止んで 1 秒たつまでは送らない");
    assert!(line.requests().is_empty());
    app.watch_call(end + Duration::from_millis(1100));
    assert!(!app.call_waiting());
    pump_until(&mut app, "依頼が届く", |_| line.requests().len() == 1);
    let request = &line.requests()[0].request;
    assert!(request.contains("胸部") && request.contains("異常なしの定型文"), "{request}");
    line.answer(&mut app, completed("胸部CT", CHEST));
    assert_eq!(app.editor.text, CHEST);

    // 声を見て聞き取りを始めても(短すぎて捨てた音など)、何も届かないまま時間が来れば口述に戻る。
    say(&mut app, "ヘイクロード");
    let spoke = Instant::now();
    set_speaking(&app, true);
    app.watch_call(spoke);
    set_speaking(&app, false);
    app.watch_call(spoke + Duration::from_secs(31));
    assert!(!app.call_waiting());
    assert_eq!(notice_of(&app), "依頼がなかったので、口述に戻りました");
    assert_eq!(line.requests().len(), 1);
    assert_eq!(app.editor.text, CHEST);
}

/// 聞き取りの間の命令語(「転送」)・記号の読み・「〜テンプレート」は依頼の一部として溜め、働かせない。
#[test]
fn commands_said_while_listening_are_part_of_the_request() {
    let mut app = app("call-listen-command");
    app.data.settings.save_history = false;
    add_template(&mut app, &["胸部CT"], CHEST);
    let line = enable_fake_assistant(&mut app);
    let fake = FakeTransfer::default();
    let sender = fake.clone();
    app.enable_transfer(Arc::new(move |request| {
        sender.sent.lock().unwrap().push(request.text.to_string());
        Ok(Delivery::Inserted)
    }));
    say(&mut app, "右肺上葉に結節を認めます");
    say(&mut app, "ヘイクロード");
    say(&mut app, "胸部CTで異常なし");
    say(&mut app, "転送");
    say(&mut app, "矢印");
    say(&mut app, "胸部CTテンプレート");
    for _ in 0..3 {
        app.poll_events();
    }
    assert!(app.request_listening());
    assert!(fake.sent().is_empty(), "転送しない");
    assert!(!app.transfer_waiting());
    assert_eq!(app.editor.text, "右肺上葉に結節を認めます", "記号もテンプレートも入れない");
    app.watch_call(Instant::now() + Duration::from_secs(31));
    pump_until(&mut app, "依頼が届く", |_| line.requests().len() == 1);
    let request = &line.requests()[0].request;
    assert!(request.contains("転送") && request.contains("テンプレート"), "{request}");
    assert!(fake.sent().is_empty());
}

/// Esc は聞き取りを取りやめ、溜めた文を捨てる。本文は変わらない。
#[test]
fn esc_discards_what_was_heard() {
    let mut app = app("call-listen-esc");
    add_template(&mut app, &["胸部CT"], CHEST);
    let line = enable_fake_assistant(&mut app);
    say(&mut app, "右肺上葉に結節を認めます");
    say(&mut app, "ヘイクロード");
    say(&mut app, "胸部CTで異常なし");
    assert!(app.request_listening());
    app.take_back();
    assert!(!app.call_waiting());
    assert_eq!(notice_of(&app), "Claude への依頼を取りやめました");
    assert!(app.live_caption.is_empty());
    assert_eq!(app.editor.text, "右肺上葉に結節を認めます");
    app.watch_call(Instant::now() + Duration::from_secs(60));
    for _ in 0..3 {
        app.poll_events();
    }
    assert!(line.requests().is_empty(), "溜めた文は送らない");
}

/// 残りの秒数は切り上げで、0 を返さない。依頼を送ったあとは送ってからの経過を数える。
#[test]
fn the_seconds_left_count_down_without_reaching_zero() {
    let mut app = app("call-count");
    add_template(&mut app, &["胸部CT"], CHEST);
    let line = enable_fake_assistant(&mut app);
    assert_eq!(app.call_wait_seconds_left(Instant::now()), None);
    say(&mut app, "ヘイクロード");
    let now = Instant::now();
    assert_eq!(app.call_wait_seconds_left(now), Some(15));
    assert_eq!(app.call_wait_seconds_left(now + Duration::from_millis(14_500)), Some(1));
    assert_eq!(app.call_wait_seconds_left(now + Duration::from_secs(16)), Some(1));
    say(&mut app, "胸部CTで");
    let now = Instant::now();
    assert_eq!(app.listen_seconds_left(now), Some(30));
    assert_eq!(app.listen_seconds_left(now + Duration::from_secs(10)), Some(20));
    assert_eq!(app.listen_seconds_left(now + Duration::from_millis(29_500)), Some(1));
    assert_eq!(app.listen_seconds_left(now + Duration::from_secs(31)), None);
    assert_eq!(app.completing_for(now), None);

    say(&mut app, "以上");
    assert_eq!(app.listen_seconds_left(Instant::now()), None);
    let sent = Instant::now();
    let elapsed = app.completing_for(sent + Duration::from_secs(3)).unwrap();
    assert!(elapsed >= Duration::from_secs(3) && elapsed < Duration::from_secs(4), "{elapsed:?}");
    pump_until(&mut app, "依頼が届く", |_| line.requests().len() == 1);
    line.answer(&mut app, completed("胸部CT", CHEST));
    assert_eq!(app.completing_for(Instant::now()), None);
}

/// user.json の聞き取りの秒数は、読むときに 15〜60 秒の 5 秒刻みに丸める。書いていない古いファイルは 30 秒。
#[test]
fn the_listening_time_in_the_user_file_is_kept_in_range() {
    let cases = [
        (r#"{"settings":{"request_listen_secs":5}}"#, 15),
        (r#"{"settings":{"request_listen_secs":999}}"#, 60),
        (r#"{"settings":{"request_listen_secs":40}}"#, 40),
        (r#"{"settings":{"request_listen_secs":17}}"#, 15),
        (r#"{"settings":{"request_listen_secs":18}}"#, 20),
        (r#"{"settings":{"history_count":50,"call_wait_secs":40}}"#, 30),
    ];
    for (i, (json, seconds)) in cases.iter().enumerate() {
        let home = std::env::temp_dir().join(format!("openspeech-it-listen-{i}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(home.join("user.json"), json).unwrap();
        let app = VoiceApp::with_store(egui::Context::default(), Store::at(&home), None);
        assert!(app.windows.startup_warning.is_none(), "{json}");
        assert_eq!(app.data.settings.request_listen(), Duration::from_secs(*seconds), "{json}");
        if json.contains("history_count") {
            assert_eq!(app.data.settings.request_listen_secs, 30);
            assert_eq!(app.data.settings.history_count, 50, "ほかの設定は残り、以前の待ちの設定は読み飛ばす");
        }
    }
}

/// 文末の「以上」("or more")は依頼から削らない(「短径1センチ以上」)。
#[test]
fn ijou_at_the_end_of_a_finding_stays_in_the_request() {
    let mut app = app("call-ijou-more");
    add_template(&mut app, &["胸部CT"], CHEST);
    let line = enable_fake_assistant(&mut app);
    say(&mut app, "ヘイクロード");
    say(&mut app, "結節は短径1センチ以上");
    assert!(app.request_listening());
    assert_eq!(app.live_caption, "依頼: 結節は短径1センチ以上");
    say(&mut app, "以上です");
    assert!(!app.call_waiting(), "「以上です」だけの発話で送る");
    pump_until(&mut app, "依頼が届く", |_| line.requests().len() == 1);
    let request = &line.requests()[0].request;
    assert!(request.contains("短径") && request.ends_with("以上"), "{request}");
}

fn set_speaking(app: &VoiceApp, on: bool) {
    app.session.live.speaking.store(on, Ordering::Relaxed);
}

/// Claude に接続していない、テンプレートが無いときは知らせだけで、本文は変えない。
#[test]
fn a_call_without_a_connection_or_templates_only_notifies() {
    let mut app = app("call-none");
    say(&mut app, "ヘイクロード、前立腺MRIのテンプレートでサイズは5かける4かける3");
    assert!(app.editor.is_empty());
    assert!(notice_of(&app).contains("Claude に接続"), "{}", notice_of(&app));
    assert!(!app.completing());
    // 呼びかけだけでも、受けられない理由はその場で知らせ、次の発話を食べない。
    say(&mut app, "ヘイクロード");
    assert!(!app.call_waiting());
    assert!(notice_of(&app).contains("Claude に接続"), "{}", notice_of(&app));

    let line = enable_fake_assistant(&mut app);
    say(&mut app, "ヘイクロード");
    assert!(!app.call_waiting());
    assert!(notice_of(&app).contains("テンプレートが登録されていません"), "{}", notice_of(&app));
    say(&mut app, "前立腺MRIのテンプレート");
    assert_eq!(app.editor.text, "前立腺 MRI のテンプレート");
    assert!(line.requests().is_empty());
    assert!(!app.completing());
}

/// 呼びかけの待ちはマイクを止めても残る(押している間だけ録る設定で、押し直して依頼を言える)。
/// 待っている間の命令語は依頼ではなく、待ちをやめて命令として働く。
#[test]
fn the_wait_survives_a_mic_stop_and_yields_to_a_command() {
    let mut app = app("call-wait");
    add_template(&mut app, &["胸部CT"], CHEST);
    let line = enable_fake_assistant(&mut app);
    say(&mut app, "右肺上葉に結節を認めます");
    say(&mut app, "ヘイクロード");
    app.stop_mic();
    assert!(app.call_waiting());
    say(&mut app, "胸部CTのテンプレートで異常なし");
    app.watch_call(Instant::now() + Duration::from_secs(31));
    pump_until(&mut app, "依頼が届く", |_| line.requests().len() == 1);
    assert_eq!(line.requests()[0].request, "胸部 CT のテンプレートで異常なし");
    line.answer(&mut app, completed("胸部CT", CHEST));
    assert_eq!(app.editor.text, format!("右肺上葉に結節を認めます{CHEST}"));

    say(&mut app, "ヘイクロード");
    assert!(app.call_waiting());
    say(&mut app, "改行");
    assert!(!app.call_waiting());
    assert_eq!(line.requests().len(), 1, "命令語は依頼にしない");
    assert!(app.editor.text.ends_with('\n'), "{:?}", app.editor.text);
}

/// 「〜テンプレート」だけの依頼は Claude に頼むまでもない。呼びかけに続けても、待っている間でも、
/// 接続が無くても、キーワードの呼び出しと同じにその本文を入れる。
#[test]
fn a_template_call_in_a_request_is_inserted_without_claude() {
    let mut app = app("call-template");
    add_template(&mut app, &["胸部CT"], CHEST);
    let line = enable_fake_assistant(&mut app);
    say(&mut app, "ヘイクロード、胸部CTのテンプレート");
    assert_eq!(app.editor.text, CHEST);
    assert!(!app.completing());
    say(&mut app, "ヘイクロード");
    assert!(app.call_waiting());
    say(&mut app, "胸部CTテンプレート");
    assert!(!app.call_waiting());
    assert_eq!(app.editor.text, format!("{CHEST}{CHEST}"));
    for _ in 0..3 {
        app.poll_events();
    }
    assert!(line.requests().is_empty(), "Claude には送らない");
    assert!(app.editor.delete_last_utterance(), "Esc で 1 発話として消える");
    assert_eq!(app.editor.text, CHEST);
}

/// 待っている間の記号の読みと、文末に命令語が続く「〜テンプレート」も依頼ではない。待ちをやめて普通に働く。
#[test]
fn symbols_and_template_calls_with_a_command_end_the_wait() {
    let mut app = app("call-wait-symbol");
    add_template(&mut app, &["胸部CT"], CHEST);
    register(&mut app, CommandAction::NewLine, &["改行"]);
    let line = enable_fake_assistant(&mut app);
    say(&mut app, "ヘイクロード");
    assert!(app.call_waiting());
    say(&mut app, "矢印");
    assert!(!app.call_waiting());
    assert_eq!(app.editor.text, "→");
    say(&mut app, "ヘイクロード");
    say(&mut app, "胸部CTテンプレート 改行");
    assert!(!app.call_waiting());
    assert_eq!(app.editor.text, format!("→{CHEST}\n"));
    say(&mut app, "ヘイクロード");
    say(&mut app, "胸部CTのテンプレートで異常なし");
    app.watch_call(Instant::now() + Duration::from_secs(31));
    pump_until(&mut app, "依頼が届く", |_| line.requests().len() == 1);
    assert_eq!(line.requests()[0].request, "胸部 CT のテンプレートで異常なし", "文のある依頼は Claude へ");
}

/// 接続が無くても「ヘイクロード、〜テンプレート」は本文を入れる(Claude を呼ばないので断らない)。
#[test]
fn a_template_call_in_a_request_works_without_a_connection() {
    let mut app = app("call-template-offline");
    add_template(&mut app, &["胸部CT"], CHEST);
    say(&mut app, "ヘイクロード胸部CTテンプレート");
    assert_eq!(app.editor.text, CHEST);
    assert!(notice_of(&app).is_empty(), "{}", notice_of(&app));
}

/// 頭のフィラーがあっても呼びかけになる(整形後の文で見る)。
#[test]
fn a_filler_before_the_call_is_ignored() {
    let mut app = app("call-filler");
    add_template(&mut app, &["胸部CT"], CHEST);
    let line = enable_fake_assistant(&mut app);
    say(&mut app, "えー、ヘイクロード、胸部CTのテンプレートで異常なし");
    pump_until(&mut app, "依頼が届く", |_| line.requests().len() == 1);
    assert_eq!(line.requests()[0].request, "胸部 CT のテンプレートで異常なし");
    assert!(app.editor.is_empty());
    line.answer(&mut app, completed("胸部CT", CHEST));
    assert_eq!(app.editor.text, CHEST);
}

/// Esc は呼びかけの待ちと、送った依頼を取りやめる(直前の発話は消さない)。取りやめた依頼の答えは捨てる。
#[test]
fn esc_cancels_the_call_and_the_request() {
    let mut app = app("call-esc");
    add_template(&mut app, &["胸部CT"], CHEST);
    add_template(&mut app, &["前立腺MRI"], PROSTATE);
    let line = enable_fake_assistant(&mut app);
    say(&mut app, "右肺上葉に結節を認めます");
    say(&mut app, "ヘイクロード");
    assert!(app.call_waiting());
    app.take_back();
    assert!(!app.call_waiting());
    assert_eq!(notice_of(&app), "Claude への依頼を取りやめました");
    assert_eq!(app.editor.text, "右肺上葉に結節を認めます");

    say(&mut app, "ヘイクロード、胸部CTのテンプレートで異常なし");
    pump_until(&mut app, "依頼が届く", |_| line.requests().len() == 1);
    app.take_back();
    assert!(!app.completing());
    assert_eq!(notice_of(&app), "Claude への依頼を取りやめました");
    assert_eq!(app.editor.text, "右肺上葉に結節を認めます");
    // 取りやめた答えは捨て、次の依頼の答えだけ当たる。
    say(&mut app, "ヘイクロード、前立腺MRIのテンプレートでサイズは5かける4かける3");
    line.answer.send(completed("胸部CT", CHEST)).unwrap();
    pump_until(&mut app, "次の依頼が届く", |_| line.requests().len() == 2);
    line.answer(&mut app, completed("前立腺MRI", PROSTATE));
    assert_eq!(app.editor.text, format!("右肺上葉に結節を認めます{PROSTATE}"));

    app.take_back();
    assert_eq!(app.editor.text, "右肺上葉に結節を認めます", "取りやめるものが無ければ直前の発話を消す");
}

/// 補完を頼んでから言った「転送」は答えを待って一緒に送る。送っている途中に戻った答えは、
/// 送り終えてから、その間に置いた発話より前に入る。
#[test]
fn a_completion_waits_for_a_transfer_in_progress() {
    let mut app = app("call-transfer");
    app.data.settings.save_history = false;
    add_template(&mut app, &["胸部CT"], CHEST);
    let line = enable_fake_assistant(&mut app);
    let fake = FakeTransfer::default();
    let sender = fake.clone();
    app.enable_transfer(Arc::new(move |request| {
        sender.sent.lock().unwrap().push(request.text.to_string());
        match sender.gate.lock().unwrap().as_ref() {
            Some(gate) => gate.recv().unwrap().map(|()| Delivery::Inserted),
            None => Ok(Delivery::Inserted),
        }
    }));

    say(&mut app, "右肺上葉に結節を認めます");
    say(&mut app, "ヘイクロード、胸部CTのテンプレートで異常なし");
    pump_until(&mut app, "依頼が届く", |_| line.requests().len() == 1);
    say(&mut app, "転送");
    app.poll_events();
    assert!(app.transfer_waiting());
    assert!(fake.sent().is_empty(), "答えが戻るまで送らない");
    line.answer(&mut app, completed("胸部CT", CHEST));
    pump_until(&mut app, "転送が済む", |_| !fake.sent().is_empty());
    assert_eq!(fake.sent(), vec![format!("右肺上葉に結節を認めます{CHEST}")]);
    pump_until(&mut app, "送った文が消える", |app| app.editor.is_empty());

    say(&mut app, "肝臓に異常なし");
    say(&mut app, "ヘイクロード、胸部CTのテンプレートで異常なし");
    pump_until(&mut app, "依頼が届く", |_| line.requests().len() == 2);
    let open = fake.close_gate();
    app.transfer();
    pump_until(&mut app, "送り始める", |_| fake.sent().len() == 2);
    say(&mut app, "脾臓に異常なし");
    line.answer.send(completed("胸部CT", CHEST)).unwrap();
    for _ in 0..5 {
        app.poll_events();
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(app.completing(), "送り終えるまで答えを当てない");
    assert_eq!(app.editor.text, "肝臓に異常なし");
    open.send(Ok(())).unwrap();
    pump_until(&mut app, "送り終える", |app| !app.completing());
    assert_eq!(app.editor.text, format!("{CHEST}脾臓に異常なし"));
}

// ---- 外部の音声認識 API(Haas のあとの段) ----

/// 偽の外部 API。答えの口から返す。呼ばれた回数と受けた用語ヒントを覚える。
struct FakeRemote {
    answers: Mutex<Receiver<Result<String, RemoteError>>>,
    calls: Arc<AtomicUsize>,
    hints: Arc<Mutex<Vec<Hints>>>,
}

impl RemoteProvider for FakeRemote {
    fn label(&self) -> String {
        "偽の外部 API".to_string()
    }

    fn transcribe(&self, wav: &[u8], hints: &Hints, _timeout: Duration) -> Result<String, RemoteError> {
        assert!(wav.starts_with(b"RIFF"), "WAV で送る");
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.hints.lock().unwrap().push(hints.clone());
        let answers = self.answers.lock().unwrap();
        answers
            .recv()
            .unwrap_or_else(|_| Err(RemoteError::Temporary("試験の終わり".to_string())))
    }
}

/// 受けた文を覚えて、そのまま返す訂正役。
struct EchoCorrector {
    seen: Arc<Mutex<Vec<String>>>,
}

impl Corrector for EchoCorrector {
    fn name(&self) -> String {
        "写す訂正役".to_string()
    }

    fn correct(&self, request: &CorrectionRequest) -> Result<String, String> {
        self.seen.lock().unwrap().push(request.text.clone());
        Ok(request.text.clone())
    }
}

struct RemoteRig {
    app: VoiceApp,
    answer: Sender<Result<String, RemoteError>>,
    calls: Arc<AtomicUsize>,
    hints: Arc<Mutex<Vec<Hints>>>,
    seen: Arc<Mutex<Vec<String>>>,
}

/// 偽の外部 API と(`with_corrector` なら)写す訂正役をつないだアプリ。
fn remote_app(tag: &str, with_corrector: bool) -> RemoteRig {
    let mut app = app(tag);
    app.data.settings.save_history = false;
    app.global.recognizer = RecognizerKind::Remote;
    let (answer, answers) = mpsc::channel();
    let calls = Arc::new(AtomicUsize::new(0));
    let hints = Arc::new(Mutex::new(Vec::new()));
    app.remote = Some(Arc::new(FakeRemote {
        answers: Mutex::new(answers),
        calls: calls.clone(),
        hints: hints.clone(),
    }));
    let seen = Arc::new(Mutex::new(Vec::new()));
    app.corrector = with_corrector.then(|| {
        Arc::new(EchoCorrector { seen: seen.clone() }) as Arc<dyn Corrector>
    });
    app.enable_corrections();
    RemoteRig {
        app,
        answer,
        calls,
        hints,
        seen,
    }
}

/// 音声つきの発話(外部 API に送る音がある)。
fn speak_aloud(app: &mut VoiceApp, text: &str) {
    let recognition = Recognition {
        text: text.to_string(),
        unsure: Vec::new(),
    };
    app.handle_utterance(recognition, Some(Arc::new(vec![0.1; 1600])));
}

fn settle(app: &mut VoiceApp) {
    pump_until(app, "確定", |app| app.pending_corrections() == 0 && !app.editor.awaiting_correction());
}

/// Haas の文はすぐ出て、API の文が届いたら差し替わり、訂正役には API の文が渡る。
#[test]
fn the_api_text_replaces_the_haas_text_and_goes_to_the_corrector() {
    let mut rig = remote_app("remote-replace", true);
    speak_aloud(&mut rig.app, "右配上葉に結節");
    assert_eq!(rig.app.editor.text, "右配上葉に結節", "Haas の文をすぐ出す");
    rig.answer.send(Ok("右肺上葉に結節。".to_string())).unwrap();
    settle(&mut rig.app);
    assert_eq!(rig.app.editor.text, "右肺上葉に結節。");
    assert_eq!(*rig.seen.lock().unwrap(), ["右肺上葉に結節。"]);
    assert_eq!(rig.calls.load(Ordering::SeqCst), 1);
}

/// API が失敗したら、Haas の文で訂正に進み、知らせる。
#[test]
fn a_failed_api_call_goes_on_with_the_haas_text() {
    let mut rig = remote_app("remote-failed", true);
    speak_aloud(&mut rig.app, "右配上葉に結節");
    rig.answer
        .send(Err(RemoteError::Temporary("時間内に答えが返りませんでした".to_string())))
        .unwrap();
    settle(&mut rig.app);
    assert_eq!(rig.app.editor.text, "右配上葉に結節");
    assert_eq!(*rig.seen.lock().unwrap(), ["右配上葉に結節"]);
    assert!(notice_of(&rig.app).contains("Haas の文で進めました"), "{}", notice_of(&rig.app));
}

/// 声でない短い発話は API に送らない(訂正には Haas の文で進む)。
#[test]
fn short_noises_are_not_sent_to_the_api() {
    let mut rig = remote_app("remote-gate", true);
    speak_aloud(&mut rig.app, "うん。");
    settle(&mut rig.app);
    assert_eq!(rig.calls.load(Ordering::SeqCst), 0);
    assert_eq!(*rig.seen.lock().unwrap(), ["うん。"]);
}

/// Haas の文に比べて長すぎる API の文は使わない(言っていない文を返したときの守り)。
#[test]
fn a_far_longer_api_answer_is_not_used() {
    let mut rig = remote_app("remote-long", true);
    speak_aloud(&mut rig.app, "結節あり");
    let invented = format!("結節あり。{}", "胸水貯留なし。".repeat(5));
    rig.answer.send(Ok(invented)).unwrap();
    settle(&mut rig.app);
    assert_eq!(rig.app.editor.text, "結節あり");
    assert_eq!(*rig.seen.lock().unwrap(), ["結節あり"]);
}

/// Haas の文で働いた命令語は、API の文の文末にあっても二度働かせない。
#[test]
fn a_command_heard_by_haas_does_not_run_again_from_the_api_text() {
    let mut rig = remote_app("remote-command", true);
    register(&mut rig.app, CommandAction::NewLine, &["改行"]);
    speak_aloud(&mut rig.app, "結節あり 改行");
    assert_eq!(rig.app.editor.text, "結節あり\n");
    rig.answer.send(Ok("結節あり。改行".to_string())).unwrap();
    settle(&mut rig.app);
    assert_eq!(rig.app.editor.text, "結節あり。\n");
    assert_eq!(*rig.seen.lock().unwrap(), ["結節あり。"]);
}

/// 訂正役がいなければ、API の文で確定する。
#[test]
fn without_a_corrector_the_api_text_settles() {
    let mut rig = remote_app("remote-alone", false);
    speak_aloud(&mut rig.app, "右配上葉に結節");
    rig.answer.send(Ok("右肺上葉に結節。".to_string())).unwrap();
    settle(&mut rig.app);
    assert_eq!(rig.app.editor.text, "右肺上葉に結節。");
    assert!(rig.seen.lock().unwrap().is_empty());
}

/// API が上限までに答えなければ Haas の文で訂正に進み、あとから届いた API の文は使わない。
#[test]
fn an_api_answer_after_the_limit_is_ignored() {
    let mut rig = remote_app("remote-late", true);
    speak_aloud(&mut rig.app, "右配上葉に結節");
    rig.app.watch_corrections(Instant::now() + Duration::from_secs(120));
    let seen = rig.seen.clone();
    pump_until(&mut rig.app, "Haas の文で訂正へ", move |_| !seen.lock().unwrap().is_empty());
    assert_eq!(*rig.seen.lock().unwrap(), ["右配上葉に結節"]);
    rig.answer.send(Ok("右肺上葉に結節。".to_string())).unwrap();
    settle(&mut rig.app);
    assert_eq!(rig.app.editor.text, "右配上葉に結節");
    assert_eq!(rig.seen.lock().unwrap().len(), 1);
}

/// Haas が命令語を働かせた発話で、API の文の文末が命令語と読めなければ Haas の文で進む
/// (「開業」のように書いた API の文を使うと、文に残るか訂正のあと二度働く)。
#[test]
fn a_garbled_command_in_the_api_text_keeps_the_haas_text() {
    let mut rig = remote_app("remote-garbled", true);
    register(&mut rig.app, CommandAction::NewLine, &["改行"]);
    speak_aloud(&mut rig.app, "結節あり 改行");
    rig.answer.send(Ok("結節あり。開業".to_string())).unwrap();
    settle(&mut rig.app);
    assert_eq!(rig.app.editor.text, "結節あり\n");
    assert_eq!(*rig.seen.lock().unwrap(), ["結節あり"]);
}

/// 手直しで覚えた置き換えは、API の文にも当たる。
#[test]
fn learned_pairs_apply_to_the_api_text_too() {
    let mut rig = remote_app("remote-learned", false);
    rig.app.data.settings.learning.enabled = true;
    rig.app.data.settings.learning.auto_apply = true;
    rig.app.data.settings.learning.min_count = 1;
    rig.app.data.memory.learn_pair("右配上", "右肺上", true);
    speak_aloud(&mut rig.app, "右配上葉に結節");
    rig.answer.send(Ok("右配上葉に結節あり。".to_string())).unwrap();
    settle(&mut rig.app);
    assert_eq!(rig.app.editor.text, "右肺上葉に結節あり。");
}

/// API の答えの改行は入れない。
#[test]
fn newlines_in_the_api_text_are_dropped() {
    let mut rig = remote_app("remote-newline", false);
    speak_aloud(&mut rig.app, "右肺上葉に結節");
    rig.answer.send(Ok("右肺上葉に\n結節。\r\n".to_string())).unwrap();
    settle(&mut rig.app);
    assert_eq!(rig.app.editor.text, "右肺上葉に結節。");
}

/// 用語ヒントは辞書の語だけ。送らない設定なら空。
#[test]
fn the_api_gets_dictionary_words_as_hints_unless_turned_off() {
    let mut rig = remote_app("remote-hints", false);
    rig.app.data.dictionary.add_word("右肺上葉", "うはいじょうよう").unwrap();
    speak_aloud(&mut rig.app, "右配上葉に結節");
    rig.answer.send(Ok("右肺上葉に結節。".to_string())).unwrap();
    settle(&mut rig.app);
    rig.app.global.remote.send_glossary = false;
    speak_aloud(&mut rig.app, "左肺下葉に結節");
    rig.answer.send(Ok("左肺下葉に結節。".to_string())).unwrap();
    settle(&mut rig.app);
    let hints = rig.hints.lock().unwrap();
    assert_eq!(hints[0].words, ["右肺上葉"]);
    assert_eq!(hints[0].language, "ja");
    assert!(hints[1].words.is_empty());
}

/// 外部 API は、選んでいて鍵があるときだけ作る。
#[test]
fn the_api_is_made_only_when_chosen_and_keyed() {
    let home = std::env::temp_dir().join(format!("openspeech-it-remote-make-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    let store = Store::at(&home);
    let mut global = openspeech::core::settings::GlobalSettings {
        recognizer: RecognizerKind::Remote,
        ..Default::default()
    };
    global.remote.provider = "deepgram".to_string();
    assert!(openspeech::app::make_remote(&store, &global).is_none(), "鍵が無い");
    store.save_remote_key("deepgram", "dg-test").unwrap();
    let made = openspeech::app::make_remote(&store, &global).expect("鍵がある");
    assert!(made.label().contains("Deepgram"));
    global.recognizer = RecognizerKind::Haas;
    assert!(openspeech::app::make_remote(&store, &global).is_none(), "選んでいない");
    assert!(store.save_remote_key("deepgram", "bad key").is_err());
    assert!(store.save_remote_key("whatever", "k").is_err());
    let _ = std::fs::remove_dir_all(&home);
}

/// 「転送」は、前の発話の API の答えと訂正を待ってから、API の文で送る。
#[test]
fn a_spoken_transfer_waits_for_the_api_and_sends_its_text() {
    let mut rig = remote_app("remote-transfer", true);
    register(&mut rig.app, CommandAction::Transfer, &["送信"]);
    let fake = FakeTransfer::default();
    let sender = fake.clone();
    rig.app.enable_transfer(Arc::new(move |request| {
        sender.sent.lock().unwrap().push(request.text.to_string());
        Ok(Delivery::Inserted)
    }));
    speak_aloud(&mut rig.app, "右配上葉に結節");
    say(&mut rig.app, "送信");
    assert!(rig.app.transfer_waiting(), "API の答えを待つ");
    rig.answer.send(Ok("右肺上葉に結節。".to_string())).unwrap();
    let sent = fake.clone();
    pump_until(&mut rig.app, "送る", move |_| !sent.sent().is_empty());
    assert_eq!(fake.sent(), ["右肺上葉に結節。"]);
}

/// ⌘Enter で送っている途中に届いた API の答えは、送った文を書き換えない(送り終えてから扱う)。
#[test]
fn an_api_answer_during_a_transfer_does_not_change_the_sent_text() {
    let mut rig = remote_app("remote-during-send", false);
    let fake = FakeTransfer::default();
    let open = fake.close_gate();
    let sender = fake.clone();
    rig.app.enable_transfer(Arc::new(move |request| {
        sender.sent.lock().unwrap().push(request.text.to_string());
        match sender.gate.lock().unwrap().as_ref() {
            Some(gate) => gate.recv().unwrap().map(|()| Delivery::Inserted),
            None => Ok(Delivery::Inserted),
        }
    }));
    speak_aloud(&mut rig.app, "右配上葉に結節");
    rig.app.transfer();
    let sending = fake.clone();
    pump_until(&mut rig.app, "送り始める", move |_| !sending.sent().is_empty());
    rig.answer.send(Ok("右肺上葉に結節。".to_string())).unwrap();
    for _ in 0..20 {
        rig.app.poll_events();
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(rig.app.editor.text, "右配上葉に結節", "送っている途中は書き換えない");
    open.send(Ok(())).unwrap();
    pump_until(&mut rig.app, "送った分が消える", |app| app.editor.is_empty());
    settle(&mut rig.app);
    assert_eq!(fake.sent(), ["右配上葉に結節"]);
    assert!(rig.app.editor.is_empty(), "{:?}", rig.app.editor.text);
}

/// 鍵の誤りのような直せない誤りのあとは、設定が変わるまで API に送らない。
#[test]
fn a_permanent_api_error_stops_sending_until_the_settings_change() {
    let mut rig = remote_app("remote-permanent", false);
    speak_aloud(&mut rig.app, "右配上葉に結節");
    rig.answer
        .send(Err(RemoteError::Permanent("鍵が正しくありません".to_string())))
        .unwrap();
    settle(&mut rig.app);
    assert!(rig.app.remote.is_none());
    assert!(notice_of(&rig.app).contains("Haas だけで動きます"), "{}", notice_of(&rig.app));
    speak_aloud(&mut rig.app, "左肺下葉に結節");
    settle(&mut rig.app);
    assert_eq!(rig.calls.load(Ordering::SeqCst), 1);
}

/// Haas の文(12 字以上)に比べて半分未満の API の文は使わない(言った文が大きく欠けた答え)。
#[test]
fn a_far_shorter_api_answer_is_not_used() {
    let mut rig = remote_app("remote-short", true);
    speak_aloud(&mut rig.app, "右肺上葉に結節。胸水なし。肝臓に嚢胞。");
    rig.answer.send(Ok("右肺上葉に結節。".to_string())).unwrap();
    settle(&mut rig.app);
    assert_eq!(rig.app.editor.text, "右肺上葉に結節。胸水なし。肝臓に嚢胞。");
    assert_eq!(*rig.seen.lock().unwrap(), ["右肺上葉に結節。胸水なし。肝臓に嚢胞。"]);
}

// ---- テンプレートの共有ライブラリ ----

const LIBRARY: &str = r#"{"generated": "2026-11-09T02:00:00Z", "count": 2,
  "categories": [{"id": "chest", "label": "胸部"}, {"id": "abdomen-pelvis", "label": "腹部・骨盤"}],
  "templates": [
    {"keywords": ["胸部CT", "胸部単純CT"], "body": ["胸部単純CT", "共有の文。"],
     "meta": {"id": "chest-ct", "title": "胸部単純CT", "category": "chest", "author": "Shintaro Kano",
              "updated": "2026-11-09", "license": "CC0-1.0"}, "file": "templates/chest/chest-ct.json"},
    {"keywords": ["前立腺MRI"], "body": "前立腺のサイズは〔　〕cm。",
     "modules": [{"name": "PI-RADS 4", "text": "辺縁域に結節を認めます。"}],
     "meta": {"id": "prostate-mri", "title": "前立腺MRI", "category": "abdomen-pelvis", "author": "",
              "updated": "2026-11-09", "license": "CC0-1.0"}, "file": "templates/abdomen-pelvis/prostate-mri.json"}]}"#;

/// 偽の口(取得・ブラウザ・クリップボード)。呼ばれた中身を覚える。
#[derive(Clone, Default)]
struct LibraryHooks {
    fetched: Arc<Mutex<Vec<String>>>,
    opened: Arc<Mutex<Vec<String>>>,
    copied: Arc<Mutex<Vec<String>>>,
}

impl LibraryHooks {
    fn install(&self, app: &mut VoiceApp, answer: Result<&'static str, &'static str>) {
        let (fetched, opened, copied) = (self.fetched.clone(), self.opened.clone(), self.copied.clone());
        app.set_library_hooks(
            Arc::new(move |url| {
                fetched.lock().unwrap().push(url.to_string());
                answer.map(str::to_string).map_err(str::to_string)
            }),
            Arc::new(move |url| {
                opened.lock().unwrap().push(url.to_string());
                Ok(())
            }),
            Arc::new(move |text| {
                copied.lock().unwrap().push(text.to_string());
                Ok(())
            }),
        );
    }
}

fn library_loaded(app: &VoiceApp) -> bool {
    app.windows
        .library
        .as_ref()
        .is_some_and(|window| !matches!(window.load, LibraryLoad::Loading))
}

/// 押したときだけ目録を取り、選んだ物を辞書に足す。重なる物は既定で飛ばし、選べば置き換える。
/// `meta` は辞書ファイルに残る。ネットワークは呼ばない(取得は偽物)。
#[test]
fn templates_are_imported_from_the_shared_library() {
    let mut app = app("library-import");
    add_template(&mut app, &["胸部CT"], CHEST);
    let hooks = LibraryHooks::default();
    hooks.install(&mut app, Ok(LIBRARY));
    for _ in 0..3 {
        app.poll_events();
    }
    assert!(hooks.fetched.lock().unwrap().is_empty(), "押すまで取りに行かない");
    app.open_template_library();
    pump_until(&mut app, "目録が届く", library_loaded);
    assert_eq!(*hooks.fetched.lock().unwrap(), vec![TEMPLATE_LIBRARY_INDEX.to_string()]);

    let picks = {
        let window = app.windows.library.as_mut().unwrap();
        window.view.checked.extend([0, 1]);
        let LibraryLoad::Ready(index) = &window.load else { panic!("{:?}", window.load) };
        assert_eq!(index.templates.len(), 2);
        window.view.picks(index)
    };
    let (message, imported) = app.import_library_templates(picks);
    assert_eq!(message, "共有ライブラリから 1 件を取り込みました。キーワードが重なる 1 件は飛ばしました");
    assert_eq!(imported, 1);
    assert_eq!(notice_of(&app), message);
    assert_eq!(app.data.dictionary.templates.len(), 2);
    assert_eq!(app.data.dictionary.templates[0].body, CHEST, "重なる物は既定で飛ばす");
    let prostate = &app.data.dictionary.templates[1];
    assert_eq!(prostate.extra["meta"]["id"], "prostate-mri");
    assert_eq!(prostate.modules.len(), 1);
    let saved = std::fs::read_to_string(app.store.dictionary_path()).unwrap();
    assert!(saved.contains("prostate-mri") && !saved.contains("templates/abdomen-pelvis"), "{saved}");

    // 取り込んだ物は「〜テンプレート」で呼べる。
    say(&mut app, "前立腺MRIテンプレート");
    assert_eq!(app.editor.text, "前立腺のサイズは〔　〕cm。");

    let picks = {
        let window = app.windows.library.as_mut().unwrap();
        window.view.checked = [0].into();
        window.view.replace.insert(0);
        let LibraryLoad::Ready(index) = &window.load else { unreachable!() };
        window.view.picks(index)
    };
    let (message, _) = app.import_library_templates(picks);
    assert_eq!(message, "共有ライブラリから 1 件を取り込みました(手元の 1 件を置き換え)");
    assert_eq!(app.data.dictionary.templates.len(), 2);
    assert_eq!(app.data.dictionary.templates[0].body, "胸部単純CT\n共有の文。");
}

/// 取り込むと 100 件を超えるときは、先頭 100 件だけ Claude に送ると知らせる。取得の失敗は知らせだけ。
#[test]
fn the_library_warns_past_one_hundred_templates_and_reports_failures() {
    let mut app = app("library-limit");
    for n in 0..99 {
        add_template(&mut app, &[&format!("検査{n}")], "文");
    }
    let hooks = LibraryHooks::default();
    hooks.install(&mut app, Ok(LIBRARY));
    app.open_template_library();
    pump_until(&mut app, "目録が届く", library_loaded);
    let picks = {
        let window = app.windows.library.as_mut().unwrap();
        window.view.checked.extend([0, 1]);
        let LibraryLoad::Ready(index) = &window.load else { unreachable!() };
        window.view.picks(index)
    };
    let (message, _) = app.import_library_templates(picks);
    assert!(
        message.contains("101 件になりました") && message.contains("先頭 100 件だけ Claude に送ります"),
        "{message}"
    );

    let mut app = app_with_library_failure();
    pump_until(&mut app, "失敗が届く", library_loaded);
    let Some(LibraryLoad::Failed(error)) = app.windows.library.as_ref().map(|w| &w.load) else {
        panic!("取得の失敗を窓に出す");
    };
    assert_eq!(error, "共有ライブラリに接続できません: 試験");
    assert!(app.notice.as_ref().unwrap().is_error);
    assert_eq!(app.data.dictionary.templates.len(), 0, "辞書は変えない");
}

fn app_with_library_failure() -> VoiceApp {
    let mut app = app("library-failure");
    LibraryHooks::default().install(&mut app, Err("共有ライブラリに接続できません: 試験"));
    app.open_template_library();
    app
}

/// 「共有…」は投稿フォームの URL に項目を埋めて開くだけ。長ければ本文とモジュールの文はクリップボードへ。
#[test]
fn sharing_a_template_opens_the_form_in_the_browser() {
    let mut app = app("library-share");
    add_template(&mut app, &["胸部CT", "胸部単純CT"], CHEST);
    add_template(&mut app, &["腹部CT"], &"肝臓に異常なし。\n".repeat(150));
    let hooks = LibraryHooks::default();
    hooks.install(&mut app, Ok(LIBRARY));

    let message = app.share_template(0).unwrap();
    assert_eq!(message, "投稿フォームをブラウザで開きました。内容を確かめて送信してください");
    let opened = hooks.opened.lock().unwrap().clone();
    assert_eq!(opened.len(), 1);
    assert!(opened[0].starts_with(TEMPLATE_SHARE_FORM), "{}", opened[0]);
    assert!(opened[0].contains("&exam=%E8%83%B8") && opened[0].contains("&findings="), "{}", opened[0]);
    assert!(hooks.copied.lock().unwrap().is_empty());

    let message = app.share_template(1).unwrap();
    assert_eq!(
        message,
        "本文と所見モジュールはクリップボードにあります。区切り線の上を「正常所見」、下を「所見モジュール」の欄に貼り付けてください"
    );
    assert_eq!(notice_of(&app), message);
    let opened = hooks.opened.lock().unwrap().clone();
    assert!(opened[1].len() <= 6000 && !opened[1].contains("&findings="), "{}", opened[1].len());
    assert!(hooks.copied.lock().unwrap()[0].starts_with("肝臓に異常なし。"));
    assert!(hooks.fetched.lock().unwrap().is_empty(), "共有では目録を取らない");
}

/// 辞書の窓でテンプレートを編集している間は取り込まない(置き換えで番号がずれ、保存が別の物を上書きするため)。
#[test]
fn nothing_is_imported_while_a_template_is_being_edited() {
    let mut app = app("library-editing");
    add_template(&mut app, &["胸部単純CT"], CHEST);
    add_template(&mut app, &["胸部CT"], "二つ目");
    let hooks = LibraryHooks::default();
    hooks.install(&mut app, Ok(LIBRARY));
    app.open_dict_tool(Some(DictTab::Templates));
    let template = app.data.dictionary.templates[1].clone();
    app.windows.dict.as_mut().unwrap().begin_edit(1, &template);
    app.open_template_library();
    pump_until(&mut app, "目録が届く", library_loaded);
    let picks = {
        let window = app.windows.library.as_mut().unwrap();
        window.view.checked.extend([0, 1]);
        window.view.replace.insert(0);
        let LibraryLoad::Ready(index) = &window.load else { unreachable!() };
        window.view.picks(index)
    };
    let before = app.data.dictionary.clone();
    let (message, imported) = app.import_library_templates(picks);
    assert_eq!((message.as_str(), imported), (IMPORT_WHILE_EDITING, 0));
    assert_eq!(app.data.dictionary, before, "辞書は変えない");
    assert!(app.notice.as_ref().unwrap().is_error);
    assert_eq!(notice_of(&app), IMPORT_WHILE_EDITING);
}

/// 窓を閉じたあとに届いた取得の失敗は知らせない。
#[test]
fn a_failure_after_the_window_closed_is_not_reported() {
    let mut app = app("library-closed");
    let (release, gate) = mpsc::channel::<()>();
    let gate = Mutex::new(gate);
    let returned = Arc::new(AtomicUsize::new(0));
    let done = returned.clone();
    app.set_library_hooks(
        Arc::new(move |_| {
            gate.lock().unwrap().recv().unwrap();
            done.fetch_add(1, Ordering::SeqCst);
            Err("共有ライブラリに接続できません: 試験".to_string())
        }),
        Arc::new(|_| Ok(())),
        Arc::new(|_| Ok(())),
    );
    app.open_template_library();
    app.windows.library = None;
    release.send(()).unwrap();
    let limit = Instant::now() + Duration::from_secs(5);
    while returned.load(Ordering::SeqCst) == 0 {
        assert!(Instant::now() < limit, "取得が返らない");
        std::thread::sleep(Duration::from_millis(5));
    }
    for _ in 0..10 {
        std::thread::sleep(Duration::from_millis(10));
        app.poll_events();
    }
    assert!(app.notice.is_none(), "{:?}", app.notice.as_ref().map(|n| &n.text));
}

/// 辞書の窓のテンプレート一覧と共有ライブラリの一覧は、長いキーワード・モジュール名・本文・題名・投稿者名の
/// 行があっても窓を広げない(Grid の列の幅が中身で決まり、窓が最大幅を超えて画面の両側で切れていた)。
/// GUI は開かず、egui の配置だけを回して窓の幅を測る。
#[test]
fn long_template_rows_do_not_widen_the_windows() {
    let ctx = egui::Context::default();
    theme::install_fonts(&ctx);
    let home = std::env::temp_dir().join(format!("openspeech-it-wide-rows-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    let mut app = VoiceApp::with_store(ctx.clone(), Store::at(&home), None);
    let width = |app: &mut VoiceApp, title: &str, draw: fn(&mut VoiceApp, &egui::Context)| {
        for _ in 0..5 {
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1440.0, 900.0))),
                ..Default::default()
            };
            let _ = ctx.run(input, |ctx| draw(app, ctx));
        }
        ctx.memory(|memory| memory.area_rect(egui::Id::new(title)))
            .unwrap_or_else(|| panic!("{title} の窓が無い"))
            .width()
    };
    let module = |name: String| Module { name, text: "所見の文。".to_string(), ..Module::default() };

    app.open_dict_tool(Some(DictTab::Templates));
    let plain = width(&mut app, "辞書", VoiceApp::draw_dict_tool);
    // 実物の辞書の最長(キーワード 65 字・モジュール 5 件)と、入力の上限いっぱい(キーワード 40 字 × 20・モジュール 30 件)。
    let real: Vec<String> = ["腹部骨盤CT", "腹部骨盤単純CT", "腹部CT", "骨盤CT", "abdomen pelvis CT", "abdominal CT", "AP CT", "CT AP"]
        .iter()
        .map(|k| k.to_string())
        .collect();
    app.data.dictionary.templates.push(Template {
        keywords: real.clone(),
        body: format!("{}\n2 行目。", "腹部骨盤単純CTを撮像しました。肝臓、胆嚢、膵臓、脾臓、腎臓に異常を認めません。"),
        modules: ["肺結節", "すりガラス影", "胸水・心嚢水", "縦隔リンパ節腫大", "肺気腫・気管支壁肥厚"]
            .iter()
            .map(|n| module(n.to_string()))
            .collect(),
        ..Template::default()
    });
    let longest: Vec<String> = (0..20).map(|i| format!("{i:02}{}", "とても長い別名のキーワード".repeat(3)).chars().take(40).collect()).collect();
    app.data.dictionary.templates.push(Template {
        keywords: longest.clone(),
        body: "改行の無い長い本文。".repeat(30),
        modules: (0..30).map(|i| module(format!("長い名前の所見モジュール{i:02}"))).collect(),
        ..Template::default()
    });
    let wide = width(&mut app, "辞書", VoiceApp::draw_dict_tool);
    assert!(app.windows.dict.is_some());
    assert!(wide <= plain + 0.5, "辞書の窓が広がった: {plain} → {wide}");

    let library = |templates: serde_json::Value| LibraryWindow {
        load: LibraryLoad::Ready(
            parse_index(
                &serde_json::json!({
                    "generated": "2026-11-09T02:00:00Z",
                    "categories": [{"id": "chest", "label": "胸部"}],
                    "templates": templates,
                })
                .to_string(),
            )
            .unwrap(),
        ),
        view: LibraryView::default(),
    };
    let entry = |keywords: &[String], title: &str, author: &str| {
        serde_json::json!({
            "keywords": keywords,
            "body": ["本文の 1 行目。".repeat(20), "2 行目。".to_string()],
            "modules": (0..5).map(|i| serde_json::json!({"name": format!("長い名前の所見モジュール{i}"), "text": "文。"})).collect::<Vec<_>>(),
            "meta": {"id": title, "title": title, "category": "chest", "author": author,
                     "updated": "2026-11-09", "license": "CC0-1.0"},
        })
    };
    app.windows.library = Some(library(serde_json::json!([entry(&["胸部CT".to_string()], "胸部CT", "")])));
    let plain = width(&mut app, "共有ライブラリから取り込む", VoiceApp::draw_template_library);
    // 手元の 2 件と重なり(「重なり 2 件: 置き換える」)、題名・投稿者名(GitHub の利用者名は 39 字まで)が長い物。
    let both: Vec<String> = real.iter().chain(&longest).cloned().collect();
    let mut window = library(serde_json::json!([
        entry(&both[..20], &"とても長い題名のテンプレート".repeat(5), &"a".repeat(39)),
        entry(&real, "腹部骨盤CT", "Shintaro-Kano"),
    ]));
    window.view.replace.insert(0);
    window.view.focus = Some(0);
    app.windows.library = Some(window);
    let wide = width(&mut app, "共有ライブラリから取り込む", VoiceApp::draw_template_library);
    assert!(app.windows.library.is_some());
    assert!(wide <= plain + 0.5, "共有ライブラリの窓が広がった: {plain} → {wide}");
}

// ---- 「ヘイクロード」の文章の手直し(post-release §7) ----

/// 手直しの答え。
fn edited(text: &str, reply: Option<&str>) -> Result<AssistantReply, String> {
    Ok(AssistantReply {
        kind: ReplyKind::Edit,
        template: None,
        text: Some(text.to_string()),
        reply: reply.map(str::to_string),
        replace: false,
    })
}

/// 依頼を言い、届くまで回す。届いた依頼を返す。
fn ask(app: &mut VoiceApp, line: &AssistantLine, spoken: &str) -> AssistantRequest {
    let count = line.requests().len();
    say(app, spoken);
    pump_until(app, "依頼が届く", |_| line.requests().len() == count + 1);
    line.requests().remove(count)
}

/// 選択範囲があれば、それを対象(selected)として添え、手直しの答えで選択だけを置き換える。前後は変えず、
/// 新しい発話は fixed(学習・AI 訂正に出さない)で、変わった字に AI 訂正の色を付け、カーソルを末尾に置く
/// (選んだままだと、次に打った 1 字で整えた文が置き換わる)。直後の Esc で元に戻り、次の Esc でも消えない。
#[test]
fn a_selected_range_is_edited_in_place_and_esc_restores_it() {
    let mut app = app("edit-selection");
    app.data.settings.save_history = false;
    let line = enable_fake_assistant(&mut app);
    say(&mut app, "胸部単純CT。");
    say(&mut app, "右肺上葉に結節を認めまる。");
    say(&mut app, "胸水なし。");
    let selected = "右肺上葉に結節を認めまる。";
    let (before, after) = app.editor.text.split_once(selected).expect("選ぶ文");
    let (before, after) = (before.to_string(), after.to_string());
    let start = before.chars().count();
    let end = start + selected.chars().count();
    app.editor.selection = (start, end);
    let request = ask(&mut app, &line, "ヘイクロード、誤字を直して");
    assert_eq!(
        request.target,
        Some(Target { text: selected.to_string(), selected: true, truncated: false })
    );
    line.answer(&mut app, edited("右肺上葉に結節を認めます。", None));
    assert_eq!(app.editor.text, format!("{before}右肺上葉に結節を認めます。{after}"));
    let last = app.editor.utterances.last().unwrap().clone();
    assert_eq!((last.start, last.end), (start, end), "選択の範囲だけを置き換える");
    assert!(app.editor.is_fixed(last.id), "学習の比較対象にしない");
    assert_eq!(app.editor.selection, (end, end), "選ばずに、カーソルを末尾に置く");
    let marked: Vec<(usize, usize)> = app
        .editor
        .spans
        .iter()
        .filter(|s| s.kind == SpanKind::Corrected)
        .map(|s| (s.start, s.end))
        .collect();
    assert_eq!(marked, vec![(end - 2, end - 1)], "変わった字(「る」→「す」)に色を付ける");
    assert_eq!(app.pending_corrections(), 0, "AI 訂正には出さない");
    let notice = notice_of(&app);
    assert!(notice.starts_with("文章を整えました(変更 1 箇所、") && notice.ends_with(" 秒)"), "{notice}");

    app.take_back();
    assert_eq!(app.editor.text, format!("{before}{selected}{after}"), "直後の Esc で元の文に戻す");
    assert_eq!(notice_of(&app), "手直しを戻しました");
    assert_eq!(app.editor.selection, (end, end));
    app.take_back();
    assert_eq!(app.editor.text, format!("{before}{selected}{after}"), "戻した文も Esc では消さない");
    assert_eq!(notice_of(&app), "手直しした文は Esc では消せません");
}

/// 選択が無ければ全文を対象(selected でない)にする。指摘は知らせに続ける。続けて話した文は末尾に入る。
/// そのあとの Esc は手直しを戻さず直前の発話を消し、次の Esc は全文の手直しを消さない(文書全体が消えない)。
#[test]
fn without_a_selection_the_whole_text_is_edited() {
    let mut app = app("edit-whole");
    app.data.settings.save_history = false;
    let line = enable_fake_assistant(&mut app);
    say(&mut app, "肺に結節結節を認めます。");
    say(&mut app, "胸水なし。");
    let request = ask(&mut app, &line, "ヘイクロード、文章を整えて");
    let whole = "肺に結節結節を認めます。胸水なし。";
    assert_eq!(
        request.target,
        Some(Target { text: whole.to_string(), selected: false, truncated: false })
    );
    line.answer(&mut app, edited("肺に結節を認めます。胸水なし。", Some("部位の記載がありません")));
    assert_eq!(app.editor.text, "肺に結節を認めます。胸水なし。");
    assert!(notice_of(&app).ends_with("。指摘: 部位の記載がありません"), "{}", notice_of(&app));
    assert_eq!(app.editor.selection, (15, 15));

    say(&mut app, "経過観察。");
    assert_eq!(app.editor.text, "肺に結節を認めます。胸水なし。経過観察。");
    app.take_back();
    assert_eq!(app.editor.text, "肺に結節を認めます。胸水なし。", "別の入力のあとは直前の発話を消す");
    app.take_back();
    assert_eq!(app.editor.text, "肺に結節を認めます。胸水なし。", "手直しは戻さず、消しもしない");
    assert_eq!(notice_of(&app), "手直しした文は Esc では消せません");

    // 手で 1 字直したあとも同じ(戻せないので控えを捨て、全文は消さない)。
    ask(&mut app, &line, "ヘイクロード、文章を整えて");
    line.answer(&mut app, edited("肺に結節を認めます。胸水はありません。", None));
    let typed = app.editor.text.replacen("結節を", "結節影を", 1);
    app.editor.sync_from_input(&typed, (4, 4));
    app.take_back();
    assert_eq!(app.editor.text, typed);
    assert_eq!(notice_of(&app), "手直しした文は Esc では消せません");
}

/// 全文が 3000 字を超えるときは先頭 3000 字だけを添え、その範囲だけを置き換える。
#[test]
fn a_long_text_sends_only_its_head() {
    let mut app = app("edit-long");
    let line = enable_fake_assistant(&mut app);
    let head = "あ".repeat(3000);
    say(&mut app, &format!("{head}いいい"));
    let request = ask(&mut app, &line, "ヘイクロード、誤字を直して");
    let target = request.target.unwrap();
    assert_eq!((target.text.chars().count(), target.selected, target.truncated), (3000, false, true));
    line.answer(&mut app, edited(&format!("{}う", "あ".repeat(2999)), None));
    assert_eq!(app.editor.text, format!("{}ういいい", "あ".repeat(2999)), "先頭の範囲だけ置き換え、残りは触らない");
}

/// 依頼に左右の語が無いのに左右が変わった答えは使わない。「右を左に直して」なら当てる。
#[test]
fn an_edit_that_flips_the_side_is_used_only_when_asked() {
    let mut app = app("edit-side");
    let line = enable_fake_assistant(&mut app);
    say(&mut app, "右肺に結節を認めます。");
    ask(&mut app, &line, "ヘイクロード、文章を整えて");
    line.answer(&mut app, edited("左肺に結節を認めます。", None));
    assert_eq!(app.editor.text, "右肺に結節を認めます。");
    assert_eq!(notice_of(&app), "左右が変わった答えなので使いませんでした");

    ask(&mut app, &line, "ヘイクロード、右を左に直して");
    line.answer(&mut app, edited("左肺に結節を認めます。", None));
    assert_eq!(app.editor.text, "左肺に結節を認めます。");
}

/// 長さの門(対象の字数 × 2 + 100 字まで)と、直すところが無い答え。
#[test]
fn edits_that_are_too_long_or_unchanged_are_not_applied() {
    let mut app = app("edit-length");
    let line = enable_fake_assistant(&mut app);
    say(&mut app, "胸水なし。");
    ask(&mut app, &line, "ヘイクロード、文章を整えて");
    line.answer(&mut app, edited(&"胸水".repeat(56), None));
    assert_eq!(app.editor.text, "胸水なし。");
    assert_eq!(notice_of(&app), "答えが長すぎるので使いませんでした");

    ask(&mut app, &line, "ヘイクロード、文章を整えて");
    line.answer(&mut app, edited("胸水なし。", Some("直すところはありません")));
    assert_eq!(app.editor.text, "胸水なし。");
    assert_eq!(notice_of(&app), "直すところはありませんでした", "同じ意味の返事は重ねない");
    let id = app.editor.utterances.last().unwrap().id;
    assert!(!app.editor.is_fixed(id), "置き換えていない");
}

/// エディターが空なら対象を添えず、手直しの答えには知らせだけ。
#[test]
fn an_edit_without_a_target_only_notifies() {
    let mut app = app("edit-empty");
    add_template(&mut app, &["胸部CT"], CHEST);
    let line = enable_fake_assistant(&mut app);
    let request = ask(&mut app, &line, "ヘイクロード、誤字を直して");
    assert_eq!(request.target, None);
    line.answer(&mut app, edited("何かの文。", None));
    assert!(app.editor.is_empty());
    assert_eq!(notice_of(&app), "対象の文章がありません");
}

/// 送ったあとに対象の範囲が変わった(口述や手直しが入った)ら当てない。範囲の後ろに続けた口述なら当てる。
#[test]
fn an_edit_is_not_applied_when_the_text_changed_meanwhile() {
    let mut app = app("edit-changed");
    let line = enable_fake_assistant(&mut app);
    say(&mut app, "結節を認めまる。");
    ask(&mut app, &line, "ヘイクロード、誤字を直して");
    app.editor.selection = (0, 0);
    say(&mut app, "右肺に");
    line.answer(&mut app, edited("結節を認めます。", None));
    assert_eq!(app.editor.text, "右肺に結節を認めまる。");
    assert_eq!(notice_of(&app), "文章が変わったので手直しを当てませんでした");

    ask(&mut app, &line, "ヘイクロード、誤字を直して");
    app.editor.move_end();
    say(&mut app, "胸水なし。");
    line.answer(&mut app, edited("右肺に結節を認めます。", None));
    assert_eq!(app.editor.text, "右肺に結節を認めます。胸水なし。", "後ろに続けた口述は残る");
}

/// テンプレートが無くても、エディターに文があれば手直しを頼める(空なら今まで通り断る)。
#[test]
fn an_edit_can_be_asked_without_templates() {
    let mut app = app("edit-no-templates");
    let line = enable_fake_assistant(&mut app);
    say(&mut app, "ヘイクロード、誤字を直して");
    assert!(notice_of(&app).starts_with("テンプレートが登録されていません"), "{}", notice_of(&app));
    assert!(line.requests().is_empty());
    say(&mut app, "結節を認めまる。");
    let request = ask(&mut app, &line, "ヘイクロード、誤字を直して");
    assert!(request.target.is_some());
}

/// AI 訂正を待っている発話があるうちの依頼は、訂正が戻ってから送る(対象は訂正後の文)。帯で待っていると示し、
/// Esc で取りやめられる。
#[test]
fn a_request_waits_for_pending_corrections() {
    let (mut app, answer, _transfer) = wired_app("edit-after-correction");
    // 学習すると訂正役と一緒に補完役を作り直し、偽の補完役が外れるので、学習は切っておく。
    app.data.settings.learning.enabled = false;
    let line = enable_fake_assistant(&mut app);
    say(&mut app, "右配上葉に結節。");
    assert_eq!(app.pending_corrections(), 1);
    say(&mut app, "ヘイクロード、文章を整えて");
    app.poll_events();
    assert!(line.requests().is_empty(), "訂正が戻るまで送らない");
    assert_eq!(app.live_caption, "依頼: 文章を整えて(訂正を待っています)");
    say(&mut app, "ヘイクロード、誤字を直して");
    assert_eq!(notice_of(&app), "前の依頼を処理しています。終わってからもう一度どうぞ");
    answer.send(Ok("右肺上葉に結節。".to_string())).unwrap();
    pump_until(&mut app, "依頼が届く", |_| line.requests().len() == 1);
    let target = line.requests()[0].target.clone().unwrap();
    assert_eq!(target.text, "右肺上葉に結節。", "対象は訂正後の文");
    line.answer(&mut app, edited("右肺上葉に結節を認めます。", None));
    assert_eq!(app.editor.text, "右肺上葉に結節を認めます。", "訂正が先に戻っても手直しが当たる");

    say(&mut app, "左腎に嚢胞。");
    say(&mut app, "ヘイクロード、文章を整えて");
    app.take_back();
    assert_eq!(notice_of(&app), "Claude への依頼を取りやめました");
    answer.send(Ok("左腎に嚢胞。".to_string())).unwrap();
    pump_until(&mut app, "訂正が戻る", |app| app.pending_corrections() == 0);
    for _ in 0..5 {
        app.poll_events();
    }
    assert_eq!(line.requests().len(), 1, "取りやめた依頼は送らない");
}

/// 依頼の指示(`request.md`)の変更を見張り、補完役を作り直して指示文に届ける。注釈は送らず、字数にも数えない。
#[test]
fn a_saved_request_guidance_reaches_the_assistant() {
    let mut app = app("request-guidance");
    app.store.save_api_key("k").unwrap();
    app.global.corrector.backend = CorrectorBackend::ApiKey;
    app.rebuild_assistant();
    let instructions = |app: &VoiceApp| app.assistant_instructions().unwrap();
    assert!(instructions(&app).contains("依頼への答え方の指示:\n# 役割"), "ファイルが無ければ既定の指示");
    assert_eq!(app.request_guidance_chars, Some(0));

    let path = app.store.ensure_request_guidance().unwrap();
    std::fs::write(&path, "<!-- 自分用のメモ -->\n試験の依頼の指示\n").unwrap();
    app.watch_files(Instant::now() + Duration::from_secs(2));
    assert!(instructions(&app).contains("依頼への答え方の指示:\n試験の依頼の指示"), "{}", instructions(&app));
    assert!(!instructions(&app).contains("# 役割") && !instructions(&app).contains("自分用のメモ"));
    assert_eq!(app.request_guidance_chars, Some(8), "注釈は数えない");
    assert!(app.assistant_instructions().unwrap().contains("テンプレート一覧:"));
}
