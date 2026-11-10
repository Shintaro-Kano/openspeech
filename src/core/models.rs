//! 認識に使うモデルの置き場所と取得。
//!
//! 置き場所はデータのフォルダの `models/<フォルダ>/`(`~/Library/Application Support/OpenSpeech/models/`)。
//! 取得元の表は repo の `assets/models.json` を実行ファイルに埋め込んだもの(名前・取得元の URL の列・大きさ・
//! SHA-256・ライセンス・出所)。取得は macOS の `/usr/bin/curl`(`~/.curlrc` は読まない `-q`)で `.part` に落とし、
//! 途中で止まれば続きから取り直す。大きさと SHA-256 が表と合ったときだけ本来の名前に変える。
//! 取りに行く先は Hugging Face。表に 2 つ以上あれば、取れなかったときに次を試す。

use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStderr, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::{Arc, Mutex, MutexGuard};

const CATALOG: &str = include_str!("../../assets/models.json");
/// 取得に使う curl と、ハッシュを計る道具(macOS に必ずある固定の場所)。
pub const CURL: &str = "/usr/bin/curl";
const SHASUM: &str = "/usr/bin/shasum";
/// 取得のあとに残しておく空き容量。
const SPARE_BYTES: u64 = 200 * 1024 * 1024;

/// 取得できるモデル 1 つ。
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct ModelFile {
    pub id: String,
    /// 画面に出す名前。
    pub title: String,
    /// `models/` の下のフォルダ名。
    pub folder: String,
    pub file: String,
    pub size: u64,
    pub sha256: String,
    /// 取得元(先頭から順に試す)。
    pub urls: Vec<String>,
    pub license: String,
    /// 出所の 1 行(取得の画面と設定画面に出す)。
    pub source: String,
}

#[derive(Deserialize)]
struct Catalog {
    models: Vec<ModelFile>,
}

/// 取得できるモデルの一覧(埋め込んだ表)。
pub fn catalog() -> Vec<ModelFile> {
    serde_json::from_str::<Catalog>(CATALOG)
        .map(|catalog| catalog.models)
        .unwrap_or_default()
}

/// 一覧から 1 つ。
pub fn find(id: &str) -> Option<ModelFile> {
    catalog().into_iter().find(|model| model.id == id)
}

/// モデルの置き場所(データのフォルダの `models/`)。
pub fn models_root() -> PathBuf {
    crate::core::store::data_root().join("models")
}

/// 使えるモデルのファイル。置き場所の決まった名前で、大きさが表と合うものだけ(途中で切れた写しを
/// 「取得済み」にしない)。
pub fn installed_in(root: &Path, model: &ModelFile) -> Option<PathBuf> {
    let path = root.join(&model.folder).join(&model.file);
    std::fs::metadata(&path)
        .is_ok_and(|meta| meta.is_file() && meta.len() == model.size)
        .then_some(path)
}

/// 使えるモデルのファイル(いつもの置き場所)。
pub fn installed(id: &str) -> Option<PathBuf> {
    find(id).and_then(|model| installed_in(&models_root(), &model))
}

/// まだ無いモデル。
pub fn missing() -> Vec<ModelFile> {
    let root = models_root();
    catalog()
        .into_iter()
        .filter(|model| installed_in(&root, model).is_none())
        .collect()
}

/// 取得の進み具合(取れた分のバイト数, 全体のバイト数)。ファイルの大きさから数える。
pub fn progress(root: &Path, models: &[ModelFile]) -> (u64, u64) {
    let total = models.iter().map(|model| model.size).sum();
    let done = models
        .iter()
        .map(|model| {
            let folder = root.join(&model.folder);
            let finished = folder.join(&model.file);
            let part = folder.join(format!("{}.part", model.file));
            std::fs::metadata(&finished)
                .or_else(|_| std::fs::metadata(&part))
                .map_or(0, |meta| meta.len().min(model.size))
        })
        .sum();
    (done, total)
}

/// 取り消したときの答え(ほかの失敗と見分ける)。
pub const CANCELLED: &str = "取得をやめました";

/// 取得の取り消し。印が立つと、動いている curl をその場で止め、次のファイルへ進ませない。
#[derive(Default)]
pub struct Cancel {
    stopped: AtomicBool,
    /// 動いている curl。止めるときはここから終わらせる(アプリの終了でも curl を残さない)。
    running: Mutex<Option<Child>>,
}

