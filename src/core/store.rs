//! 保存先の管理。JSON へ、途中で落ちても壊れないよう一時ファイル経由で書く。
//! 置き場所は `~/Library/Application Support/OpenSpeech`(試験と画面の撮影では `OPENSPEECH_HOME`)。

use super::diag;
use super::dictionary::Dictionary;
use super::settings::GlobalSettings;
use super::user_data::UserData;
use chrono::Local;
use serde::de::DeserializeOwned;
use serde::Serialize;
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

/// 自動バックアップを保つ世代数。
pub const BACKUP_GENERATIONS: usize = 5;
pub const DATA_DIR_NAME: &str = "OpenSpeech";
/// 利用者が手で編集できる辞書(単語・記号の読み・テンプレート)。
pub const DICTIONARY_FILE: &str = "dictionary.json";
pub const HOME_ENV: &str = "OPENSPEECH_HOME";
/// AI への指示の雛形。「訂正の指示を編集…」を初めて押したときに書く。中身は訂正役の既定の指示そのもの。
pub const GUIDANCE_TEMPLATE: &str = crate::core::corrector::DEFAULT_GUIDANCE;
/// 2026-11-08 より前の雛形の前書き(規則はアプリが固定、と書いてあった 2 行)。
const OLD_GUIDANCE_PREAMBLE: &str = "\
ここに書いたことは、認識した文を直すときの手がかりとして AI に渡されます。
規則(内容を変えない・左右を変えない・出力は直した文だけ)はアプリが固定していて、ここでは変えられません。
";
/// 2026-11-08 から 1.1.0 までの雛形の前書き(素の 3 行。AI にそのまま送られていた)。
const PLAIN_GUIDANCE_PREAMBLE: &str = "\
ここに書いたことは、認識した文を直すときの指示として AI にそのまま渡されます。自由に書き換えられます。
空にするとアプリの既定(初めに書かれていたこの内容)が使われます。
アプリが決めているのは、直した文だけを返して説明を書かないという出力の形と、左右が変わった答えを使わない守りだけです。
";

/// 利用者が手で書き換える指示のファイル。無ければ空として読み、「…の指示を編集…」で初めて開くときに雛形を書く。
#[derive(Debug, Clone, Copy)]
pub struct InstructionFile {
    /// データのフォルダの中のファイル名。
    pub name: &'static str,
    /// 初めて作るときに書く中身。
    pub template: &'static str,
    /// 知らせと診断ログに出す名前。
    pub label: &'static str,
}

/// 訂正の指示(訂正役の直し方)。
pub const GUIDANCE_FILE: InstructionFile = InstructionFile {
    name: "guidance.md",
    template: GUIDANCE_TEMPLATE,
    label: "補足",
};
/// 依頼の指示(「ヘイクロード」の補完役の答え方)。
pub const REQUEST_FILE: InstructionFile = InstructionFile {
    name: "request.md",
    template: crate::core::assistant::DEFAULT_REQUEST_GUIDANCE,
    label: "依頼の指示",
};

/// データの置き場所。`OPENSPEECH_HOME` があればそこ(試験と画面の撮影)、無ければ
/// `~/Library/Application Support/OpenSpeech`。
pub fn data_root() -> PathBuf {
    if let Some(root) = std::env::var_os(HOME_ENV).map(PathBuf::from) {
        return root;
    }
    let data_dir = dirs::data_dir().unwrap_or_else(|| PathBuf::from("."));
    data_dir.join(DATA_DIR_NAME)
}

#[derive(Debug, Clone)]
pub struct Store {
    pub root: PathBuf,
    /// 読み込みで起きた問題。画面に知らせるためにためておく。
    warnings: Arc<Mutex<Vec<String>>>,
}

impl Store {
    pub fn open() -> Self {
        Store::at(&Self::default_root())
    }

    fn default_root() -> PathBuf {
        data_root()
    }

    pub fn at(root: &Path) -> Self {
        Store {
            root: root.to_path_buf(),
            warnings: Arc::default(),
        }
    }

    fn warn(&self, message: String) {
        eprintln!("{message}");
        if let Ok(mut warnings) = self.warnings.lock() {
            warnings.push(message);
        }
    }

    /// たまっている問題の知らせを取り出す。
    pub fn take_warnings(&self) -> Vec<String> {
        self.warnings
            .lock()
            .map_or_else(|_| Vec::new(), |mut warnings| std::mem::take(&mut *warnings))
    }

    fn global_path(&self) -> PathBuf {
        self.root.join("global.json")
    }

    fn user_path(&self) -> PathBuf {
        self.root.join("user.json")
    }

    fn api_key_path(&self) -> PathBuf {
        self.root.join("claude_api_key")
    }

    /// 訂正役(Claude)の API キー。設定 JSON には入れず、所有者だけが読めるファイルに置く。
    pub fn load_api_key(&self) -> Option<String> {
        read_key(&self.api_key_path())
    }

    /// 空なら削除する。
    pub fn save_api_key(&self, key: &str) -> Result<(), String> {
        write_key(&self.root, &self.api_key_path(), key)
    }

    /// 外部の音声認識 API の鍵(`<provider>_api_key`)。置き場と権限は Claude の鍵と同じ。
    /// 手で書き換えて使えない文字(改行など)が入った鍵は使わない(curl の設定に別の行を足させない)。
    pub fn load_remote_key(&self, provider: &str) -> Option<String> {
        if !crate::core::remote_asr::is_known_provider(provider) {
            return None;
        }
        let key = read_key(&self.root.join(crate::core::remote_asr::key_file_name(provider)))?;
        if !crate::core::claude_api::usable_key(&key) {
            crate::core::diag::log("外部 API の鍵に使えない文字があるので使わない");
            return None;
        }
        Some(key)
    }

