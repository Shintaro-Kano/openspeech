//! 外部コマンドの起動と、この Mac の Claude Code(`claude -p`、サブスクのログイン)の呼び出しの共通部分。
//! 訂正役・補完役が Claude Code を起動するときに使う。

use serde_json::Value;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

/// 外部の CLI が終わったあと、出力を読み切るのを待つ上限。
const DRAIN: Duration = Duration::from_secs(1);
/// 外部の CLI へ渡す PATH(本体のあるフォルダの後ろに足す)。
const CLI_PATH: &str = "/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin";
/// Claude Code を探す場所(ホームからの相対)。Dock から開くと端末の PATH が渡らないので、よくある場所を見る。
const CLAUDE_IN_HOME: [&str; 3] = [
    ".local/bin/claude",
    ".claude/local/claude",
    ".npm-global/bin/claude",
];
const CLAUDE_ON_SYSTEM: [&str; 2] = ["/opt/homebrew/bin/claude", "/usr/local/bin/claude"];

/// サブスクのログインで動く CLI の起動。API キーの環境変数があるとサブスクではなくそちらが使われるので、
/// 環境は渡すものだけにする。
pub(crate) fn clean_command(binary: &Path) -> Command {
    let mut command = Command::new(binary);
    let folder = binary.parent().unwrap_or(Path::new("/usr/bin"));
    command
        .env_clear()
        .env("PATH", format!("{}:{CLI_PATH}", folder.display()));
    for name in ["HOME", "USER", "LOGNAME", "TMPDIR", "LANG"] {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    command
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

/// 実行ファイルを、ホームの下のよくある場所 → システムのよくある場所 → PATH の順に探す。
pub(crate) fn find_executable(name: &str, in_home: &[&str], on_system: &[&str]) -> Option<PathBuf> {
    let in_home = dirs::home_dir()
        .into_iter()
        .flat_map(|home| in_home.iter().map(move |path| home.join(path)));
    let on_system = on_system.iter().map(PathBuf::from);
    let on_path: Vec<PathBuf> = std::env::var_os("PATH")
        .iter()
        .flat_map(std::env::split_paths)
        .map(|folder| folder.join(name))
        .collect();
    in_home
        .chain(on_system)
        .chain(on_path)
        .find(|path| is_executable(path))
}

/// この Mac の Claude Code を探す。
pub fn find_claude() -> Option<PathBuf> {
    find_executable("claude", &CLAUDE_IN_HOME, &CLAUDE_ON_SYSTEM)
}

/// 一時ファイルの名前の頭(`<頭>-<プロセス番号>-<連番>`)。`remove_stale_private_files` が掃除する。
pub(crate) const PROMPT_PREFIX: &str = "openspeech-prompt";
pub(crate) const KEY_PREFIX: &str = "openspeech-key";
/// 外部の音声認識 API に送る本文(音声・JSON)。
pub(crate) const BODY_PREFIX: &str = "openspeech-body";

/// 前に動いていたこのアプリが終わり際に残した一時ファイル(システムプロンプト、API キーの curl 設定)を消す。
/// 終了は処理役の通信を待たないので、待っている間に終えると残る。名前のプロセス番号が今は動いていないものだけ消す。
/// 起動時に 1 回呼ぶ。消した数を返す。
pub fn remove_stale_private_files() -> usize {
    remove_stale_in(&std::env::temp_dir())
}

fn remove_stale_in(dir: &Path) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let mut removed = 0;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(rest) = [PROMPT_PREFIX, KEY_PREFIX, BODY_PREFIX]
            .iter()
            .find_map(|prefix| name.strip_prefix(prefix)?.strip_prefix('-'))
        else {
            continue;
        };
        let Some(pid) = rest.split('-').next().and_then(|p| p.parse::<i32>().ok()) else {
            continue;
        };
        if pid <= 0 || pid == std::process::id() as i32 || process_alive(pid) {
            continue;
        }
        if entry.file_type().is_ok_and(|kind| kind.is_file()) && std::fs::remove_file(entry.path()).is_ok() {
            removed += 1;
        }
    }
    removed
}

