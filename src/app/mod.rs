//! アプリ全体の状態と動作。画面の描き方は `ui` 以下に分けている。
//!
//! 流れ: マイク → 切り出し → 認識(途中結果も) → 判定 1(命令語・テンプレート。一致したら AI を呼ばない)
//!       → 整形 → 学習した置き換え → エディター(仮表示) → AI 訂正 → 判定 2(訂正文の文末の命令語・テンプレート)
//!       → 確定(話した順) → 転送(手直しがあれば学習)。
//! 外部の音声認識 API(選んだとき): 文として入った発話の音を API にも送り、届いた文で差し替えてから AI 訂正へ。

use crate::core::apple_speech::AppleSpeech;
use crate::core::assistant::{
    MAX_TEMPLATES, match_call, ApiAssistant, Assistant, AssistantReply, AssistantRequest, Call,
    ClaudeCodeAssistant,
};
use crate::core::audio::{self, Playback};
use crate::core::caps_lock::{self, CapsFollower, MicRequest, Watcher};
use crate::core::corrector::{
    self, ClaudeCodeCorrector, ClaudeCorrector, CorrectionRequest, Corrector, MAX_CONTEXT_CHARS,
};
use crate::core::judge::{accept_correction, changed_ranges, changes_laterality, changes_meaning};
use crate::core::diag;
use crate::core::dictionary::{template_key, Dictionary, TEMPLATE_WORD};
use crate::core::editor_model::{CorrectionState, EditorModel, Span, SpanKind};
use crate::core::history::{self, History};
use crate::core::iphone_link::{LinkStep, LinkWatch};
use crate::core::mic_access;
use crate::core::models;
use crate::core::proc;
use crate::core::speechcpp::{self, SpeechCpp, VoiceDetector};
use crate::core::recognizer::{build_words, Recognition, RecognizeOptions, Recognizer};
use crate::core::record_key;
use crate::core::remote_asr::{self, CurlHttp, RemoteError, RemoteProvider};
use crate::core::segmenter::{SegmenterConfig, DEFAULT_SENSITIVITY, SAMPLE_RATE};
use crate::core::session::{Session, SessionEvent, SharedOptions};
use crate::core::settings::{
    CorrectorBackend, GlobalSettings, RecognizerKind, RecordShortcut, TransferMethod,
};
use crate::core::store::{trim_voice_log, Store};
use crate::core::text_filter::{apply_prefilters, apply_transfer_filters, disabled_ranges};
use crate::core::transfer::{
    self, Delivery, Target, TargetTracker, TransferError, TransferRequest,
};
use crate::core::user_data::UserData;
use crate::core::voice_command::{
    all_words, match_after_correction, match_utterance, normalized, CommandAction, Match,
};
use crate::ui::{theme, Windows};
use global_hotkey::hotkey::HotKey;
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

mod assistant_flow;
mod caps;
mod corrections;
mod files;
mod mic;
mod transfer_flow;
mod workers;

use assistant_flow::RequestState;
use caps::Keys;
use corrections::CorrectionQueue;
use files::FileWatch;
use mic::Mic;
use transfer_flow::TransferState;
use workers::*;

pub const APP_NAME: &str = "OpenSpeech";
pub const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MicState {
    Off,
    On,
}

/// 画面の下に出す知らせ。
#[derive(Debug, Clone)]
pub struct Notice {
    pub text: String,
    pub is_error: bool,
    pub until: Instant,
}

/// 音声の転送の完了で伝えること。画面を見ていないので、確かめていない文を送ったら知らせる。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendWarning {
    /// AI 訂正を通っていない文を含む。
    Uncorrected,
}

impl SendWarning {
    fn message(self) -> &'static str {
        match self {
            Self::Uncorrected => "AI 訂正を通っていない文を含めて転送しました",
        }
    }
}

/// 転送の実体。試験では他のアプリへ送らない偽物に差し替える。
pub type TransferFn =
    Arc<dyn for<'a> Fn(TransferRequest<'a>) -> Result<Delivery, TransferError> + Send + Sync>;

/// 転送の待ちと途中に届いた発話。
struct HeldUtterance {
    recognition: Recognition,
    audio: Option<Arc<Vec<f32>>>,
    /// 話した時点の転送先。文末の「転送」はここへ送る。
    target: Option<Target>,
    /// 置いている間に転送が取りやめられた。文は入れるが「転送」は働かせない。
    transfer_cancelled: bool,
}

/// 音声の「転送」で未確定の発話を待っている間の控え。待ちの長さは発話の上限(`CORRECTION_WAIT`)で決まる。
struct VoiceTransfer {
    /// 話した時点の転送先。待つ間に別のアプリへ移っても、話した先へ送る。
    target: Option<Target>,
}

/// 確定を待っている発話。話した順に並べ、この順に確定する。
struct Unconfirmed {
    id: u64,
    /// これを過ぎたら認識文のまま確定する。
    deadline: Instant,
    /// 話した時点の転送先。訂正のあとで「転送」と分かったら、ここへ送る。
    target: Option<Target>,
    /// 取りやめや手入力のあとの発話。訂正のあとで「転送」と分かっても働かせない。
    transfer_cancelled: bool,
    /// 戻った訂正。前の発話の確定を待つ間ここに置く。
    done: Option<CorrectionDone>,
}

pub struct ScreenshotJob {
    pub path: PathBuf,
    pub frames: u32,
    pub requested: bool,
}