    /// 空なら削除する。知らない provider の鍵は書かない。
    pub fn save_remote_key(&self, provider: &str, key: &str) -> Result<(), String> {
        if !crate::core::remote_asr::is_known_provider(provider) {
            return Err(format!("知らない外部 API です: {provider}"));
        }
        let path = self.root.join(crate::core::remote_asr::key_file_name(provider));
        write_key(&self.root, &path, key)
    }

    fn instruction_path(&self, file: InstructionFile) -> PathBuf {
        self.root.join(file.name)
    }

    /// 指示のファイルを読む。無ければ空。別の文字コードで保存されたものは読めない(Err)。
    /// 先頭の BOM は見えない字として残さず、NUL は Claude Code の引数に渡せないので除く。
    pub fn read_instructions(&self, file: InstructionFile) -> std::io::Result<String> {
        match std::fs::read_to_string(self.instruction_path(file)) {
            Ok(text) => Ok(text.trim_start_matches('\u{feff}').replace('\0', "")),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(String::new()),
            Err(error) => Err(error),
        }
    }

    /// 指示のファイルの中身。読めなければ診断ログに残し、空(アプリの既定)で続ける。
    pub fn load_instructions(&self, file: InstructionFile) -> String {
        self.read_instructions(file).unwrap_or_else(|error| {
            diag::log(format!("{}を読めません: {error}", file.label));
            String::new()
        })
    }

    /// 指示のファイルの場所を返す。無ければ雛形を書き、あれば触らない。
    pub fn ensure_instructions(&self, file: InstructionFile) -> Result<PathBuf, String> {
        std::fs::create_dir_all(&self.root).map_err(|e| format!("保存先を作れません: {e}"))?;
        let path = self.instruction_path(file);
        let mut options = std::fs::OpenOptions::new();
        match options.write(true).create_new(true).open(&path) {
            Ok(mut file_handle) => {
                if let Err(error) = file_handle.write_all(file.template.as_bytes()) {
                    // 途中のファイルを残すと、次に押したとき雛形が入らないまま開く。
                    let _ = std::fs::remove_file(&path);
                    return Err(format!("{}のファイルを作れません: {error}", file.label));
                }
            }
            Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
            Err(error) => return Err(format!("{}のファイルを作れません: {error}", file.label)),
        }
        Ok(path)
    }

    /// 指示のファイルの更新時刻。無ければ None。
    pub fn instructions_modified(&self, file: InstructionFile) -> Option<SystemTime> {
        std::fs::metadata(self.instruction_path(file))
            .and_then(|meta| meta.modified())
            .ok()
    }

    /// 訂正の指示(`guidance.md`)を読む。
    pub fn read_guidance(&self) -> std::io::Result<String> {
        self.read_instructions(GUIDANCE_FILE)
    }

    /// 利用者が書く AI への補足。読めなければ診断ログに残し、補足なしで続ける。
    pub fn load_guidance(&self) -> String {
        self.load_instructions(GUIDANCE_FILE)
    }

    /// 補足のファイルの場所を返す。無ければ雛形を書き、あれば触らない。
    pub fn ensure_guidance(&self) -> Result<PathBuf, String> {
        self.ensure_instructions(GUIDANCE_FILE)
    }

    /// 補足のファイルの更新時刻。無ければ None。
    pub fn guidance_modified(&self) -> Option<SystemTime> {
        self.instructions_modified(GUIDANCE_FILE)
    }

    /// 依頼の指示(`request.md`)を読む。
    pub fn read_request_guidance(&self) -> std::io::Result<String> {
        self.read_instructions(REQUEST_FILE)
    }

    /// 依頼の指示。読めなければ診断ログに残し、アプリの既定で続ける。
    pub fn load_request_guidance(&self) -> String {
        self.load_instructions(REQUEST_FILE)
    }

    /// 依頼の指示のファイルの場所を返す。無ければ雛形を書き、あれば触らない。
    pub fn ensure_request_guidance(&self) -> Result<PathBuf, String> {
        self.ensure_instructions(REQUEST_FILE)
    }

    /// 依頼の指示のファイルの更新時刻。無ければ None。
    pub fn request_guidance_modified(&self) -> Option<SystemTime> {
        self.instructions_modified(REQUEST_FILE)
    }

    /// `guidance.md` の引き継ぎ。どちらも 1 回だけ起き(差し替えたあとは同じ前書きで始まらない)、同じ起動で続けて
    /// 起きてよい。何かをしたときは、その知らせ(2 つなら改行でつなぐ)を返す。
    /// 1. 規則をアプリの固定から指示のファイルへ移した(2026-11-08)とき: 古い前書きで始まるファイルは、前書きと
    ///    規則の節を今の雛形のものに差し替える(「# 分野」から後ろは利用者の文なので触らない)。
    /// 2. 前書きを AI に送らない注釈にした(1.1.0 の後)とき: 素の 3 行の前書きで始まるファイルは、その 3 行を
    ///    今の雛形の注釈に差し替える(規則の節は利用者の文なので触らない)。
    ///
    /// どちらも控えを `backup/guidance-<時刻>.md` に取ってから書く。
    pub fn migrate_guidance(&self) -> Option<String> {
        let notices: Vec<String> = [self.migrate_old_preamble(), self.migrate_plain_preamble()]
            .into_iter()
            .flatten()
            .collect();
        (!notices.is_empty()).then(|| notices.join("\n"))
    }

