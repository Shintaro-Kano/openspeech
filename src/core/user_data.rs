//! 保存する利用者のデータ一式(設定・辞書・学習)。

use super::dictionary::Dictionary;
use super::learning::CorrectionMemory;
use super::settings::UserSettings;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct UserData {
    pub settings: UserSettings,
    /// 辞書は 2026-11-07 から別ファイル(`dictionary.json`、手で編集できる)。user.json には書かず、
    /// 古い user.json にあれば読んで最初の辞書ファイルにする(`Store::load_user`)。
    #[serde(skip_serializing)]
    pub dictionary: Dictionary,
    /// 訂正の記憶(学習)。
    pub memory: CorrectionMemory,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn old_user_data_with_templates_and_learning_still_loads() {
        let json = r#"{"settings":{"history_count":10},"dictionary":{"words":[]},
            "templates":[{"reading":"x","body":"y","aliases":[],"confirm":false}],
            "learning":{"choices":{},"voice_seconds":3.5}}"#;
        let data: UserData = serde_json::from_str(json).unwrap();
        assert_eq!(data.settings.history_count, 10);
        assert!(data.memory.pairs.is_empty());
        assert!(data.dictionary.templates.is_empty());
    }
}