/// Mac のマイクが開いてから、音が届いているかを確かめるまでの時間。iPhone は `LinkWatch` が見る。
const MIC_SILENCE_CHECK: Duration = Duration::from_secs(2);
/// 近くの iPhone を探し直す間隔。
const IPHONE_SCAN_INTERVAL: Duration = Duration::from_secs(3);
/// アクセシビリティの許可を確かめ直す間隔。
const ACCESSIBILITY_CHECK_INTERVAL: Duration = Duration::from_secs(2);
/// AI への補足のファイルの更新時刻を見る間隔。
const GUIDANCE_CHECK_INTERVAL: Duration = Duration::from_secs(2);
/// 同時に進める訂正の数。
const CORRECTION_WORKERS: usize = 3;
/// 呼びかけだけを聞いたあと、依頼を待つ長さ(§3.2 の D5)。
const CALL_WAIT: Duration = Duration::from_secs(15);
/// 発話ごとに AI 訂正の戻りを待つ上限。過ぎたら認識文のまま確定する(遅れて戻れば、手直しの前なら当てる)。
/// 実測(Claude Code、2026-11-06): 6 秒を超えたのは Sonnet が単位 E の 58 回中 4 回(最長 12.7 秒)、
/// 実際の口述の 77 回中 14 回(最長 12.3 秒)、崩れた文を含む F0 の 44 回中 15 回(最長 19.1 秒)。
/// Haiku は F0 の 44 回中 0 回、Opus は口述の 21 回中 0 回、Fable は 28 回中 2 回。
const CORRECTION_WAIT: Duration = Duration::from_secs(6);
/// 訂正文の判定で許す、元の文からの縮み(字)。文を語に置き換えた答えで命令を働かせない。
/// 句読点の分だけ見込む。超えたら認識文を残して知らせる。誤って送るより害が小さい。
const MATCH_SHRINK_LIMIT: usize = 2;
/// 転送の処理役がこれだけ返らなければ、待つのをやめて発話の受け取りに戻る。
const TRANSFER_STUCK: Duration = Duration::from_secs(30);
/// 訂正役に渡す学習例の上限。
const EXAMPLE_LIMIT: usize = 40;
/// 終了時に Haas のモデルの解放を待つ上限。読み込みの途中でも済む長さ(最初の読み込みが 2.8 秒)。
const ENGINE_CLOSE_WAIT: Duration = Duration::from_secs(5);
/// 認識エンジンに渡す学習語(hotwords)の上限。
const HOTWORD_LIMIT: usize = 50;

/// `--screenshot` で何フレーム描いてから保存するか。
pub const SCREENSHOT_FRAMES: u32 = 120;

pub struct VoiceApp {
    pub store: Store,
    pub global: GlobalSettings,
    pub data: UserData,
    pub session: Session,
    pub options: SharedOptions,
    pub engine_name: Option<String>,
    pub editor: EditorModel,
    /// 音声やコマンドで動かしたカーソルを画面へ反映するか。
    pub selection_dirty: bool,
    pub history: History,
    pub mic: MicState,
    pub tracker: TargetTracker,
    pub notice: Option<Notice>,
    pub windows: Windows,
    pub compact: bool,
    pub playback: Option<Playback>,
    pub screenshot: Option<ScreenshotJob>,
    /// 話している最中の途中結果(確定したら消える)。
    pub live_caption: String,
    /// アクセシビリティの許可があるか(転送に必要)。
    pub accessibility_ok: bool,
    accessibility_checked_at: Option<Instant>,
    /// 最後の転送で使った届け方。
    pub last_delivery: Option<Delivery>,
    /// マイクと iPhone の開け閉めの状態(`mic.rs`)。録音しているかは `mic`。
    microphone: Mic,
    pub ctx: egui::Context,
    /// 閉じる確認を済ませたら true。
    pub allow_close: bool,
    /// 起動直後に窓の大きさを確かめたか(前回を小さな表示のまま終えた場合の立て直し)。
    pub size_checked: bool,
    /// 転送の口と途中の状態、転送の待ちに置いた発話(`transfer_flow.rs`)。
    transfers: TransferState,
    /// AI 訂正の待ち(`corrections.rs`)。
    corrections: CorrectionQueue,
    /// 「ヘイクロード」の補完役と依頼の状態(`assistant_flow.rs`)。
    requests: RequestState,
    /// 録音の切り替えキー(`caps.rs`)。
    keys: Keys,
    /// 取得中のモデル(設定の「認識」の「モデルを取得」)。取得中はマイクを開かない。落とすと取得も止まる。
    download: Option<models::Download>,
    /// 最後の取得で取れなかった理由(設定の「認識」と初回の案内に出す)。
    pub download_error: Option<String>,
    voice_serial: u64,
    /// 認識結果の訂正役。無効か、接続に必要なものが無ければ None。
    pub corrector: Option<Arc<dyn Corrector>>,
    /// 外部の音声認識 API(Haas のあとの段)。選んでいないか、鍵が無ければ None。
    pub remote: Option<Arc<dyn RemoteProvider>>,
    /// 利用者が手で書き換えるファイルの見張り(`files.rs`)。
    files: FileWatch,
    /// 補足の字数(設定画面の表示用)。読めないファイルなら None。
    pub guidance_chars: Option<usize>,
}

/// 外部の音声認識 API を用意する。選んでいない・知らない provider・鍵が無いときは None(Haas だけで動く)。
pub fn make_remote(store: &Store, global: &GlobalSettings) -> Option<Arc<dyn RemoteProvider>> {
    if global.recognizer != RecognizerKind::Remote {
        return None;
    }
    let provider = &global.remote.provider;
    if !remote_asr::is_known_provider(provider) {
        diag::log("知らない外部 API が設定されているので Haas だけで動く");
        return None;
    }
    let Some(key) = store.load_remote_key(provider) else {
        diag::log("外部 API の鍵が無いので Haas だけで動く");
        return None;
    };
    remote_asr::make_provider(&global.remote, key, Arc::new(CurlHttp))
}

/// 選ばれた認識エンジンを用意する。Mac 内蔵の音声認識が使えなければ Haas に戻し、記録に残す。
/// Haas のモデルか推論ライブラリが無ければ None(「認識エンジンが見つかりません」)。
pub fn make_recognizer(global: &GlobalSettings) -> Option<Box<dyn Recognizer>> {
    if global.recognizer == RecognizerKind::Apple {
        match make_apple() {
            Ok(engine) => return Some(engine),
            Err(reason) => {
                diag::log(format!("Mac 内蔵の音声認識が使えないため Haas に戻す: {reason}"));
            }
        }
    }
    match make_haas() {
        Ok(engine) => Some(engine),
        Err(reason) => {
            diag::log(format!("Haas が使えない: {reason}"));
            None
        }
    }
}

