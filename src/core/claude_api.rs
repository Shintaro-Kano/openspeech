//! Anthropic の Messages API への送信。訂正役とテンプレート補完で共用する。
//! HTTP の道具は持たず `curl` を呼ぶ。API キーは所有者だけが読める一時ファイル(curl の設定)で渡し、引数に出さない。
//! 誤りの知らせには役の名前(「訂正役」「補完役」)を入れる。

use super::proc::{PrivateFile, KEY_PREFIX};
use serde_json::{json, Value};
use std::io::Write;
use std::process::{Command, Stdio};

pub(crate) const API_URL: &str = "https://api.anthropic.com/v1/messages";
pub(crate) const API_VERSION: &str = "2026-10-21";
/// macOS に必ずある curl(PATH を見ない)。
const CURL: &str = "/usr/bin/curl";
/// API キーに使えない文字があるときの知らせ。
pub const UNUSABLE_KEY: &str = "API キーに使えない文字(改行・空白・引用符など)が入っています。貼り付け直してください";

/// API キーとして curl の設定ファイルに書いてよいか。前後の空白を除いて、表示できる ASCII だけ(空白・引用符・
/// バックスラッシュを除く)。改行が入ると設定ファイルに別の行を足せてしまう。
pub fn usable_key(key: &str) -> bool {
    let key = key.trim();
    !key.is_empty()
        && key
            .chars()
            .all(|c| c.is_ascii_graphic() && c != '"' && c != '\\' && c != '\'')
}

/// Messages API に送る本文。考える量は最小(effort low)、出力は `schema` の形に固定する。
/// system は 1 つ目(規則・用語集)だけキャッシュし、変わりやすいもの(`uncached`)は 2 つ目に分けて載せる。
pub(crate) fn messages_body(
    model: &str,
    max_tokens: u32,
    system: &str,
    uncached: Option<&str>,
    user_message: String,
    schema: Value,
) -> Value {
    let mut system_blocks = vec![json!({
        "type": "text",
        "text": system,
        "cache_control": {"type": "ephemeral"}
    })];
    if let Some(uncached) = uncached {
        system_blocks.push(json!({"type": "text", "text": uncached}));
    }
    json!({
        "model": model,
        "max_tokens": max_tokens,
        "system": system_blocks,
        "messages": [{"role": "user", "content": user_message}],
        "output_config": {
            "effort": "low",
            "format": {
                "type": "json_schema",
                "schema": schema
            }
        }
    })
}