impl Cancel {
    /// 印を立て、動いている curl を終わらせてから戻る。
    pub fn stop(&self) {
        self.stopped.store(true, Ordering::SeqCst);
        if let Some(child) = self.running().as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    fn running(&self) -> MutexGuard<'_, Option<Child>> {
        self.running.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn is_stopped(&self) -> bool {
        self.stopped.load(Ordering::SeqCst)
    }
}

/// 取得を 1 本のスレッドで進める。終わったら `finished` が結果を返す。落とすと(アプリの終了も)取得を止める。
pub struct Download {
    pub models: Vec<ModelFile>,
    pub root: PathBuf,
    result: Receiver<Result<(), String>>,
    cancel: Arc<Cancel>,
}

impl Download {
    /// `models` を順に取る。`wake` は終わったときに呼ぶ(画面を描き直させる)。
    pub fn start(models: Vec<ModelFile>, root: PathBuf, wake: impl Fn() + Send + 'static) -> Self {
        let (sender, result) = mpsc::channel();
        let cancel = Arc::new(Cancel::default());
        let (list, folder, stopper) = (models.clone(), root.clone(), cancel.clone());
        std::thread::spawn(move || {
            let outcome = fetch_all_with(&list, &folder, Path::new(CURL), &stopper);
            let _ = sender.send(outcome);
            wake();
        });
        Download {
            models,
            root,
            result,
            cancel,
        }
    }

    /// 終わっていれば結果。まだなら None。
    pub fn finished(&self) -> Option<Result<(), String>> {
        match self.result.try_recv() {
            Ok(outcome) => Some(outcome),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => Some(Err("取得が途中で止まりました".to_string())),
        }
    }

    /// (取れた分, 全体)のバイト数。
    pub fn progress(&self) -> (u64, u64) {
        progress(&self.root, &self.models)
    }

    /// 取得をやめる。取れた途中の分(`.part`)は残り、次に続きから取る。
    pub fn cancel(&self) {
        self.cancel.stop();
    }

    /// 利用者がやめたか(終わり方を失敗として見せないため)。
    pub fn was_cancelled(&self) -> bool {
        self.cancel.is_stopped()
    }
}

impl Drop for Download {
    fn drop(&mut self) {
        self.cancel.stop();
    }
}

/// 足りない容量があれば、その説明。
fn check_space(root: &Path, models: &[ModelFile]) -> Result<(), String> {
    let needed: u64 = models
        .iter()
        .filter(|model| !root.join(&model.folder).join(&model.file).is_file())
        .map(|model| model.size)
        .sum();
    let Some(free) = free_space(root) else {
        return Ok(());
    };
    if free < needed + SPARE_BYTES {
        return Err(format!(
            "空き容量が足りません(あと {} MB 要ります)",
            (needed + SPARE_BYTES - free).div_ceil(1_000_000)
        ));
    }
    Ok(())
}

/// そのフォルダのあるディスクの空き容量。分からなければ None。
fn free_space(folder: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let existing = folder.ancestors().find(|path| path.exists())?;
    let path = std::ffi::CString::new(existing.as_os_str().as_bytes()).ok()?;
    let mut stats: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(path.as_ptr(), &mut stats) } != 0 {
        return None;
    }
    Some(stats.f_bavail as u64 * stats.f_frsize as u64)
}

/// 順に取る。1 つでも取れなければ、その理由を返す(取れた分は残す)。
pub fn fetch_all(models: &[ModelFile], root: &Path, curl: &Path) -> Result<(), String> {
    fetch_all_with(models, root, curl, &Cancel::default())
}

fn fetch_all_with(models: &[ModelFile], root: &Path, curl: &Path, cancel: &Cancel) -> Result<(), String> {
    std::fs::create_dir_all(root).map_err(|e| format!("置き場所を作れません: {e}"))?;
    check_space(root, models)?;
    let mut failures = Vec::new();
    // 1 つ取れなくても残りは取る(認識のモデルだけでも使えるように)。
    for model in models {
        if cancel.is_stopped() {
            break;
        }
        if let Err(message) = fetch_one_with(model, root, curl, cancel) {
            if message != CANCELLED {
                failures.push(message);
            }
        }
    }
    // やめたときも、それまでの本当の失敗は伝える。
    if cancel.is_stopped() {
        failures.insert(0, CANCELLED.to_string());
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(failures.join("。"))
    }
}

/// 1 つ取る。すでに正しいファイルがあれば何もしない。
pub fn fetch_one(model: &ModelFile, root: &Path, curl: &Path) -> Result<PathBuf, String> {
    fetch_one_with(model, root, curl, &Cancel::default())
}

