//! 設定。使い方の設定(user.json)と、この Mac の機器の設定(global.json)に分かれる。
//! 以前の版の JSON もそのまま読める(無い項目は既定値、知らない項目は無視)。
//! 読めない JSON は設定全体が既定値に戻るので、列挙の値を消すときは `alias` で古い名前を受ける。

use super::text_filter::FilterSettings;
use super::voice_command::{default_command_words, deserialize_known_command_words, CommandWords};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// 画面の明暗。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum ThemeChoice {
    #[default]
    System,
    Light,
    Dark,
}

impl ThemeChoice {
    pub const ALL: [ThemeChoice; 3] = [Self::System, Self::Light, Self::Dark];

    pub fn label(self) -> &'static str {
        match self {
            Self::System => "Mac の設定に合わせる",
            Self::Light => "ライト",
            Self::Dark => "ダーク",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppearanceSettings {
    pub theme: ThemeChoice,
    /// 文字と部品の拡大率。
    pub scale: f32,
    /// 起動時にバーだけの小さな表示にする。
    pub compact_on_start: bool,
}

impl Default for AppearanceSettings {
    fn default() -> Self {
        AppearanceSettings {
            theme: ThemeChoice::System,
            scale: 1.0,
            compact_on_start: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct EditorSettings {
    pub font_size: f32,
    pub confirm_close: bool,
    pub volume_warning: bool,
    pub show_return: bool,
    pub top_most: bool,
}

impl Default for EditorSettings {
    fn default() -> Self {
        EditorSettings {
            font_size: 20.0,
            confirm_close: true,
            volume_warning: true,
            show_return: true,
            top_most: false,
        }
    }
}

/// 利用者が選んだキーの組み合わせ。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyCombo {
    /// Mac のキーコード。
    pub key_code: u16,
    pub command: bool,
    pub control: bool,
    pub option: bool,
    pub shift: bool,
    /// 画面に出す名前(押した時点のキーボード配列での呼び名)。
    pub label: String,
}

/// 録音を切り替えるキー。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum RecordShortcut {
    /// Caps Lock がオンの間は認識し、オフの間は止める。
    #[default]
    CapsLock,
    Key(KeyCombo),
}

impl RecordShortcut {
    pub fn label(&self) -> &str {
        match self {
            RecordShortcut::CapsLock => "Caps Lock",
            RecordShortcut::Key(combo) => &combo.label,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RecordSettings {
    pub shortcut: RecordShortcut,
    /// true: 押している間 ON、false: 押すごとに切り替え(Caps Lock では使わない)。
    pub hold: bool,
}

/// 切り出しの設定。感度と「周囲の話し声」の段は 2026-11-08 に外した(声の判定役が入り、
/// どちらも何を変えるのか分かりにくい割に効きが薄い。保存データに残っていても無視する)。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct EngineSettings {
    /// 話し終わりと判断する無音の長さ(ミリ秒)。短いほど速く出るが、文の途中で切れやすい。
    pub post_silence_ms: u32,
}

impl Default for EngineSettings {
    fn default() -> Self {
        EngineSettings {
            post_silence_ms: 600,
        }
    }
}

/// 無音の長さの選べる範囲(ミリ秒)。
pub const POST_SILENCE_RANGE: std::ops::RangeInclusive<u32> = 300..=1200;
/// マイクの増幅の選べる範囲(倍)。
pub const MIC_GAIN_RANGE: std::ops::RangeInclusive<f32> = 0.5..=8.0;

/// 文章を入力先へ届ける方法。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum TransferMethod {
    /// アクセシビリティ経由で直接差し込み、できなければ貼り付け。
    #[default]
    Auto,
    /// クリップボードに入れて ⌘V。
    Paste,
    /// 1 文字ずつキー入力として送る(貼り付けを受け付けないアプリ向け)。
    Type,
}

impl TransferMethod {
    pub const ALL: [TransferMethod; 3] = [Self::Auto, Self::Paste, Self::Type];

    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => "自動(直接差し込み → 貼り付け)",
            Self::Paste => "貼り付け(⌘V)",
            Self::Type => "キー入力",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TransferSettings {
    pub method: TransferMethod,
    /// 貼り付けに使ったあと、元のクリップボードの文字を戻す。
    pub restore_clipboard: bool,
    /// 送ったあとに Enter を押す(チャット欄など)。
    pub enter_after: bool,
}

impl Default for TransferSettings {
    fn default() -> Self {
        TransferSettings {
            method: TransferMethod::Auto,
            restore_clipboard: true,
            enter_after: false,
        }
    }
}

/// 学習(訂正の記憶)の設定。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LearningSettings {
    /// AI 訂正と手直しから「誤認識 → 正しい語」を覚える。
    pub enabled: bool,
    /// 覚えた置き換えを、AI 訂正を待たずに認識直後の文へ当てる。
    pub auto_apply: bool,
    /// 自動で当てるのに必要な回数(手直しは 1 回を 3 回分と数える)。
    pub min_count: u32,
}

impl Default for LearningSettings {
    fn default() -> Self {
        LearningSettings {
            enabled: true,
            auto_apply: true,
            min_count: 2,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct UserSettings {
    pub filters: FilterSettings,
    pub editor: EditorSettings,
    pub appearance: AppearanceSettings,
    pub record: RecordSettings,
    pub engine: EngineSettings,
    pub transfer: TransferSettings,
    /// 音声コマンドの言葉(操作ごと、既定は空)。以前の `voice_commands` とは形が違うので名前も変えた。
    /// 外した操作(タブ・最初に戻る・最後に進む)の行が残っていても読み飛ばし、設定全体を失わない。
    #[serde(deserialize_with = "deserialize_known_command_words")]
    pub command_words: Vec<CommandWords>,
    /// Claude を呼ぶ言葉(テンプレート補完の入口)。既定は「ヘイクロード」の 2 表記(カタカナ・ひらがな)。空なら呼べない。
    pub call_words: Vec<String>,
    pub learning: LearningSettings,
    pub history_count: usize,
    pub save_history: bool,
    pub auto_backup: bool,
}

impl Default for UserSettings {
    fn default() -> Self {
        UserSettings {
            filters: FilterSettings::default(),
            editor: EditorSettings::default(),
            appearance: AppearanceSettings::default(),
            record: RecordSettings::default(),
            engine: EngineSettings::default(),
            transfer: TransferSettings::default(),
            command_words: default_command_words(),
            call_words: super::assistant::default_call_words(),
            learning: LearningSettings::default(),
            history_count: 20,
            save_history: true,
            auto_backup: true,
        }
    }
}

/// 保存履歴数の選択肢。
pub const HISTORY_COUNTS: [usize; 6] = [5, 10, 20, 50, 100, 200];

/// 音声保存の設定。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct VoiceLogSettings {
    pub enabled: bool,
    pub folder: Option<PathBuf>,
    /// 保存量の上限(MB)。
    pub limit_mb: u64,
}

impl Default for VoiceLogSettings {
    fn default() -> Self {
        VoiceLogSettings {
            // 医療の音声が残るため既定は無効。
            enabled: false,
            folder: None,
            limit_mb: 20,
        }
    }
}

/// 認識結果の訂正役(Claude)の設定。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CorrectorSettings {
    /// 認識結果を Claude で訂正する。
    pub enabled: bool,
    /// 訂正役のモデル ID。
    pub model: String,
    /// Claude への接続方法。
    pub backend: CorrectorBackend,
}

/// Claude への接続方法。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum CorrectorBackend {
    /// API キーで直接送る。商用の条件で扱われ、学習には使われない。
    #[default]
    ApiKey,
    /// この Mac の Claude Code にログイン済みのサブスクで送る。個人向けの条件で扱われる。
    ClaudeCode,
}

impl CorrectorBackend {
    pub const ALL: [CorrectorBackend; 2] = [Self::ApiKey, Self::ClaudeCode];

    pub fn label(self) -> &'static str {
        match self {
            Self::ApiKey => "API キー",
            Self::ClaudeCode => "Claude Code(サブスクのログイン)",
        }
    }
}

impl Default for CorrectorSettings {
    fn default() -> Self {
        CorrectorSettings {
            enabled: true,
            model: super::corrector::DEFAULT_MODEL.to_string(),
            backend: CorrectorBackend::ApiKey,
        }
    }
}


/// 認識エンジンの種類。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum RecognizerKind {
    /// 既定。Haas-ja v1.0(speech.cpp、Metal)。1.0 より前の版で選べた「自動」とほかの 2 つのエンジンの名前も
    /// これとして読む(名前を受けないと global.json 全体が既定値に戻る)。
    #[default]
    #[serde(alias = "Auto", alias = "Whisper", alias = "ReazonSpeech")]
    Haas,
    /// Mac 内蔵の音声認識(SpeechAnalyzer、macOS 26 以降)。
    Apple,
    /// 外部の音声認識 API(`GlobalSettings::remote`)。Haas が先に動き、その文を即時表示と命令語に使う。
    Remote,
}

impl RecognizerKind {
    pub const ALL: [RecognizerKind; 3] = [Self::Haas, Self::Apple, Self::Remote];

    pub fn label(self) -> &'static str {
        match self {
            RecognizerKind::Haas => "Haas-ja v1.0(speech.cpp)",
            RecognizerKind::Apple => "Mac 内蔵の音声認識",
            RecognizerKind::Remote => "外部の音声認識 API(Haas と併用)",
        }
    }
}

/// 外部の音声認識 API の設定(`RecognizerKind::Remote` のとき使う)。鍵は `<provider>_api_key` のファイル。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RemoteSettings {
    /// "openai"・"deepgram"・"gemini-flash"。知らない値なら使わない(設定は壊さない)。
    pub provider: String,
    /// 空なら provider ごとの既定。
    pub model: String,
    /// 辞書の語を用語ヒントとして渡すか(読みと句は渡さない)。
    pub send_glossary: bool,
    /// 答えを待つ上限(秒)。過ぎたら Haas の文で進む。
    pub timeout_secs: u16,
}