    fn migrate_old_preamble(&self) -> Option<String> {
        let original = self.read_guidance().ok()?;
        // Windows 風の改行で保存されていても前書きと見比べられるようにする(書き戻しは LF)。
        let text = original.replace("\r\n", "\n");
        let rest = text.strip_prefix(OLD_GUIDANCE_PREAMBLE)?;
        let head = GUIDANCE_TEMPLATE.split("# 分野").next()?;
        let merged = format!("{head}{}", rest.trim_start_matches('\n'));
        Some(self.rewrite_guidance(
            &original,
            &merged,
            "AI への指示(guidance.md)に、これまでアプリが固定していた規則を書き込みました。書き換えられます。前の内容の控え: ",
            "AI への指示(guidance.md)に規則を書き込めません: ",
        ))
    }

    fn migrate_plain_preamble(&self) -> Option<String> {
        let original = self.read_guidance().ok()?;
        let text = original.replace("\r\n", "\n");
        let rest = text.strip_prefix(PLAIN_GUIDANCE_PREAMBLE)?;
        let merged = format!("{}{rest}", guidance_comment_preamble());
        Some(self.rewrite_guidance(
            &original,
            &merged,
            "AI への指示(guidance.md)の先頭の説明を、AI に送られない注釈に直しました。控え: ",
            "AI への指示(guidance.md)の先頭の説明を直せません: ",
        ))
    }

    /// 控えを取ってから `guidance.md` を書き換え、知らせ(`done` か `failed` に控えの場所か理由を続けた文)を返す。
    fn rewrite_guidance(&self, original: &str, merged: &str, done: &str, failed: &str) -> String {
        let stamp = Local::now().format("%Y%m%d_%H%M%S");
        let folder = self.root.join("backup");
        let mut copy = folder.join(format!("guidance-{stamp}.md"));
        // 同じ起動で 2 つの引き継ぎが同じ秒に起きても、前の控えを上書きしない。
        let mut serial = 2;
        while copy.exists() {
            copy = folder.join(format!("guidance-{stamp}-{serial}.md"));
            serial += 1;
        }
        let result = std::fs::create_dir_all(&folder)
            .and_then(|()| std::fs::write(&copy, original))
            .and_then(|()| {
                std::fs::write(self.instruction_path(GUIDANCE_FILE), merged).inspect_err(|_| {
                    // 本体を書けなければ控えも要らない(起動のたびに控えが増えないように)。
                    let _ = std::fs::remove_file(&copy);
                })
            });
        let notice = match result {
            Ok(()) => format!("{done}{}", copy.display()),
            Err(error) => format!("{failed}{error}"),
        };
        // 利用者のファイルを書き換えたので、起動時の知らせにも出す。
        self.warn(notice.clone());
        notice
    }

    /// 転送履歴の日別ファイルの置き場。
    pub fn history_dir(&self) -> PathBuf {
        self.root.join("history")
    }

    pub fn voice_log_dir(&self, global: &GlobalSettings) -> PathBuf {
        global
            .voice_log
            .folder
            .clone()
            .unwrap_or_else(|| self.root.join("VoiceLog"))
    }

    pub fn load_global(&self) -> GlobalSettings {
        self.read_or_default(&self.global_path())
    }

    pub fn save_global(&self, global: &GlobalSettings) -> Result<(), String> {
        write_json(&self.global_path(), global)
    }

    /// 設定・学習を user.json から、辞書を辞書ファイルから読む。辞書ファイルが無ければ
    /// user.json にあった辞書(2026-11-07 より前の保存)か既定の辞書で作る。
    pub fn load_user(&self) -> UserData {
        let mut data: UserData = self.read_or_default(&self.user_path());
        match self.read_dictionary() {
            Ok(Some(dictionary)) => data.dictionary = dictionary,
            Ok(None) => {
                if let Err(message) = self.save_dictionary(&data.dictionary) {
                    self.warn(format!("辞書ファイルを作れません: {message}"));
                }
            }
            Err(reason) => {
                // 読めないファイル(編集の途中など)は触らず、今の辞書で続ける。直せば見張り(`watch_files`)が読む。
                // user.json のようによけて作り直すと、利用者が開いている編集中のファイルが消える。
                self.warn(format!(
                    "辞書ファイル({DICTIONARY_FILE})を読めません({reason})。直すとアプリを開いたまま反映されます"
                ));
            }
        }
        data
    }

    /// 設定と学習を保存する(辞書は `save_dictionary`)。
    /// 辞書ファイルを作れていない間だけ、辞書も user.json に残す(古い形。次の起動の引き継ぎで戻る)。
    pub fn save_user(&self, data: &UserData) -> Result<(), String> {
        if self.dictionary_path().is_file() {
            return write_json(&self.user_path(), data);
        }
        let mut value = serde_json::to_value(data).map_err(|e| e.to_string())?;
        value["dictionary"] = serde_json::to_value(&data.dictionary).map_err(|e| e.to_string())?;
        write_json(&self.user_path(), &value)
    }

    /// 辞書ファイルを日時付きの名前(`dictionary.conflict-<日時>.json`)に変えて残す。
    /// 画面の辞書で書き直す前に、外で編集された中身を失わないため。
    pub fn set_dictionary_aside(&self) -> Result<PathBuf, String> {
        let stamp = Local::now().format("%Y%m%d_%H%M%S");
        let keep = self
            .dictionary_path()
            .with_extension(format!("conflict-{stamp}.json"));
        std::fs::rename(self.dictionary_path(), &keep)
            .map(|()| keep)
            .map_err(|e| format!("辞書ファイルをよけられません: {e}"))
    }

    pub fn dictionary_path(&self) -> PathBuf {
        self.root.join(DICTIONARY_FILE)
    }

