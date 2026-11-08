//! 外部の音声認識 API を、Haas のあとの段として使う(計画書 P4、D3・D12)。
//!
//! Haas は API を選んでいても先に動き、その文を即時表示と命令語の判定に使う。API の文は届いたら表示を差し替え、
//! Claude の訂正にはそちらを渡す。API に送るのは Haas が声と判定した区間(`worth_sending`)だけで、失敗・時間切れ・
//! 長すぎる・短すぎる答え(`plausible_length`)のときは Haas の文のまま進む。
//!
//! 用語ヒントは辞書の語(短い用語)だけを渡し、読みと句は渡さない(`hint_words`)。句を渡すと、ヒント語を組み合わせた
//! 言っていない所見文を返す API があった(G-API-0、2026-11-08)。
//!
//! HTTP の道具は持たず `/usr/bin/curl` を呼ぶ(`http.rs`)。鍵・音声・本文は所有者だけが読める一時ファイルで渡し、
//! 引数に出さない。

mod deepgram;
mod gemini_flash;
pub(crate) mod http;
mod openai;

use crate::core::dictionary::Dictionary;
use crate::core::settings::RemoteSettings;
use std::sync::Arc;
use std::time::Duration;

pub use http::{Body, CurlHttp, Http, HttpRequest};

/// 送る言語(このアプリは日本語の口述だけ)。
pub const LANGUAGE: &str = "ja";
/// 場の説明(OpenAI の `prompt`)。G-API-0 で使った 1 文。
pub const SCENE: &str = "放射線科医が読影レポートを口述した音声。句読点は「、」「。」。";
/// これ以下の字数の Haas の文は、声でないとみなして API に送らない(咳・ため息・物音。単位 H で 0〜3 字が 26%)。
pub const NOISE_GATE_CHARS: usize = 3;
/// 用語ヒントの上限(語)。
pub const MAX_HINT_WORDS: usize = 50;
/// 用語ヒントにする語の長さの上限(字)。これより長いものは句とみなして渡さない。
pub const MAX_HINT_CHARS: usize = 16;
/// 待つ時間の既定(秒)。
pub const DEFAULT_TIMEOUT_SECS: u16 = 15;

/// provider の一覧(設定の値, 画面の名前, 既定のモデル, 画面の注記)。
pub const PROVIDERS: [(&str, &str, &str, &str); 3] = [
    (
        openai::ID,
        "OpenAI(gpt-transcribe)",
        openai::DEFAULT_MODEL,
        "音声は OpenAI に送られます(OpenAI の文書では、API に送ったデータは既定で学習に使われません)。",
    ),
    (
        deepgram::ID,
        "Deepgram(nova-3)",
        deepgram::DEFAULT_MODEL,
        "音声は Deepgram に送られます(このアプリは、モデルの改善に使わない指定を毎回付けます)。",
    ),
    (
        gemini_flash::ID,
        "Gemini Flash(精度優先)",
        gemini_flash::DEFAULT_MODEL,
        "精度優先。1 発話に 3 秒ほど待ちます。音声は Google に送られます。",
    ),
];

/// 知っている provider か。
pub fn is_known_provider(id: &str) -> bool {
    PROVIDERS.iter().any(|(known, ..)| *known == id)
}

/// provider の画面の名前。知らない値なら None。
pub fn provider_label(id: &str) -> Option<&'static str> {
    PROVIDERS.iter().find(|(known, ..)| *known == id).map(|(_, label, ..)| *label)
}

/// provider の既定のモデル。
pub fn default_model(id: &str) -> &'static str {
    PROVIDERS
        .iter()
        .find(|(known, ..)| *known == id)
        .map_or("", |(_, _, model, _)| *model)
}

/// 鍵のファイルの名前(`<provider>_api_key`)。
pub fn key_file_name(id: &str) -> String {
    format!("{id}_api_key")
}

/// API に渡す用語ヒント。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Hints {
    /// 辞書の語(表記だけ。読みと句は渡さない)。
    pub words: Vec<String>,
    pub language: &'static str,
}

/// API の誤り。
#[derive(Debug, Clone, PartialEq)]
pub enum RemoteError {
    /// 鍵が無い。
    NoKey,
    /// しばらくすれば通るかもしれない(時間切れ・混雑・接続・サービスの一時的な不調)。
    Temporary(String),
    /// 設定を直さないと通らない(鍵の誤り・権限・モデル名・応答の形)。
    Permanent(String),
}

impl std::fmt::Display for RemoteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RemoteError::NoKey => write!(f, "外部 API の鍵が設定されていません"),
            RemoteError::Temporary(message) | RemoteError::Permanent(message) => write!(f, "{message}"),
        }
    }
}

/// 外部の音声認識 API 1 つ。`wav` は 16 kHz・mono・16 bit の WAV。
pub trait RemoteProvider: Send + Sync {
    fn label(&self) -> String;
    fn transcribe(&self, wav: &[u8], hints: &Hints, timeout: Duration) -> Result<String, RemoteError>;
}

