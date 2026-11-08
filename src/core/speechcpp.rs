//! Haas-ja v1.0 を speech.cpp(ggml)の C API で動かす認識エンジン(このアプリ独自)。
//!
//! 推論ライブラリ(`libspeech.4.dylib`)は実行時に読み込む(dlopen)。ビルド時にはリンクしないので、
//! ライブラリやモデルが無い環境でもアプリは動く。公開の型は不透明なので構造体の並びは写さない。
//! 関数の形は `vendor/speech-cpp/include/speech.h`(API 4.1)を写したもので、
//! ライブラリを更新するときは見比べる(`header_matches_what_this_file_assumes` が前提を確かめる)。
//!
//! モデルは 6 億パラメータで、句読点と算用数字を出す。英字は小文字で出るので大文字に揃える。
//! 登録単語は keywords(1 行 1 語)として 1 発話ごとに渡す。ただし英字を含む語は渡さない(`keyword_lines`)。
//! 字ごとの確率は得られないので、あやしい語の印は付かない。
//!
//! Metal は、モデルを持ったまま `exit()` に入ると abort する(本機で exit 134 を確認。`speech_model_free` の注記)。
//! そこで読み込んだモデルを `LOADED` に控え、`Drop`(エンジンの切り替え)と `close_for_exit`(アプリの終了処理)で解放する。
//! `close_for_exit` は読み込みや認識の途中なら終わるのを待つので、切り替えた直後の古いエンジンも守れる。
//! アプリの終了処理を通らない終わり方(`exit` の直接の呼び出し、主スレッドの panic)の受け皿に `atexit` も登録する。
//! 後から登録したものが先に走るので、ggml の後始末より先に走るよう、最初のモデルを読んだ後に登録する。
//! そのため最初の読み込みの間は効かず、その間にこの道で終わると abort する(`close_for_exit` を通る道は守れる)。
//!
//! 同じライブラリで Silero VAD(声の区間を見つけるモデル)も動かす(`VoiceDetector`)。発話の切り出し(`segmenter`)が
//! 20 ms ごとに「いまの音は声か」を聞く判定役で、CPU の 1 スレッドで動かす(1 回 0.04 ms ほど。Metal を使わないので
//! 終了時の abort も無い)。鍵は `DETECTOR` で `LOADED` と分け、Haas の読み込み(初回 2.8 秒)や認識の間も判定を止めない。

use std::ffi::{c_char, c_float, c_int, c_void, CStr, CString};
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard, Once, OnceLock, TryLockError};
use std::collections::HashMap;
use std::time::{Duration, Instant};

use libloading::Library;

use super::diag;
use super::dictionary::to_katakana;
use super::audio::read_wav;
use super::recognizer::{drop_hallucination, Recognition, RecognizeOptions, Recognizer};
use super::segmenter::{Verdict, VoiceJudge, SAMPLE_RATE};

/// 実行時に読む共有ライブラリ。名前の数字は C API の版(`API_MAJOR`)。
pub const LIBRARY_FILE: &str = "libspeech.4.dylib";
/// 1 発話に渡すキーワードの上限。
pub const MAX_KEYWORDS: usize = 200;
/// 認識のモデルのファイル名(置き場所と取りに行く先は `models.rs` と `assets/models.json`)。
pub const MODEL_FILE: &str = "Haas-616M-ja-v1.0-Q8_0.gguf";
/// モデルが無いときの案内。
pub const MODEL_MISSING: &str =
    "認識のモデルがありません。設定の「認識」で「モデルを取得」を押してください(約 0.66 GB)";
pub const ENGINE_NAME: &str = "Haas-ja v1.0 (speech.cpp)";
/// 声の判定(Silero VAD)のモデルのファイル名。
pub const VAD_FILE: &str = "silero-vad-309K-v6.2-F32.gguf";
/// 声の判定モデルが無いときの案内。無くても音量だけで切り出す。
pub const VAD_MISSING: &str = "声の判定モデルがありません(設定の「認識」の「モデルを取得」で一緒に取れます。あると物音を声と取り違えにくくなります)";
/// `speech.h` の `speech_task` のうち、声の区間を見つけるモデル(SPEECH_TASK_DETECTION)。
const TASK_DETECTION: c_int = 2;
/// 声と確かめるのに要る長さ(Silero の min_speech_duration_ms)。これより短い音は物音として捨てる。
/// Silero の既定は 250。短い命令語(「はい」約 300 ms)を落とさないよう少し短くした。
pub const MIN_SPEECH_MS: u32 = 200;
/// この版のライブラリの C API。major が違うと関数の形が違うかもしれないので読まない。
const API_MAJOR: c_int = 4;
const SPEECH_OK: c_int = 0;
/// 読み込みと同時に短い認識を 1 回行い、Metal の準備を最初の発話の前に済ませる
/// (本機で、無しなら初回 0.19 秒、有りなら初回から 0.016 秒)。
const LOAD_WARMUP: c_int = 1;
/// `atexit` で、認識の途中のスレッドを待つ上限。
const EXIT_WAIT: Duration = Duration::from_secs(3);
/// 数の直後なら小文字のままにする英字(単位)。
const UNITS: [&str; 22] = [
    "mm", "cm", "m", "km", "cc", "ml", "dl", "l", "ng", "mg", "g", "kg", "ug", "msv", "mmhg", "hz",
    "khz", "mhz", "ms", "s", "sec", "min",
];
/// 話していない文に入り込むので keywords に渡さない語(本機で測定。分野の語の中で目立って悪かったもの)。
/// 「単純」は語が無くても録音 13 本で出るのに、渡すと「かん…」で始まる合成音 72 本のうち 20 本に入り込んだ。
/// 「肝嚢胞」は利用者の口述「肝胆膵…」に入り込んだ(所見の無い文に所見が入る)。
const KEYWORD_EXCLUDED: [&str; 2] = ["単純", "肝嚢胞"];
/// モデルが受け取る最短の音(本機で「at least 320」と拒まれる)。発話の切り出しの最短は 1600 なので、ふつうは届かない。
const MIN_SAMPLES: usize = 320;