impl Default for RemoteSettings {
    fn default() -> Self {
        RemoteSettings {
            provider: "openai".to_string(),
            model: String::new(),
            send_glossary: true,
            timeout_secs: crate::core::remote_asr::DEFAULT_TIMEOUT_SECS,
        }
    }
}

/// パソコン全体の設定。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GlobalSettings {
    /// 使う認識エンジン。
    pub recognizer: RecognizerKind,
    pub input_device: Option<String>,
    /// 近くの iPhone(連係で現れる「〇〇のiPhoneのマイク」)が見つかれば、それを優先して使う。
    pub prefer_iphone: bool,
    /// 最後に音が届いた iPhone のマイク。2 台見えるときはこちらを選ぶ。
    pub last_iphone: Option<String>,
    pub mic_gain: f32,
    pub voice_log: VoiceLogSettings,
    /// 外部の音声認識 API。
    pub remote: RemoteSettings,
    /// 認識結果の訂正役(Claude)。
    pub corrector: CorrectorSettings,
    /// 初めて開いたときの案内を閉じたか。
    pub first_run_shown: bool,
}

impl Default for GlobalSettings {
    fn default() -> Self {
        GlobalSettings {
            recognizer: RecognizerKind::Haas,
            input_device: None,
            prefer_iphone: false,
            last_iphone: None,
            mic_gain: 1.0,
            voice_log: VoiceLogSettings::default(),
            remote: RemoteSettings::default(),
            corrector: CorrectorSettings::default(),
            first_run_shown: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::voice_command::CommandAction;

    #[test]
    fn settings_survive_a_save_with_missing_keys() {
        let settings: UserSettings = serde_json::from_str("{\"history_count\":20}").unwrap();
        assert_eq!(settings.history_count, 20);
        assert_eq!(settings.editor.font_size, 20.0);
        assert_eq!(settings.engine.post_silence_ms, 600);
        assert_eq!(settings.transfer.method, TransferMethod::Auto);
        assert_eq!(settings.command_words, default_command_words(), "項目が無ければ既定の 2 操作");
    }

    /// 2026-11-07 に外した操作(タブ・移動)の行が保存データに残っていても、ほかの設定は読める。
    #[test]
    fn rows_of_removed_commands_do_not_reset_the_settings() {
        let json = r#"{"history_count": 7, "command_words": [
            {"action": "NewLine", "words": ["かいぎょう"]},
            {"action": "Transfer", "words": ["てんそう"]},
            {"action": "Tab", "words": ["たぶ"]},
            {"action": "MoveStart", "words": ["さいしょにもどる"]},
            {"action": "MoveEnd", "words": ["さいごにすすむ"]}]}"#;
        let settings: UserSettings = serde_json::from_str(json).unwrap();
        assert_eq!(settings.history_count, 7);
        let words: Vec<(CommandAction, Vec<String>)> = settings
            .command_words
            .iter()
            .map(|c| (c.action, c.words.clone()))
            .collect();
        assert_eq!(
            words,
            vec![
                (CommandAction::NewLine, vec!["かいぎょう".to_string()]),
                (CommandAction::Transfer, vec!["てんそう".to_string()]),
            ]
        );
    }