/// 設定と鍵から provider を作る。知らない provider なら None(設定を壊さず、Haas だけで動く)。
pub fn make_provider(
    settings: &RemoteSettings,
    key: String,
    http: Arc<dyn Http>,
) -> Option<Arc<dyn RemoteProvider>> {
    let model = match settings.model.trim() {
        "" => default_model(&settings.provider).to_string(),
        model => model.to_string(),
    };
    // モデル名は URL の経路や問い合わせに入るので、英数字と `.`・`_`・`-` だけにする。
    if !model.chars().all(|c| c.is_ascii_alphanumeric() || ".-_".contains(c)) {
        crate::core::diag::log("外部 API のモデル名に使えない文字があるので使わない");
        return None;
    }
    let provider: Arc<dyn RemoteProvider> = match settings.provider.as_str() {
        openai::ID => Arc::new(openai::OpenAi::new(key, model, http)),
        deepgram::ID => Arc::new(deepgram::Deepgram::new(key, model, http)),
        gemini_flash::ID => Arc::new(gemini_flash::GeminiFlash::new(key, model, http)),
        _ => return None,
    };
    Some(provider)
}

/// 辞書から用語ヒントの語を選ぶ。表記だけで、空白や句読点を含むもの・長いもの(句)は渡さない。重複は除く。
pub fn hint_words(dictionary: &Dictionary) -> Vec<String> {
    let mut words: Vec<String> = Vec::new();
    for entry in &dictionary.words {
        let word = entry.word.trim();
        let phrase = word
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || "、。,.，．".contains(c));
        if word.is_empty() || phrase || word.chars().count() > MAX_HINT_CHARS {
            continue;
        }
        if !words.iter().any(|known| known == word) {
            words.push(word.to_string());
        }
        if words.len() >= MAX_HINT_WORDS {
            break;
        }
    }
    words
}

/// API に送る価値のある発話か(Haas の文が声らしい長さか)。
pub fn worth_sending(haas_text: &str) -> bool {
    haas_text.trim().chars().count() > NOISE_GATE_CHARS
}

/// API の文が、Haas の文から見て長すぎるか(2 倍 + 20 字を超える)。内容の突合ではなく、言っていない文を
/// 長々と返したときの守り。
pub fn too_long(haas_text: &str, api_text: &str) -> bool {
    api_text.chars().count() > haas_text.chars().count() * 2 + 20
}

/// 下限の門番を掛ける Haas の文の長さ(字)。短い発話は API の文が半分になってもありうる。
pub const SHORT_GATE_CHARS: usize = 12;

/// API の文が、Haas の文から見て短すぎるか(Haas の文が 12 字以上で、その半分未満)。言った文が大きく欠けた
/// 答えを使わないための守り。内容の突合(左右・否定・節)はしない(Haas の方が誤りが多い。計画書 P4 (5))。
pub fn too_short(haas_text: &str, api_text: &str) -> bool {
    let haas = haas_text.chars().count();
    haas >= SHORT_GATE_CHARS && api_text.chars().count() * 2 < haas
}

/// API の文が、Haas の文から見てありうる長さか(長すぎも短すぎもしない)。
pub fn plausible_length(haas_text: &str, api_text: &str) -> bool {
    !too_long(haas_text, api_text) && !too_short(haas_text, api_text)
}

/// HTTP の状態から誤りを作る。`who` は知らせに入れる provider の名前。
pub(crate) fn status_error(status: u16, who: &str, detail: &str) -> RemoteError {
    let detail = detail.chars().take(200).collect::<String>();
    match status {
        401 => RemoteError::Permanent(format!("{who} の鍵が正しくありません")),
        402 => RemoteError::Permanent(format!("{who} の残高がありません")),
        403 => RemoteError::Permanent(format!("{who} の鍵に権限がありません: {detail}")),
        404 => RemoteError::Permanent(format!("{who} のモデルが使えません: {detail}")),
        408 | 429 => RemoteError::Temporary(format!("{who} が混み合っています")),
        500..=599 => RemoteError::Temporary(format!("{who} のサービスが一時的に使えません")),
        _ => RemoteError::Permanent(format!("{who} のエラー(HTTP {status}): {detail}")),
    }
}