/// `speech.h` の関数。戻り値の `c_int` は `speech_status`。
struct Api {
    api_version_major: unsafe extern "C" fn() -> c_int,
    version: unsafe extern "C" fn() -> *const c_char,
    last_error: unsafe extern "C" fn() -> *const c_char,
    option_from_name: unsafe extern "C" fn(*const c_char, *mut c_int) -> c_int,
    load_params_new: unsafe extern "C" fn(*mut *mut c_void) -> c_int,
    load_params_free: unsafe extern "C" fn(*mut c_void),
    load_params_set_warmup: unsafe extern "C" fn(*mut c_void, c_int) -> c_int,
    load_params_set_device: unsafe extern "C" fn(*mut c_void, *const c_char) -> c_int,
    load_params_set_threads: unsafe extern "C" fn(*mut c_void, c_int) -> c_int,
    model_load: unsafe extern "C" fn(*const c_char, *const c_void, *mut *mut c_void) -> c_int,
    model_free: unsafe extern "C" fn(*mut c_void),
    model_get_info: unsafe extern "C" fn(*const c_void, *mut *mut c_void) -> c_int,
    model_info_device: unsafe extern "C" fn(*const c_void) -> *const c_char,
    model_info_task: unsafe extern "C" fn(*const c_void) -> c_int,
    model_info_free: unsafe extern "C" fn(*mut c_void),
    request_new: unsafe extern "C" fn(*mut c_void, *mut *mut c_void) -> c_int,
    request_free: unsafe extern "C" fn(*mut c_void),
    request_set_audio: unsafe extern "C" fn(*mut c_void, *const c_float, usize, c_int) -> c_int,
    request_set_string: unsafe extern "C" fn(*mut c_void, c_int, *const c_char) -> c_int,
    request_set_int: unsafe extern "C" fn(*mut c_void, c_int, i64) -> c_int,
    request_set_float: unsafe extern "C" fn(*mut c_void, c_int, f64) -> c_int,
    transcribe: unsafe extern "C" fn(*mut c_void) -> c_int,
    request_result: unsafe extern "C" fn(*const c_void) -> *const c_void,
    result_text: unsafe extern "C" fn(*const c_void) -> *const c_char,
    detection_start: unsafe extern "C" fn(*const c_void, c_int, *mut *mut c_void) -> c_int,
    detection_free: unsafe extern "C" fn(*mut c_void),
    detection_push: unsafe extern "C" fn(*mut c_void, *const c_float, usize) -> c_int,
    detection_region_count: unsafe extern "C" fn(*const c_void) -> usize,
    detection_region: unsafe extern "C" fn(*const c_void, usize, *mut f64, *mut f64) -> c_int,
    detection_speaking: unsafe extern "C" fn(*const c_void, *mut f64, *mut c_int) -> c_int,
    /// 上の関数の実体。プロセスの終わりまで閉じない(`atexit` で解放に使う)。
    _library: Library,
}

/// 読み込んだライブラリ。開くのは 1 回だけで、閉じない。
static API: OnceLock<Result<Api, String>> = OnceLock::new();
/// 読み込み済みのモデルの番地。読み込み・認識・解放はこの鍵を持って行うので、
/// 認識の途中でモデルを解放することは無い。`close_for_exit` は鍵を返さないので、
/// その後で別のスレッドがライブラリに触ることも無い(待ったままプロセスが終わる)。
static LOADED: Mutex<Vec<usize>> = Mutex::new(Vec::new());
static AT_EXIT: Once = Once::new();
/// 読み込んだ声の判定モデルの番地(0 は無し)。判定の 1 回ごとにこの鍵を持つので、終了処理が判定の途中で
/// モデルを解放することは無い。
static DETECTOR: Mutex<usize> = Mutex::new(0);
/// 生きている判定(録音ごとに 1 つ)の数。終了処理は 0 のときだけモデルを解放する
/// (CPU のモデルなので、残したまま終わっても abort しない)。
static DETECTIONS: AtomicUsize = AtomicUsize::new(0);
/// 声の判定モデルを読めなかった理由(設定画面の案内に使う)。読めたら None。
static VAD_FAILURE: Mutex<Option<String>> = Mutex::new(None);
/// `close_for_exit` が済んだ。
static CLOSED: AtomicBool = AtomicBool::new(false);
/// `exit` に渡した終了コード。`atexit` の受け皿(`close_at_exit`)は終了コードを受け取れないので、ここで控える。
static EXIT_CODE: AtomicI32 = AtomicI32::new(0);

fn loaded() -> MutexGuard<'static, Vec<usize>> {
    LOADED.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn detector() -> MutexGuard<'static, usize> {
    DETECTOR.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

unsafe fn symbol<T: Copy>(library: &Library, name: &str) -> Result<T, String> {
    library
        .get::<T>(format!("{name}\0").as_bytes())
        .map(|found| *found)
        .map_err(|e| format!("推論ライブラリに {name} がありません: {e}"))
}

fn open_api(path: &Path) -> Result<Api, String> {
    unsafe {
        let library = Library::new(path)
            .map_err(|e| format!("推論ライブラリを読み込めません({}): {e}", path.display()))?;
        let api = Api {
            api_version_major: symbol(&library, "speech_api_version_major")?,
            version: symbol(&library, "speech_version")?,
            last_error: symbol(&library, "speech_last_error")?,
            option_from_name: symbol(&library, "speech_option_from_name")?,
            load_params_new: symbol(&library, "speech_load_params_new")?,
            load_params_free: symbol(&library, "speech_load_params_free")?,
            load_params_set_warmup: symbol(&library, "speech_load_params_set_warmup")?,
            load_params_set_device: symbol(&library, "speech_load_params_set_device")?,
            load_params_set_threads: symbol(&library, "speech_load_params_set_threads")?,
            model_load: symbol(&library, "speech_model_load")?,
            model_free: symbol(&library, "speech_model_free")?,
            model_get_info: symbol(&library, "speech_model_get_info")?,
            model_info_device: symbol(&library, "speech_model_info_device")?,
            model_info_task: symbol(&library, "speech_model_info_task")?,
            model_info_free: symbol(&library, "speech_model_info_free")?,
            request_new: symbol(&library, "speech_request_new")?,
            request_free: symbol(&library, "speech_request_free")?,
            request_set_audio: symbol(&library, "speech_request_set_audio")?,
            request_set_string: symbol(&library, "speech_request_set_string")?,
            request_set_int: symbol(&library, "speech_request_set_int")?,
            request_set_float: symbol(&library, "speech_request_set_float")?,
            transcribe: symbol(&library, "speech_transcribe")?,
            request_result: symbol(&library, "speech_request_result")?,
            result_text: symbol(&library, "speech_result_text")?,
            detection_start: symbol(&library, "speech_detection_start")?,
            detection_free: symbol(&library, "speech_detection_free")?,
            detection_push: symbol(&library, "speech_detection_push")?,
            detection_region_count: symbol(&library, "speech_detection_region_count")?,
            detection_region: symbol(&library, "speech_detection_region")?,
            detection_speaking: symbol(&library, "speech_detection_speaking")?,
            _library: library,
        };
        let major = (api.api_version_major)();
        if major != API_MAJOR {
            return Err(format!(
                "推論ライブラリの版が合いません(API {major}、このアプリは {API_MAJOR})"
            ));
        }
        Ok(api)
    }
}

fn api(path: &Path) -> Result<&'static Api, String> {
    API.get_or_init(|| open_api(path))
        .as_ref()
        .map_err(Clone::clone)
}

/// 失敗した関数の理由を文にする。理由の文はこのスレッドの次の呼び出しまで有効なので、すぐ写す。
unsafe fn failure(api: &Api, status: c_int) -> String {
    let message = text_at((api.last_error)());
    if message.is_empty() {
        format!("speech.cpp の処理に失敗しました(状態 {status})")
    } else {
        message
    }
}

unsafe fn check(api: &Api, status: c_int) -> Result<(), String> {
    if status == SPEECH_OK {
        Ok(())
    } else {
        Err(failure(api, status))
    }
}