    /// 辞書ファイルを読む。無ければ None、JSON として読めなければ Err(理由)。
    pub fn read_dictionary(&self) -> Result<Option<Dictionary>, String> {
        match std::fs::read(self.dictionary_path()) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map(Some)
                .map_err(|error| error.to_string()),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.to_string()),
        }
    }

    pub fn save_dictionary(&self, dictionary: &Dictionary) -> Result<(), String> {
        write_json(&self.dictionary_path(), dictionary)
    }

    /// 辞書ファイルの更新時刻。無ければ None。
    pub fn dictionary_modified(&self) -> Option<SystemTime> {
        std::fs::metadata(self.dictionary_path())
            .and_then(|meta| meta.modified())
            .ok()
    }

    /// 利用者のデータ(設定・学習・辞書)を日時の付いたフォルダへ控える。
    pub fn backup_user(&self, data: &UserData) -> Result<(), String> {
        let folder = self.root.join("backup").join("auto");
        let stamp = Local::now().format("%Y%m%d_%H%M%S").to_string();
        write_json(&folder.join(&stamp).join("user.json"), data)?;
        write_json(&folder.join(&stamp).join(DICTIONARY_FILE), &data.dictionary)?;
        let mut existing: Vec<PathBuf> = std::fs::read_dir(&folder)
            .map_err(|e| e.to_string())?
            .flatten()
            .map(|e| e.path())
            .collect();
        existing.sort();
        let excess = existing.len().saturating_sub(BACKUP_GENERATIONS);
        for old in existing.into_iter().take(excess) {
            let _ = std::fs::remove_dir_all(old);
        }
        Ok(())
    }

    /// JSON を読む。無ければ既定値。読めないときは、次の保存で失わないようファイルをよけて既定値を返す。
    fn read_or_default<T: DeserializeOwned + Default>(&self, path: &Path) -> T {
        let parsed = match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(|error| error.to_string()),
            Err(error) if error.kind() == ErrorKind::NotFound => return T::default(),
            Err(error) => Err(error.to_string()),
        };
        parsed.unwrap_or_else(|reason| {
            self.set_aside(path, &reason);
            T::default()
        })
    }

    /// 読めないファイルを別の名前に変える(読む権限が無くても名前は変えられる)。
    fn set_aside(&self, path: &Path, reason: &str) {
        let stamp = Local::now().format("%Y%m%d_%H%M%S");
        let keep = path.with_extension(format!("unreadable-{stamp}.json"));
        let name = path.file_name().map_or_else(String::new, |n| n.to_string_lossy().to_string());
        self.warn(match std::fs::rename(path, &keep) {
            Ok(()) => format!(
                "{name} を読めなかったので既定の設定で開きました({reason})。元のファイルは {} に残しています",
                keep.display()
            ),
            Err(error) => format!("{name} を読めず、よけることもできませんでした({reason} / {error})"),
        });
    }
}

/// 今の雛形の先頭の注釈(`<!--` の行から `-->` だけの行まで)。
fn guidance_comment_preamble() -> &'static str {
    const END: &str = "\n-->\n";
    let end = GUIDANCE_TEMPLATE.find(END).map_or(0, |at| at + END.len());
    &GUIDANCE_TEMPLATE[..end]
}

fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<(), String> {
    let folder = path.parent().ok_or("保存先が不正です")?;
    std::fs::create_dir_all(folder).map_err(|e| format!("保存先を作れません: {e}"))?;
    let bytes = serde_json::to_vec_pretty(value).map_err(|e| e.to_string())?;
    let temporary = path.with_extension("tmp");
    std::fs::write(&temporary, bytes).map_err(|e| format!("保存できません: {e}"))?;
    std::fs::rename(&temporary, path).map_err(|e| format!("保存できません: {e}"))
}

/// このアプリが保存した音声か(`YYYYMMDD_HHMMSS_連番.wav`)。
fn is_voice_log(path: &Path) -> bool {
    let digits = |text: &str, length: Option<usize>| {
        !text.is_empty()
            && text.bytes().all(|b| b.is_ascii_digit())
            && length.is_none_or(|length| text.len() == length)
    };
    let stem = path.file_stem().and_then(|stem| stem.to_str()).unwrap_or("");
    let parts: Vec<&str> = stem.split('_').collect();
    path.extension().is_some_and(|extension| extension == "wav")
        && matches!(parts.as_slice(), [day, time, serial]
            if digits(day, Some(8)) && digits(time, Some(6)) && digits(serial, None))
}

/// 音声保存の上限(MB)を超えた分を古い順に消す。保存先は利用者が選べるので、自分の音声だけを数えて消す。
pub fn trim_voice_log(folder: &Path, limit_mb: u64) {
    let mut files: Vec<(std::time::SystemTime, u64, PathBuf)> = std::fs::read_dir(folder)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| is_voice_log(&entry.path()))
        .filter_map(|entry| {
            let meta = entry.metadata().ok()?;
            Some((meta.modified().ok()?, meta.len(), entry.path()))
        })
        .collect();
    files.sort();
    let mut total: u64 = files.iter().map(|f| f.1).sum();
    let limit = limit_mb * 1024 * 1024;
    for (_, size, path) in files {
        if total <= limit {
            break;
        }
        if std::fs::remove_file(path).is_ok() {
            total -= size;
        }
    }
}