/// そのエンジンが使えない理由。使えるなら None。設定画面の案内に使う。
pub fn engine_problem(kind: RecognizerKind) -> Option<String> {
    let missing = match kind {
        RecognizerKind::Apple => {
            if AppleSpeech::is_available() {
                return None;
            }
            "macOS 26 以降と補助プログラムが必要です"
        }
        // 外部 API を選んでも Haas が先に動く(即時表示と命令語)ので、Haas が要る。鍵の有無は画面が別に出す。
        RecognizerKind::Haas | RecognizerKind::Remote => {
            if speechcpp::find_model().is_none() {
                speechcpp::MODEL_MISSING
            } else if speechcpp::find_library().is_none() {
                "推論ライブラリ(speech.cpp)が見つかりません"
            } else {
                return None;
            }
        }
    };
    Some(missing.to_string())
}

/// 声の判定(Silero VAD)が使えない理由。使えるなら None。設定画面の案内に使う。
pub fn detector_problem() -> Option<String> {
    if speechcpp::find_vad_model().is_none() {
        Some(speechcpp::VAD_MISSING.to_string())
    } else if speechcpp::find_library().is_none() {
        Some("推論ライブラリ(speech.cpp)が見つかりません".to_string())
    } else {
        // ファイルはあるが読めなかった(声の判定モデルでない、壊れているなど)。
        speechcpp::detector_failure().map(|reason| format!("声の判定を使えません: {reason}"))
    }
}

/// 声の判定モデル(Silero VAD)を読む。無ければ理由を記録して None(切り出しは音量だけで動く)。
fn make_detector() -> Option<Arc<VoiceDetector>> {
    let (Some(model), Some(library)) = (speechcpp::find_vad_model(), speechcpp::find_library())
    else {
        diag::log("声の判定モデルが無いので、切り出しは音量だけ");
        return None;
    };
    match VoiceDetector::load(&library, &model) {
        Ok(detector) => Some(Arc::new(detector)),
        Err(message) => {
            diag::log(format!("声の判定を使えない: {message}"));
            None
        }
    }
}

/// Haas-ja v1.0(speech.cpp)を用意する。モデルか推論ライブラリが無ければ理由を返す。
fn make_haas() -> Result<Box<dyn Recognizer>, String> {
    let model = speechcpp::find_model().ok_or(speechcpp::MODEL_MISSING)?;
    let library =
        speechcpp::find_library().ok_or("推論ライブラリ(speech.cpp)が見つかりません")?;
    SpeechCpp::new(&library, &model).map(|engine| Box::new(engine) as Box<dyn Recognizer>)
}

/// Mac 内蔵の音声認識(macOS 26 以降)。補助プロセスが無ければ理由を返す。
fn make_apple() -> Result<Box<dyn Recognizer>, String> {
    if !AppleSpeech::is_available() {
        return Err("macOS 26 以降と補助プログラム(openspeech-speech)が必要です".to_string());
    }
    let helper = AppleSpeech::find_helper().ok_or("補助プログラムが見つかりません")?;
    Ok(Box::new(AppleSpeech::new(helper)))
}

/// 補足の字数。前後の空白は訂正役に渡らないので数えない。読めなければ None。
/// 訂正役は学習のたびに作り直すので、切ったことの記録は、ファイルを読み直すここで 1 回だけ残す。
fn guidance_length(store: &Store) -> Option<usize> {
    let length = store.read_guidance().ok()?.trim().chars().count();
    if length > corrector::MAX_GUIDANCE_CHARS {
        diag::log(format!(
            "補足が長いので {} 字で切った(全 {length} 字)",
            corrector::MAX_GUIDANCE_CHARS
        ));
    }
    Some(length)
}

/// 命令語すべて(名前・読み・登録した言い方)。
fn command_word_list(data: &UserData) -> Vec<String> {
    all_words(&data.settings.command_words)
        .map(|(word, _)| word)
        .collect()
}

/// 訂正役を用意する。設定で無効、または接続に必要なもの(API キー、Claude Code)が無ければ None。
/// 学習した「誤認識 → 正しい語」の例を添える。
pub fn make_corrector(
    store: &Store,
    global: &GlobalSettings,
    data: &UserData,
) -> Option<Arc<dyn Corrector>> {
    if !global.corrector.enabled {
        return None;
    }
    let examples: Vec<(String, String)> = if data.settings.learning.enabled {
        data.memory
            .examples(EXAMPLE_LIMIT)
            .into_iter()
            .map(|(from, to)| (from.to_string(), to.to_string()))
            .collect()
    } else {
        Vec::new()
    };
    let guidance = store.load_guidance();
    // 訂正のあとの判定で読むので、AI に消されたり言い換えられたりしないように渡す。
    let protected: Vec<String> = command_word_list(data)
        .into_iter()
        .chain(data.dictionary.template_keywords().map(str::to_string))
        .chain((!data.dictionary.templates.is_empty()).then(|| TEMPLATE_WORD.to_string()))
        .collect();
    let model = &global.corrector.model;
    match global.corrector.backend {
        CorrectorBackend::ApiKey => {
            let key = store.load_api_key()?;
            Some(Arc::new(ClaudeCorrector::new(
                model,
                &key,
                &data.dictionary,
                &guidance,
                &protected,
                &examples,
            )))
        }
        CorrectorBackend::ClaudeCode => {
            let binary = proc::find_claude()?;
            Some(Arc::new(ClaudeCodeCorrector::new(
                model,
                &binary,
                &store.root,
                &data.dictionary,
                &guidance,
                &protected,
                &examples,
            )))
        }
    }
}

/// 「ヘイ、クロード」の補完役を用意する。接続は訂正役と同じ設定(API キーか Claude Code)を使い、
/// 訂正の有効・無効には従わない(呼びかけは利用者がその都度するので、訂正を止めていても使える)。
/// 接続に必要なものが無ければ None。
pub fn make_assistant(
    store: &Store,
    global: &GlobalSettings,
    data: &UserData,
) -> Option<Arc<dyn Assistant>> {
    let model = &global.corrector.model;
    match global.corrector.backend {
        CorrectorBackend::ApiKey => {
            let key = store.load_api_key()?;
            Some(Arc::new(ApiAssistant::new(model, &key, &data.dictionary)))
        }
        CorrectorBackend::ClaudeCode => {
            let binary = proc::find_claude()?;
            Some(Arc::new(ClaudeCodeAssistant::new(
                model,
                &binary,
                &store.root,
                &data.dictionary,
            )))
        }
    }
}