/// プロセスを終える前に、残っているモデルをすべて解放する。読み込みや認識の途中なら、最長 `wait` 待つ。
/// 解放したら鍵を持ったままにする(`LOADED` の説明)ので、これを呼んだ後はこのモジュールのエンジンは使えない。
/// 待ちきれなければ false で、このまま `exit` に入ると ggml の後始末が abort する。
pub fn close_for_exit(wait: Duration) -> bool {
    if CLOSED.load(Ordering::SeqCst) {
        return true;
    }
    let deadline = Instant::now() + wait;
    let mut models = loop {
        match LOADED.try_lock() {
            Ok(models) => break models,
            Err(TryLockError::Poisoned(poisoned)) => break poisoned.into_inner(),
            Err(TryLockError::WouldBlock) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(TryLockError::WouldBlock) => return false,
        }
    };
    // 声の判定は 1 回 0.04 ms ほどなので、鍵はすぐ空く。
    let mut detector = loop {
        match DETECTOR.try_lock() {
            Ok(detector) => break detector,
            Err(TryLockError::Poisoned(poisoned)) => break poisoned.into_inner(),
            Err(TryLockError::WouldBlock) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(TryLockError::WouldBlock) => return false,
        }
    };
    if let Some(Ok(api)) = API.get() {
        for model in models.drain(..) {
            unsafe { (api.model_free)(model as *mut c_void) };
        }
        // 判定が生きていれば(録音中の終了)モデルは残す。判定は CLOSED を見て触らない。
        if *detector != 0 && DETECTIONS.load(Ordering::SeqCst) == 0 {
            unsafe { (api.model_free)(*detector as *mut c_void) };
            *detector = 0;
        }
    }
    CLOSED.store(true, Ordering::SeqCst);
    std::mem::forget(models);
    drop(detector);
    true
}

/// `close_for_exit` が済んだか。
pub fn is_closed() -> bool {
    CLOSED.load(Ordering::SeqCst)
}

/// 主スレッドの panic を控える(`std` は 101 で終わるが、`atexit` の受け皿は控えた値で終わるため)。
/// 前に入っていた panic の表示はそのまま走らせる。
pub fn keep_exit_code_on_main_panic() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if std::thread::current().name() == Some("main") {
            EXIT_CODE.store(101, Ordering::SeqCst);
        }
        previous(info);
    }));
}

/// 終了コードを控えてから終わる。診断の引数の終了はすべてここを通す(`atexit` の受け皿が同じコードで終われるように)。
pub fn exit(code: i32) -> ! {
    EXIT_CODE.store(code, Ordering::SeqCst);
    std::process::exit(code)
}

/// アプリの終了処理を通らずに `exit` へ入ったときの受け皿。解放できなければ、ggml の後始末を飛ばして終わる。
/// そのときも `exit` に渡した終了コードで終わる(診断の失敗を成功にすり替えない)。
extern "C" fn close_at_exit() {
    if !close_for_exit(EXIT_WAIT) {
        unsafe { libc::_exit(EXIT_CODE.load(Ordering::SeqCst)) };
    }
}

// ---- モデルとライブラリの置き場所 ----

fn c_string(path: &Path) -> Result<CString, String> {
    CString::new(path.to_string_lossy().as_bytes())
        .map_err(|_| format!("使えないパスです: {}", path.display()))
}

/// C の文字列を写す。NULL なら空。
unsafe fn text_at(pointer: *const c_char) -> String {
    if pointer.is_null() {
        String::new()
    } else {
        CStr::from_ptr(pointer).to_string_lossy().into_owned()
    }
}

/// モデルのファイルを探す。
pub fn find_model() -> Option<PathBuf> {
    super::models::installed("haas-ja")
}

/// 声の判定モデル(Silero VAD)のファイルを探す。
pub fn find_vad_model() -> Option<PathBuf> {
    super::models::installed("silero-vad")
}

/// 開発時にライブラリを置いている場所(このプロジェクトの vendor)。
pub fn vendored_library_dir() -> Option<PathBuf> {
    super::mic_access::development_root().map(|root| root.join("vendor/speech-cpp/lib"))
}

/// 推論ライブラリの場所。アプリ束の Frameworks、次に開発時の vendor の順に探す。
pub fn find_library() -> Option<PathBuf> {
    let bundled = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join("../Frameworks")));
    bundled
        .into_iter()
        .chain(vendored_library_dir())
        .map(|dir| dir.join(LIBRARY_FILE))
        .find(|path| path.is_file())
}

/// keywords に渡せる語か。`=` は読みの区切りと取られ、行の始めにあると依頼ごと拒まれる。制御文字は行を割る。
/// 英字を含む語は渡さない。**検査の種別が別の種別に書き換わる**(本機の合成音 5 声で、`腹部MRI` を渡すと
/// 話した「腹部CT」が 3 声で「腹部MRI」に、`頭部MRI` で「頭部MRA」「頭部MRS」が「頭部MRI」になり、
/// `CT` で「CD20」が「CT」に、`MRI` で「MRS」が「MRI」になった)。略語は近い綴りが別の意味を持つ。
/// 大文字にするだけなら `uppercase_abbreviations` が字を変えずに行う。
fn usable_keyword(word: &str) -> bool {
    !word.is_empty()
        && !KEYWORD_EXCLUDED.contains(&word)
        && !word.contains('=')
        && !word.contains(char::is_control)
        && !word
            .chars()
            .any(|c| c.is_ascii_alphabetic() || matches!(c, 'Ａ'..='Ｚ' | 'ａ'..='ｚ'))
}

/// 登録単語を keywords の形(1 行 1 語、読みがあれば「表記=読み」)にする。
/// 読みは同音の取り違え(「主流」と「腫瘤」)の手がかりで、K0 の計測(2026-11-07)では表記だけより効いた。
pub fn keyword_lines<'a>(
    words: impl IntoIterator<Item = &'a str>,
    readings: &HashMap<String, String>,
) -> String {
    let mut out = String::new();
    for word in words
        .into_iter()
        .map(str::trim)
        .filter(|word| usable_keyword(word))
        .take(MAX_KEYWORDS)
    {
        out.push_str(word);
        if let Some(reading) = readings.get(word).map(|r| keyword_reading(r)) {
            if !reading.is_empty() {
                out.push('=');
                out.push_str(&reading);
            }
        }
        out.push('\n');
    }
    out
}

/// 読みを keywords の形にする: カタカナにし、変換止めの「.」・空白・区切りの「=」「,」を除く。
fn keyword_reading(reading: &str) -> String {
    to_katakana(reading)
        .chars()
        .filter(|c| !c.is_whitespace() && !matches!(c, '.' | '．' | '=' | ',' | '，' | '、'))
        .filter(|c| !c.is_control())
        .collect()
}

/// 単位の前に来る字か(5mm、五mm、数cm、10μg、mg/dl)。
fn comes_before_unit(c: char) -> bool {
    c.is_numeric() || "数一二三四五六七八九十百千μµ/".contains(c)
}

