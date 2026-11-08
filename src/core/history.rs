//! 転送履歴。日別ファイルにも書き出す。
//! 置き場は `<データ>/history/YYYY-MM-DD.log`。1 件は `HH:MM:SS` の行、本文、空行(UTF-8、改行は LF)。
//! 本文の中の、空白だけの行(空行を含む)は空白を 1 つ足して書き、読むときに 1 つ減らす(見出しの前の空行と
//! 取り違えず、本文は元のまま戻る)。

use chrono::{Local, NaiveDate};
use std::collections::VecDeque;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq)]
pub struct HistoryEntry {
    pub date: NaiveDate,
    pub time: String,
    pub text: String,
}

impl HistoryEntry {
    pub fn title(&self) -> String {
        let first_line = self.text.lines().next().unwrap_or("");
        let preview: String = first_line.chars().take(40).collect();
        format!("{} {} {preview}", self.date.format("%m/%d"), self.time)
    }
}

#[derive(Debug, Clone, Default)]
pub struct History {
    /// 新しいものが先頭。
    pub entries: VecDeque<HistoryEntry>,
    pub capacity: usize,
}

/// 日別ファイル 1 つ。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct DayFile {
    date: NaiveDate,
    path: PathBuf,
    size: u64,
}

impl History {
    pub fn new(capacity: usize) -> Self {
        History {
            entries: VecDeque::new(),
            capacity,
        }
    }

    pub fn push(&mut self, entry: HistoryEntry) {
        self.entries.push_front(entry);
        self.entries.truncate(self.capacity);
    }

    pub fn set_capacity(&mut self, capacity: usize) {
        self.capacity = capacity;
        self.entries.truncate(capacity);
    }

    pub fn latest(&self) -> Option<&HistoryEntry> {
        self.entries.front()
    }

    /// 日別ファイルから、新しいものを先に読み戻す。
    pub fn load(folder: &Path, capacity: usize) -> Self {
        let mut history = History::new(capacity);
        let files = day_files(folder);
        for file in files.iter().rev() {
            if history.entries.len() >= capacity {
                break;
            }
            let bytes = std::fs::read(&file.path).unwrap_or_default();
            // UTF-8 だけ(途中で切れた字があっても全体を別の符号で読み直さない)。
            let mut entries = parse_log(&String::from_utf8_lossy(&bytes), file.date);
            entries.reverse();
            for entry in entries {
                if history.entries.len() < capacity {
                    history.entries.push_back(entry);
                }
            }
        }
        history
    }
}

/// 日別ファイルを残す日数。これより古い日のファイルは消す(読影文が平文で残るため、無期限にしない)。
pub const KEEP_DAYS: i64 = 90;
/// 日別ファイルの合計の上限。超えたら古い日から消す。
pub const KEEP_BYTES: u64 = 5 * 1024 * 1024;

/// そのフォルダの日別ファイル(古い順)。決まった名前と拡張子 `.log` のものだけ。
fn day_files(folder: &Path) -> Vec<DayFile> {
    let mut files: Vec<DayFile> = std::fs::read_dir(folder)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter_map(|path| {
            let date = date_of_file(&path)?;
            let size = std::fs::metadata(&path).ok().filter(|m| m.is_file())?.len();
            Some(DayFile { date, path, size })
        })
        .collect();
    files.sort();
    files
}

/// 古い日別ファイルを消す。`KEEP_DAYS` より前の日と、合計が `KEEP_BYTES` を超える分
/// (古い日から)。今日の分は残す。消した数を返す。
pub fn prune(folder: &Path, today: NaiveDate) -> usize {
    let mut files = day_files(folder);
    // 時計がずれてできた未来の日付のファイルは、容量の上限で最初に消す(今日の分より先に)。
    files.sort_by_key(|file| (file.date <= today, file.date));
    let oldest_kept = today - chrono::Duration::days(KEEP_DAYS);
    let mut total: u64 = files.iter().map(|file| file.size).sum();
    let mut removed = 0;
    for file in &files {
        let too_old = file.date < oldest_kept;
        let too_big = total > KEEP_BYTES && file.date != today;
        if (too_old || too_big) && std::fs::remove_file(&file.path).is_ok() {
            total -= file.size;
            removed += 1;
        }
    }
    removed
}