impl VoiceApp {
    /// 通常の起動。全体ショートカットと転送先の追跡、転送を有効にする。
    pub fn new(ctx: egui::Context, screenshot: Option<PathBuf>) -> Self {
        let store = Store::open();
        // 動作の記録の置き場はここで決める(`with_store` で決めると、同じプロセスの試験どうしで記録先を奪い合う)。
        diag::set_path(Some(store.root.join("logs").join("voice.log")));
        let mut app = Self::with_store(ctx, store, screenshot);
        let stale = proc::remove_stale_private_files();
        if stale > 0 {
            diag::log(format!("前の終了で残った一時ファイルを {stale} 個消した"));
        }
        app.session.warm_up();
        app.keys.hotkeys = GlobalHotKeyManager::new().ok();
        app.register_hotkey();
        if app.screenshot.is_none() {
            caps_lock::prevent_app_nap();
            let wake = app.ctx.clone();
            app.keys.caps_watcher = Some(Watcher::start(move || wake.request_repaint()));
        }
        app.tracker = TargetTracker::start();
        app.enable_transfer(Arc::new(transfer::send));
        app.enable_corrections();
        app.rebuild_assistant();
        if !app.global.first_run_shown && app.screenshot.is_none() {
            app.windows.first_run = true;
        }
        app.check_accessibility(true);
        if !app.accessibility_ok && app.screenshot.is_none() {
            // Mac の確認画面を出す(一覧に載せてもらう)。すでに断られていれば何も出ない。
            transfer::request_accessibility();
        }
        app
    }

    /// OS に触らない状態で作る。試験はこちらを使い、クリップボードや他のアプリに影響を与えない。
    pub fn with_store(ctx: egui::Context, store: Store, screenshot: Option<PathBuf>) -> Self {
        let global = store.load_global();
        let mut data = store.load_user();
        data.settings.command_words = normalized(&data.settings.command_words);
        // 以前に覚えた組でも、所見の意味を変えるものは使わない。
        data.memory
            .pairs
            .retain(|pair| !changes_meaning(&pair.from, &pair.to));
        let recognizer = make_recognizer(&global);
        let engine_name = recognizer.as_ref().map(|r| r.name());
        diag::log(format!(
            "起動 {APP_NAME} {APP_VERSION} engine={} prefer_iphone={} device={}",
            engine_name.as_deref().unwrap_or("なし"),
            global.prefer_iphone,
            global
                .input_device
                .as_deref()
                .map_or("(既定)".to_string(), audio::display_device_name)
        ));
        let options: SharedOptions = Arc::new(Mutex::new(recognize_options(&data)));
        let mut session = new_session(recognizer, options.clone(), &ctx);
        // 声の判定モデルは Session が持ち、エンジンの作り直し(`hand_over_to`)でも引き継がれる。
        session.set_detector(make_detector());
        // 古い日の転送履歴は起動のたびにも消す(転送しない日が続いても残り続けないように)。
        history::prune(&store.history_dir(), chrono::Local::now().date_naive());
        let history = History::load(&store.history_dir(), data.settings.history_count);
        // 規則をファイルへ移した引き継ぎ(2026-11-08)。時刻を読むのはその後で、指示を読む前。その間に保存されても、次の見張りで気づける。
        if let Some(notice) = store.migrate_guidance() {
            diag::log(&notice);
        }
        let guidance_modified = store.guidance_modified();
        let guidance_chars = guidance_length(&store);
        let dictionary_modified = store.dictionary_modified();
        let saved_dictionary = data.dictionary.clone();
        diag::log(format!("辞書 {}", dictionary_summary(&data.dictionary)));
        let corrector = make_corrector(&store, &global, &data);
        let remote = make_remote(&store, &global);
        theme::install_style(
            &ctx,
            data.settings.appearance.theme,
            data.settings.appearance.scale,
        );
        let warnings = store.take_warnings();
        for warning in &warnings {
            diag::log(warning);
        }
        let windows = Windows {
            startup_warning: (!warnings.is_empty()).then(|| warnings.join("\n")),
            ..Windows::default()
        };
        VoiceApp {
            store,
            global,
            data,
            session,
            options,
            engine_name,
            editor: EditorModel::new(),
            selection_dirty: false,
            history,
            mic: MicState::Off,
            tracker: TargetTracker::default(),
            notice: None,
            windows,
            compact: false,
            playback: None,
            screenshot: screenshot.map(|path| ScreenshotJob {
                path,
                frames: 0,
                requested: false,
            }),
            live_caption: String::new(),
            accessibility_ok: false,
            accessibility_checked_at: None,
            last_delivery: None,
            microphone: Mic::default(),
            ctx,
            allow_close: false,
            size_checked: false,
            transfers: TransferState::new(),
            corrections: CorrectionQueue::new(),
            requests: RequestState::new(),
            keys: Keys::default(),
            download: None,
            download_error: None,
            voice_serial: 0,
            corrector,
            remote,
            files: FileWatch {
                guidance_modified,
                dictionary_modified,
                saved_dictionary,
                checked_at: None,
            },
            guidance_chars,
        }
    }

    /// 転送の処理役を動かす。作った直後は何も送らない(試験はここで偽物を渡す)。
    /// 呼ぶたびに処理役を作り直す(古い口は落ち、古いスレッドは列が空になって終わる)。試験は偽物の転送に差し替える。
    pub fn enable_transfer(&mut self, send: TransferFn) {
        self.transfers.jobs = Some(spawn_transfer_worker(
            self.transfers.tx.clone(),
            self.ctx.clone(),
            send,
        ));
    }