/// 小文字で出た英字を大文字に揃える。報告の英字はほとんどが略語(CT、MRA、T2、S8)で、字は変えないので別の語にならない。
/// 数の直後の単位(5mm、3 cm)はそのまま。ただし 1 字で後に数が続くなら略語(L4L5、N0M0)。
/// 大小が混ざる語(pH、Gd)はすべて大文字になる。
fn uppercase_abbreviations(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut index = 0;
    while index < chars.len() {
        if !chars[index].is_ascii_alphabetic() {
            out.push(chars[index]);
            index += 1;
            continue;
        }
        let length = chars[index..]
            .iter()
            .take_while(|c| c.is_ascii_alphabetic())
            .count();
        let word: String = chars[index..index + length].iter().collect();
        let unit_position = chars[..index]
            .iter()
            .rev()
            .find(|c| !c.is_whitespace())
            .is_some_and(|c| comes_before_unit(*c));
        let number_follows = chars[index + length..]
            .iter()
            .find(|c| !c.is_whitespace())
            .is_some_and(char::is_ascii_digit);
        let unit = unit_position
            && UNITS.contains(&word.to_lowercase().as_str())
            && !(length == 1 && number_follows);
        if unit {
            out.push_str(&word);
        } else {
            out.push_str(&word.to_uppercase());
        }
        index += length;
    }
    out
}

/// 認識の文を仕上げる。「えー」を「――」と書くことがあるので、文字が無ければ何も聞こえなかったことにする。
fn finish(text: &str) -> Recognition {
    let text = uppercase_abbreviations(text.trim());
    if !text.chars().any(char::is_alphanumeric) {
        return Recognition::default();
    }
    drop_hallucination(Recognition {
        text,
        unsure: Vec::new(),
    })
}

// ---- 認識エンジン ----

pub struct SpeechCpp {
    library_path: PathBuf,
    model_path: PathBuf,
    /// 読み込んだモデルの番地(`LOADED` の値)。0 はまだ読んでいない。
    model: usize,
    /// keywords の番号。読み込むときにライブラリに名前で聞く。
    keywords_option: c_int,
    /// keywords を拒まれたことを記録したか(記録を小さく保つ)。
    keywords_refusal_logged: bool,
}

impl SpeechCpp {
    pub fn new(library: &Path, model: &Path) -> Result<Self, String> {
        if !model.is_file() {
            return Err(format!("Haas のモデルが見つかりません: {}", model.display()));
        }
        Ok(SpeechCpp {
            library_path: library.to_path_buf(),
            model_path: model.to_path_buf(),
            model: 0,
            keywords_option: 0,
            keywords_refusal_logged: false,
        })
    }

    /// ライブラリとモデルを読む(初回だけ)。
    fn load(&mut self) -> Result<(), String> {
        if self.model != 0 {
            return Ok(());
        }
        let api = api(&self.library_path)?;
        let path = c_string(&self.model_path)?;
        let name = CString::new("keywords").expect("literal");
        let mut option = 0;
        let mut model: *mut c_void = ptr::null_mut();
        // 読み込みの途中で終了処理が走っても、終わるのを待ってから解放するよう、鍵を持ったまま読む。
        let mut models = loaded();
        unsafe {
            check(api, (api.option_from_name)(name.as_ptr(), &mut option))?;
            let mut params: *mut c_void = ptr::null_mut();
            check(api, (api.load_params_new)(&mut params))?;
            let mut status = (api.load_params_set_warmup)(params, LOAD_WARMUP);
            if status == SPEECH_OK {
                status = (api.model_load)(path.as_ptr(), params, &mut model);
            }
            let result = check(api, status);
            (api.load_params_free)(params);
            result.map_err(|message| {
                format!(
                    "Haas のモデルを読めません({}): {message}",
                    self.model_path.display()
                )
            })?;
        }
        models.push(model as usize);
        let (version, device) = unsafe { (text_at((api.version)()), device_name(api, model)) };
        drop(models);
        self.model = model as usize;
        self.keywords_option = option;
        // ggml の後始末は最初の読み込みで登録される。その後に登録すれば、こちらが先に走る。
        AT_EXIT.call_once(|| unsafe {
            libc::atexit(close_at_exit);
        });
        diag::log(format!(
            "speech.cpp {version} device={device} model={}",
            self.model_path
                .file_name()
                .map_or(String::new(), |name| name.to_string_lossy().into_owned())
        ));
        Ok(())
    }

    /// 1 回の認識。`keywords` が空でなければ語の手がかりとして渡す。
    fn transcribe(&self, samples: &[f32], keywords: &str) -> Result<String, String> {
        let api = api(&self.library_path)?;
        let keywords =
            CString::new(keywords).map_err(|_| "登録単語に使えない文字があります".to_string())?;
        let _models = loaded();
        unsafe {
            let mut request: *mut c_void = ptr::null_mut();
            check(
                api,
                (api.request_new)(self.model as *mut c_void, &mut request),
            )?;
            let result = (|| {
                check(
                    api,
                    (api.request_set_audio)(
                        request,
                        samples.as_ptr(),
                        samples.len(),
                        SAMPLE_RATE as c_int,
                    ),
                )?;
                if !keywords.as_bytes().is_empty() {
                    check(
                        api,
                        (api.request_set_string)(
                            request,
                            self.keywords_option,
                            keywords.as_ptr(),
                        ),
                    )?;
                }
                check(api, (api.transcribe)(request))?;
                let result = (api.request_result)(request);
                if result.is_null() {
                    return Err("認識結果を受け取れません".to_string());
                }
                Ok(text_at((api.result_text)(result)))
            })();
            (api.request_free)(request);
            result
        }
    }

    fn decode(
        &mut self,
        samples: &[f32],
        options: &RecognizeOptions,
    ) -> Result<Recognition, String> {
        self.load()?;
        if samples.len() < MIN_SAMPLES {
            return Ok(Recognition::default());
        }
        self.decode_with(
            samples,
            &keyword_lines(options.words.iter().map(String::as_str), &options.readings),
        )
    }

    fn decode_with(&mut self, samples: &[f32], keywords: &str) -> Result<Recognition, String> {
        let text = match self.transcribe(samples, keywords) {
            // 語の手がかりのために発話を失わない。拒まれたら語なしでもう一度認識する。
            Err(message) if !keywords.is_empty() => {
                if !self.keywords_refusal_logged {
                    self.keywords_refusal_logged = true;
                    diag::log(format!("keywords なしで認識し直す: {message}"));
                }
                self.transcribe(samples, "")
            }
            other => other,
        }?;
        Ok(finish(&text))
    }
}

/// モデルが動く装置の名前(MTL0 など)。
unsafe fn device_name(api: &Api, model: *mut c_void) -> String {
    let mut info: *mut c_void = ptr::null_mut();
    if (api.model_get_info)(model, &mut info) != SPEECH_OK || info.is_null() {
        return "不明".to_string();
    }
    let name = text_at((api.model_info_device)(info));
    (api.model_info_free)(info);
    name
}