/// 日別ファイルをすべて消す(設定の「転送履歴をすべて消す」)。消した数を返す。
/// 消せないファイルがあっても残りは消し、最後に失敗を返す。
pub fn clear(folder: &Path) -> Result<usize, String> {
    let mut removed = 0;
    let mut failure = None;
    for file in day_files(folder) {
        match std::fs::remove_file(&file.path) {
            Ok(()) => removed += 1,
            Err(error) => failure = Some(format!("転送履歴を消せません: {error}")),
        }
    }
    match failure {
        Some(message) => Err(message),
        None => Ok(removed),
    }
}

fn date_of_file(path: &Path) -> Option<NaiveDate> {
    if path.extension()?.to_str()? != "log" {
        return None;
    }
    let stem = path.file_stem()?.to_str()?;
    // 書き戻して同じ名前になるものだけ(`2026-1-8`・前後の空白などは日別ファイルとみなさない)。
    let date = NaiveDate::parse_from_str(stem, "%Y-%m-%d").ok()?;
    (date.format("%Y-%m-%d").to_string() == stem).then_some(date)
}

/// `HH:MM:SS` の形か。
fn is_time(text: &str) -> bool {
    text.len() == 8
        && text.bytes().enumerate().all(|(i, b)| {
            if i == 2 || i == 5 {
                b == b':'
            } else {
                b.is_ascii_digit()
            }
        })
}

/// このアプリの日別ファイルを項目に分ける。見出しは、ファイルの頭か空行の直後にある `HH:MM:SS` だけの行。
/// 本文の中の空白だけの行は空白を 1 つ足して書いてある(`append_log`)ので、1 つ減らす。
pub fn parse_log(text: &str, date: NaiveDate) -> Vec<HistoryEntry> {
    // (項目, 本文の行, まだ本文と決まっていない空行の数)
    let mut entries: Vec<(HistoryEntry, Vec<String>, usize)> = Vec::new();
    let mut after_blank = true;
    for line in text.split('\n') {
        // 手で直して CRLF になったファイルも読む(このアプリは LF でしか書かない)。
        let line = line.strip_suffix('\r').unwrap_or(line);
        if after_blank && is_time(line) {
            let entry = HistoryEntry {
                date,
                time: line.to_string(),
                text: String::new(),
            };
            entries.push((entry, Vec::new(), 0));
        } else if let Some((_, lines, blanks)) = entries.last_mut() {
            if line.is_empty() {
                // 項目の終わりの空行(とファイルの終わり)。続けて見出しでない行が来たら(壊れたファイル)、
                // 本文の空行として足す。
                *blanks += 1;
            } else {
                lines.extend(std::iter::repeat_n(String::new(), *blanks));
                *blanks = 0;
                if line.bytes().all(|b| b == b' ') {
                    lines.push(line[1..].to_string());
                } else {
                    lines.push(line.to_string());
                }
            }
        }
        after_blank = line.is_empty();
    }
    entries
        .into_iter()
        .map(|(mut entry, lines, _)| {
            entry.text = lines.join("\n");
            entry
        })
        .collect()
}