    /// 訂正の処理役を動かす。呼ぶたびに作り直す(`enable_transfer` と同じ)。
    pub fn enable_corrections(&mut self) {
        self.corrections.jobs = Some(spawn_ai_worker(
            self.corrections.tx.clone(),
            self.ctx.clone(),
        ));
        self.corrections.remote_jobs = Some(spawn_remote_worker(
            self.corrections.remote_tx.clone(),
            self.ctx.clone(),
        ));
    }

    // ---- 知らせ ----

    pub fn notify(&mut self, text: impl Into<String>) {
        self.notice = Some(Notice {
            text: text.into(),
            is_error: false,
            until: Instant::now() + Duration::from_secs(4),
        });
    }

    pub fn notify_error(&mut self, text: impl Into<String>) {
        self.notice = Some(Notice {
            text: text.into(),
            is_error: true,
            until: Instant::now() + Duration::from_secs(8),
        });
    }

    // ---- 表示 ----

    /// 外観の設定(明暗・拡大率)を画面へ反映する。
    pub fn apply_appearance(&mut self) {
        let appearance = &self.data.settings.appearance;
        theme::install_style(&self.ctx, appearance.theme, appearance.scale);
        self.ctx
            .send_viewport_cmd(egui::ViewportCommand::WindowLevel(self.window_level()));
    }

    /// 窓を常に手前に出すか。小さな表示のときと、設定で「常に手前」にしたとき。
    pub fn window_level(&self) -> egui::WindowLevel {
        if self.compact || self.data.settings.editor.top_most {
            egui::WindowLevel::AlwaysOnTop
        } else {
            egui::WindowLevel::Normal
        }
    }

    // ---- 認識エンジン ----

    pub fn refresh_engine_options(&mut self) {
        self.session.set_options(recognize_options(&self.data));
        self.rebuild_corrector();
    }

    /// 学習の記憶が変わったあとの共通処理(画面の辞書ツールからも呼ぶ)。
    pub fn after_memory_change(&mut self) {
        self.refresh_engine_options();
        self.save_data();
    }

    /// 認識エンジンの選択が変わったときに作り直す。
    pub fn rebuild_engine(&mut self) {
        let was = self.mic;
        self.stop_mic();
        let recognizer = make_recognizer(&self.global);
        self.engine_name = recognizer.as_ref().map(|r| r.name());
        let mut next = new_session(recognizer, self.options.clone(), &self.ctx);
        self.session.hand_over_to(&mut next);
        self.session = next;
        self.session.warm_up();
        self.refresh_engine_options();
        if was == MicState::On {
            self.start_mic();
        }
    }

    /// 足りないモデルを取りに行く(設定の「認識」と初回の案内のボタン)。録音中なら止める。
    pub fn start_model_download(&mut self) {
        if self.download.is_some() {
            return;
        }
        let missing = models::missing();
        if missing.is_empty() {
            self.notify("モデルはそろっています");
            return;
        }
        self.stop_mic();
        self.download_error = None;
        diag::log(format!("モデルの取得を始める({} 個)", missing.len()));
        let wake = self.ctx.clone();
        self.download = Some(models::Download::start(
            missing,
            models::models_root(),
            move || wake.request_repaint(),
        ));
    }

    /// 取得をやめる(途中の分は残り、次に続きから取る)。
    pub fn cancel_model_download(&mut self) {
        if let Some(download) = &self.download {
            download.cancel();
        }
    }

    /// 取得の進み具合(取れた分, 全体)のバイト数。取得していなければ None。
    pub fn model_download_progress(&self) -> Option<(u64, u64)> {
        self.download.as_ref().map(models::Download::progress)
    }

    /// 取得が終わったら、エンジンと声の判定を作り直す。取得中は進み具合を描き直させる。
    fn watch_download(&mut self) {
        let Some(download) = &self.download else {
            return;
        };
        let Some(result) = download.finished() else {
            self.ctx.request_repaint_after(Duration::from_millis(500));
            return;
        };
        let cancelled = download.was_cancelled();
        self.download = None;
        // 取れた分は使う(認識のモデルだけ取れて声の判定が取れなかった、なども)。
        if self.engine_name.is_none() {
            self.rebuild_engine();
        }
        // 声の判定はまだ無いときだけ読む(読み込み済みのものを空で上書きしない)。
        if !self.session.has_detector() {
            if let Some(detector) = make_detector() {
                self.session.set_detector(Some(detector));
            }
        }
        match result {
            Ok(()) => {
                diag::log("モデルを取得した");
                self.notify("モデルを取得しました。マイクを入れると使えます");
            }
            // やめたのは失敗ではない。取れた分は残り、次は続きから取る(やめる前の本当の失敗があれば下で見せる)。
            Err(message) if cancelled && message == models::CANCELLED => {
                diag::log("モデルの取得をやめた");
                self.notify("モデルの取得をやめました。次は続きから取ります");
            }
            Err(message) => {
                diag::log(format!("モデルを取得できない({} 字)", message.chars().count()));
                self.notify_error(message.clone());
                self.download_error = Some(message);
            }
        }
    }

    // ---- 認識結果の受け取り ----