/// プロセスが動いているか。信号 0 は存在と権限だけを確かめる(権限が無いのは別の利用者のものが動いている)。
fn process_alive(pid: i32) -> bool {
    let answered = unsafe { libc::kill(pid, 0) } == 0;
    answered || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// 所有者だけが読める一時ファイル。作るときに中身を書き、落とすときに消す。
/// 引数に出したくないもの(API キー、用語集と訂正例を含むシステムプロンプト)を外部コマンドへ渡すのに使う。
pub(crate) struct PrivateFile {
    path: PathBuf,
}

impl PrivateFile {
    pub(crate) fn create(prefix: &str, contents: &str) -> Result<Self, String> {
        Self::create_bytes(prefix, contents.as_bytes())
    }

    /// 中身がバイト列(音声など)の一時ファイル。
    pub(crate) fn create_bytes(prefix: &str, contents: &[u8]) -> Result<Self, String> {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let serial = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("{prefix}-{}-{serial}", std::process::id()));
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(&path)
            .map_err(|e| format!("一時ファイルを作れません: {e}"))?;
        // 先に消す口を作っておく(書けなかったときも残さない)。
        let private = PrivateFile { path };
        file.write_all(contents)
            .map_err(|e| format!("一時ファイルに書けません: {e}"))?;
        Ok(private)
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for PrivateFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// 別のスレッドで読み切る(読まないとパイプが詰まって相手が止まることがある)。
fn read_all(source: Option<impl Read + Send + 'static>) -> Receiver<String> {
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let mut text = String::new();
        if let Some(mut source) = source {
            let _ = source.read_to_string(&mut text);
        }
        let _ = sender.send(text);
    });
    receiver
}

/// 標準入力へ文を渡して実行し、(標準出力, 標準エラー)を返す。上限を超えたら止める。
fn run_with_input(
    mut command: Command,
    input: &str,
    timeout: Duration,
) -> Result<(String, String), String> {
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("Claude Code を起動できません: {e}"))?;
    let stdout = read_all(child.stdout.take());
    let stderr = read_all(child.stderr.take());
    if let Some(mut stdin) = child.stdin.take() {
        // 先に相手が終わっていたら書けないが、その場合は結果の方に理由が出る。
        let _ = stdin.write_all(input.as_bytes());
    }
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("Claude Code の応答が遅いので待つのをやめました".to_string());
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("Claude Code の結果を読めません: {error}"));
            }
        }
    }
    // 孫のプロセスが出力の口を持ったまま残ると読み終わらないので、ここも待ちすぎない。
    let read = |output: Receiver<String>| output.recv_timeout(DRAIN).unwrap_or_default();
    Ok((read(stdout), read(stderr)))
}

/// Claude Code に、道具も設定も読まず、会話も残さず、1 回で形を固定した答えだけを返させる引数。
/// システムプロンプトはファイルで渡す(引数に置くと同じ利用者の `ps` で見える)。
/// 形を固定しないと説明文が混ざることがある。`--bare` はサブスクのログインを読まないので使えない。
fn claude_code_arguments(model: &str, system_file: &Path, schema: &Value, effort: &str) -> Vec<String> {
    let system_file = system_file.to_string_lossy();
    let schema = schema.to_string();
    [
        "-p",
        "--model",
        model,
        "--system-prompt-file",
        &system_file,
        "--tools",
        "",
        "--setting-sources",
        "",
        "--strict-mcp-config",
        "--disable-slash-commands",
        "--no-session-persistence",
        "--effort",
        effort,
        "--json-schema",
        &schema,
        "--output-format",
        "json",
    ]
    .iter()
    .map(|argument| argument.to_string())
    .collect()
}

/// `claude -p` の起動。起動する場所(`work_dir`)はデータのフォルダにして、そこの設定やフックを読ませない。
fn claude_code_command(binary: &Path, work_dir: &Path, arguments: &[String]) -> Command {
    let mut command = clean_command(binary);
    command
        .current_dir(work_dir)
        .args(arguments)
        .env("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1");
    command
}

/// `claude -p` に求める答え: 形(`--json-schema`)と考える量(`--effort`)。
pub(crate) struct Answer<'a> {
    pub(crate) schema: &'a Value,
    pub(crate) effort: &'a str,
}

/// システムプロンプトをファイルに置いて `claude -p` を実行し、(標準出力, 標準エラー)を返す。
/// ファイルは終わったら消す。
pub(crate) fn run_claude_code(
    binary: &Path,
    work_dir: &Path,
    model: &str,
    system: &str,
    answer: Answer,
    message: &str,
    timeout: Duration,
) -> Result<(String, String), String> {
    let system_file = PrivateFile::create(PROMPT_PREFIX, system)?;
    let arguments = claude_code_arguments(model, system_file.path(), answer.schema, answer.effort);
    let command = claude_code_command(binary, work_dir, &arguments);
    run_with_input(command, message, timeout)
}