/// 誤りの応答の本文から、利用者に見せる説明を取り出す(`error.message` か `err_msg`)。
pub(crate) fn error_detail(body: &str) -> String {
    let Ok(root) = serde_json::from_str::<serde_json::Value>(body) else {
        return String::new();
    };
    root["error"]["message"]
        .as_str()
        .or_else(|| root["err_msg"].as_str())
        .or_else(|| root["error"].as_str())
        .unwrap_or("")
        .to_string()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::core::dictionary::Word;
    use std::sync::Mutex;

    /// 送った要求を覚え、決まった答えを返す偽の HTTP。
    pub(crate) struct FakeHttp {
        pub(crate) sent: Mutex<Vec<HttpRequest>>,
        pub(crate) answer: Mutex<Result<(u16, String), String>>,
    }

    impl FakeHttp {
        pub(crate) fn answering(status: u16, body: &str) -> Arc<Self> {
            Arc::new(FakeHttp {
                sent: Mutex::new(Vec::new()),
                answer: Mutex::new(Ok((status, body.to_string()))),
            })
        }

        pub(crate) fn last(&self) -> HttpRequest {
            self.sent.lock().unwrap().last().cloned().expect("要求が送られていない")
        }
    }

    impl Http for FakeHttp {
        fn post(&self, request: &HttpRequest, _timeout: Duration) -> Result<(u16, String), String> {
            self.sent.lock().unwrap().push(request.clone());
            self.answer.lock().unwrap().clone()
        }
    }

    fn word(word: &str, reading: &str) -> Word {
        Word {
            word: word.to_string(),
            reading: reading.to_string(),
            ..Word::default()
        }
    }

    #[test]
    fn hints_are_short_dictionary_words_without_readings_or_phrases() {
        let mut dictionary = Dictionary {
            words: vec![
            word("腹部造影CT", "ふくぶぞうえいしーてぃー"),
            word("胸水貯留なし。", "きょうすいちょりゅうなし"),
            word("肝 嚢胞", ""),
            word("右肺上葉", "うはいじょうよう"),
            word("右肺上葉", ""),
            word("とても長い用語の名前でそのまま句になっているもの", ""),
            word("", "よみ"),
            word("制御\u{0}文字", ""),
            ],
            ..Dictionary::default()
        };
        assert_eq!(hint_words(&dictionary), ["腹部造影CT", "右肺上葉"]);
        dictionary.words = (0..80).map(|i| word(&format!("語{i}"), "")).collect();
        assert_eq!(hint_words(&dictionary).len(), MAX_HINT_WORDS);
    }

    #[test]
    fn short_noises_are_not_sent_and_long_answers_are_not_believed() {
        assert!(!worth_sending("うん。"));
        assert!(!worth_sending("  ピン "));
        assert!(worth_sending("結節あり。"));
        let haas = "右肺上葉に結節。";
        assert!(plausible_length(haas, "右肺上葉に5mmの結節を認めます。"));
        assert!(plausible_length(haas, &"あ".repeat(36)));
        assert!(!plausible_length(haas, &"あ".repeat(37)));
        // 12 字以上の Haas の文の半分未満は短すぎる。12 字未満なら下限は掛けない。
        let long_haas = "右肺上葉に結節。胸水なし。";
        assert_eq!(long_haas.chars().count(), 13);
        assert!(plausible_length(long_haas, "右肺上葉に結節。"));
        assert!(plausible_length(long_haas, "右肺上葉に結節"), "7 字は 13 字の半分以上");
        assert!(!plausible_length(long_haas, "右肺上葉に結"), "6 字は半分未満");
        assert!(plausible_length("結節あり。", "結"));
    }

    #[test]
    fn providers_are_known_by_id_and_unknown_ones_are_ignored() {
        assert!(is_known_provider("openai") && is_known_provider("deepgram") && is_known_provider("gemini-flash"));
        assert!(!is_known_provider("whatever"));
        let http = FakeHttp::answering(200, "{}");
        let mut settings = RemoteSettings {
            provider: "whatever".to_string(),
            ..RemoteSettings::default()
        };
        assert!(make_provider(&settings, "k".to_string(), http.clone()).is_none());
        settings.provider = "deepgram".to_string();
        settings.model = "../../v1/files/x".to_string();
        assert!(make_provider(&settings, "k".to_string(), http.clone()).is_none(), "経路を変える名前は断る");
        settings.model = String::new();
        let provider = make_provider(&settings, "k".to_string(), http).unwrap();
        assert!(provider.label().contains("Deepgram"));
        assert_eq!(key_file_name("gemini-flash"), "gemini-flash_api_key");
    }

    #[test]
    fn statuses_become_temporary_or_permanent_errors() {
        assert!(matches!(status_error(401, "X", ""), RemoteError::Permanent(_)));
        assert!(matches!(status_error(429, "X", ""), RemoteError::Temporary(_)));
        assert!(matches!(status_error(503, "X", ""), RemoteError::Temporary(_)));
        assert!(matches!(status_error(400, "X", "bad"), RemoteError::Permanent(m) if m.contains("bad")));
        assert_eq!(error_detail(r#"{"error":{"message":"m1"}}"#), "m1");
        assert_eq!(error_detail(r#"{"err_msg":"m2"}"#), "m2");
        assert_eq!(error_detail("not json"), "");
    }
}