    pub fn poll_events(&mut self) {
        // 発話を処理する前に見る。保存の直後の発話から新しい補足で直すため。
        self.watch_files(Instant::now());
        self.poll_hotkey();
        self.follow_caps_lock();
        self.scan_for_iphone(Instant::now());
        self.check_accessibility(false);
        while let Ok(done) = self.corrections.remote_rx.try_recv() {
            self.receive_remote(done);
        }
        while let Ok(done) = self.corrections.rx.try_recv() {
            self.receive_correction(done);
        }
        while let Ok(done) = self.requests.rx.try_recv() {
            if self.requests.discarded > 0 {
                self.requests.discarded -= 1;
                diag::log("補完: 取りやめた依頼の答えを捨てる");
                continue;
            }
            self.requests.held = Some(done);
        }
        for event in self.session.poll() {
            match event {
                SessionEvent::Recognized { recognition, audio } => {
                    self.data.memory.voice_seconds += audio.len() as f64 / SAMPLE_RATE as f64;
                    self.save_voice_log(&audio);
                    self.handle_utterance(recognition, Some(audio));
                }
                SessionEvent::FileRecognized { recognition } => {
                    self.handle_utterance(recognition, None);
                }
                SessionEvent::Partial { text } => {
                    if self.mic == MicState::On {
                        self.live_caption = text;
                    }
                }
                SessionEvent::MicOpened { generation, took } => {
                    self.mic_ready(generation, took)
                }
                SessionEvent::MicFailed {
                    generation,
                    message,
                } => self.mic_failed(generation, message),
                SessionEvent::Error(message) => self.notify_error(message),
            }
        }
        while let Ok(done) = self.transfers.rx.try_recv() {
            self.finish_transfer(done);
        }
        self.watch_mic(Instant::now());
        self.watch_download();
        self.watch_corrections(Instant::now());
        self.watch_call(Instant::now());
        self.finish_voice_transfer();
        self.watch_transfer(Instant::now());
        self.release_held_utterances();
        if self.playback.as_ref().is_some_and(|p| !p.is_playing()) {
            self.playback = None;
        }
    }

    fn save_voice_log(&mut self, samples: &[f32]) {
        if !self.global.voice_log.enabled {
            return;
        }
        let folder = self.store.voice_log_dir(&self.global);
        if std::fs::create_dir_all(&folder).is_err() {
            return;
        }
        self.voice_serial += 1;
        let name = format!(
            "{}_{:04}.wav",
            chrono::Local::now().format("%Y%m%d_%H%M%S"),
            self.voice_serial
        );
        if let Err(message) = crate::core::recognizer::write_wav(&folder.join(name), samples) {
            self.notify_error(message);
        }
        trim_voice_log(&folder, self.global.voice_log.limit_mb);
    }

    /// 発話 1 回分を受け取る。文字入力でも同じ流れを試せるよう公開している。
    pub fn handle_utterance(&mut self, recognition: Recognition, audio: Option<Arc<Vec<f32>>>) {
        self.live_caption.clear();
        if recognition.text.trim().is_empty() {
            return;
        }
        // 送っている文のための「転送」の言い直しは聞き流す(Esc で取りやめたあとに生き返らせない)。
        // 先頭だけ送っている間は、残りの文のための「転送」なので置いておく。
        let matched = match_utterance(
            &recognition.text,
            &self.data.settings.command_words,
            &self.data.dictionary.symbols,
        );
        let repeated = matches!(
            &matched,
            Match::Command { prefix, action: CommandAction::Transfer, .. } if prefix.is_empty()
        );
        let for_the_rest = self.transfers.sending_head && self.transfer_sending();
        if repeated && self.transfer_busy() && self.transfers.held.is_empty() && !for_the_rest {
            return;
        }
        let target = self.current_target();
        self.transfers.held.push(HeldUtterance {
            recognition,
            audio,
            target,
            transfer_cancelled: false,
        });
        self.release_held_utterances();
    }

    fn process_utterance(&mut self, held: HeldUtterance) {
        let HeldUtterance {
            mut recognition,
            audio,
            target,
            transfer_cancelled,
        } = held;
        // 呼びかけ語は判定 1 より先に見る(依頼は本文に入れず、命令語やテンプレートとしても見ない)。
        // 転送の待ちと途中は置いておき、話した順に処理する(「転送」のあとの依頼が転送に混ざらない)。
        // 依頼が「〜テンプレート」だけなら、呼びかけ語を除いた文に書き換えて下の普通の経路で入れる。
        if self.take_call(&mut recognition) {
            return;
        }
        // 取りやめのあとは手で直している途中かもしれないので、カーソルの位置ではなく文末へ入れ、
        // 手で動かしていたカーソルは戻す。文末にあったなら、続きの発話のためにそのまま進める。
        let length = self.editor.len();
        let selection = self.editor.selection;
        let keep_selection = transfer_cancelled && selection != (length, length);
        if transfer_cancelled {
            self.editor.move_end();
        }
        match match_utterance(
            &recognition.text,
            &self.data.settings.command_words,
            &self.data.dictionary.symbols,
        ) {
            Match::Symbol(symbol) => self.insert_symbol(&symbol),
            Match::Command {
                prefix,
                action,
                count,
            } => {
                if !prefix.is_empty() {
                    let part = Recognition {
                        unsure: unsure_within(&recognition, &prefix),
                        text: prefix,
                    };
                    self.insert_text(part, audio, target.clone(), transfer_cancelled, true);
                }
                if !(transfer_cancelled && action == CommandAction::Transfer) {
                    self.apply_action(action, count, target);
                }
            }
            Match::Text => self.insert_text(recognition, audio, target, transfer_cancelled, false),
        }
        if keep_selection {
            self.editor.selection = selection;
        }
    }

    /// エディターの文を転送している途中か(`TransferState::sending`)。
    fn transfer_sending(&self) -> bool {
        self.transfers.sending()
    }

    /// 音声の転送の待ちか、エディターの文の転送の途中か(`TransferState::busy`)。
    fn transfer_busy(&self) -> bool {
        self.transfers.busy()
    }

    /// 音声の転送で未確定の発話を待っているか。
    pub fn transfer_waiting(&self) -> bool {
        self.transfers.voice.is_some()
    }

    /// 転送の待ちと途中に置いている発話の数。
    pub fn held_utterance_count(&self) -> usize {
        self.transfers.held.len()
    }

    /// 発話を話した順に処理する。途中の「転送」でまた待ちに入ったら、残りは次へ回す。
    fn release_held_utterances(&mut self) {
        // 補完の答えは、送っている途中でなければ当てる。音声の転送の待ちの間は入れる(依頼は「転送」より先なので一緒に送る)。
        if !self.transfer_sending() {
            if let Some(done) = self.requests.held.take() {
                self.receive_completion(done);
            }
        }
        while !self.transfer_busy() && !self.transfers.held.is_empty() {
            let held = self.transfers.held.remove(0);
            self.process_utterance(held);
        }
    }