/// Claude Code の失敗の知らせを、利用者が次にすることが分かる文にする。
/// 知らせはログにも残るので、話した文が混ざらないよう、英数字だけの 1 行のときだけ原文を添える。
fn claude_code_failure(detail: &str) -> String {
    let lower = detail.to_lowercase();
    if lower.contains("log in") || lower.contains("login") || lower.contains("logged in") {
        return "Claude Code にログインしていません。ターミナルで claude を開いて /login してください"
            .to_string();
    }
    if lower.contains("limit") {
        return "Claude の利用上限に達しました。時間をおくか、API キーの接続に切り替えてください"
            .to_string();
    }
    let line = detail.lines().next().unwrap_or("").trim();
    if line.is_empty() || !line.is_ascii() {
        return "Claude Code からエラーが返りました".to_string();
    }
    let short: String = line.chars().take(120).collect();
    format!("Claude Code のエラー: {short}")
}

/// `claude -p --output-format json` の出力から、形を指定した答え(JSON のオブジェクト)を取り出す。
/// それ以外の文は説明が混ざることがあるので使わない。訂正役とテンプレート補完で共用する。
pub(crate) fn claude_code_structured(stdout: &str, stderr: &str) -> Result<Value, String> {
    let Ok(root) = serde_json::from_str::<Value>(stdout.trim()) else {
        // 標準出力には話した文が混ざることがあるので、理由は標準エラーからだけ取る。
        return Err(claude_code_failure(stderr));
    };
    if root["is_error"].as_bool().unwrap_or(false) {
        let detail = root["result"].as_str().or(root["subtype"].as_str());
        return Err(claude_code_failure(detail.unwrap_or("")));
    }
    if root["structured_output"].is_object() {
        return Ok(root["structured_output"].clone());
    }
    root["result"]
        .as_str()
        .and_then(|text| serde_json::from_str::<Value>(text).ok())
        .filter(Value::is_object)
        .ok_or_else(|| "Claude から答えが返りませんでした".to_string())
}