/// 履歴に 1 件追記する(新しい置き場)。
pub fn append_log(folder: &Path, entry: &HistoryEntry) -> std::io::Result<()> {
    std::fs::create_dir_all(folder)?;
    let path = folder.join(format!("{}.log", entry.date.format("%Y-%m-%d")));
    // 前の書き込みが途中で切れていても、次の項目が前の本文に混ざらないよう、区切りの空行を補う。
    let lead = match std::fs::read(&path) {
        Ok(bytes) if bytes.is_empty() || bytes.ends_with(b"\n\n") => "",
        Ok(bytes) if bytes.ends_with(b"\n") => "\n",
        Ok(_) => "\n\n",
        Err(_) => "",
    };
    // 本文の改行は LF にそろえ、空白だけの行(空行を含む)は空白を 1 つ足して、見出しの前の空行と区別する。
    let lines: Vec<String> = entry
        .text
        .replace("\r\n", "\n")
        .replace('\r', "\n")
        .split('\n')
        .map(|line| {
            if line.bytes().all(|b| b == b' ') {
                format!(" {line}")
            } else {
                line.to_string()
            }
        })
        .collect();
    let record = format!("{lead}{}\n{}\n\n", entry.time, lines.join("\n"));
    // 1 項目を 1 回で書く。
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    file.write_all(record.as_bytes())
}