    /// 文を仮表示して AI 訂正を依頼する。訂正役がいなければその場で確定する。
    /// `target` と `transfer_cancelled` は、訂正のあとで「転送」と分かったときに使う。
    /// `spoken_command` は文末の命令語を判定 1 で切り離した発話か。
    fn insert_text(
        &mut self,
        recognition: Recognition,
        audio: Option<Arc<Vec<f32>>>,
        target: Option<Target>,
        transfer_cancelled: bool,
        spoken_command: bool,
    ) {
        let filtered = apply_prefilters(
            &recognition.text,
            &self.data.settings.filters,
            &self.data.dictionary,
        );
        // 発話全体が「キーワード+テンプレート」ならテンプレート。「えー」などを除いた文でも確かめる。
        let dictionary = &self.data.dictionary;
        let template = dictionary
            .find_template(&recognition.text)
            .or_else(|| dictionary.find_template(&filtered))
            .map(|template| template.body.clone());
        if let Some(body) = template {
            self.data.memory.utterances += 1;
            self.insert_template(&body, audio);
            return;
        }
        // 「えー。」から不要語を除くと句点だけが残る(句読点を出すエンジンのとき)。それは入れない。
        let only_marks = filtered
            .chars()
            .all(|c| c.is_whitespace() || "、。，．,.!！?？".contains(c));
        if filtered.is_empty() || (only_marks && filtered != recognition.text) {
            return;
        }
        self.data.memory.utterances += 1;
        // 学習した置き換えを、AI 訂正を待たずに当てる(回数が足りているものだけ)。
        let learning = &self.data.settings.learning;
        let (text, learned, applied) = if learning.enabled && learning.auto_apply {
            let applied = self
                .data
                .memory
                .apply_with_pairs(&filtered, learning.min_count);
            (applied.text, applied.changed, applied.pairs)
        } else {
            (filtered.clone(), Vec::new(), Vec::new())
        };
        if !learned.is_empty() {
            diag::log(format!("学習した置き換えを {} 箇所に適用", learned.len()));
        }
        let mut unsure = relocate_unsure(&recognition, &text);
        unsure.extend(disabled_ranges(&text, &self.data.dictionary));
        let start = {
            let (a, b) = self.editor.selection;
            a.min(b)
        };
        let context = tail(&self.editor.slice(0, start), MAX_CONTEXT_CHARS);
        let id = self.editor.insert_recognized(&text, &unsure, audio.clone());
        if let Some(utterance) = self.editor.utterances.iter_mut().find(|u| u.id == id) {
            utterance.applied = applied.clone();
            utterance.spoken_command = spoken_command;
        }
        for (from, to) in &learned {
            self.editor.spans.push(Span {
                start: start + from,
                end: start + to,
                kind: SpanKind::Corrected,
            });
        }
        self.selection_dirty = true;
        // 仮表示はもう出ている。訂正が戻るか上限を過ぎたら、話した順に確定する。
        let request = CorrectionRequest {
            text,
            unsure: self
                .editor
                .spans
                .iter()
                .filter(|s| s.kind == SpanKind::Unsure && s.start >= start)
                .map(|s| (s.start - start, s.end - start))
                .collect(),
            context,
        };
        // 外部の音声認識 API を使うなら、その答えを待ってから訂正を依頼する(待つ上限はその分のばす)。
        let remote_wait = self.remote_timeout();
        let waiting = if self.enqueue_remote(id, &recognition.text, audio.as_ref(), applied.clone(), request.clone()) {
            Some(remote_wait + CORRECTION_WAIT)
        } else if self.enqueue_correction(id, applied, request) {
            Some(CORRECTION_WAIT)
        } else {
            None
        };
        if let Some(wait) = waiting {
            self.editor.set_correction(id, CorrectionState::Waiting);
            self.corrections.unconfirmed.push_back(Unconfirmed {
                id,
                deadline: Instant::now() + wait,
                target,
                transfer_cancelled,
                done: None,
            });
        }
        self.after_insert();
    }

    /// テンプレートの文を入れる。登録した文をそのまま使うので、AI 訂正と学習は通さない。
    pub fn insert_template(&mut self, body: &str, audio: Option<Arc<Vec<f32>>>) {
        self.editor.insert_template(body, audio);
        self.selection_dirty = true;
        self.after_insert();
    }

    /// エディターに文が入ったあとの共通処理。小さな窓のままだと入った文が見えないので広げる。
    fn after_insert(&mut self) {
        if self.compact {
            let ctx = self.ctx.clone();
            self.set_compact(false, &ctx);
        }
    }

    fn insert_plain(&mut self, text: &str) {
        self.editor.insert_plain(text);
        self.selection_dirty = true;
    }

    /// 記号の読みで入れる文字。直前が同じ句読点なら重ねない
    /// (認識エンジンや AI が文末に「。」を付けたあとに「まる」と言うと「。。」になる)。
    fn insert_symbol(&mut self, symbol: &str) {
        let (a, b) = self.editor.selection;
        let punctuation = symbol.chars().count() == 1 && "。、，．".contains(symbol);
        if a == b && punctuation && self.editor.slice(0, a).ends_with(symbol) {
            diag::log(format!("記号「{symbol}」は直前と同じなので入れない"));
            return;
        }
        self.insert_plain(symbol);
    }

    /// `target` は話した時点の転送先。
    /// `count` は続けて言った回数(「改行改行」は 2 行)。転送は何度言っても 1 回。
    fn apply_action(&mut self, action: CommandAction, count: usize, target: Option<Target>) {
        match action {
            CommandAction::NewLine => self.insert_plain(&"\n".repeat(count.max(1))),
            CommandAction::Transfer => self.transfer_after_corrections(target),
        }
        self.selection_dirty = true;
    }

    // ---- 再生 ----

    pub fn toggle_playback(&mut self) {
        if let Some(playback) = self.playback.take() {
            playback.stop();
            return;
        }
        let audio = self
            .editor
            .utterance_at(self.editor.cursor())
            .and_then(|u| u.audio.clone());
        match audio {
            Some(samples) => match audio::play(samples) {
                Ok(playback) => self.playback = Some(playback),
                Err(message) => self.notify_error(message),
            },
            None => self.notify("この位置に再生できる音声はありません"),
        }
    }