fn fetch_one_with(model: &ModelFile, root: &Path, curl: &Path, cancel: &Cancel) -> Result<PathBuf, String> {
    let folder = root.join(&model.folder);
    std::fs::create_dir_all(&folder).map_err(|e| format!("置き場所を作れません: {e}"))?;
    let finished = folder.join(&model.file);
    if installed_in(root, model).as_deref() == Some(finished.as_path()) {
        return Ok(finished);
    }
    // 決まった名前で大きさの違うファイル(途中で切れた写しなど)は置き換える。
    let _ = std::fs::remove_file(&finished);
    let part = folder.join(format!("{}.part", model.file));
    // 表より大きい途中の分は、続きから取っても合わないので消す。
    if std::fs::metadata(&part).is_ok_and(|meta| meta.len() > model.size) {
        let _ = std::fs::remove_file(&part);
    }
    let mut last_error = format!("{} の取得元がありません", model.title);
    for url in &model.urls {
        if cancel.is_stopped() {
            return Err(CANCELLED.to_string());
        }
        // 前の取得先で途中まで取れた分は、同じファイルなので続きから取る。合わなければ消して取り直す。
        // 1 分のあいだ 1 KB/秒を下回ったら止まったとみなす。https の取得先は転送先も https に限る。
        let mut command = Command::new(curl);
        command
            .args(["-q", "-sS", "-f", "-L", "--retry", "2", "--connect-timeout", "20"])
            .args(["--speed-limit", "1024", "--speed-time", "60", "-C", "-", "-o"])
            .arg(&part);
        if url.starts_with("https://") {
            command.args(["--proto", "=https", "--proto-redir", "=https"]);
        }
        command
            .arg(url)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        // 起動と登録は取り消し側と同じ鍵の中で行う。取り消しが先なら起動せず、後なら登録した子を止めてもらえる。
        let stderr = {
            let mut running = cancel.running();
            if cancel.is_stopped() {
                return Err(CANCELLED.to_string());
            }
            let mut child = command
                .spawn()
                .map_err(|e| format!("curl を起動できません: {e}"))?;
            let stderr = child.stderr.take();
            *running = Some(child);
            stderr
        };
        // 待つ間は取り消しの印を見続け、立ったら子を kill する(`wait_cancellable`)。
        let output = wait_cancellable(stderr, cancel);
        if cancel.is_stopped() {
            return Err(CANCELLED.to_string());
        }
        match output {
            Some((true, _)) => {}
            Some((false, detail)) => {
                last_error = format!("{} を取れませんでした: {}", model.title, detail.trim());
                continue;
            }
            None => return Err(CANCELLED.to_string()),
        }
        match verify(&part, model) {
            Ok(()) => {
                std::fs::rename(&part, &finished)
                    .map_err(|e| format!("取ったファイルを置けません: {e}"))?;
                return Ok(finished);
            }
            Err(problem) => {
                last_error = format!("{}: {problem}", model.title);
                let _ = std::fs::remove_file(&part);
            }
        }
    }
    Err(last_error)
}

/// `cancel` に登録した子プロセスを待つ。取り消されたら kill する。(成功したか, 標準エラー) を返す。
/// 取り消しなら None。子は終わるまで回収(wait)しないので、番号が使い回されることもない。
fn wait_cancellable(mut stderr: Option<ChildStderr>, cancel: &Cancel) -> Option<(bool, String)> {
    let reader = std::thread::spawn(move || {
        let mut text = String::new();
        if let Some(stderr) = stderr.as_mut() {
            use std::io::Read;
            let _ = stderr.read_to_string(&mut text);
        }
        text
    });
    loop {
        let state = {
            let mut running = cancel.running();
            let child = running.as_mut()?;
            if cancel.is_stopped() {
                let _ = child.kill();
                let _ = child.wait();
                *running = None;
                return None;
            }
            let state = child.try_wait();
            if !matches!(state, Ok(None)) {
                *running = None;
            }
            state
        };
        match state {
            Ok(Some(status)) => {
                let detail = reader.join().unwrap_or_default();
                return Some((status.success(), detail));
            }
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(100)),
            Err(error) => return Some((false, format!("curl を待てません: {error}"))),
        }
    }
}

/// 大きさと SHA-256 が表と合うか。
fn verify(path: &Path, model: &ModelFile) -> Result<(), String> {
    let size = std::fs::metadata(path).map(|meta| meta.len()).unwrap_or(0);
    if size != model.size {
        return Err(format!("大きさが違います({size} バイト、正しくは {})", model.size));
    }
    let digest = sha256(path)?;
    if !digest.eq_ignore_ascii_case(&model.sha256) {
        return Err("中身が表と違います(SHA-256 が合いません)".to_string());
    }
    Ok(())
}