pub fn new_entry(text: &str) -> HistoryEntry {
    let now = Local::now();
    HistoryEntry {
        date: now.date_naive(),
        time: now.format("%H:%M:%S").to_string(),
        text: text.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn day_folder(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("openspeech-history-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn day(text: &str) -> NaiveDate {
        NaiveDate::parse_from_str(text, "%Y-%m-%d").unwrap()
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        names.sort();
        names
    }

    /// 90 日より前の日と、5 MB を超える分(古い日から)を消す。今日の分と、ほかのファイルは消さない。
    #[test]
    fn old_and_oversized_day_files_are_pruned() {
        let dir = day_folder("prune");
        let today = day("2026-11-08");
        let write = |date: &str, bytes: usize| {
            std::fs::write(dir.join(format!("{date}.log")), vec![b'x'; bytes]).unwrap();
        };
        write("2026-08-01", 10);
        write("2026-08-10", 10);
        write("2026-11-01", 3 * 1024 * 1024);
        write("2026-11-07", 3 * 1024 * 1024);
        write("2026-11-08", 10);
        std::fs::write(dir.join("memo.txt"), b"keep").unwrap();
        std::fs::write(dir.join("2026-01-01.txt"), b"keep").unwrap();
        assert_eq!(prune(&dir, today), 3);
        assert_eq!(
            names(&dir),
            ["2026-01-01.txt", "2026-11-07.log", "2026-11-08.log", "memo.txt"]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// ちょうど 90 日前の日は残し、その前の日は消す。容量が小さくても日数で消える。
    #[test]
    fn the_ninetieth_day_is_kept_and_the_one_before_is_not() {
        let dir = day_folder("prune-days");
        for date in ["2026-08-09", "2026-08-10", "2026-11-08"] {
            std::fs::write(dir.join(format!("{date}.log")), b"x").unwrap();
        }
        assert_eq!(prune(&dir, day("2026-11-08")), 1);
        assert_eq!(names(&dir), ["2026-08-10.log", "2026-11-08.log"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 今日の 1 本だけで上限を超えても、今日の分は消さない。
    #[test]
    fn todays_file_is_kept_even_beyond_the_size_limit() {
        let dir = day_folder("prune-today");
        std::fs::write(dir.join("2026-11-08.log"), vec![b'x'; 6 * 1024 * 1024]).unwrap();
        assert_eq!(prune(&dir, day("2026-11-08")), 0);
        assert_eq!(names(&dir), ["2026-11-08.log"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 「すべて消す」は日別ファイルだけを消す。
    #[test]
    fn clearing_removes_the_day_files_only() {
        let dir = day_folder("clear");
        std::fs::write(dir.join("2026-11-08.log"), b"x").unwrap();
        std::fs::write(dir.join("2026-11-07.log"), b"x").unwrap();
        std::fs::write(dir.join("memo.txt"), b"keep").unwrap();
        std::fs::write(dir.join("2026-11-06.log~"), b"keep").unwrap();
        assert_eq!(clear(&dir).unwrap(), 2);
        assert_eq!(names(&dir), ["2026-11-06.log~", "memo.txt"]);
        assert_eq!(History::load(&dir, 20).entries.len(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn date() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 7, 7).unwrap()
    }

    #[test]
    fn newest_comes_first_and_capacity_holds() {
        let mut history = History::new(2);
        for i in 0..3 {
            history.push(new_entry(&format!("t{i}")));
        }
        assert_eq!(history.entries.len(), 2);
        assert_eq!(history.latest().unwrap().text, "t2");
    }

    /// 書いた形をそのまま読み戻せる。本文の空行や、時刻だけの行があっても項目を取り違えない。
    #[test]
    fn saved_entries_load_back_newest_first() {
        let dir = day_folder("roundtrip");
        let mut entry = new_entry("一件目");
        entry.date = date();
        entry.time = "10:00:00".to_string();
        append_log(&dir, &entry).unwrap();
        entry.text = "二件目\n\n12:00:00\n続き".to_string();
        entry.time = "11:00:00".to_string();
        append_log(&dir, &entry).unwrap();
        assert_eq!(names(&dir), ["2026-07-07.log"]);
        let written = std::fs::read_to_string(dir.join("2026-07-07.log")).unwrap();
        assert!(written.starts_with("10:00:00\n一件目\n\n11:00:00\n"), "{written:?}");
        let history = History::load(&dir, 10);
        assert_eq!(history.entries.len(), 2);
        assert_eq!(history.entries[0].text, "二件目\n\n12:00:00\n続き");
        // 空白だけの行・末尾の改行・CR だけの改行も読み戻せる(CR は LF になる)。
        for text in ["a\n \nb", "abc\n", " ", "", "a\n\n\n", "A\n\r\r\n12:00:00\nB", "  x\n   "] {
            let mut entry = new_entry(text);
            entry.date = date();
            entry.time = "12:00:00".to_string();
            append_log(&dir, &entry).unwrap();
            let back = History::load(&dir, 1);
            let expected = text.replace("\r\n", "\n").replace('\r', "\n");
            assert_eq!(back.entries[0].text, expected, "{text:?}");
        }
        assert_eq!(history.entries[1].text, "一件目");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 名前が決まった形どおりのものだけが日別ファイル。未来の日付のものは容量の上限で先に消える。
    #[test]
    fn only_exact_names_count_and_future_files_go_first() {
        let dir = day_folder("names");
        for name in ["2026-1-8.log", " 2026-11-08.log", "+2026-11-08.log"] {
            std::fs::write(dir.join(name), b"x").unwrap();
        }
        assert_eq!(clear(&dir).unwrap(), 0);
        std::fs::write(dir.join("2027-01-01.log"), vec![b'x'; 5 * 1024 * 1024]).unwrap();
        std::fs::write(dir.join("2026-11-01.log"), b"x").unwrap();
        assert_eq!(prune(&dir, day("2026-11-08")), 1);
        assert!(dir.join("2026-11-01.log").is_file());
        assert!(!dir.join("2027-01-01.log").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 前の書き込みが途中で切れていても、次の項目は別の項目として読める。字が切れていても他は読める。
    #[test]
    fn a_cut_off_entry_does_not_swallow_the_next_one() {
        let dir = day_folder("cut");
        let mut bytes = "10:00:00\n右肺上葉".as_bytes().to_vec();
        bytes.extend_from_slice(&"結".as_bytes()[..2]);
        std::fs::write(dir.join("2026-07-07.log"), bytes).unwrap();
        let mut entry = new_entry("次の件");
        entry.date = date();
        entry.time = "11:00:00".to_string();
        append_log(&dir, &entry).unwrap();
        let history = History::load(&dir, 10);
        assert_eq!(history.entries.len(), 2);
        assert_eq!(history.entries[0].text, "次の件");
        assert!(history.entries[1].text.starts_with("右肺上葉"), "{:?}", history.entries[1].text);
        let _ = std::fs::remove_dir_all(&dir);
    }

}