    #[test]
    fn old_voice_input_bar_settings_still_load() {
        // 0.1 にあって今は無い項目(bar, sender, learning_enabled)は無視される。
        let json = r#"{"input_mode":"Direct","bar":{"color":"Blue","scale":1.0},"sender":{"method":"Auto"},
            "learning_enabled":true,"editor":{"font_size":24.0,"text_color":[0,0,0]},"engine":{"sensitivity":0.7}}"#;
        let settings: UserSettings = serde_json::from_str(json).unwrap();
        assert_eq!(settings.editor.font_size, 24.0);
        assert_eq!(settings.engine.post_silence_ms, 600, "感度の行は無視して既定");
        assert!(settings.learning.enabled);
    }

    /// 外した項目(直接入力・分野・一時停止・古い音声コマンド)が残っていても読め、残す項目は保たれる。
    #[test]
    fn settings_saved_before_the_simplification_still_load() {
        let json = r#"{"input_mode":"Direct",
            "direct":{"edit":true,"auto_accept":false,"accept_seconds":5},
            "engine":{"sensitivity":0.55,"speed_vs_accuracy":0.9,"profile":"General",
                "mark_unsure":false,"noise_margin":1,"post_silence_ms":700,"live_preview":false},
            "record":{"shortcut":"CapsLock","hold":true,"start_paused":true},
            "transfer":{"method":"Paste","restore_clipboard":false,"ai_wait_ms":9000,"enter_after":true},
            "voice_commands":[{"name":"改行","readings":["かいぎょー"],"surfaces":["改行"],
                "enabled":true,"action":"NewLine"},
                {"name":"外部","readings":["がいぶ"],"surfaces":[],"enabled":false,
                "action":{"External":{"program":"x","wait":true}}}],
            "history_count":50}"#;
        let settings: UserSettings = serde_json::from_str(json).unwrap();
        assert_eq!(settings.engine.post_silence_ms, 700);
        assert!(settings.record.hold);
        assert_eq!(settings.transfer.method, TransferMethod::Paste);
        assert!(!settings.transfer.restore_clipboard);
        assert!(settings.transfer.enter_after);
        assert_eq!(settings.history_count, 50);
        assert_eq!(settings.command_words, default_command_words());
        let saved = serde_json::to_string(&settings).unwrap();
        for gone in [
            "input_mode",
            "direct",
            "voice_commands",
            "profile",
            "start_paused",
            "sensitivity",
            "noise_margin",
        ] {
            assert!(!saved.contains(gone), "{gone}");
        }
    }

    /// 1.0 より前の版にあった「さらに 2 倍にする」(`mic_boost`)が残った global.json も、ほかの項目を保ったまま読める。
    #[test]
    fn an_old_mic_boost_key_is_ignored() {
        let global: GlobalSettings =
            serde_json::from_str("{\"mic_gain\":4.0,\"mic_boost\":true,\"recognizer\":\"Apple\"}").unwrap();
        assert_eq!(global.mic_gain, 4.0);
        assert_eq!(global.recognizer, RecognizerKind::Apple);
        assert!(!serde_json::to_string(&global).unwrap().contains("mic_boost"));
    }

    #[test]
    fn old_global_settings_load_without_the_iphone_key() {
        let json = "{\"input_device\":\"MacBook Proのマイク\",\"mic_gain\":2.4}";
        let global: GlobalSettings = serde_json::from_str(json).unwrap();
        assert!(!global.prefer_iphone);
        assert_eq!(global.input_device.as_deref(), Some("MacBook Proのマイク"));
        assert_eq!(global.mic_gain, 2.4);
        assert!(global.corrector.enabled);
        assert_eq!(global.corrector.model, "claude-haiku-5-5");
        assert_eq!(global.corrector.backend, CorrectorBackend::ApiKey);
    }

    /// 照合の経路は外した(P5)。古い global.json の `checker` は読み飛ばし、ほかの項目は残る。
    #[test]
    fn old_checker_settings_are_skipped() {
        let json = "{\"checker\":{\"enabled\":true,\"model\":\"x\"},\"mic_gain\":1.5}";
        let global: GlobalSettings = serde_json::from_str(json).unwrap();
        assert_eq!(global.mic_gain, 1.5);
        assert!(!serde_json::to_string(&global).unwrap().contains("checker"));
    }

    #[test]
    fn the_old_engines_load_as_haas() {
        let json = r#"{"recognizer":"Auto","reazon_model":null,"whisper_binary":null,
            "resident_engine":false,"mic_gain":2.4,"logged_in_user":"ユーザー"}"#;
        let global: GlobalSettings = serde_json::from_str(json).unwrap();
        assert_eq!(global.recognizer, RecognizerKind::Haas);
        assert_eq!(global.mic_gain, 2.4, "ほかの項目は既定値に戻らない");
        for name in ["ReazonSpeech", "Whisper", "Auto", "Haas"] {
            let json = format!(r#"{{"recognizer":"{name}","mic_gain":1.7}}"#);
            let global: GlobalSettings = serde_json::from_str(&json).unwrap();
            assert_eq!(global.recognizer, RecognizerKind::Haas, "{name}");
            assert_eq!(global.mic_gain, 1.7, "{name}");
        }
        let apple: GlobalSettings = serde_json::from_str(r#"{"recognizer":"Apple"}"#).unwrap();
        assert_eq!(apple.recognizer, RecognizerKind::Apple);
        assert_eq!(GlobalSettings::default().recognizer, RecognizerKind::Haas);
        let saved = serde_json::to_string(&GlobalSettings {
            recognizer: RecognizerKind::Apple,
            ..GlobalSettings::default()
        })
        .unwrap();
        let read: GlobalSettings = serde_json::from_str(&saved).unwrap();
        assert_eq!(read.recognizer, RecognizerKind::Apple);
        // エンジンが増えても(外部 API など)この試験は変えずに済むよう、既定が先頭で Apple もあることだけを見る。
        assert_eq!(RecognizerKind::ALL[0], RecognizerKind::Haas, "既定が先頭");
        assert!(RecognizerKind::ALL.contains(&RecognizerKind::Apple));
        assert_eq!(RecognizerKind::Haas.label(), "Haas-ja v1.0(speech.cpp)");
    }

    #[test]
    fn the_old_record_key_gives_way_to_caps_lock() {
        let json = r#"{"record":{"key":8,"hold":true,"start_paused":false}}"#;
        let settings: UserSettings = serde_json::from_str(json).unwrap();
        assert_eq!(settings.record.shortcut, RecordShortcut::CapsLock);
        assert!(settings.record.hold);
    }

    #[test]
    fn a_chosen_key_survives_a_save() {
        let mut settings = UserSettings::default();
        settings.record.shortcut = RecordShortcut::Key(KeyCombo {
            key_code: 0x4b,
            command: false,
            control: false,
            option: false,
            shift: false,
            label: "テンキー /".to_string(),
        });
        let json = serde_json::to_string(&settings).unwrap();
        let loaded: UserSettings = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded.record, settings.record);
        assert_eq!(loaded.record.shortcut.label(), "テンキー /");
    }
}