impl Drop for SpeechCpp {
    fn drop(&mut self) {
        if self.model == 0 {
            return;
        }
        let mut models = loaded();
        models.retain(|model| *model != self.model);
        if let Some(Ok(api)) = API.get() {
            unsafe { (api.model_free)(self.model as *mut c_void) };
        }
        self.model = 0;
    }
}

impl Recognizer for SpeechCpp {
    fn name(&self) -> String {
        ENGINE_NAME.to_string()
    }

    fn prepare(&mut self) -> Result<(), String> {
        self.load()
    }

    fn recognize(
        &mut self,
        samples: &[f32],
        options: &RecognizeOptions,
    ) -> Result<Recognition, String> {
        self.decode(samples, options)
    }

    fn recognize_file(
        &mut self,
        path: &Path,
        options: &RecognizeOptions,
    ) -> Result<Recognition, String> {
        let samples = read_wav(path)?;
        self.decode(&samples, options)
    }

    /// 本機で 1 発話 0.02 秒ほどなので、途中結果にも使える。
    fn supports_partial(&self) -> bool {
        true
    }
}

// ---- 声の判定(Silero VAD) ----

/// 声の区間を見つけるモデル(Silero VAD)。プロセスで 1 つだけ読み(`DETECTOR`)、録音ごとに `start` で判定を作る。
/// CPU の 1 スレッドで動かす(20 ms ごとの判定が 1 回 0.04 ms ほど。公式の文書の勧め)。
pub struct VoiceDetector {
    library_path: PathBuf,
    model_path: PathBuf,
    min_speech_option: c_int,
    threshold_option: c_int,
}

/// 声の判定モデルを読めなかった理由。読めたか、まだ読んでいなければ None。
pub fn detector_failure() -> Option<String> {
    VAD_FAILURE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
}

impl VoiceDetector {
    /// ライブラリとモデルを読む。声の判定モデルでなければ拒む。失敗の理由は `detector_failure` で読める。
    pub fn load(library: &Path, model: &Path) -> Result<Self, String> {
        let result = Self::load_inner(library, model);
        let mut failure = VAD_FAILURE
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *failure = result.as_ref().err().cloned();
        result
    }

    fn load_inner(library: &Path, model: &Path) -> Result<Self, String> {
        if !model.is_file() {
            return Err(VAD_MISSING.to_string());
        }
        let api = api(library)?;
        let path = c_string(model)?;
        // 終了処理の後は LOADED の鍵が返らない(待ち続けないよう先に見る)。
        if CLOSED.load(Ordering::SeqCst) {
            return Err("終了処理の後なので声の判定モデルを読めません".to_string());
        }
        // 読み込みは Haas と同時に走らせない(鍵の順は `close_for_exit` と同じ LOADED → DETECTOR)。
        let models = loaded();
        let mut detector = detector();
        if CLOSED.load(Ordering::SeqCst) {
            return Err("終了処理の後なので声の判定モデルを読めません".to_string());
        }
        if *detector != 0 {
            return Err("声の判定モデルは読み込み済みです".to_string());
        }
        let (mut min_speech_option, mut threshold_option) = (0, 0);
        let mut loaded: *mut c_void = ptr::null_mut();
        unsafe {
            let name = CString::new("min_speech_duration_ms").expect("literal");
            check(api, (api.option_from_name)(name.as_ptr(), &mut min_speech_option))?;
            let name = CString::new("threshold").expect("literal");
            check(api, (api.option_from_name)(name.as_ptr(), &mut threshold_option))?;
            let device = CString::new("cpu").expect("literal");
            let mut params: *mut c_void = ptr::null_mut();
            check(api, (api.load_params_new)(&mut params))?;
            let mut status = (api.load_params_set_device)(params, device.as_ptr());
            if status == SPEECH_OK {
                status = (api.load_params_set_threads)(params, 1);
            }
            if status == SPEECH_OK {
                status = (api.load_params_set_warmup)(params, LOAD_WARMUP);
            }
            if status == SPEECH_OK {
                status = (api.model_load)(path.as_ptr(), params, &mut loaded);
            }
            let result = check(api, status);
            (api.load_params_free)(params);
            result.map_err(|message| {
                format!("声の判定モデルを読めません({}): {message}", model.display())
            })?;
            let task = model_task(api, loaded);
            if task != TASK_DETECTION {
                (api.model_free)(loaded);
                return Err(format!(
                    "声の判定モデルではありません({}、task={task})",
                    model.display()
                ));
            }
        }
        *detector = loaded as usize;
        let (version, device) = unsafe { (text_at((api.version)()), device_name(api, loaded)) };
        drop(detector);
        drop(models);
        // ggml の後始末は最初の読み込みで登録される。その後に登録すれば、こちらが先に走る(Haas と同じ)。
        AT_EXIT.call_once(|| unsafe {
            libc::atexit(close_at_exit);
        });
        diag::log(format!(
            "声の判定 speech.cpp {version} device={device} model={}",
            file_name(model)
        ));
        Ok(VoiceDetector {
            library_path: library.to_path_buf(),
            model_path: model.to_path_buf(),
            min_speech_option,
            threshold_option,
        })
    }

    pub fn model_name(&self) -> String {
        file_name(&self.model_path)
    }

    /// 1 回の録音の判定を作る。`threshold` は声とみなす確率(省くとモデルの既定 0.5)。
    pub fn start(&self, threshold: Option<f32>) -> Result<Detection, String> {
        let api = api(&self.library_path)?;
        let detector = detector();
        if CLOSED.load(Ordering::SeqCst) || *detector == 0 {
            return Err("声の判定モデルが解放されています".to_string());
        }
        let model = *detector as *mut c_void;
        unsafe {
            let mut request: *mut c_void = ptr::null_mut();
            check(api, (api.request_new)(model, &mut request))?;
            let result = (|| -> Result<usize, String> {
                check(
                    api,
                    (api.request_set_int)(
                        request,
                        self.min_speech_option,
                        i64::from(MIN_SPEECH_MS),
                    ),
                )?;
                if let Some(threshold) = threshold {
                    check(
                        api,
                        (api.request_set_float)(request, self.threshold_option, f64::from(threshold)),
                    )?;
                }
                let mut detection: *mut c_void = ptr::null_mut();
                check(
                    api,
                    (api.detection_start)(request, SAMPLE_RATE as c_int, &mut detection),
                )?;
                Ok(detection as usize)
            })();
            (api.request_free)(request);
            let detection = result?;
            DETECTIONS.fetch_add(1, Ordering::SeqCst);
            Ok(Detection {
                detection,
                pushed: 0,
                given: 0,
                failure_logged: false,
                name: format!("Silero VAD ({})", self.model_name()),
            })
        }
    }
}

/// モデルの仕事の種類(`speech_task`)。
unsafe fn model_task(api: &Api, model: *mut c_void) -> c_int {
    let mut info: *mut c_void = ptr::null_mut();
    if (api.model_get_info)(model, &mut info) != SPEECH_OK || info.is_null() {
        return -1;
    }
    let task = (api.model_info_task)(info);
    (api.model_info_free)(info);
    task
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map_or(String::new(), |name| name.to_string_lossy().into_owned())
}

