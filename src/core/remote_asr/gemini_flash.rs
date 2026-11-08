//! Gemini Flash に音声を聞かせて書き起こす(`generateContent`、精度優先。1 発話に 3 秒ほど)。要求の形は
//! G-API-0(2026-11-08)で通したもの: `inline_data` audio/wav、`responseMimeType: application/json` と
//! `responseJsonSchema {text}`、`thinkingConfig.thinkingLevel: low`、`temperature 0`。指示文も計測と同じもの。

use super::http::{Body, Http, HttpRequest};
use super::{error_detail, status_error, Hints, RemoteError, RemoteProvider};
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;

pub(super) const ID: &str = "gemini-flash";
pub(super) const DEFAULT_MODEL: &str = "gemini-3.8-flash";
const BASE: &str = "https://generativelanguage.googleapis.com/v1beta/models";
const SYSTEM: &str = "これは放射線科医が読影レポートを口述した音声です。話された内容を逐語で書き起こしてください。\
言っていない語を足さないでください。\n話者は句読点を「テン」「マル」と声に出して言います。これは文字にせず「、」「。」で\
書いてください。\n書き起こしの本文だけを JSON の text に入れて返してください。";
const USER: &str = "この音声を書き起こしてください。";
const GLOSSARY_HEAD: &str =
    "用語集(この話者がよく使う語。音が合うときだけこの表記で書く。音が合わない語を当てはめない):\n";

pub(super) struct GeminiFlash {
    key: String,
    model: String,
    http: Arc<dyn Http>,
}

impl GeminiFlash {
    pub(super) fn new(key: String, model: String, http: Arc<dyn Http>) -> Self {
        GeminiFlash { key, model, http }
    }

    fn request(&self, wav: &[u8], hints: &Hints) -> HttpRequest {
        let mut user = USER.to_string();
        if !hints.words.is_empty() {
            user.push('\n');
            user.push_str(GLOSSARY_HEAD);
            user.push_str(&hints.words.join("、"));
        }
        let body = json!({
            "system_instruction": {"parts": [{"text": SYSTEM}]},
            "contents": [{
                "role": "user",
                "parts": [
                    {"inline_data": {"mime_type": "audio/wav", "data": base64(wav)}},
                    {"text": user}
                ]
            }],
            "generationConfig": {
                "temperature": 0,
                "responseMimeType": "application/json",
                "responseJsonSchema": {
                    "type": "object",
                    "properties": {"text": {"type": "string"}},
                    "required": ["text"]
                },
                "thinkingConfig": {"thinkingLevel": "low"}
            }
        });
        HttpRequest {
            url: format!("{BASE}/{}:generateContent", self.model),
            headers: vec![("x-goog-api-key".to_string(), self.key.trim().to_string())],
            body: Body::Json(body.to_string()),
        }
    }
}

impl RemoteProvider for GeminiFlash {
    fn label(&self) -> String {
        format!("Gemini({})", self.model)
    }

    fn transcribe(&self, wav: &[u8], hints: &Hints, timeout: Duration) -> Result<String, RemoteError> {
        if self.key.trim().is_empty() {
            return Err(RemoteError::NoKey);
        }
        let (status, body) = self
            .http
            .post(&self.request(wav, hints), timeout)
            .map_err(|message| RemoteError::Temporary(format!("Gemini: {message}")))?;
        if status != 200 {
            return Err(status_error(status, "Gemini", &error_detail(&body)));
        }
        let root: serde_json::Value = serde_json::from_str(&body)
            .map_err(|_| RemoteError::Permanent("Gemini の応答が読めません".to_string()))?;
        // 考えた分(thought)は捨て、答えの部分をつなぐ。答えは {"text": …} の JSON。
        let parts = root["candidates"][0]["content"]["parts"]
            .as_array()
            .ok_or_else(|| RemoteError::Permanent("Gemini の応答に文がありません".to_string()))?;
        let answer: String = parts
            .iter()
            .filter(|part| !part["thought"].as_bool().unwrap_or(false))
            .filter_map(|part| part["text"].as_str())
            .collect();
        let parsed: serde_json::Value = serde_json::from_str(&answer)
            .map_err(|_| RemoteError::Permanent("Gemini の答えの形が違います".to_string()))?;
        parsed["text"]
            .as_str()
            .map(|text| text.trim().to_string())
            .ok_or_else(|| RemoteError::Permanent("Gemini の答えの形が違います".to_string()))
    }
}

/// 標準の base64(`=` で埋める)。
fn base64(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = chunk.iter().enumerate().fold(0u32, |n, (i, b)| n | (u32::from(*b) << (16 - 8 * i)));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(TABLE[((n >> (18 - 6 * i)) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::super::tests::FakeHttp;
    use super::*;

    #[test]
    fn base64_matches_the_standard_alphabet_and_padding() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(&[0xff, 0xfe, 0x00, 0x10]), "//4AEA==");
    }

    #[test]
    fn the_request_is_generate_content_with_low_thinking_and_a_json_schema() {
        let answer = r#"{"candidates":[{"content":{"parts":[{"text":"考え中","thought":true},{"text":"{\"text\":\"右肺上葉に結節。\"}"}]}}]}"#;
        let http = FakeHttp::answering(200, answer);
        let provider = GeminiFlash::new("g".to_string(), DEFAULT_MODEL.to_string(), http.clone());
        let hints = Hints {
            words: vec!["右肺上葉".to_string(), "結節".to_string()],
            language: "ja",
        };
        assert_eq!(provider.transcribe(b"foo", &hints, Duration::from_secs(15)).unwrap(), "右肺上葉に結節。");
        let request = http.last();
        assert_eq!(
            request.url,
            "https://generativelanguage.googleapis.com/v1beta/models/gemini-3.8-flash:generateContent"
        );
        assert_eq!(request.headers, [("x-goog-api-key".to_string(), "g".to_string())]);
        let Body::Json(body) = request.body else {
            panic!("JSON ではない");
        };
        let body: serde_json::Value = serde_json::from_str(&body).unwrap();
        let config = &body["generationConfig"];
        assert_eq!(config["thinkingConfig"]["thinkingLevel"], "low");
        assert_eq!(config["temperature"], 0);
        assert_eq!(config["responseMimeType"], "application/json");
        assert_eq!(config["responseJsonSchema"]["required"][0], "text");
        let parts = &body["contents"][0]["parts"];
        assert_eq!(parts[0]["inline_data"]["mime_type"], "audio/wav");
        assert_eq!(parts[0]["inline_data"]["data"], "Zm9v");
        assert!(parts[1]["text"].as_str().unwrap().ends_with("右肺上葉、結節"));
    }

    #[test]
    fn an_answer_in_the_wrong_shape_is_an_error() {
        let http = FakeHttp::answering(200, r#"{"candidates":[{"content":{"parts":[{"text":"ただの文"}]}}]}"#);
        let provider = GeminiFlash::new("g".to_string(), DEFAULT_MODEL.to_string(), http);
        assert!(matches!(
            provider.transcribe(b"", &Hints::default(), Duration::from_secs(1)),
            Err(RemoteError::Permanent(_))
        ));
    }
}
