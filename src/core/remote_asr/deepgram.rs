//! Deepgram の録音 API(`POST /v1/listen`、本文は WAV)。要求の形は G-API-0(2026-11-08)で通したもの:
//! `model=nova-3`、`language=ja`(`multi` は要らない)、`smart_format=true`、`keyterm` に辞書の語(1 語ずつ)。
//! 答えは `results.channels[0].alternatives[0].transcript`。
//! Deepgram は既定では要求をモデルの改善に使って保持する(Model Improvement Program)。医療の音声なので、
//! すべての要求に `mip_opt_out=true` を付けて外す(外した要求は応答のあと保持しない、と Deepgram の文書にある。
//! 2026-11-08 に確認)。

use super::http::{percent_encode, Body, Http, HttpRequest};
use super::{error_detail, status_error, Hints, RemoteError, RemoteProvider};
use std::sync::Arc;
use std::time::Duration;

pub(super) const ID: &str = "deepgram";
pub(super) const DEFAULT_MODEL: &str = "nova-3";
const URL: &str = "https://api.deepgram.com/v1/listen";

pub(super) struct Deepgram {
    key: String,
    model: String,
    http: Arc<dyn Http>,
}

impl Deepgram {
    pub(super) fn new(key: String, model: String, http: Arc<dyn Http>) -> Self {
        Deepgram { key, model, http }
    }

    fn request(&self, wav: &[u8], hints: &Hints) -> HttpRequest {
        let mut query = vec![
            ("model", self.model.clone()),
            ("language", hints.language.to_string()),
            ("smart_format", "true".to_string()),
            ("mip_opt_out", "true".to_string()),
        ];
        query.extend(hints.words.iter().map(|word| ("keyterm", word.clone())));
        let query: Vec<String> = query
            .iter()
            .map(|(name, value)| format!("{name}={}", percent_encode(value)))
            .collect();
        HttpRequest {
            url: format!("{URL}?{}", query.join("&")),
            headers: vec![("Authorization".to_string(), format!("Token {}", self.key.trim()))],
            body: Body::Binary {
                content_type: "audio/wav".to_string(),
                bytes: wav.to_vec(),
            },
        }
    }
}

impl RemoteProvider for Deepgram {
    fn label(&self) -> String {
        format!("Deepgram({})", self.model)
    }

    fn transcribe(&self, wav: &[u8], hints: &Hints, timeout: Duration) -> Result<String, RemoteError> {
        if self.key.trim().is_empty() {
            return Err(RemoteError::NoKey);
        }
        let (status, body) = self
            .http
            .post(&self.request(wav, hints), timeout)
            .map_err(|message| RemoteError::Temporary(format!("Deepgram: {message}")))?;
        if status != 200 {
            return Err(status_error(status, "Deepgram", &error_detail(&body)));
        }
        let root: serde_json::Value = serde_json::from_str(&body)
            .map_err(|_| RemoteError::Permanent("Deepgram の応答が読めません".to_string()))?;
        root["results"]["channels"][0]["alternatives"][0]["transcript"]
            .as_str()
            .map(|text| text.trim().to_string())
            .ok_or_else(|| RemoteError::Permanent("Deepgram の応答に文がありません".to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::FakeHttp;
    use super::*;

    #[test]
    fn the_request_carries_language_ja_smart_format_the_opt_out_and_one_keyterm_per_word() {
        let http = FakeHttp::answering(
            200,
            r#"{"metadata":{"duration":3.2},"results":{"channels":[{"alternatives":[{"transcript":"右肺上葉に結節。"}]}]}}"#,
        );
        let provider = Deepgram::new("dg".to_string(), DEFAULT_MODEL.to_string(), http.clone());
        let hints = Hints {
            words: vec!["右肺上葉".to_string(), "S&P".to_string()],
            language: "ja",
        };
        assert_eq!(provider.transcribe(b"RIFF", &hints, Duration::from_secs(15)).unwrap(), "右肺上葉に結節。");
        let request = http.last();
        assert_eq!(
            request.url,
            "https://api.deepgram.com/v1/listen?model=nova-3&language=ja&smart_format=true&mip_opt_out=true\
             &keyterm=%E5%8F%B3%E8%82%BA%E4%B8%8A%E8%91%89&keyterm=S%26P"
        );
        assert_eq!(request.headers, [("Authorization".to_string(), "Token dg".to_string())]);
        assert!(matches!(request.body, Body::Binary { ref content_type, .. } if content_type == "audio/wav"));
    }

    #[test]
    fn a_busy_service_is_a_temporary_error() {
        let http = FakeHttp::answering(429, r#"{"err_msg":"slow down"}"#);
        let provider = Deepgram::new("dg".to_string(), DEFAULT_MODEL.to_string(), http);
        let hints = Hints::default();
        assert!(matches!(provider.transcribe(b"", &hints, Duration::from_secs(1)), Err(RemoteError::Temporary(_))));
    }
}
