//! 保存と、利用者が手で書き換えるファイル(guidance.md・dictionary.json)の見張り。

use super::*;

/// 利用者が手で書き換えるファイル(guidance.md・dictionary.json)の見張りの状態。
pub(super) struct FileWatch {
    /// 訂正役に渡した補足のファイルの更新時刻。無ければ None。
    pub(super) guidance_modified: Option<SystemTime>,
    /// 辞書ファイルの更新時刻(開いた時点、または最後に読んだ・書いた時点)。
    pub(super) dictionary_modified: Option<SystemTime>,
    /// 最後にファイルへ書いた(または読んだ)辞書。変わったときだけ書くための控え。
    pub(super) saved_dictionary: Dictionary,
    /// 最後に補足と辞書のファイルを見た時刻(どちらも `GUIDANCE_CHECK_INTERVAL` ごとに見る)。
    pub(super) checked_at: Option<Instant>,
}

impl VoiceApp {
    // ---- 保存 ----

    pub fn save_data(&mut self) {
        if let Err(message) = self.store.save_user(&self.data) {
            self.notify_error(message);
        }
        // 辞書は手で編集するファイルなので、変わったときだけ書く(学習のたびに書き換えない)。
        if self.data.dictionary != self.files.saved_dictionary {
            self.write_dictionary();
        }
    }

    /// 画面で変えた辞書をファイルへ書く。見張り(`watch_dictionary`)が読む前に外で変わっていたファイルと、
    /// 読めないファイル(編集の途中)は、上書きせずに日時付きの名前で残してから書く。
    pub(super) fn write_dictionary(&mut self) {
        let on_disk = self.store.dictionary_modified();
        let changed_outside = on_disk.is_some()
            && (on_disk != self.files.dictionary_modified || self.store.read_dictionary().is_err());
        if changed_outside {
            match self.store.set_dictionary_aside() {
                Ok(kept) => {
                    let name = kept
                        .file_name()
                        .map_or_else(String::new, |n| n.to_string_lossy().to_string());
                    diag::log(format!("辞書ファイルが外で変わっていたので {name} に残した"));
                    self.notify(format!(
                        "辞書ファイルが外で変わっていたので、その内容を {name} に残して画面の辞書で書き直しました"
                    ));
                }
                Err(message) => {
                    self.notify_error(format!("{message}。辞書は保存していません"));
                    return;
                }
            }
        }
        match self.store.save_dictionary(&self.data.dictionary) {
            Ok(()) => {
                self.files.saved_dictionary = self.data.dictionary.clone();
                self.files.dictionary_modified = self.store.dictionary_modified();
            }
            Err(message) => self.notify_error(message),
        }
    }

    pub fn save_global(&mut self) {
        if let Err(message) = self.store.save_global(&self.global) {
            self.notify_error(message);
        }
    }

    /// 補足のファイルが変わっていたら訂正役を作り直す。時刻を受けるのは、試験で見る周期を進めるため。
    /// 利用者が手で編集するファイル(AI への補足・辞書)の更新を見張り、変わっていれば読み直す。
    pub fn watch_files(&mut self, now: Instant) {
        if self
            .files.checked_at
            .is_some_and(|at| now.saturating_duration_since(at) < GUIDANCE_CHECK_INTERVAL)
        {
            return;
        }
        self.files.checked_at = Some(now);
        self.watch_guidance();
        self.watch_dictionary();
    }

    pub(super) fn watch_guidance(&mut self) {
        let modified = self.store.guidance_modified();
        if modified == self.files.guidance_modified {
            return;
        }
        self.files.guidance_modified = modified;
        self.guidance_chars = guidance_length(&self.store);
        diag::log(format!(
            "AI への指示の更新を検知({})",
            self.guidance_chars
                .map_or("読めない".to_string(), |chars| format!("{chars} 字"))
        ));
        self.rebuild_corrector();
    }

    /// 辞書ファイルが外で書き換えられたら読み直し、認識エンジンの語と AI の用語集を作り直す。
    /// 読めない間は今の辞書のまま続け、消されていれば今の辞書で作り直す。
    pub(super) fn watch_dictionary(&mut self) {
        let modified = self.store.dictionary_modified();
        if modified == self.files.dictionary_modified {
            return;
        }
        self.files.dictionary_modified = modified;
        match self.store.read_dictionary() {
            Ok(Some(dictionary)) => {
                if dictionary == self.data.dictionary {
                    return;
                }
                self.data.dictionary = dictionary.clone();
                self.files.saved_dictionary = dictionary;
                diag::log(format!(
                    "辞書ファイルの更新を検知({})",
                    dictionary_summary(&self.data.dictionary)
                ));
                // 辞書の画面で直していたテンプレートの番号は、読み直した辞書では別の物を指しうる。
                if let Some(tool) = self.windows.dict.as_mut() {
                    if tool.editing.take().is_some() {
                        self.notify("辞書ファイルが変わったので、編集中だったテンプレートは新規の登録に戻しました");
                    }
                }
                self.refresh_engine_options();
            }
            Ok(None) => {
                if let Err(message) = self.store.save_dictionary(&self.data.dictionary) {
                    self.notify_error(message);
                }
                self.files.dictionary_modified = self.store.dictionary_modified();
            }
            Err(reason) => {
                diag::log(format!("辞書ファイルを読めません(今の辞書のまま): {reason}"));
                self.notify_error(format!("辞書ファイルを読めません: {reason}"));
            }
        }
    }
}