/// 鍵のファイルを読む(前後の空白を除く)。無いか空なら None。
fn read_key(path: &Path) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// 鍵を所有者だけが読めるファイルに書く。空なら消す。改行・空白・引用符を含む鍵は断る(curl の設定に書くため)。
fn write_key(root: &Path, path: &Path, key: &str) -> Result<(), String> {
    let key = key.trim();
    if key.is_empty() {
        if path.exists() {
            std::fs::remove_file(path).map_err(|e| format!("API キーを消せません: {e}"))?;
        }
        return Ok(());
    }
    if !crate::core::claude_api::usable_key(key) {
        return Err(crate::core::claude_api::UNUSABLE_KEY.to_string());
    }
    std::fs::create_dir_all(root).map_err(|e| format!("保存先を作れません: {e}"))?;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .map_err(|e| format!("API キーを保存できません: {e}"))?;
    writeln!(file, "{key}").map_err(|e| format!("API キーを保存できません: {e}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::dictionary::default_symbols;

    fn temp_store(tag: &str) -> Store {
        let root =
            std::env::temp_dir().join(format!("openspeech-store-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        Store::at(&root)
    }

    #[test]
    fn the_template_is_written_only_when_the_file_is_missing() {
        let store = temp_store("guidance");
        assert_eq!(store.load_guidance(), "");
        assert_eq!(store.guidance_modified(), None);
        let path = store.ensure_guidance().unwrap();
        assert_eq!(path, store.root.join("guidance.md"));
        assert_eq!(store.load_guidance(), GUIDANCE_TEMPLATE);
        assert!(store.guidance_modified().is_some());
        std::fs::write(&path, "自分で書いた補足").unwrap();
        store.ensure_guidance().unwrap();
        assert_eq!(store.load_guidance(), "自分で書いた補足", "2 回目は触らない");
        std::fs::write(&path, "").unwrap();
        store.ensure_guidance().unwrap();
        assert_eq!(store.load_guidance(), "", "空にしたのも利用者の選択");
        let _ = std::fs::remove_dir_all(&store.root);
    }

    /// 別の文字コードで保存されても、補足なしで訂正は続ける。
    /// 古い前書き(規則はアプリが固定)で始まるファイルは、1 回だけ規則の節を書き込み、控えを残す。
    #[test]
    fn the_old_preamble_is_replaced_by_the_rules_once() {
        let store = temp_store("guidance-migrate");
        std::fs::create_dir_all(&store.root).unwrap();
        let path = store.root.join("guidance.md");
        let old = format!("{OLD_GUIDANCE_PREAMBLE}\n# 分野\n自分の分野\n\n# 表記\n- 自分の決まり\n");
        std::fs::write(&path, &old).unwrap();
        let notice = store.migrate_guidance().expect("書き込んだ知らせ");
        assert!(notice.contains("書き込みました"), "{notice}");
        assert!(!notice.contains("注釈"), "今の雛形の前書きを書くので、2 段目は続けて起きない: {notice}");
        let text = store.load_guidance();
        assert!(text.starts_with("<!--\nここに書いたことは、認識した文を直すときの指示として"));
        assert!(text.contains("# してはいけないこと"));
        assert!(text.ends_with("# 分野\n自分の分野\n\n# 表記\n- 自分の決まり\n"), "{text}");
        assert!(!text.contains("ここでは変えられません"));
        assert!(!text.contains("\n\n\n"), "空行を増やさない: {text}");
        let copies: Vec<_> = std::fs::read_dir(store.root.join("backup"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().to_string())
            .filter(|name| name.starts_with("guidance-"))
            .collect();
        assert_eq!(copies.len(), 1, "{copies:?}");
        assert_eq!(std::fs::read_to_string(store.root.join("backup").join(&copies[0])).unwrap(), old);
        assert!(store.migrate_guidance().is_none(), "2 回目は触らない");
        assert_eq!(store.load_guidance(), text);
        let warnings = store.take_warnings();
        assert_eq!(warnings.len(), 1, "起動時の知らせに 1 行: {warnings:?}");
        assert!(warnings[0].contains("書き込みました"));
    }

    /// Windows 風の改行(CRLF)で保存した古いファイルも前書きと見比べ、LF で書き戻す。
    #[test]
    fn a_crlf_guidance_file_is_migrated_too() {
        let store = temp_store("guidance-crlf");
        std::fs::create_dir_all(&store.root).unwrap();
        let path = store.root.join("guidance.md");
        let old = format!("{OLD_GUIDANCE_PREAMBLE}\n# 分野\n自分の分野\n").replace('\n', "\r\n");
        std::fs::write(&path, &old).unwrap();
        assert!(store.migrate_guidance().is_some());
        let text = store.load_guidance();
        assert!(!text.contains('\r'));
        assert!(text.ends_with("# 分野\n自分の分野\n"), "{text}");
        assert!(text.contains("# してはいけないこと"));
    }

    /// 自分で書いたファイル、雛形のままのファイル、無いファイルは触らない。
    #[test]
    fn other_guidance_files_are_left_alone() {
        let store = temp_store("guidance-keep");
        assert!(store.migrate_guidance().is_none(), "無ければ何もしない");
        std::fs::create_dir_all(&store.root).unwrap();
        let path = store.root.join("guidance.md");
        std::fs::write(&path, "自分の指示").unwrap();
        assert!(store.migrate_guidance().is_none());
        assert_eq!(store.load_guidance(), "自分の指示");
        store.ensure_guidance().unwrap();
        std::fs::write(&path, GUIDANCE_TEMPLATE).unwrap();
        assert!(store.migrate_guidance().is_none());
        assert_eq!(store.load_guidance(), GUIDANCE_TEMPLATE);
        assert!(!store.root.join("backup").exists());
    }

    #[test]
    fn unreadable_guidance_counts_as_none() {
        let store = temp_store("guidance-bad");
        std::fs::create_dir_all(&store.root).unwrap();
        std::fs::write(store.root.join("guidance.md"), [0x82, 0xa0, 0xff]).unwrap();
        assert_eq!(store.load_guidance(), "");
        assert!(store.read_guidance().is_err(), "画面に知らせられるよう無いときと区別する");
        let _ = std::fs::remove_dir_all(&store.root);
    }

    #[test]
    fn invisible_characters_do_not_reach_the_prompt() {
        let store = temp_store("guidance-marks");
        std::fs::create_dir_all(&store.root).unwrap();
        let path = store.root.join("guidance.md");
        std::fs::write(&path, "\u{feff}").unwrap();
        assert_eq!(store.load_guidance(), "", "BOM だけなら補足なし");
        std::fs::write(&path, "\u{feff}分野\0は放射線").unwrap();
        assert_eq!(store.load_guidance(), "分野は放射線");
        let _ = std::fs::remove_dir_all(&store.root);
    }

    /// 1.1.0 までの雛形の素の 3 行の前書きで始まるファイルは、1 回だけその 3 行を注釈に直し、控えを残す。
    /// 規則の節(利用者の文)は触らない。
    #[test]
    fn the_plain_preamble_becomes_a_comment_once() {
        let store = temp_store("guidance-comment");
        std::fs::create_dir_all(&store.root).unwrap();
        let path = store.root.join("guidance.md");
        let old = format!("{PLAIN_GUIDANCE_PREAMBLE}\n# 直してよいもの\n- 自分の規則\n\n# 分野\n自分の分野\n");
        std::fs::write(&path, &old).unwrap();
        let notice = store.migrate_guidance().expect("直した知らせ");
        assert!(notice.starts_with("AI への指示(guidance.md)の先頭の説明を、AI に送られない注釈に直しました。控え: "), "{notice}");
        let text = store.load_guidance();
        assert_eq!(text, format!("{}\n# 直してよいもの\n- 自分の規則\n\n# 分野\n自分の分野\n", guidance_comment_preamble()));
        let sent = crate::core::guidance::strip_comments(&text);
        assert_eq!(sent.trim(), "# 直してよいもの\n- 自分の規則\n\n# 分野\n自分の分野", "注釈は送らない");
        let copies: Vec<_> = std::fs::read_dir(store.root.join("backup"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        assert_eq!(copies.len(), 1, "{copies:?}");
        assert_eq!(std::fs::read_to_string(&copies[0]).unwrap(), old);
        assert!(store.migrate_guidance().is_none(), "2 回目は触らない");
        assert_eq!(store.load_guidance(), text);
        assert_eq!(store.take_warnings().len(), 1, "起動時の知らせに 1 行");

        // CRLF で保存されていても同じ(書き戻しは LF)。手で前書きを書き換えた物は触らない。
        std::fs::write(&path, old.replace('\n', "\r\n")).unwrap();
        assert!(store.migrate_guidance().is_some());
        assert!(store.load_guidance().starts_with("<!--\n") && !store.load_guidance().contains('\r'));
        std::fs::write(&path, "ここに書いたことは、自分のメモです。\n# 規則\n").unwrap();
        assert!(store.migrate_guidance().is_none());
        let _ = std::fs::remove_dir_all(&store.root);
    }

    /// 依頼の指示(`request.md`)も同じ作り: 無ければ空、初めて開くときに雛形、あれば触らない。
    #[test]
    fn the_request_guidance_file_works_like_the_guidance_file() {
        let store = temp_store("request-guidance");
        assert_eq!(store.load_request_guidance(), "");
        assert_eq!(store.request_guidance_modified(), None);
        let path = store.ensure_request_guidance().unwrap();
        assert_eq!(path, store.root.join("request.md"));
        assert_eq!(store.load_request_guidance(), crate::core::assistant::DEFAULT_REQUEST_GUIDANCE);
        assert!(store.request_guidance_modified().is_some());
        assert_eq!(store.load_guidance(), "", "訂正の指示とは別のファイル");
        std::fs::write(&path, "\u{feff}自分の\0指示").unwrap();
        store.ensure_request_guidance().unwrap();
        assert_eq!(store.load_request_guidance(), "自分の指示", "2 回目は触らない。BOM と NUL は除く");
        let _ = std::fs::remove_dir_all(&store.root);
    }

    #[test]
    fn the_template_carries_the_field_words_and_says_the_rules_are_fixed() {
        for word in crate::core::recognizer::FIELD_WORDS {
            assert!(GUIDANCE_TEMPLATE.contains(&format!("- {word}\n")), "{word}");
        }
        for heading in ["# 分野", "# 表記", "# よく使う語"] {
            assert!(GUIDANCE_TEMPLATE.contains(heading), "{heading}");
        }
        assert!(GUIDANCE_TEMPLATE.starts_with("<!--\nここに書いたことは"));
        assert!(GUIDANCE_TEMPLATE.contains("この <!-- --> で囲んだ部分は AI に送られません。\n-->\n\n# 直してよいもの"));
        assert_eq!(guidance_comment_preamble().lines().last(), Some("-->"));
        assert!(GUIDANCE_TEMPLATE.contains("# してはいけないこと"));
        assert!(!GUIDANCE_TEMPLATE.contains("ここでは変えられません"));
        // 読影の文に限らず「数は算用数字」と書くと、一般の文の「三種類」まで「3種類」に書き換わった(導入前の評価)。
        assert!(GUIDANCE_TEMPLATE.contains("読影レポートの文だけに当てはめる"));
        assert!(GUIDANCE_TEMPLATE.contains("一般の文章では数の書き方を変えない"));
        // 「単位は mm」と言い切ると mL や mSv を巻き込みかねない。
        assert!(GUIDANCE_TEMPLATE.contains("長さの単位は mm・cm"));
        assert!(GUIDANCE_TEMPLATE.chars().count() < crate::core::corrector::MAX_GUIDANCE_CHARS);
    }

    #[test]
    fn user_data_round_trips() {
        let store = temp_store("rt");
        let mut data = UserData::default();
        data.dictionary.add_word("肺気腫", "はいきしゅ").unwrap();
        data.dictionary
            .save_template(None, "胸部正常", "異常なし。\n以上。")
            .unwrap();
        data.memory.learn("右配上葉", "右肺上葉", false);
        // 起動時に辞書ファイルができてから、設定と学習を保存する順。
        store.save_dictionary(&data.dictionary).unwrap();
        store.save_user(&data).unwrap();
        assert_eq!(store.load_user(), data);
        // 辞書は user.json には入らない(学習のたびに書き換わるファイルに手で編集する物を置かない)。
        let user = std::fs::read_to_string(store.root.join("user.json")).unwrap();
        assert!(!user.contains("肺気腫"), "{user}");
        let _ = std::fs::remove_dir_all(&store.root);
    }

    /// 以前の保存形式では辞書も user.json に入っていた(記号や注記の項目は無い)。
    fn write_old_user_json(path: &Path, data: &UserData) {
        let mut value = serde_json::to_value(data).unwrap();
        let mut dictionary = serde_json::to_value(&data.dictionary).unwrap();
        dictionary.as_object_mut().unwrap().remove("symbols");
        dictionary.as_object_mut().unwrap().remove("_note");
        value["dictionary"] = dictionary;
        write_json(path, &value).unwrap();
    }

    #[test]
    fn the_dictionary_lives_in_its_own_file() {
        let store = temp_store("dict-file");
        // 初回は既定の辞書(記号入り)を作る。
        assert_eq!(store.load_user(), UserData::default());
        let text = std::fs::read_to_string(store.dictionary_path()).unwrap();
        assert!(text.contains("\"_note\""), "{text}");
        assert!(text.contains("まる"), "{text}");
        // 手で編集すると次の読み込みで反映される。本文は行の配列でもよく、知らない項目は残る。
        let edited = r#"{
            "words": [{"word": "肺気腫", "reading": "はいきしゅ"}],
            "symbols": [{"readings": ["まる"], "text": "。"}],
            "templates": [{"keywords": ["胸部正常"], "body": ["異常なし。", "以上。"], "memo": "自由項目"}]
        }"#;
        std::fs::write(store.dictionary_path(), edited).unwrap();
        let data = store.load_user();
        assert_eq!(data.dictionary.words[0].word, "肺気腫");
        assert_eq!(data.dictionary.symbols.len(), 1);
        assert_eq!(data.dictionary.templates[0].body, "異常なし。\n以上。");
        assert_eq!(data.dictionary.templates[0].extra["memo"], "自由項目");
        store.save_dictionary(&data.dictionary).unwrap();
        let written = std::fs::read_to_string(store.dictionary_path()).unwrap();
        assert!(written.contains("自由項目"), "{written}");
        assert!(written.contains("\"異常なし。\",") , "複数行の本文は行の配列で書く: {written}");
        let _ = std::fs::remove_dir_all(&store.root);
    }

    #[test]
    fn an_old_user_json_dictionary_moves_to_the_dictionary_file() {
        let store = temp_store("dict-migrate");
        let mut data = UserData::default();
        data.dictionary.add_word("肺気腫", "はいきしゅ").unwrap();
        data.dictionary.save_template(None, "胸部正常", "異常なし。").unwrap();
        write_old_user_json(&store.root.join("user.json"), &data);
        let loaded = store.load_user();
        assert_eq!(loaded.dictionary.words, data.dictionary.words);
        assert_eq!(loaded.dictionary.templates, data.dictionary.templates);
        assert_eq!(loaded.dictionary.symbols, default_symbols(), "記号の項目が無ければ既定の記号を足す");
        let written: Dictionary =
            serde_json::from_slice(&std::fs::read(store.dictionary_path()).unwrap()).unwrap();
        assert_eq!(written, loaded.dictionary);
        assert!(store.take_warnings().is_empty());
        let _ = std::fs::remove_dir_all(&store.root);
    }

    /// 読めない辞書ファイル(編集の途中)は触らない。よけたり作り直したりすると、開いている編集中のファイルが消える。
    #[test]
    fn an_unreadable_dictionary_file_is_left_alone() {
        let store = temp_store("dict-broken");
        std::fs::create_dir_all(&store.root).unwrap();
        std::fs::write(store.dictionary_path(), b"{not json").unwrap();
        assert_eq!(store.load_user().dictionary, Dictionary::default());
        assert_eq!(std::fs::read(store.dictionary_path()).unwrap(), b"{not json");
        let names: Vec<String> = std::fs::read_dir(&store.root)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(names, vec!["dictionary.json"], "ほかのファイルを作らない");
        let warnings = store.take_warnings();
        assert_eq!(warnings.len(), 1, "画面に知らせる");
        assert!(warnings[0].contains("読めません"), "{warnings:?}");
        let _ = std::fs::remove_dir_all(&store.root);
    }

    /// 辞書ファイルを書けない間は、辞書を user.json に残す(引き継ぎの一回きりの書き込みに失敗しても失わない)。
    #[test]
    fn the_dictionary_stays_in_user_json_until_its_file_can_be_written() {
        let store = temp_store("dict-unwritable");
        let mut data = UserData::default();
        data.dictionary.add_word("肺気腫", "はいきしゅ").unwrap();
        write_old_user_json(&store.root.join("user.json"), &data);
        // 同じ名前のフォルダがあると辞書ファイルを作れない。
        std::fs::create_dir_all(store.dictionary_path()).unwrap();
        let loaded = store.load_user();
        assert_eq!(loaded.dictionary.words, data.dictionary.words);
        assert_eq!(store.take_warnings().len(), 1);
        store.save_user(&loaded).unwrap();
        let user = std::fs::read_to_string(store.root.join("user.json")).unwrap();
        assert!(user.contains("肺気腫"), "辞書ファイルが無い間は user.json に残す: {user}");
        // 邪魔が無くなれば、次の起動で辞書ファイルへ移る。
        std::fs::remove_dir_all(store.dictionary_path()).unwrap();
        let again = store.load_user();
        assert_eq!(again.dictionary.words, data.dictionary.words);
        assert!(store.dictionary_path().is_file());
        store.save_user(&again).unwrap();
        let user = std::fs::read_to_string(store.root.join("user.json")).unwrap();
        assert!(!user.contains("肺気腫"), "{user}");
        let _ = std::fs::remove_dir_all(&store.root);
    }

    #[test]
    fn a_hand_written_word_may_omit_its_reading_and_keep_extra_fields() {
        let store = temp_store("dict-sparse-word");
        std::fs::create_dir_all(&store.root).unwrap();
        let text = r#"{"memo": "先頭のメモ", "words": [{"word": "肺気腫"}, {"word": "結節", "reading": "けっせつ", "tag": "胸部"}],
                      "symbols": [{"readings": ["ぷらす"], "text": "＋", "memo": "足した"}]}"#;
        std::fs::write(store.dictionary_path(), text).unwrap();
        let loaded = store.load_user().dictionary;
        assert_eq!(loaded.words.len(), 2);
        assert_eq!(loaded.words[0].reading, "");
        assert_eq!(loaded.words[1].extra["tag"], "胸部");
        assert_eq!(loaded.extra["memo"], "先頭のメモ");
        store.save_dictionary(&loaded).unwrap();
        let written = std::fs::read_to_string(store.dictionary_path()).unwrap();
        for kept in ["先頭のメモ", "\"tag\"", "足した", "\"_note\""] {
            assert!(written.contains(kept), "{kept}: {written}");
        }
        let _ = std::fs::remove_dir_all(&store.root);
    }

    #[test]
    fn missing_data_gets_defaults() {
        let store = temp_store("new");
        assert_eq!(store.load_user(), UserData::default());
    }

    #[test]
    fn unreadable_data_is_kept_aside() {
        let store = temp_store("broken");
        std::fs::create_dir_all(&store.root).unwrap();
        std::fs::write(store.root.join("user.json"), b"{not json").unwrap();
        assert_eq!(store.load_user(), UserData::default());
        let kept: Vec<_> = std::fs::read_dir(&store.root)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().contains("unreadable"))
            .collect();
        assert_eq!(kept.len(), 1);
        assert_eq!(std::fs::read(kept[0].path()).unwrap(), b"{not json");
        assert_eq!(store.take_warnings().len(), 1, "画面に知らせる");
        assert!(store.take_warnings().is_empty());
        let _ = std::fs::remove_dir_all(&store.root);
    }

    #[test]
    fn a_file_without_read_permission_is_not_overwritten() {
        use std::os::unix::fs::PermissionsExt;
        let store = temp_store("denied");
        let mut data = UserData::default();
        data.memory.learn("右配上葉", "右肺上葉", false);
        store.save_user(&data).unwrap();
        let path = store.root.join("user.json");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
        assert_eq!(store.load_user(), UserData::default());
        store.save_user(&UserData::default()).unwrap();
        let kept = std::fs::read_dir(&store.root)
            .unwrap()
            .flatten()
            .find(|e| e.file_name().to_string_lossy().starts_with("user.unreadable"))
            .unwrap()
            .path();
        std::fs::set_permissions(&kept, std::fs::Permissions::from_mode(0o600)).unwrap();
        let old: UserData = serde_json::from_slice(&std::fs::read(&kept).unwrap()).unwrap();
        assert_eq!(old.memory.pairs.len(), 1, "元のデータは残る");
        let _ = std::fs::remove_dir_all(&store.root);
    }

    #[test]
    fn automatic_backups_keep_a_few_generations_in_their_own_folder() {
        let store = temp_store("backup");
        let manual = store.root.join("backup/pre-cleanup");
        std::fs::create_dir_all(&manual).unwrap();
        for index in 0..BACKUP_GENERATIONS + 2 {
            let old = store.root.join(format!("backup/auto/2020010{index}_000000"));
            std::fs::create_dir_all(old).unwrap();
        }
        store.backup_user(&UserData::default()).unwrap();
        let count = std::fs::read_dir(store.root.join("backup/auto")).unwrap().count();
        assert_eq!(count, BACKUP_GENERATIONS);
        assert!(manual.is_dir(), "自動のもの以外は消さない");
        let _ = std::fs::remove_dir_all(&store.root);
    }

    #[test]
    fn voice_log_is_trimmed_to_the_limit() {
        let store = temp_store("vl");
        std::fs::create_dir_all(&store.root).unwrap();
        let big = store.root.join("20261105_165800_0001.wav");
        std::fs::write(&big, vec![0u8; 2 * 1024 * 1024]).unwrap();
        trim_voice_log(&store.root, 1);
        assert!(!big.exists());
        let _ = std::fs::remove_dir_all(&store.root);
    }

    /// 保存先には利用者の別のファイルがあるかもしれない。
    #[test]
    fn trimming_leaves_other_files_in_the_folder() {
        let store = temp_store("vl-other");
        std::fs::create_dir_all(&store.root).unwrap();
        let names = ["先月の原稿.docx", "会議.wav", "20261105.wav", "a_b_c.wav"];
        for name in names {
            std::fs::write(store.root.join(name), vec![0u8; 2 * 1024 * 1024]).unwrap();
        }
        trim_voice_log(&store.root, 1);
        for name in names {
            assert!(store.root.join(name).exists(), "{name}");
        }
        let _ = std::fs::remove_dir_all(&store.root);
    }
}