/// 試験用の偽の `claude`。システムプロンプトのファイルの中身と `--json-schema` の値を `dir/claude.log` に書き、
/// 標準入力を読み捨てて `reply` を返す。本物の `claude` は呼ばない。
#[cfg(test)]
pub(crate) fn fake_claude(dir: &Path, reply: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(dir).unwrap();
    let log = dir.join("claude.log");
    let script = format!(
        "#!/bin/sh\nwhile [ $# -gt 0 ]; do\n  case \"$1\" in\n    --system-prompt-file) echo \"SYSTEM:\"; cat \"$2\"; echo;;\n    --json-schema) echo \"SCHEMA: $2\";;\n    --effort) echo \"EFFORT: $2\";;\n  esac\n  shift\ndone >> '{}'\ncat > /dev/null\nprintf '%s' '{}'\n",
        log.display(),
        reply.replace('\'', "'\\''")
    );
    let fake = dir.join("claude");
    std::fs::write(&fake, script).unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    fake
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 動いていないプロセスの名前の付いた一時ファイルだけを消す。
    #[test]
    fn stale_private_files_of_finished_processes_are_removed() {
        let dir = std::env::temp_dir().join(format!("openspeech-stale-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut finished = Command::new("/usr/bin/true").spawn().unwrap();
        let dead = finished.id();
        finished.wait().unwrap();
        let mine = std::process::id();
        let names = [
            format!("{PROMPT_PREFIX}-{dead}-0"),
            format!("{KEY_PREFIX}-{dead}-3"),
            format!("{PROMPT_PREFIX}-{mine}-1"),
            format!("{PROMPT_PREFIX}-x-1"),
            "openspeech-other".to_string(),
        ];
        for name in &names {
            std::fs::write(dir.join(name), "x").unwrap();
        }
        assert_eq!(remove_stale_in(&dir), 2);
        assert!(!dir.join(&names[0]).exists() && !dir.join(&names[1]).exists());
        for name in &names[2..] {
            assert!(dir.join(name).exists(), "{name}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn claude_code_is_asked_for_one_fixed_answer_with_nothing_loaded() {
        let schema = json!({"type": "object", "additionalProperties": false});
        let arguments = claude_code_arguments("claude-haiku-5-5", Path::new("/tmp/p"), &schema, "low");
        let after = |flag: &str| {
            let index = arguments.iter().position(|a| a == flag).unwrap();
            arguments[index + 1].clone()
        };
        assert_eq!(arguments[0], "-p");
        assert_eq!(after("--model"), "claude-haiku-5-5");
        assert_eq!(after("--system-prompt-file"), "/tmp/p");
        assert_eq!(after("--tools"), "");
        assert_eq!(after("--setting-sources"), "");
        assert_eq!(after("--output-format"), "json");
        assert_eq!(after("--effort"), "low");
        let medium = claude_code_arguments("claude-opus-5-5", Path::new("/tmp/p"), &schema, "medium");
        let at = medium.iter().position(|a| a == "--effort").unwrap();
        assert_eq!(medium[at + 1], "medium");
        assert!(after("--json-schema").contains("additionalProperties"));
        assert!(arguments.iter().any(|a| a == "--no-session-persistence"));
        assert!(arguments.iter().any(|a| a == "--strict-mcp-config"));
        assert!(arguments.iter().any(|a| a == "--disable-slash-commands"));
        assert!(!arguments.iter().any(|a| a == "--system-prompt"), "本文を引数に出さない");
        assert!(
            !arguments.iter().any(|a| a == "--bare"),
            "--bare はサブスクのログインを読まない"
        );
    }

    #[test]
    fn claude_code_does_not_inherit_an_api_key() {
        let command = claude_code_command(
            Path::new("/opt/homebrew/bin/claude"),
            Path::new("/tmp"),
            &["-p".to_string()],
        );
        let envs: Vec<(String, Option<String>)> = command
            .get_envs()
            .map(|(name, value)| {
                (
                    name.to_string_lossy().to_string(),
                    value.map(|v| v.to_string_lossy().to_string()),
                )
            })
            .collect();
        let names: Vec<&str> = envs
            .iter()
            .filter(|(_, value)| value.is_some())
            .map(|(name, _)| name.as_str())
            .collect();
        assert!(names.contains(&"PATH"));
        assert!(!names.iter().any(|n| n.starts_with("ANTHROPIC")), "{names:?}");
        let value = |name: &str| {
            envs.iter()
                .find(|(n, _)| n == name)
                .and_then(|(_, v)| v.clone())
                .unwrap()
        };
        assert!(
            value("PATH").starts_with("/opt/homebrew/bin:"),
            "本体のあるフォルダを先に見る"
        );
        assert_eq!(value("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC"), "1");
        assert_eq!(command.get_current_dir(), Some(Path::new("/tmp")));
    }

    /// システムプロンプトは所有者だけが読めるファイルで渡し、終わったら消す。
    #[test]
    fn the_system_prompt_file_is_private_and_removed_after_the_call() {
        use std::os::unix::fs::PermissionsExt;
        let file = PrivateFile::create("openspeech-test", "規則と用語集").unwrap();
        let path = file.path().to_path_buf();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "規則と用語集");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        drop(file);
        assert!(!path.exists());

        // 偽の claude が、渡されたファイルの中身と引数を標準出力へ返す。
        let dir = std::env::temp_dir().join(format!("openspeech-fake-claude-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let fake = dir.join("claude");
        std::fs::write(
            &fake,
            "#!/bin/sh\nwhile [ $# -gt 0 ]; do\n  if [ \"$1\" = --system-prompt-file ]; then cat \"$2\"; echo; echo \"$2\"; fi\n  shift\ndone\n",
        )
        .unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        let (stdout, _) = run_claude_code(
            &fake,
            &dir,
            "claude-haiku-5-5",
            "規則と用語集",
            Answer { schema: &json!({}), effort: "low" },
            "",
            Duration::from_secs(5),
        )
        .unwrap();
        let mut lines = stdout.lines();
        assert_eq!(lines.next(), Some("規則と用語集"));
        let used = PathBuf::from(lines.next().unwrap());
        assert!(!used.exists(), "終わったら消す");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_stuck_process_is_stopped_at_the_limit() {
        let mut command = Command::new("/bin/sleep");
        command.arg("30");
        let began = Instant::now();
        let error = run_with_input(command, "", Duration::from_millis(100)).unwrap_err();
        assert!(error.contains("待つのをやめました"), "{error}");
        assert!(began.elapsed() < Duration::from_secs(5));
        let (stdout, _) = run_with_input(
            Command::new("/bin/cat"),
            "そのまま",
            Duration::from_secs(5),
        )
        .unwrap();
        assert_eq!(stdout, "そのまま");
    }

    /// 終わったあとに孫が出力の口を持ったまま残っても、いつまでも待たない。
    #[test]
    fn a_leftover_child_does_not_hold_the_answer_forever() {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "sleep 30 & exit 0"]);
        let began = Instant::now();
        let _ = run_with_input(command, "", Duration::from_secs(5));
        assert!(began.elapsed() < Duration::from_secs(4), "{:?}", began.elapsed());
    }
}