    pub fn shutdown(&mut self) {
        // 取得中なら止める(終わったあとに curl が残って落とし続けないように)。
        self.download = None;
        self.stop_mic();
        // 録音していないのにランプが点いたままにならないよう、Caps Lock を戻す。
        if self.keys.caps_watcher.is_some()
            && self.data.settings.record.shortcut == RecordShortcut::CapsLock
        {
            caps_lock::set(false);
        }
        self.save_data();
        if self.data.settings.auto_backup {
            let _ = self.store.backup_user(&self.data);
        }
        self.save_global();
        // Metal はモデルを持ったまま終わると abort するので、切り替え前の古いエンジンの分も含めて解放する。
        if !speechcpp::close_for_exit(ENGINE_CLOSE_WAIT) {
            // 読み込みや認識が返らない。保存は済んでいるので、ggml の後始末を飛ばして終わる。
            diag::log("Haas のモデルを解放できないまま終了する");
            unsafe { libc::_exit(0) };
        }
    }
}

/// 文の末尾 n 文字。
fn tail(text: &str, count: usize) -> String {
    let total = text.chars().count();
    text.chars().skip(total.saturating_sub(count)).collect()
}

pub fn recognize_options(data: &UserData) -> RecognizeOptions {
    let mut words = build_words(
        &data.dictionary,
        &data.settings.command_words,
        &data.settings.call_words,
    );
    if data.settings.learning.enabled {
        for word in data.memory.hotwords(HOTWORD_LIMIT) {
            if !words.contains(&word) {
                words.push(word);
            }
        }
    }
    // 命令語の名前と、読みのある登録単語は、Haas の keywords に「表記=読み」で渡す(同音の取り違えの手がかり)。
    let readings = CommandAction::ALL
        .iter()
        .map(|action| (action.label().to_string(), action.reading().to_string()))
        .chain(
            data.dictionary
                .words
                .iter()
                .filter(|word| !word.reading.trim().is_empty())
                .map(|word| (word.word.trim().to_string(), word.reading.trim().to_string())),
        )
        .collect();
    RecognizeOptions { words, readings }
}

/// 診断ログ用の辞書の大きさ。
fn dictionary_summary(dictionary: &Dictionary) -> String {
    format!(
        "単語 {}・記号 {}・テンプレート {}",
        dictionary.words.len(),
        dictionary.symbols.len(),
        dictionary.templates.len()
    )
}

fn new_session(
    recognizer: Option<Box<dyn Recognizer>>,
    options: SharedOptions,
    ctx: &egui::Context,
) -> Session {
    let repaint = ctx.clone();
    Session::new(
        recognizer,
        options,
        Arc::new(move || repaint.request_repaint()),
    )
}

/// 文末のコマンドを除いた部分について、あやしい語の位置を取り出す。
fn unsure_within(recognition: &Recognition, prefix: &str) -> Vec<(usize, usize)> {
    let Some(byte) = recognition.text.find(prefix) else {
        return Vec::new();
    };
    let offset = recognition.text[..byte].chars().count();
    let end = offset + prefix.chars().count();
    recognition
        .unsure
        .iter()
        .filter(|(start, stop)| *start >= offset && *stop <= end)
        .map(|(start, stop)| (start - offset, stop - offset))
        .collect()
}

/// 整形や学習の置き換えで文字がずれたあと、あやしい語の位置を探し直す。置き換えで消えた語は外れる。
fn relocate_unsure(recognition: &Recognition, filtered: &str) -> Vec<(usize, usize)> {
    if recognition.text == filtered {
        return recognition.unsure.clone();
    }
    let source: Vec<char> = recognition.text.chars().collect();
    let mut ranges = Vec::new();
    let mut from_byte = 0;
    for (start, end) in &recognition.unsure {
        let word: String = source[*start..(*end).min(source.len())].iter().collect();
        if let Some(found) = filtered[from_byte..].find(&word) {
            let byte = from_byte + found;
            let char_start = filtered[..byte].chars().count();
            ranges.push((char_start, char_start + word.chars().count()));
            from_byte = byte + word.len();
        }
    }
    ranges
}

/// 手直しの 1 件: 機械が置いた文、手直し後の文、入れたときに当てていた学習の組。
type ManualEdit = (String, String, Vec<(String, String)>);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unsure_ranges_follow_filtered_text() {
        let recognition = Recognition {
            text: "えーと、開業です".to_string(),
            unsure: vec![(4, 6)],
        };
        assert_eq!(relocate_unsure(&recognition, "開業です"), vec![(0, 2)]);
    }

    #[test]
    fn unsure_marks_survive_a_trailing_command() {
        let recognition = Recognition {
            text: "結節を認めます 改行".to_string(),
            unsure: vec![(0, 2), (8, 10)],
        };
        assert_eq!(unsure_within(&recognition, "結節を認めます"), vec![(0, 2)]);
    }

    #[test]
    fn missing_unsure_words_are_dropped() {
        let recognition = Recognition {
            text: "えーと".to_string(),
            unsure: vec![(0, 3)],
        };
        assert!(relocate_unsure(&recognition, "").is_empty());
    }

    #[test]
    fn learned_words_become_hotwords() {
        let mut data = UserData::default();
        data.memory.learn("右配上葉", "右肺上葉", true);
        let options = recognize_options(&data);
        assert!(options.words.iter().any(|w| w == "右肺上"));
    }

    #[test]
    fn command_words_become_hotwords() {
        let mut data = UserData::default();
        data.settings.command_words[0].words = vec!["次の行へ".to_string()];
        let options = recognize_options(&data);
        assert!(options.words.iter().any(|w| w == "次の行へ"));
    }

    #[test]
    fn template_keywords_become_hotwords() {
        let mut data = UserData::default();
        data.dictionary
            .save_template(None, "胸部正常、腹部正常", "異常なし。")
            .unwrap();
        let options = recognize_options(&data);
        assert!(options.words.iter().any(|w| w == "胸部正常"));
        assert!(options.words.iter().any(|w| w == "腹部正常"));
    }
}
