//! 動作の記録。不具合の切り分けのため、録音・切り出し・認識・訂正の節目をファイルに書き残す。
//! 話した文章そのものは書かない(字数だけ)。保存先は保存フォルダの logs/voice.log で、
//! 1MB を超えたら前の分を voice.old に回す。記録先を決めるまでは何も書かない(診断コマンドや試験では黙る)。

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

const LIMIT_BYTES: u64 = 1024 * 1024;

static PATH: OnceLock<Mutex<Option<PathBuf>>> = OnceLock::new();
/// 書き込みは 1 本ずつ。大きさを見てから回すまでの間に別のスレッドが入ると、回したばかりの新しい分を回してしまう。
static WRITING: Mutex<()> = Mutex::new(());

fn slot() -> &'static Mutex<Option<PathBuf>> {
    PATH.get_or_init(|| Mutex::new(None))
}

/// 記録先を決める。`None` で記録をやめる。
pub fn set_path(path: Option<PathBuf>) {
    if let Ok(mut slot) = slot().lock() {
        *slot = path;
    }
}

pub fn path() -> Option<PathBuf> {
    slot().lock().ok().and_then(|slot| slot.clone())
}

/// 1 行書き残す。失敗しても黙って続ける(記録のために本体を止めない)。
pub fn log(message: impl AsRef<str>) {
    let Some(path) = path() else {
        return;
    };
    let line = format!(
        "{} {}\n",
        chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.3f"),
        message.as_ref()
    );
    let _ = append(&path, &line);
}

fn append(path: &Path, line: &str) -> std::io::Result<()> {
    let _one_at_a_time = WRITING.lock();
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    if fs::metadata(path).is_ok_and(|meta| meta.len() > LIMIT_BYTES) {
        let _ = fs::rename(path, path.with_extension("old"));
    }
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    file.write_all(line.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 記録先は全体で 1 つなので、同時に走る他の試験の行が混ざる。自分の印の付いた行だけを見る。
    #[test]
    fn lines_are_appended_with_a_time_and_rotated_at_the_limit() {
        let dir = std::env::temp_dir().join(format!("voice-diag-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let path = dir.join("logs").join("voice.log");
        set_path(Some(path.clone()));
        log("一行目");
        log("二行目");
        let text = fs::read_to_string(&path).unwrap();
        let mine: Vec<&str> = text.lines().filter(|l| l.contains("行目")).collect();
        assert_eq!(mine.len(), 2, "{text}");
        assert!(mine[0].ends_with("一行目"));
        assert!(mine[0].starts_with("20"), "{text}");
        fs::write(&path, vec![b'x'; LIMIT_BYTES as usize + 1]).unwrap();
        log("回した後");
        assert!(path.with_extension("old").exists());
        let text = fs::read_to_string(&path).unwrap();
        assert!(
            text.contains("回した後") && !text.contains("一行目"),
            "{text}"
        );
        set_path(None);
        log("記録先なし");
        assert!(!fs::read_to_string(&path).unwrap().contains("記録先なし"));
        let _ = fs::remove_dir_all(&dir);
    }
}