/// curl で JSON を送り、(HTTP 状態, 本文) を返す。`timeout_seconds` は curl の `--max-time`。
/// `role` は誤りの知らせに入れる役の名前。
pub(crate) fn post_json(
    api_key: &str,
    body: &Value,
    timeout_seconds: &str,
    role: &str,
) -> Result<(u16, String), String> {
    if api_key.trim().is_empty() {
        return Err("API キーが設定されていません".to_string());
    }
    if !usable_key(api_key) {
        return Err(UNUSABLE_KEY.to_string());
    }
    let key_file = PrivateFile::create(
        KEY_PREFIX,
        &format!("header = \"x-api-key: {}\"\n", api_key.trim()),
    )?;
    // 固定の場所の curl を使い、`-q` で利用者の ~/.curlrc を読ませない(先頭に置く必要がある)。
    let mut child = Command::new(CURL)
        .args(["-q", "-sS", "--max-time", timeout_seconds, "-X", "POST", API_URL])
        .args(["-H", "content-type: application/json"])
        .args(["-H", &format!("anthropic-version: {API_VERSION}")])
        .arg("-K")
        .arg(key_file.path())
        .args(["--data-binary", "@-", "-w", "\n%{http_code}"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("curl を起動できません: {e}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        // 先に curl が終わっていたら書けないが、その場合は結果の方に理由が出る。
        let _ = stdin.write_all(body.to_string().as_bytes());
    }
    let output = child
        .wait_with_output()
        .map_err(|e| format!("curl の結果を読めません: {e}"))?;
    drop(key_file);
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("{role}に接続できません: {}", stderr.trim()));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    split_status(&stdout).ok_or_else(|| format!("{role}の応答が不正です"))
}

/// curl の出力(本文の後ろに改行と HTTP 状態)を分ける。
fn split_status(stdout: &str) -> Option<(u16, String)> {
    let (body, code) = stdout.trim_end().rsplit_once('\n')?;
    let status: u16 = code.trim().parse().ok()?;
    Some((status, body.to_string()))
}

/// API の応答から、答えの文(text のブロックをつないだもの)を取り出す。誤りの応答は利用者向けの文にする。
/// 答えの形はそれぞれの役が読む。`role` は誤りの知らせに入れる役の名前。
pub(crate) fn response_text(status: u16, body: &str, role: &str) -> Result<String, String> {
    let root: Value = serde_json::from_str(body)
        .map_err(|_| format!("{role}の応答が読めません(HTTP {status})"))?;
    if status != 200 {
        let message = root["error"]["message"].as_str().unwrap_or("").to_string();
        return Err(match status {
            401 => "API キーが正しくありません".to_string(),
            402 => "API の残高がありません".to_string(),
            403 => format!("API キーに権限がありません: {message}"),
            404 => format!("{role}のモデルが使えません: {message}"),
            429 => format!("{role}が混み合っています"),
            500..=599 => format!("{role}のサービスが一時的に使えません"),
            _ => format!("{role}のエラー(HTTP {status}): {message}"),
        });
    }
    match root["stop_reason"].as_str() {
        Some("refusal") => return Err(format!("{role}が応答を断りました")),
        Some("max_tokens") => return Err(format!("{role}の出力が長すぎて途中で切れました")),
        _ => {}
    }
    // 考えた分(thinking)が先に来ることがあるので、種類で選ぶ。
    let text: String = root["content"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|block| block["type"] == "text")
        .filter_map(|block| block["text"].as_str())
        .collect();
    let text = text.trim();
    if text.is_empty() {
        return Err(format!("{role}から文が返りませんでした"));
    }
    Ok(text.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errors_name_the_role_that_failed() {
        let busy = r#"{"error":{"message":"overloaded"}}"#;
        assert_eq!(response_text(429, busy, "補完役").unwrap_err(), "補完役が混み合っています");
        assert_eq!(response_text(429, busy, "訂正役").unwrap_err(), "訂正役が混み合っています");
        assert!(response_text(200, "not json", "補完役").unwrap_err().starts_with("補完役"));
        let cut = r#"{"stop_reason":"max_tokens","content":[]}"#;
        assert!(response_text(200, cut, "補完役").unwrap_err().starts_with("補完役"));
    }

    #[test]
    fn the_status_line_is_split_from_the_body() {
        assert_eq!(split_status("{\"a\":1}\n200"), Some((200, "{\"a\":1}".to_string())));
        assert_eq!(split_status("{}\n429\n"), Some((429, "{}".to_string())));
        assert_eq!(split_status("no status"), None);
    }

    #[test]
    fn keys_that_could_break_the_curl_config_are_refused() {
        assert!(usable_key("sk-ant-api03-AbC_123-xyz"));
        assert!(usable_key("  sk-ant-api03-abc\n"), "前後の空白と改行は除いて見る");
        for bad in ["sk-ant\nheader = \"x\"", "sk ant", "sk-\"ant", "sk-\\ant", "sk-'ant", "鍵"] {
            assert!(!usable_key(bad), "{bad:?}");
            let error = post_json(bad, &json!({}), "1", "訂正役").unwrap_err();
            assert_eq!(error, UNUSABLE_KEY, "{bad:?}");
        }
    }

    #[test]
    fn a_missing_key_is_refused_before_curl_runs() {
        let error = post_json("  ", &json!({}), "1", "訂正役").unwrap_err();
        assert!(error.contains("API キー"), "{error}");
    }

    #[test]
    fn the_body_caches_only_the_first_system_block() {
        let body = messages_body("m", 10, "規則", Some("例"), "文".to_string(), json!({}));
        assert_eq!(body["system"][0]["cache_control"]["type"], "ephemeral");
        assert!(body["system"][1].get("cache_control").is_none());
        assert_eq!(body["max_tokens"], 10);
        let body = messages_body("m", 10, "規則", None, "文".to_string(), json!({}));
        assert_eq!(body["system"].as_array().unwrap().len(), 1);
    }
}