/// 1 回の録音の声の判定。マイクの音を 20 ms ずつ聞かせ、声が続いているかを答える。1 つのスレッドで使う。
pub struct Detection {
    /// `speech_detection` の番地。
    detection: usize,
    /// 聞かせた音の長さ(サンプル)。
    pushed: usize,
    /// 受け取った声の区間の数。
    given: usize,
    failure_logged: bool,
    name: String,
}

impl VoiceJudge for Detection {
    fn hear(&mut self, frame: &[f32]) -> Verdict {
        let Some(Ok(api)) = API.get() else {
            return Verdict::UNJUDGED;
        };
        let _detector = detector();
        if CLOSED.load(Ordering::SeqCst) {
            return Verdict::UNJUDGED;
        }
        self.pushed += frame.len();
        let detection = self.detection as *mut c_void;
        unsafe {
            let status = (api.detection_push)(detection, frame.as_ptr(), frame.len());
            if status != SPEECH_OK {
                // 聞かせられなかった(終わった判定など)。音量だけの判定に戻す。
                if !self.failure_logged {
                    self.failure_logged = true;
                    diag::log(format!("声の判定に失敗: {}", failure(api, status)));
                }
                return Verdict::UNJUDGED;
            }
            let mut start = 0f64;
            let mut kept: c_int = 0;
            let speaking = (api.detection_speaking)(detection, &mut start, &mut kept) != 0;
            let count = (api.detection_region_count)(detection);
            let mut ended = None;
            if count > self.given {
                let (mut from, mut to) = (0f64, 0f64);
                if (api.detection_region)(detection, count - 1, &mut from, &mut to) == SPEECH_OK {
                    ended = Some(to);
                }
                self.given = count;
            }
            Verdict {
                speaking,
                kept: speaking && kept != 0,
                ended,
            }
        }
    }

    fn name(&self) -> String {
        self.name.clone()
    }
}

