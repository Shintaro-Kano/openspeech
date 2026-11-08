//! 外部 API への POST を `/usr/bin/curl` で送る。URL・ヘッダー(鍵)・フォームの欄は curl の設定ファイルに、
//! 本文(音声・JSON)は別の一時ファイルに書く。どちらも所有者だけが読める(0600)ファイルで、送り終えたら消す
//! (`PrivateFile`)。引数に出るのは時間の上限と設定ファイルの場所だけ。

use crate::core::proc::{PrivateFile, BODY_PREFIX, KEY_PREFIX};
use std::process::{Command, Stdio};
use std::time::Duration;

/// macOS に必ずある curl(PATH を見ない)。
const CURL: &str = "/usr/bin/curl";

/// 要求の本文。
#[derive(Debug, Clone, PartialEq)]
pub enum Body {
    /// JSON(文字列にしたもの)。
    Json(String),
    /// そのままのバイト列(音声)。
    Binary { content_type: String, bytes: Vec<u8> },
    /// multipart/form-data。文字の欄と、ファイル 1 つ。
    Multipart {
        fields: Vec<(String, String)>,
        file_field: String,
        file_name: String,
        content_type: String,
        bytes: Vec<u8>,
    },
}

/// POST の要求 1 つ。`headers` には鍵が入る(設定ファイルにだけ書く)。
#[derive(Debug, Clone, PartialEq)]
pub struct HttpRequest {
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Body,
}

/// POST を送り、(HTTP 状態, 本文) を返す。接続できない・時間切れは Err(説明)。
pub trait Http: Send + Sync {
    fn post(&self, request: &HttpRequest, timeout: Duration) -> Result<(u16, String), String>;
}

/// `/usr/bin/curl` で送る。
pub struct CurlHttp;

impl Http for CurlHttp {
    fn post(&self, request: &HttpRequest, timeout: Duration) -> Result<(u16, String), String> {
        let body = match &request.body {
            Body::Json(text) => PrivateFile::create_bytes(BODY_PREFIX, text.as_bytes())?,
            Body::Binary { bytes, .. } | Body::Multipart { bytes, .. } => {
                PrivateFile::create_bytes(BODY_PREFIX, bytes)?
            }
        };
        let config = PrivateFile::create(KEY_PREFIX, &curl_config(request, &body.path().display().to_string()))?;
        let output = Command::new(CURL)
            .args(curl_args(timeout, config.path()))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .map_err(|e| format!("curl を起動できません: {e}"))?;
        drop((config, body));
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(match output.status.code() {
                Some(28) => "時間内に答えが返りませんでした".to_string(),
                _ => format!("接続できません: {}", stderr.trim()),
            });
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        let (text, code) = stdout
            .trim_end()
            .rsplit_once('\n')
            .ok_or("応答が不正です")?;
        let status = code.trim().parse::<u16>().map_err(|_| "応答が不正です".to_string())?;
        Ok((status, text.to_string()))
    }
}

/// curl の引数。鍵・語・音声は設定ファイルと本文のファイルにあり、ここに出るのは時間の上限と設定ファイルの
/// 場所だけ。`-q` は先頭に置く(利用者の ~/.curlrc を読ませない)。
pub(crate) fn curl_args(timeout: Duration, config: &std::path::Path) -> Vec<std::ffi::OsString> {
    let seconds = timeout.as_secs().max(1).to_string();
    let mut args: Vec<std::ffi::OsString> = ["-q", "-sS", "--max-time", &seconds, "-X", "POST", "-w", "\n%{http_code}", "-K"]
        .iter()
        .map(std::ffi::OsString::from)
        .collect();
    args.push(config.as_os_str().to_owned());
    args
}

/// curl の設定ファイルの中身。`body_path` は本文(音声・JSON)を書いた一時ファイル。
pub(crate) fn curl_config(request: &HttpRequest, body_path: &str) -> String {
    let mut lines = vec![format!("url = {}", quote(&request.url))];
    for (name, value) in &request.headers {
        lines.push(format!("header = {}", quote(&format!("{name}: {value}"))));
    }
    match &request.body {
        Body::Json(_) => {
            lines.push(format!("header = {}", quote("Content-Type: application/json")));
            lines.push(format!("data-binary = {}", quote(&format!("@{body_path}"))));
        }
        Body::Binary { content_type, .. } => {
            lines.push(format!("header = {}", quote(&format!("Content-Type: {content_type}"))));
            lines.push(format!("data-binary = {}", quote(&format!("@{body_path}"))));
        }
        Body::Multipart {
            fields,
            file_field,
            file_name,
            content_type,
            ..
        } => {
            // 文字の欄は form-string(値の先頭の `@`・`<` をファイルの読み込みと解釈させない)。
            for (name, value) in fields {
                lines.push(format!("form-string = {}", quote(&format!("{name}={value}"))));
            }
            lines.push(format!(
                "form = {}",
                quote(&format!("{file_field}=@{body_path};type={content_type};filename={file_name}"))
            ));
        }
    }
    lines.join("\n") + "\n"
}

