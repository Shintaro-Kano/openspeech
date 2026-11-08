//! テンプレートの共有ライブラリ: 目録の取得(押したときだけ、別スレッド)、選んだ物の取り込み、
//! 投稿フォームを既定のブラウザで開く共有。アプリ自身は何も送らない。

use super::*;
use crate::core::template_library::{
    fetch_index, import_templates, parse_index, share_link, LibraryIndex, Pick, TEMPLATE_LIBRARY_INDEX,
};
use crate::ui::library::{LibraryLoad, LibraryWindow};
use std::process::{Command, Stdio};

/// 辞書の窓でテンプレートを編集している間は取り込まない(置き換えで番号がずれ、保存が別のテンプレートを上書きする)。
pub const IMPORT_WHILE_EDITING: &str =
    "辞書の窓で編集中のテンプレートを保存するか取りやめてから取り込んでください";

/// 目録を取る口(URL → 中身)。試験では偽物に差し替え、ネットワークを呼ばない。
pub type FetchFn = Arc<dyn Fn(&str) -> Result<String, String> + Send + Sync>;
/// 既定のブラウザで URL を開く口。
pub type OpenFn = Arc<dyn Fn(&str) -> Result<(), String> + Send + Sync>;
/// クリップボードに文を入れる口。
pub type CopyFn = Arc<dyn Fn(&str) -> Result<(), String> + Send + Sync>;

/// 共有ライブラリの口と、取得中の目録。
pub(super) struct LibraryState {
    fetch: FetchFn,
    open: OpenFn,
    copy: CopyFn,
    /// 取得中の目録の答え。
    rx: Option<Receiver<Result<LibraryIndex, String>>>,
}

impl LibraryState {
    pub(super) fn new() -> Self {
        LibraryState {
            fetch: Arc::new(|url| fetch_index(std::path::Path::new(models::CURL), url)),
            open: Arc::new(open_in_browser),
            copy: Arc::new(copy_to_clipboard),
            rx: None,
        }
    }
}

/// 既定のブラウザで開く(`/usr/bin/open`。シェルを通さない)。
fn open_in_browser(url: &str) -> Result<(), String> {
    let status = Command::new("/usr/bin/open")
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|e| format!("ブラウザを開けません: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err("ブラウザを開けません".to_string())
    }
}

fn copy_to_clipboard(text: &str) -> Result<(), String> {
    arboard::Clipboard::new()
        .and_then(|mut clipboard| clipboard.set_text(text.to_string()))
        .map_err(|e| format!("クリップボードに書けません: {e}"))
}

impl VoiceApp {
    /// 共有ライブラリの口を差し替える(試験は偽物を渡す)。
    pub fn set_library_hooks(&mut self, fetch: FetchFn, open: OpenFn, copy: CopyFn) {
        self.library.fetch = fetch;
        self.library.open = open;
        self.library.copy = copy;
    }

    /// 「共有ライブラリから取り込む…」: 窓を開き、目録を取りに行く。
    pub fn open_template_library(&mut self) {
        self.windows.library = Some(LibraryWindow::default());
        self.start_library_fetch();
    }

    /// 目録を別スレッドで取る(窓の「もう一度」も)。前の取得の答えは捨てる。
    pub fn start_library_fetch(&mut self) {
        if let Some(window) = &mut self.windows.library {
            window.load = LibraryLoad::Loading;
        }
        let (tx, rx) = mpsc::channel();
        self.library.rx = Some(rx);
        let fetch = self.library.fetch.clone();
        let ctx = self.ctx.clone();
        diag::log("共有ライブラリ: 目録を取りに行く");
        std::thread::spawn(move || {
            let result = fetch(TEMPLATE_LIBRARY_INDEX).and_then(|text| parse_index(&text));
            let _ = tx.send(result);
            ctx.request_repaint();
        });
    }

    /// 取得の答えを窓に渡す。失敗は知らせだけ。
    pub(super) fn receive_library(&mut self) {
        let Some(result) = self.library.rx.as_ref().and_then(|rx| rx.try_recv().ok()) else {
            return;
        };
        self.library.rx = None;
        let load = match result {
            Ok(index) => {
                diag::log(format!(
                    "共有ライブラリ: {} 件(読めなかった物 {} 件)",
                    index.templates.len(),
                    index.skipped.len()
                ));
                LibraryLoad::Ready(index)
            }
            Err(message) => {
                diag::log(format!("共有ライブラリ: 取得できない({message})"));
                // 窓を閉じたあとに届いた失敗は知らせない。
                if self.windows.library.is_some() {
                    self.notify_error(message.clone());
                }
                LibraryLoad::Failed(message)
            }
        };
        if let Some(window) = &mut self.windows.library {
            window.load = load;
        }
    }

    /// 辞書の窓でテンプレートを編集しているか(その間は取り込まない)。
    pub fn editing_a_template(&self) -> bool {
        self.windows.dict.as_ref().is_some_and(|dict| dict.editing.is_some())
    }

    /// 選んだテンプレートを辞書に足し、保存して知らせる。知らせた文と、入れた件数(足した + 置き換えた)を返す。
    /// 辞書の窓で編集中なら何も取り込まない。
    pub fn import_library_templates(&mut self, picks: Vec<Pick>) -> (String, usize) {
        if self.editing_a_template() {
            self.notify_error(IMPORT_WHILE_EDITING);
            return (IMPORT_WHILE_EDITING.to_string(), 0);
        }
        let summary = import_templates(&mut self.data.dictionary, picks);
        if summary.added + summary.replaced > 0 {
            self.save_data();
            self.refresh_engine_options();
        }
        let message = summary.message(self.data.dictionary.templates.len());
        diag::log(format!(
            "共有ライブラリ: 取り込み {} 件、置き換え {} 件、飛ばした {} 件、取り込めない {} 件",
            summary.added,
            summary.replaced,
            summary.skipped,
            summary.failed.len()
        ));
        let imported = summary.added + summary.replaced;
        if imported == 0 && !summary.failed.is_empty() {
            self.notify_error(message.clone());
        } else {
            self.notify(message.clone());
        }
        (message, imported)
    }

    /// 「共有…」: その 1 件で投稿フォームの URL を組み、既定のブラウザで開く。URL に入りきらない本文と
    /// モジュールの文はクリップボードに入れる。知らせた文(失敗なら Err)を返す。
    pub fn share_template(&mut self, index: usize) -> Result<String, String> {
        let Some(template) = self.data.dictionary.templates.get(index) else {
            return Err("テンプレートが見つかりません".to_string());
        };
        let link = share_link(template);
        let result = link
            .clipboard
            .as_deref()
            .map_or(Ok(()), |text| (self.library.copy)(text))
            .and_then(|()| (self.library.open)(&link.url))
            .map(|()| {
                if link.clipboard.is_some() {
                    "本文と所見モジュールはクリップボードにあります。区切り線の上を「正常所見」、下を「所見モジュール」の欄に貼り付けてください".to_string()
                } else {
                    "投稿フォームをブラウザで開きました。内容を確かめて送信してください".to_string()
                }
            });
        diag::log(format!(
            "共有: 投稿フォームを開く(URL {} 字{})",
            link.url.len(),
            if link.clipboard.is_some() { "、本文はクリップボード" } else { "" }
        ));
        match &result {
            Ok(message) => self.notify(message.clone()),
            Err(message) => self.notify_error(message.clone()),
        }
        result
    }
}