/// ファイルの SHA-256(`shasum -a 256`)。
fn sha256(path: &Path) -> Result<String, String> {
    // shasum は perl の script なので、利用者の環境変数(PERL5OPT など)を渡さない。
    let output = Command::new(SHASUM)
        .env_clear()
        .args(["-a", "256", "-b"])
        .arg(path)
        .output()
        .map_err(|e| format!("shasum を起動できません: {e}"))?;
    let text = String::from_utf8_lossy(&output.stdout);
    text.split_whitespace()
        .next()
        .filter(|digest| digest.len() == 64)
        .map(str::to_string)
        .ok_or_else(|| "SHA-256 を計れません".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("openspeech-models-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 試験用のモデル(中身は `bytes`)。取りに行く先は手元のファイル(`file://`)。
    fn local_model(dir: &Path, bytes: &[u8], sources: &[&str]) -> ModelFile {
        let source = dir.join("source.bin");
        std::fs::write(&source, bytes).unwrap();
        ModelFile {
            id: "test".to_string(),
            title: "試験のモデル".to_string(),
            folder: "test".to_string(),
            file: "test.gguf".to_string(),
            size: bytes.len() as u64,
            sha256: sha256(&source).unwrap(),
            urls: sources
                .iter()
                .map(|name| format!("file://{}", dir.join(name).display()))
                .collect(),
            license: "MIT".to_string(),
            source: "試験".to_string(),
        }
    }

    #[test]
    fn the_catalog_lists_haas_and_the_voice_detector_with_their_sources() {
        let models = catalog();
        let ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, ["haas-ja", "silero-vad"]);
        for model in &models {
            assert_eq!(model.sha256.len(), 64, "{}", model.id);
            assert!(!model.urls.is_empty(), "取りに行く先が無い: {}", model.id);
            assert!(model.urls[0].starts_with("https://huggingface.co/"));
            assert!(model.urls.iter().all(|url| url.starts_with("https://")));
            assert!(!model.license.is_empty() && !model.source.is_empty());
        }
        let haas = find("haas-ja").unwrap();
        assert_eq!(haas.file, crate::core::speechcpp::MODEL_FILE);
        assert!(haas.source.contains("Third Intelligence") && haas.source.contains("sakasegawa"));
    }

    #[test]
    fn a_model_is_fetched_checked_and_put_in_place() {
        let dir = temp("fetch");
        let model = local_model(&dir, b"weights of the model", &["source.bin"]);
        let root = dir.join("models");
        let path = fetch_one(&model, &root, Path::new(CURL)).unwrap();
        assert_eq!(path, root.join("test/test.gguf"));
        assert_eq!(std::fs::read(&path).unwrap(), b"weights of the model");
        assert!(!root.join("test/test.gguf.part").exists());
        assert_eq!(installed_in(&root, &model), Some(path));
        assert_eq!(progress(&root, std::slice::from_ref(&model)), (model.size, model.size));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_broken_first_source_falls_back_to_the_next() {
        let dir = temp("fallback");
        let model = local_model(&dir, b"the real weights", &["missing.bin", "wrong.bin", "source.bin"]);
        std::fs::write(dir.join("wrong.bin"), b"other weights!!!").unwrap();
        let root = dir.join("models");
        let path = fetch_one(&model, &root, Path::new(CURL)).unwrap();
        assert_eq!(std::fs::read(path).unwrap(), b"the real weights");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_stopped_download_resumes_from_the_part_file() {
        let dir = temp("resume");
        let bytes: Vec<u8> = (0..100_000u32).map(|i| (i % 251) as u8).collect();
        let model = local_model(&dir, &bytes, &["source.bin"]);
        let root = dir.join("models");
        std::fs::create_dir_all(root.join("test")).unwrap();
        std::fs::write(root.join("test/test.gguf.part"), &bytes[..30_000]).unwrap();
        assert_eq!(progress(&root, std::slice::from_ref(&model)), (30_000, 100_000));
        let path = fetch_one(&model, &root, Path::new(CURL)).unwrap();
        assert_eq!(std::fs::read(path).unwrap(), bytes);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_model_that_cannot_be_fetched_reports_why_and_leaves_nothing_named_as_the_model() {
        let dir = temp("fail");
        let mut model = local_model(&dir, b"weights", &["missing.bin"]);
        let root = dir.join("models");
        let error = fetch_one(&model, &root, Path::new(CURL)).unwrap_err();
        assert!(error.contains("試験のモデル"), "{error}");
        assert!(!root.join("test/test.gguf").exists());
        // 中身が表と違うものは置かない。
        model.urls = vec![format!("file://{}", dir.join("source.bin").display())];
        model.sha256 = "0".repeat(64);
        let error = fetch_one(&model, &root, Path::new(CURL)).unwrap_err();
        assert!(error.contains("SHA-256"), "{error}");
        assert!(!root.join("test/test.gguf").exists());
        assert!(!root.join("test/test.gguf.part").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 1 つ取れなくても、取れたものは置く(認識のモデルだけでも使えるように)。
    #[test]
    fn a_failed_model_does_not_stop_the_others() {
        let dir = temp("partial");
        let good = local_model(&dir, b"good weights", &["source.bin"]);
        let mut bad = good.clone();
        bad.id = "bad".to_string();
        bad.folder = "bad".to_string();
        bad.urls = vec![format!("file://{}", dir.join("missing.bin").display())];
        let root = dir.join("models");
        let error = fetch_all(&[bad, good.clone()], &root, Path::new(CURL)).unwrap_err();
        assert!(error.contains("試験のモデル"), "{error}");
        assert!(installed_in(&root, &good).is_some(), "取れた方は置く");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 表より大きい途中の分と、決まった名前の途中で切れたファイルは、取り直す。
    #[test]
    fn oversized_parts_and_truncated_files_are_replaced() {
        let dir = temp("replace");
        let model = local_model(&dir, b"exact weights", &["source.bin"]);
        let root = dir.join("models");
        std::fs::create_dir_all(root.join("test")).unwrap();
        std::fs::write(root.join("test/test.gguf.part"), b"exact weights and more garbage").unwrap();
        fetch_one(&model, &root, Path::new(CURL)).unwrap();
        std::fs::write(root.join("test/test.gguf"), b"exact").unwrap();
        assert!(installed_in(&root, &model).is_none(), "大きさの違うファイルは取得済みとしない");
        let path = fetch_one(&model, &root, Path::new(CURL)).unwrap();
        assert_eq!(std::fs::read(path).unwrap(), b"exact weights");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 取り消すと、動いている curl をその場で終わらせてから戻る(アプリの終了で curl が残らない)。
    #[test]
    fn cancelling_kills_the_running_curl_before_returning() {
        let dir = temp("cancel-running");
        let model = local_model(&dir, b"weights", &["source.bin"]);
        let pid_file = dir.join("curl.pid");
        let fake = dir.join("curl");
        std::fs::write(
            &fake,
            format!("#!/bin/sh\necho $$ > '{}'\nexec /bin/sleep 30\n", pid_file.display()),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        let cancel = Arc::new(Cancel::default());
        let root = dir.join("models");
        let worker = {
            let (cancel, fake, model) = (cancel.clone(), fake.clone(), model.clone());
            std::thread::spawn(move || fetch_all_with(std::slice::from_ref(&model), &root, &fake, &cancel))
        };
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let pid = loop {
            if let Some(pid) = std::fs::read_to_string(&pid_file)
                .ok()
                .and_then(|text| text.trim().parse::<i32>().ok())
            {
                break pid;
            }
            assert!(std::time::Instant::now() < deadline, "偽の curl が起動しない");
            std::thread::sleep(std::time::Duration::from_millis(20));
        };
        let started = std::time::Instant::now();
        cancel.stop();
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
        // stop が戻った時点で、curl はもう居ない(回収まで済んでいる)。
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1, "curl が残っている");
        let error = worker.join().unwrap().unwrap_err();
        assert!(error.contains("やめました"), "{error}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_cancelled_download_stops_before_the_next_model() {
        let dir = temp("cancel");
        let model = local_model(&dir, b"weights", &["source.bin"]);
        let cancel = Cancel::default();
        cancel.stop();
        let root = dir.join("models");
        let error = fetch_all_with(std::slice::from_ref(&model), &root, Path::new(CURL), &cancel).unwrap_err();
        assert!(error.contains("やめました"), "{error}");
        assert!(installed_in(&root, &model).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_disk_without_room_is_refused_before_downloading() {
        let dir = temp("space");
        let mut model = local_model(&dir, b"weights", &["source.bin"]);
        model.size = u64::MAX / 4;
        let error = fetch_all(&[model], &dir.join("models"), Path::new(CURL)).unwrap_err();
        assert!(error.contains("空き容量"), "{error}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