/// curl の設定ファイルの引用符付きの値。`\` と `"` と改行は逃がす(行を足させない)。
fn quote(value: &str) -> String {
    let mut out = String::from("\"");
    for c in value.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            // NUL は設定の行をそこで切ってしまう。
            '\0' => {}
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// URL の問い合わせの値を % で逃がす(英数字と `-_.~` 以外)。
pub(crate) fn percent_encode(value: &str) -> String {
    let mut out = String::new();
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(byte as char),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secrets_and_fields_go_to_the_config_file_with_quoting() {
        let request = HttpRequest {
            url: "https://example.invalid/v1".to_string(),
            headers: vec![("Authorization".to_string(), "Bearer sk-test".to_string())],
            body: Body::Multipart {
                fields: vec![
                    ("keywords[]".to_string(), "@/etc/passwd".to_string()),
                    ("prompt".to_string(), "a \"b\" \\ c\nheader = \"x: y\"".to_string()),
                ],
                file_field: "file".to_string(),
                file_name: "audio.wav".to_string(),
                content_type: "audio/wav".to_string(),
                bytes: vec![1, 2, 3],
            },
        };
        let config = curl_config(&request, "/tmp/openspeech-body-1-0");
        assert!(config.contains("header = \"Authorization: Bearer sk-test\"\n"));
        // `@` で始まる語もファイルとして読ませない。
        assert!(config.contains("form-string = \"keywords[]=@/etc/passwd\"\n"));
        // 改行と引用符は逃がすので、値から別の行を足せない。
        assert!(config.contains("form-string = \"prompt=a \\\"b\\\" \\\\ c\\nheader = \\\"x: y\\\"\"\n"));
        assert_eq!(config.lines().filter(|line| line.starts_with("header")).count(), 1);
        assert!(config.contains("form = \"file=@/tmp/openspeech-body-1-0;type=audio/wav;filename=audio.wav\""));
    }

    /// curl の引数には、鍵も、鍵の入った設定ファイルの中身も出ない(`ps` で見えない)。
    #[test]
    fn the_command_line_carries_no_key_or_config_contents() {
        let request = HttpRequest {
            url: "https://example.invalid/v1".to_string(),
            headers: vec![("Authorization".to_string(), "Bearer sk-secret-key".to_string())],
            body: Body::Multipart {
                fields: vec![("keywords[]".to_string(), "右肺上葉".to_string())],
                file_field: "file".to_string(),
                file_name: "audio.wav".to_string(),
                content_type: "audio/wav".to_string(),
                bytes: vec![1, 2, 3],
            },
        };
        let config = curl_config(&request, "/tmp/openspeech-body-1-0");
        let args = curl_args(Duration::from_millis(1500), std::path::Path::new("/tmp/openspeech-key-1-1"));
        let joined: Vec<String> = args.iter().map(|a| a.to_string_lossy().into_owned()).collect();
        assert_eq!(joined[0], "-q", "~/.curlrc を読ませない");
        assert_eq!(joined[joined.iter().position(|a| a == "--max-time").unwrap() + 1], "1");
        assert_eq!(joined.last().unwrap(), "/tmp/openspeech-key-1-1");
        for arg in &joined {
            assert!(!arg.contains("sk-secret-key") && !arg.contains("右肺上葉") && !arg.contains("Authorization"), "{arg}");
            assert!(!config.lines().any(|line| line == arg), "設定ファイルの行が引数に出ている: {arg}");
        }
    }

    #[test]
    fn binary_and_json_bodies_are_read_from_the_body_file() {
        let binary = HttpRequest {
            url: "https://example.invalid/listen?a=b".to_string(),
            headers: vec![],
            body: Body::Binary {
                content_type: "audio/wav".to_string(),
                bytes: vec![0],
            },
        };
        let config = curl_config(&binary, "/tmp/b");
        assert!(config.contains("header = \"Content-Type: audio/wav\"\ndata-binary = \"@/tmp/b\"\n"));
        let json = HttpRequest {
            body: Body::Json("{}".to_string()),
            ..binary
        };
        assert!(curl_config(&json, "/tmp/j").contains("Content-Type: application/json"));
        assert_eq!(percent_encode("右 a&b=c"), "%E5%8F%B3%20a%26b%3Dc");
    }
}
