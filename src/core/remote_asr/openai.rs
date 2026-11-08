//! OpenAI の書き起こし(`POST /v1/audio/transcriptions`、multipart)。要求の形は G-API-0(2026-11-08)で通したもの:
//! `model`、`languages[]=ja`(`language` ではない)、`prompt` に場の説明 1 文、`keywords[]` に辞書の語。答えは `{"text"}`。

use super::http::{Body, Http, HttpRequest};
use super::{error_detail, status_error, Hints, RemoteError, RemoteProvider, SCENE};
use std::sync::Arc;
use std::time::Duration;

pub(super) const ID: &str = "openai";
pub(super) const DEFAULT_MODEL: &str = "gpt-transcribe";
const URL: &str = "https://api.openai.com/v1/audio/transcriptions";

pub(super) struct OpenAi {
    key: String,
    model: String,
    http: Arc<dyn Http>,
}

impl OpenAi {
    pub(super) fn new(key: String, model: String, http: Arc<dyn Http>) -> Self {
        OpenAi { key, model, http }
    }

    fn request(&self, wav: &[u8], hints: &Hints) -> HttpRequest {
        let mut fields = vec![
            ("model".to_string(), self.model.clone()),
            ("languages[]".to_string(), hints.language.to_string()),
            ("prompt".to_string(), SCENE.to_string()),
        ];
        fields.extend(hints.words.iter().map(|word| ("keywords[]".to_string(), word.clone())));
        HttpRequest {
            url: URL.to_string(),
            headers: vec![("Authorization".to_string(), format!("Bearer {}", self.key.trim()))],
            body: Body::Multipart {
                fields,
                file_field: "file".to_string(),
                file_name: "audio.wav".to_string(),
                content_type: "audio/wav".to_string(),
                bytes: wav.to_vec(),
            },
        }
    }
}

impl RemoteProvider for OpenAi {
    fn label(&self) -> String {
        format!("OpenAI({})", self.model)
    }

    fn transcribe(&self, wav: &[u8], hints: &Hints, timeout: Duration) -> Result<String, RemoteError> {
        if self.key.trim().is_empty() {
            return Err(RemoteError::NoKey);
        }
        let (status, body) = self
            .http
            .post(&self.request(wav, hints), timeout)
            .map_err(|message| RemoteError::Temporary(format!("OpenAI: {message}")))?;
        if status != 200 {
            return Err(status_error(status, "OpenAI", &error_detail(&body)));
        }
        let root: serde_json::Value = serde_json::from_str(&body)
            .map_err(|_| RemoteError::Permanent("OpenAI の応答が読めません".to_string()))?;
        root["text"]
            .as_str()
            .map(|text| text.trim().to_string())
            .ok_or_else(|| RemoteError::Permanent("OpenAI の応答に文がありません".to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::FakeHttp;
    use super::*;

    fn hints() -> Hints {
        Hints {
            words: vec!["右肺上葉".to_string(), "腹部造影CT".to_string()],
            language: "ja",
        }
    }

    #[test]
    fn the_request_is_multipart_with_languages_keywords_and_the_scene() {
        let http = FakeHttp::answering(200, r#"{"text":" 右肺上葉に結節。 ","usage":{}}"#);
        let provider = OpenAi::new("sk-test".to_string(), DEFAULT_MODEL.to_string(), http.clone());
        let text = provider.transcribe(b"RIFF", &hints(), Duration::from_secs(15)).unwrap();
        assert_eq!(text, "右肺上葉に結節。");
        let request = http.last();
        assert_eq!(request.url, URL);
        assert_eq!(request.headers, [("Authorization".to_string(), "Bearer sk-test".to_string())]);
        let Body::Multipart { fields, bytes, content_type, .. } = request.body else {
            panic!("multipart ではない");
        };
        let field = |name: &str| -> Vec<String> {
            fields.iter().filter(|(n, _)| n == name).map(|(_, v)| v.clone()).collect()
        };
        assert_eq!(field("model"), ["gpt-transcribe"]);
        assert_eq!(field("languages[]"), ["ja"]);
        assert!(field("language").is_empty(), "language ではなく languages[]");
        assert_eq!(field("prompt"), [SCENE]);
        assert_eq!(field("keywords[]"), ["右肺上葉", "腹部造影CT"]);
        assert_eq!((bytes.as_slice(), content_type.as_str()), (b"RIFF".as_slice(), "audio/wav"));
    }

    #[test]
    fn errors_and_missing_keys_are_reported() {
        let http = FakeHttp::answering(401, r#"{"error":{"message":"bad key"}}"#);
        let provider = OpenAi::new("sk".to_string(), DEFAULT_MODEL.to_string(), http.clone());
        assert!(matches!(provider.transcribe(b"", &hints(), Duration::from_secs(1)), Err(RemoteError::Permanent(_))));
        let none = OpenAi::new(" ".to_string(), DEFAULT_MODEL.to_string(), http);
        assert_eq!(none.transcribe(b"", &hints(), Duration::from_secs(1)), Err(RemoteError::NoKey));
    }
}