impl Drop for Detection {
    fn drop(&mut self) {
        let _detector = detector();
        DETECTIONS.fetch_sub(1, Ordering::SeqCst);
        if CLOSED.load(Ordering::SeqCst) {
            // モデルはもう無いかもしれない。プロセスが終わるので残す。
            return;
        }
        if let Some(Ok(api)) = API.get() {
            unsafe { (api.detection_free)(self.detection as *mut c_void) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use std::time::Instant;

    fn options(words: &[&str]) -> RecognizeOptions {
        RecognizeOptions {
            words: words.iter().map(|word| word.to_string()).collect(),
            readings: HashMap::new(),
        }
    }

    fn sample() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/sample-ja.wav")
    }

    /// 合成音を作る(macOS の say)。声が無ければ None。
    fn spoken(text: &str, voice: &str) -> Option<PathBuf> {
        let path = std::env::temp_dir().join(format!("haas-say-{}.wav", std::process::id()));
        Command::new("say")
            .args(["-v", voice, "-o"])
            .arg(&path)
            .args(["--data-format=LEI16@16000", "--channels=1", text])
            .status()
            .is_ok_and(|status| status.success())
            .then_some(path)
    }

    /// `Api` の関数の形を C の型で書いたもの。ヘッダーと違えばコンパイラーが拒む。
    const HEADER_CHECK: &str = r#"
#include "speech.h"
_Static_assert(sizeof(speech_status) == sizeof(int), "speech_status");
_Static_assert(sizeof(speech_option) == sizeof(int), "speech_option");
_Static_assert(sizeof(size_t) == sizeof(void *), "size_t");
_Static_assert(SPEECH_OK == 0, "SPEECH_OK");
_Static_assert(SPEECH_API_VERSION_MAJOR == 4, "API_MAJOR");
int (*a1)(void) = speech_api_version_major;
const char *(*a2)(void) = speech_version;
const char *(*a3)(void) = speech_last_error;
speech_status (*a4)(const char *, speech_option *) = speech_option_from_name;
speech_status (*a5)(speech_load_params **) = speech_load_params_new;
void (*a6)(speech_load_params *) = speech_load_params_free;
speech_status (*a7)(speech_load_params *, int) = speech_load_params_set_warmup;
speech_status (*a8)(const char *, const speech_load_params *, speech_model **) = speech_model_load;
void (*a9)(speech_model *) = speech_model_free;
speech_status (*a10)(const speech_model *, speech_model_info **) = speech_model_get_info;
const char *(*a11)(const speech_model_info *) = speech_model_info_device;
void (*a12)(speech_model_info *) = speech_model_info_free;
speech_status (*a13)(speech_model *, speech_request **) = speech_request_new;
void (*a14)(speech_request *) = speech_request_free;
speech_status (*a15)(speech_request *, const float *, size_t, int) = speech_request_set_audio;
speech_status (*a16)(speech_request *, speech_option, const char *) = speech_request_set_string;
speech_status (*a17)(speech_request *) = speech_transcribe;
const speech_result *(*a18)(const speech_request *) = speech_request_result;
const char *(*a19)(const speech_result *) = speech_result_text;
speech_status (*a20)(speech_load_params *, const char *) = speech_load_params_set_device;
speech_status (*a21)(speech_load_params *, int) = speech_load_params_set_threads;
speech_task (*a22)(const speech_model_info *) = speech_model_info_task;
speech_status (*a23)(speech_request *, speech_option, int64_t) = speech_request_set_int;
speech_status (*a24)(speech_request *, speech_option, double) = speech_request_set_float;
speech_status (*a25)(const speech_request *, int, speech_detection **) = speech_detection_start;
void (*a26)(speech_detection *) = speech_detection_free;
speech_status (*a27)(speech_detection *, const float *, size_t) = speech_detection_push;
size_t (*a28)(const speech_detection *) = speech_detection_region_count;
speech_status (*a29)(const speech_detection *, size_t, double *, double *) = speech_detection_region;
int (*a30)(const speech_detection *, double *, int *) = speech_detection_speaking;
_Static_assert(sizeof(speech_task) == sizeof(int), "speech_task");
_Static_assert(SPEECH_TASK_DETECTION == 2, "TASK_DETECTION");
"#;

    #[test]
    fn header_matches_what_this_file_assumes() {
        assert!(LIBRARY_FILE.contains(&format!(".{API_MAJOR}.")));
        assert_eq!(std::mem::size_of::<c_int>(), 4);
        let dir = std::env::temp_dir().join(format!("speech-header-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let source = dir.join("check.c");
        std::fs::write(&source, HEADER_CHECK).unwrap();
        let include = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("vendor/speech-cpp/include");
        let output = Command::new("cc")
            .args(["-fsyntax-only", "-Werror", "-I"])
            .arg(&include)
            .arg(&source)
            .output();
        let _ = std::fs::remove_dir_all(&dir);
        let Ok(output) = output else {
            skip_unless_ci("C コンパイラーが無い");
            return;
        };
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn keywords_are_one_a_line_without_what_the_library_refuses() {
        let none = HashMap::new();
        let lines = keyword_lines(
            [" 右肺上葉 ", "", "  ", "=読み", "語=読み", "改\n行", "腫瘤"],
            &none,
        );
        assert_eq!(lines, "右肺上葉\n腫瘤\n");
        assert_eq!(keyword_lines(["", " "], &none), "");
        assert_eq!(keyword_lines(["単純", "肝嚢胞", "胸水"], &none), "胸水\n");
    }

    #[test]
    fn keywords_carry_readings_as_katakana() {
        let readings: HashMap<String, String> = [
            ("腫瘤", "しゅりゅう"),
            ("肝胆膵脾腎副腎", "かんたん すいひ.じんふくじん"),
            ("胸水", ""),
            ("単純", "たんじゅん"),
        ]
        .into_iter()
        .map(|(w, r)| (w.to_string(), r.to_string()))
        .collect();
        let lines = keyword_lines(["腫瘤", "肝胆膵脾腎副腎", "胸水", "単純", "結節"], &readings);
        assert_eq!(
            lines,
            "腫瘤=シュリュウ\n肝胆膵脾腎副腎=カンタンスイヒジンフクジン\n胸水\n結節\n"
        );
    }

    #[test]
    fn words_with_latin_letters_never_become_keywords() {
        // 渡すと話した検査の種別が書き換わる(`usable_keyword` の説明)。
        let words = ["腹部MRI", "頭部MRI", "胸部CT", "CT", "MRI", "T2強調像", "ＣＴ", "ｍｒｉ"];
        let none = HashMap::new();
        assert_eq!(keyword_lines(words, &none), "");
        let built = crate::core::recognizer::build_words(&Default::default(), &[], &[]);
        let lines = keyword_lines(built.iter().map(String::as_str), &none);
        assert!(lines.contains("胸水\n"), "{lines}");
        assert!(!lines.contains("MRI") && !lines.contains("CT"), "{lines}");
    }

    #[test]
    fn abbreviations_are_uppercased_without_changing_a_letter() {
        for (written, expected) in [
            ("胸部単純ct。", "胸部単純CT。"),
            ("頭部mraを施行", "頭部MRAを施行"),
            ("頭部mrsと腹部mrcp", "頭部MRSと腹部MRCP"),
            ("cd20陽性", "CD20陽性"),
            ("t2強調像とdwi", "T2強調像とDWI"),
            ("肝s 8", "肝S 8"),
            ("t1wi", "T1WI"),
            ("1.5tの3dct", "1.5Tの3DCT"),
            ("5mmの結節", "5mmの結節"),
            ("3 cm大", "3 cm大"),
            ("5.5×4.5 cm", "5.5×4.5 cm"),
            ("10mgと20ml", "10mgと20ml"),
            ("100ccと10msv", "100ccと10msv"),
            ("10mg/dlと3ml/kg", "10mg/dlと3ml/kg"),
            ("数mmの結節", "数mmの結節"),
            ("五mmと５cm", "五mmと５cm"),
            ("10μg", "10μg"),
            ("N0m0とL4l5とL5s1", "N0M0とL4L5とL5S1"),
            ("s5s 8", "S5S 8"),
            ("結節を認めます", "結節を認めます"),
            ("CT", "CT"),
        ] {
            assert_eq!(uppercase_abbreviations(written), expected);
        }
    }

    #[test]
    fn a_text_without_letters_is_nothing_heard() {
        assert_eq!(finish(" ―― "), Recognition::default());
        assert_eq!(finish("。"), Recognition::default());
        assert_eq!(finish(""), Recognition::default());
        assert_eq!(finish("次回予告。"), Recognition::default());
        assert_eq!(finish(" 胸部単純ct。 ").text, "胸部単純CT。");
        assert_eq!(finish("5").text, "5");
    }

    #[test]
    fn the_number_of_keywords_is_limited() {
        let none = HashMap::new();
        let words: Vec<String> = (0..MAX_KEYWORDS + 5).map(|i| format!("語{i}")).collect();
        let lines = keyword_lines(words.iter().map(String::as_str), &none);
        assert_eq!(lines.lines().count(), MAX_KEYWORDS);
        // 渡せない語は枠を使わない。
        let mut words = vec!["CT"; MAX_KEYWORDS];
        words.push("胸水");
        assert_eq!(keyword_lines(words, &none), "胸水\n");
    }


    #[test]
    fn a_missing_model_is_refused_and_the_message_says_how_to_get_it() {
        assert!(SpeechCpp::new(Path::new("x"), Path::new("/無い/モデル.gguf")).is_err());
        assert!(MODEL_MISSING.contains("モデルを取得"));
        assert_eq!(super::super::models::find("haas-ja").unwrap().file, MODEL_FILE);
        assert_eq!(super::super::models::find("silero-vad").unwrap().file, VAD_FILE);
    }

    #[test]
    #[ignore = "モデルが要る"]
    fn recognizes_the_sample_when_the_model_is_installed() {
        let (Some(library), Some(model)) = (find_library(), find_model()) else {
            panic!("Haas のモデルかライブラリが無い");
        };
        let mut engine = SpeechCpp::new(&library, &model).unwrap();
        assert_eq!(engine.name(), ENGINE_NAME);
        assert!(engine.supports_partial());
        engine.prepare().unwrap();

        let plain = engine.recognize_file(&sample(), &options(&[])).unwrap();
        assert!(plain.text.contains("結節を認めます"), "{}", plain.text);
        assert!(plain.unsure.is_empty(), "字ごとの確率は無い");

        // 登録語が keywords として効く。
        let words = options(&["結節", "肺上葉", "前立腺"]);
        let began = Instant::now();
        let with_words = engine.recognize_file(&sample(), &words).unwrap();
        assert!(
            with_words.text.contains("右肺上葉に5ミリの結節"),
            "{}",
            with_words.text
        );
        assert!(
            began.elapsed() < Duration::from_secs(2),
            "遅すぎる: {:?}",
            began.elapsed()
        );

        // 英字を含む語を渡しても、話した検査の種別は変わらず、英字は大文字で出る(渡すと「腹部MRI」になる声)。
        if let Some(wav) = spoken("腹部CTを施行しました", "Eddy") {
            let text = engine.recognize_file(&wav, &options(&["腹部MRI"])).unwrap().text;
            let _ = std::fs::remove_file(&wav);
            assert!(text.contains("CT") && !text.contains("MRI"), "{text}");
        }

        // 拒まれる語があっても発話は失わない(`keyword_lines` を通らない値で確かめる)。
        let audio = read_wav(&sample()).unwrap();
        assert!(engine.transcribe(&audio, "=読み\n").is_err());
        let refused = engine.decode_with(&audio, "=読み\n").unwrap();
        assert!(refused.text.contains("結節を認めます"), "{}", refused.text);

        // 無音・短すぎる音・空の音で落ちない。
        let silence = vec![0.0f32; SAMPLE_RATE];
        let nothing = engine.recognize(&silence, &words).unwrap();
        // 無音や雑音からは短い語が出ることがある(本機で「うん。」「こんにちは。」)。
        assert!(
            nothing.text.chars().count() <= 6,
            "無音から長い文を作らない: {}",
            nothing.text
        );
        assert_eq!(engine.recognize(&[0.1; 1], &words).unwrap(), Recognition::default());
        assert_eq!(engine.recognize(&[], &words).unwrap(), Recognition::default());
        engine.recognize(&vec![0.01; MIN_SAMPLES], &words).unwrap();
    }

    /// 子プロセスの役。値はどう終わるか。
    const EXIT_CHILD: &str = "OPENSPEECH_EXIT_CHILD";
    /// 子が最後まで走ったことの印。試験の名前がずれると子は何もせず exit 0 になり、
    /// 子の assert が失敗しても `close_at_exit` の `_exit(0)` が終了コードを潰すことがある。
    const EXIT_CHILD_DONE: &str = "exit-child-done";

    fn finish_child() -> ! {
        println!("{EXIT_CHILD_DONE}");
        std::process::exit(0);
    }

    /// 環境に無いものがあれば飛ばす。CI(`CI` が立っている)では用意してあるはずなので失敗にする。
    fn skip_unless_ci(what: &str) {
        assert!(std::env::var_os("CI").is_none(), "{what}(CI では用意しておくこと)");
        eprintln!("{what}ので飛ばす");
    }

    fn exit_status(mode: &str) -> Option<i32> {
        let output = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "--include-ignored",
                "--nocapture",
                "core::speechcpp::tests::a_loaded_model_never_aborts_the_exit",
            ])
            .env(EXIT_CHILD, mode)
            .output()
            .unwrap();
        assert!(
            String::from_utf8_lossy(&output.stdout).contains(EXIT_CHILD_DONE),
            "{mode}: 子が最後まで走っていない"
        );
        output.status.code()
    }

    /// 認識を続けるスレッドを動かす。アプリと同じく、発話の間には鍵が空く。
    fn keep_recognizing(mut engine: SpeechCpp) {
        std::thread::spawn(move || {
            let audio = vec![0.01f32; SAMPLE_RATE * 2];
            while engine.recognize(&audio, &options(&["結節"])).is_ok() {
                std::thread::sleep(Duration::from_millis(30));
            }
        });
    }

    #[test]
    #[ignore = "モデルが要る"]
    fn a_loaded_model_never_aborts_the_exit() {
        let (Some(library), Some(model)) = (find_library(), find_model()) else {
            panic!("Haas のモデルかライブラリが無い");
        };
        let Ok(mode) = std::env::var(EXIT_CHILD) else {
            for mode in ["idle", "closed", "busy", "busy-atexit", "stuck-atexit"] {
                assert_eq!(exit_status(mode), Some(0), "{mode}: abort なら None");
            }
            // 最初の読み込みの途中で終わる(`atexit` はまだ登録されていない)。
            for delay in [0, 20, 60, 100, 140, 180] {
                let mode = format!("loading-{delay}");
                assert_eq!(exit_status(&mode), Some(0), "{mode}: abort なら None");
            }
            // 診断が失敗のコードで終わるとき、解放できなくても同じコードで終わる(0 にすり替えない)。
            assert_eq!(exit_status("stuck-atexit-code"), Some(3));
            return;
        };
        let mut engine = SpeechCpp::new(&library, &model).unwrap();
        if let Some(delay) = mode.strip_prefix("loading-") {
            std::thread::spawn(move || {
                let _ = engine.prepare();
                std::mem::forget(engine);
            });
            std::thread::sleep(Duration::from_millis(delay.parse().unwrap()));
            assert!(close_for_exit(Duration::from_secs(30)));
            finish_child();
        }
        engine.prepare().unwrap();
        match mode.as_str() {
            // アプリの終了処理を通らない。`atexit` が解放する。
            "idle" => std::mem::forget(engine),
            // 終了処理。鍵は返さず、二度目(`atexit`)は待たない。
            "closed" => {
                std::mem::forget(engine);
                assert!(close_for_exit(Duration::ZERO));
                assert!(is_closed());
                assert!(LOADED.try_lock().is_err(), "解放後のモデルに触らせない");
                let began = Instant::now();
                assert!(close_for_exit(Duration::from_secs(1)));
                assert!(began.elapsed() < Duration::from_millis(100));
            }
            // 認識の途中なら、鍵が空くのを待って解放する。
            "busy" => {
                keep_recognizing(engine);
                std::thread::sleep(Duration::from_millis(100));
                assert!(close_for_exit(Duration::from_secs(2)));
            }
            "busy-atexit" => {
                keep_recognizing(engine);
                std::thread::sleep(Duration::from_millis(100));
            }
            // 認識が返らない。解放できないので、ggml の後始末を飛ばして終わる。
            "stuck-atexit" => {
                std::mem::forget(engine);
                std::thread::spawn(|| {
                    let _models = loaded();
                    std::thread::sleep(Duration::from_secs(60));
                });
                std::thread::sleep(Duration::from_millis(100));
            }
            "stuck-atexit-code" => {
                std::mem::forget(engine);
                std::thread::spawn(|| {
                    let _models = loaded();
                    std::thread::sleep(Duration::from_secs(60));
                });
                std::thread::sleep(Duration::from_millis(100));
                println!("{EXIT_CHILD_DONE}");
                super::exit(3);
            }
            other => panic!("{other}"),
        }
        finish_child();
    }

    /// 声の判定モデルがあれば、見本の音で声の区間を見つけ、無音では見つけないこと。1 フレームの判定が速いこと。
    #[test]
    fn the_voice_detector_hears_the_sample_and_not_silence_when_installed() {
        let (Some(library), Some(model)) = (find_library(), find_vad_model()) else {
            skip_unless_ci("声の判定モデルが無い");
            return;
        };
        let detector = VoiceDetector::load(&library, &model).unwrap();
        let samples = read_wav(&sample()).unwrap();
        let mut detection = detector.start(None).unwrap();
        assert!(detection.name().contains("Silero"));
        let began = Instant::now();
        let mut frames = 0usize;
        let mut kept = false;
        let mut ended = None;
        let frame = SAMPLE_RATE / 50;
        for chunk in samples.chunks(frame).chain(std::iter::repeat_n(&[0.0f32; 320][..], 50)) {
            let verdict = detection.hear(chunk);
            frames += 1;
            kept |= verdict.kept;
            if verdict.ended.is_some() {
                ended = verdict.ended;
            }
        }
        let per_frame = began.elapsed().as_secs_f64() / frames as f64;
        assert!(kept, "見本の声を声と確かめていない");
        let ended = ended.expect("声の区間が確定していない");
        let length = samples.len() as f64 / SAMPLE_RATE as f64;
        assert!(ended > 0.5 && ended <= length + 0.2, "終わり {ended} 秒(音は {length} 秒)");
        assert!(per_frame < 0.002, "1 フレーム {per_frame} 秒");
        let mut quiet = detector.start(None).unwrap();
        let mut any = false;
        for _ in 0..100 {
            let verdict = quiet.hear(&[0.0; 320]);
            any |= verdict.kept || verdict.ended.is_some();
        }
        assert!(!any, "無音を声と言った");
    }
}
