//! 「ヘイ、クロード」のテンプレート補完(このアプリ独自、計画書の単位 B)。
//!
//! 呼びかけ語(既定は「ヘイクロード」)で始まる発話を Claude への依頼にする。Claude は辞書ファイルの
//! テンプレートから依頼に合うものを 1 つ選び、依頼で言われた値を入れた本文を返す。質問には答えない
//! (合うテンプレートが無ければ短い返事だけ)。モードは持たず、呼びかけ 1 回 = 依頼 1 回。
//! 呼びかけだけの発話なら、次の発話を依頼として待つ(`app/assistant_flow.rs` の `call_waiting`)。
//!
//! 接続は訂正役と同じ 2 通り(API キー、この Mac の Claude Code)で、通信の道具は `claude_api.rs`・`proc.rs` のものを使う。
//! Claude に渡るのは依頼の文、テンプレートの一覧、用語集、直前に補完した文(言い直し用)。音声は送らない。

use super::claude_api;
use super::corrector::{self, model_label};
use super::proc;
use super::dictionary::{template_key, Dictionary, Module};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// 既定の呼びかけ語。ならすとどちらも「へいくろーど」になり、「ヘイ、クロード」「へいクロード。」にも当たる
/// (認識エンジンは「ヘイクロード」「へいクロード」と書く。本機の合成音 6 本で確かめた、2026-11-07)。
/// 句読点入りの表記は既定に入れない(設定の入力欄は「、」で語を区切るので、「ヘイ」と「クロード」に割れる)。
pub const DEFAULT_CALL_WORDS: [&str; 2] = ["ヘイクロード", "へいくろーど"];
/// 一覧に載せるテンプレートの上限。
pub const MAX_TEMPLATES: usize = 100;
/// 補完した本文の上限(字)。これより長い答えは捨てる(テンプレートの文より長くなる理由がない)。
pub const MAX_COMPLETION_CHARS: usize = 3000;
/// 補完した本文は長くても数百トークン。考える分を含めてもこれで足りる。
const MAX_TOKENS: u32 = 2048;
/// API の応答を待つ上限(curl の --max-time)。訂正より長い出力なので訂正役より長く取る。
const API_TIMEOUT_SECONDS: &str = "15";
/// Claude Code の答えを待つ上限。起動の分だけ API より長く取る。
const CLAUDE_CODE_TIMEOUT: Duration = Duration::from_secs(25);
/// 誤りの知らせに入れる役の名前。
const ROLE_NAME: &str = "補完役";

const ROLE: &str = "音声入力アプリの中で、放射線科医の所見のテンプレートを補完する係。\
話者は放射線科医で、依頼は音声認識の文なので、誤りや読みのままの語を含むことがある。\n\
テンプレートは検査名ごとの正常所見で、所見モジュール(異常があるときに差し替える文)が付くことがある。\n\
規則:\n\
1. 下の「テンプレート一覧」から依頼に合うものを 1 つ選ぶ。検査名(別名を含む)で言われればそれ、無ければ内容で選ぶ。合うものが無ければ template と text を null にし、reply に理由を 30 字以内で書く。\n\
2. 依頼で言われた値(部位・大きさ・個数・有無など)を正常所見の該当する箇所に入れる。言われていない箇所は文のまま残し、所見を作らない。〔　〕は言われなければ〔　〕のまま。\n\
3. テンプレートが計算値を含むときだけ計算し、式を括弧で併記する(例: 約 64 mL(0.52×5.5×4.5×5.0))。\n\
4. 所見モジュールは、依頼でその所見が言われたときだけ使う。使うときは正常所見のうち同じ臓器・項目の文をモジュールの文と差し替え、ほかの文は残す。「使い方」があればそれに従う(全体を差し替える、文を足す、など)。言われていないモジュールは使わない。\n\
5. 左右・否定は言われた通り。言い換え・要約・説明文・Markdown は書かない。\n\
6. text は補完した本文だけ。template は選んだテンプレートの先頭のキーワード。reply は text が null のときだけ書く。\n\
7. 「直前に補完した文章」が添えられていて、依頼がその文章の言い直し(値の変更・追記・削除)なら replace を true にし、text にはその文章を直した全文を書く。新しい依頼なら replace は false。\n";

/// 既定の呼びかけ語。
pub fn default_call_words() -> Vec<String> {
    DEFAULT_CALL_WORDS.iter().map(|w| w.to_string()).collect()
}

/// 発話に呼びかけ語があったか。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Call {
    /// 呼びかけではない(普通の発話)。
    None,
    /// 呼びかけだけ。次の発話を依頼として待つ。
    Alone,
    /// 呼びかけに続けて依頼を言った。中身は依頼の文(句読点・空白を除いた残り)。
    WithRequest(String),
}

fn trim_marks(text: &str) -> &str {
    text.trim_matches(|c: char| c.is_whitespace() || "、。，．,.!！?？・:：「」『』()（）".contains(c))
}

/// 発話が呼びかけ語で始まるかを調べる。比べ方はテンプレートのキーワードと同じ `template_key`
/// (句読点・空白・全角半角・カタカナ/ひらがなの差を吸収。「ヘイ、クロード」も「へいクロード。」も同じ語)。
/// 依頼の取り出しは、元の文を 1 文字ずつ伸ばして、ならすと呼びかけ語になる最短の接頭辞を探し、
/// その後ろの句読点・空白を落とす(ならした文からは元に戻せないため)。長い呼びかけ語から調べる。
pub fn match_call(text: &str, call_words: &[String]) -> Call {
    let utterance = trim_marks(text);
    let key = template_key(utterance);
    if key.is_empty() {
        return Call::None;
    }
    let mut keys: Vec<String> = Vec::new();
    for word in call_words {
        let word_key = template_key(word);
        if !word_key.is_empty() && !keys.contains(&word_key) {
            keys.push(word_key);
        }
    }
    keys.sort_by_key(|k| std::cmp::Reverse(k.chars().count()));
    for word_key in keys {
        if key == word_key {
            return Call::Alone;
        }
        if !key.starts_with(&word_key) {
            continue;
        }
        let chars: Vec<char> = utterance.chars().collect();
        let mut taken = String::new();
        for (index, c) in chars.iter().enumerate() {
            taken.push(*c);
            if template_key(&taken) == word_key {
                let rest: String = chars[index + 1..].iter().collect();
                let rest = trim_marks(&rest);
                return if rest.is_empty() {
                    Call::Alone
                } else {
                    Call::WithRequest(rest.to_string())
                };
            }
        }
    }
    Call::None
}

/// 1 回の依頼。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AssistantRequest {
    /// 依頼の文(呼びかけ語を除き、整形済み)。
    pub request: String,
    /// 直前に補完した本文がエディターに手付かずで残っていれば添える(「体積を 70 に直して」のような言い直し用)。
    pub previous: Option<String>,
}

/// Claude の答え。`text` があれば補完した本文、無ければ `reply` が理由。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AssistantReply {
    /// 選んだテンプレートの先頭のキーワード(知らせと試験に使う)。
    pub template: Option<String>,
    pub text: Option<String>,
    pub reply: Option<String>,
    /// 直前に補完した文章の言い直しか(そのときは新しく足さず、その文章を置き換える)。
    pub replace: bool,
}

pub trait Assistant: Send + Sync {
    fn name(&self) -> String;
    fn complete(&self, request: &AssistantRequest) -> Result<AssistantReply, String>;
    /// 渡している指示文。テンプレートが載っているかを試験で確かめるのに使う。
    fn instructions(&self) -> &str {
        ""
    }
}

/// テンプレート一覧の節。「### キーワード(「、」区切り)」の行に正常所見を続け、所見モジュールがあれば
/// 「- 名前: 文」(続きの行は字下げ)と「使い方」を添える。
fn template_section(dictionary: &Dictionary) -> String {
    let mut section = String::from("\nテンプレート一覧:\n");
    if dictionary.templates.is_empty() {
        section.push_str("(登録なし)\n");
        return section;
    }
    for template in dictionary.templates.iter().take(MAX_TEMPLATES) {
        let keywords: Vec<&str> = template
            .keywords
            .iter()
            .map(|k| k.trim())
            .filter(|k| !k.is_empty())
            .collect();
        if keywords.is_empty() || template.body.trim().is_empty() {
            continue;
        }
        section.push_str("### ");
        section.push_str(&keywords.join("、"));
        section.push('\n');
        section.push_str(template.body.trim_end());
        section.push('\n');
        // 文の無いモジュール(名前だけ書いた物)は渡さない。
        let modules: Vec<&Module> = template
            .modules
            .iter()
            .filter(|m| !m.name.trim().is_empty() && !m.text.trim().is_empty())
            .collect();
        if !modules.is_empty() {
            section.push_str("所見モジュール:\n");
            for module in modules {
                let text = module.text.trim().replace('\n', "\n  ");
                section.push_str(&format!("- {}: {text}\n", module.name.trim()));
                let note = module.note.trim();
                if !note.is_empty() {
                    section.push_str(&format!("  使い方: {}\n", note.replace('\n', " ")));
                }
            }
        }
        section.push('\n');
    }
    section
}

/// 規則、テンプレート一覧、用語集をこの順に並べる。辞書が変わらない限り同じ文なので、API 側のキャッシュが効く。
pub fn build_system_prompt(dictionary: &Dictionary) -> String {
    [
        ROLE,
        &template_section(dictionary),
        &corrector::glossary_section(dictionary),
    ]
    .concat()
}

pub fn build_user_message(request: &AssistantRequest) -> String {
    let mut message = String::from("依頼:\n");
    message.push_str(request.request.trim());
    if let Some(previous) = request.previous.as_deref().map(str::trim).filter(|p| !p.is_empty()) {
        message.push_str("\n\n直前に補完した文章(言い直しの依頼ならこれを直す):\n");
        message.push_str(previous);
    }
    message
}

/// 答えの形。4 つとも必ず書かせ、無い文は null にする。
fn output_schema() -> Value {
    let nullable_text = json!({"anyOf": [{"type": "string"}, {"type": "null"}]});
    json!({
        "type": "object",
        "properties": {
            "template": nullable_text,
            "text": nullable_text,
            "reply": nullable_text,
            "replace": {"type": "boolean"}
        },
        "required": ["template", "text", "reply", "replace"],
        "additionalProperties": false
    })
}

/// Messages API に送る本文。system はキャッシュし、考える量は最小(effort low)、出力は形を固定する。
pub fn request_body(model: &str, system: &str, request: &AssistantRequest) -> Value {
    claude_api::messages_body(
        model,
        MAX_TOKENS,
        system,
        None,
        build_user_message(request),
        output_schema(),
    )
}

/// 形を指定した答え({"template", "text", "reply"})を読む。空の文字列は null と同じに扱う。
/// 本文も返事も無い答え、長すぎる本文は失敗にする。
pub fn reply_from_value(value: &Value) -> Result<AssistantReply, String> {
    let field = |name: &str| {
        value[name]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    let reply = AssistantReply {
        template: field("template"),
        text: field("text"),
        reply: field("reply"),
        replace: value["replace"].as_bool().unwrap_or(false),
    };
    if let Some(text) = &reply.text {
        if text.chars().count() > MAX_COMPLETION_CHARS {
            return Err("補完した文が長すぎるので使いませんでした".to_string());
        }
    }
    if reply.text.is_none() && reply.reply.is_none() {
        return Err("Claude から答えが返りませんでした".to_string());
    }
    Ok(reply)
}

/// API の応答から答えを読む。
pub fn parse_api_reply(status: u16, body: &str) -> Result<AssistantReply, String> {
    let text = claude_api::response_text(status, body, ROLE_NAME)?;
    let value: Value =
        serde_json::from_str(&text).map_err(|_| "Claude の答えが読めません".to_string())?;
    reply_from_value(&value)
}

/// API キーで直接送る補完役。
pub struct ApiAssistant {
    model: String,
    api_key: String,
    system: String,
}

impl ApiAssistant {
    pub fn new(model: &str, api_key: &str, dictionary: &Dictionary) -> Self {
        ApiAssistant {
            model: model.to_string(),
            api_key: api_key.to_string(),
            system: build_system_prompt(dictionary),
        }
    }
}

impl Assistant for ApiAssistant {
    fn name(&self) -> String {
        model_label(&self.model)
    }

    fn complete(&self, request: &AssistantRequest) -> Result<AssistantReply, String> {
        let body = request_body(&self.model, &self.system, request);
        let (status, response) =
            claude_api::post_json(&self.api_key, &body, API_TIMEOUT_SECONDS, ROLE_NAME)?;
        parse_api_reply(status, &response)
    }

    fn instructions(&self) -> &str {
        &self.system
    }
}

/// この Mac の Claude Code(サブスクのログイン)を使う補完役。依頼ごとに `claude -p` を起動する。
pub struct ClaudeCodeAssistant {
    model: String,
    binary: PathBuf,
    /// 起動する場所。ここの設定やフックを読まないよう、データのフォルダを使う。
    work_dir: PathBuf,
    system: String,
}

impl ClaudeCodeAssistant {
    pub fn new(model: &str, binary: &Path, work_dir: &Path, dictionary: &Dictionary) -> Self {
        ClaudeCodeAssistant {
            model: model.to_string(),
            binary: binary.to_path_buf(),
            work_dir: work_dir.to_path_buf(),
            system: build_system_prompt(dictionary),
        }
    }
}

impl Assistant for ClaudeCodeAssistant {
    fn name(&self) -> String {
        format!("{}・Claude Code", model_label(&self.model))
    }

    fn complete(&self, request: &AssistantRequest) -> Result<AssistantReply, String> {
        let message = build_user_message(request);
        let (stdout, stderr) = proc::run_claude_code(
            &self.binary,
            &self.work_dir,
            &self.model,
            &self.system,
            &output_schema(),
            &message,
            CLAUDE_CODE_TIMEOUT,
        )?;
        let value = proc::claude_code_structured(&stdout, &stderr)?;
        reply_from_value(&value)
    }

    fn instructions(&self) -> &str {
        &self.system
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::dictionary::{Template, Word};

    fn words(list: &[&str]) -> Vec<String> {
        list.iter().map(|w| w.to_string()).collect()
    }

    #[test]
    fn the_default_call_words_match_in_any_spelling() {
        let calls = default_call_words();
        for spoken in ["ヘイ、クロード", "へいクロード。", "ヘイクロード", "へいくろーど", " ヘイ クロード "] {
            assert_eq!(match_call(spoken, &calls), Call::Alone, "{spoken}");
        }
        // 認識エンジンの出力そのまま(合成音 2026-11-07)。
        assert_eq!(
            match_call("ヘイクロード前立腺 MRIのテンプレートサイズは5.54.5 5.0", &calls),
            Call::WithRequest("前立腺 MRIのテンプレートサイズは5.54.5 5.0".to_string())
        );
        assert_eq!(
            match_call("ヘイ、クロード、サイズを70に直して。", &calls),
            Call::WithRequest("サイズを70に直して".to_string())
        );
        assert_eq!(
            match_call("へいくろーど、胸部CTで異常なしの定型文", &calls),
            Call::WithRequest("胸部CTで異常なしの定型文".to_string())
        );
    }

    #[test]
    fn other_sentences_are_not_calls() {
        let calls = default_call_words();
        for spoken in [
            "クロード以外のモデルも試しました。",
            "ネイクロード東武MRIで慢性虚血性変化のみ",
            "ヘイ",
            "",
            "。",
            "右肺上葉に結節を認めます",
        ] {
            assert_eq!(match_call(spoken, &calls), Call::None, "{spoken}");
        }
        assert_eq!(match_call("ヘイクロード", &[]), Call::None);
        assert_eq!(match_call("ヘイクロード", &words(&["", "、"])), Call::None);
    }

    #[test]
    fn the_longest_call_word_wins() {
        let calls = words(&["クロード", "ヘイクロード"]);
        assert_eq!(
            match_call("ヘイクロード、所見を", &calls),
            Call::WithRequest("所見を".to_string())
        );
        assert_eq!(
            match_call("クロードさん、所見を", &calls),
            Call::WithRequest("さん、所見を".to_string())
        );
    }

    fn dictionary() -> Dictionary {
        let mut dictionary = Dictionary::default();
        dictionary.words.push(Word::new("腫瘤", "しゅりゅう"));
        dictionary.templates.push(Template {
            keywords: words(&["前立腺MRI", "前立腺"]),
            body: "前立腺MRI\n前立腺のサイズは〔　〕×〔　〕×〔　〕cm、推定〔　〕mL。".to_string(),
            modules: vec![
                Module {
                    name: "肉柱形成".to_string(),
                    text: "膀胱壁は肥厚し、\n肉柱形成もみられます。".to_string(),
                    note: "膀胱の文と差し替える".to_string(),
                    ..Module::default()
                },
                Module {
                    name: "PI-RADS 4/5".to_string(),
                    text: "結節を認めます。".to_string(),
                    ..Module::default()
                },
                Module {
                    name: "名前だけ".to_string(),
                    ..Module::default()
                },
            ],
            ..Template::default()
        });
        dictionary.templates.push(Template {
            keywords: words(&["胸部CT"]),
            body: "胸部単純CT\n肺に明らかな異常陰影を認めません。".to_string(),
            ..Template::default()
        });
        dictionary.templates.push(Template {
            keywords: words(&[" "]),
            body: "キーワードの無いテンプレートは載せない".to_string(),
            ..Template::default()
        });
        dictionary
    }

    #[test]
    fn the_instructions_list_the_templates_and_the_glossary() {
        let system = build_system_prompt(&dictionary());
        assert!(system.starts_with(ROLE));
        assert!(system.contains("### 前立腺MRI、前立腺\n前立腺MRI\n前立腺のサイズは"));
        assert!(
            system.contains(
                "mL。\n所見モジュール:\n- 肉柱形成: 膀胱壁は肥厚し、\n  肉柱形成もみられます。\n  使い方: 膀胱の文と差し替える\n- PI-RADS 4/5: 結節を認めます。\n\n### 胸部CT"
            ),
            "{system}"
        );
        assert!(system.contains("### 胸部CT\n胸部単純CT\n肺に明らかな異常陰影を認めません。\n\n"), "モジュールの無いテンプレートには節を付けない");
        assert!(!system.contains("名前だけ"), "文の無いモジュールは渡さない");
        assert!(!system.contains("キーワードの無い"));
        assert!(system.contains("用語集(表記 / 読み):\n- 腫瘤 / しゅりゅう"));
        assert!(build_system_prompt(&Dictionary::default()).contains("(登録なし)"));
    }

    #[test]
    fn the_user_message_carries_the_request_and_the_previous_text() {
        let request = AssistantRequest {
            request: "前立腺MRI。サイズは 5.5×4.5×5.0".to_string(),
            previous: None,
        };
        assert_eq!(build_user_message(&request), "依頼:\n前立腺MRI。サイズは 5.5×4.5×5.0");
        let again = AssistantRequest {
            request: "体積を 70 に直して".to_string(),
            previous: Some("前立腺MRI\n推定 64 mL。".to_string()),
        };
        let message = build_user_message(&again);
        assert!(message.starts_with("依頼:\n体積を 70 に直して\n\n直前に補完した文章"));
        assert!(message.ends_with("\n前立腺MRI\n推定 64 mL。"));
        assert_eq!(
            build_user_message(&AssistantRequest {
                previous: Some("  ".to_string()),
                ..again
            }),
            "依頼:\n体積を 70 に直して"
        );
    }

    #[test]
    fn the_request_body_caches_the_system_and_fixes_the_shape() {
        let body = request_body(
            "claude-opus-5-5",
            "規則",
            &AssistantRequest {
                request: "胸部CT".to_string(),
                previous: None,
            },
        );
        assert_eq!(body["model"], "claude-opus-5-5");
        assert_eq!(body["system"][0]["cache_control"]["type"], "ephemeral");
        assert_eq!(body["output_config"]["effort"], "low");
        let schema = &body["output_config"]["format"]["schema"];
        assert_eq!(schema["required"], json!(["template", "text", "reply", "replace"]));
        assert_eq!(schema["additionalProperties"], false);
        assert_eq!(body["messages"][0]["content"], "依頼:\n胸部CT");
    }

    #[test]
    fn claude_code_gets_the_templates_as_its_system_prompt() {
        let assistant = ClaudeCodeAssistant::new(
            "claude-opus-5-5",
            Path::new("/usr/bin/true"),
            Path::new("/tmp"),
            &dictionary(),
        );
        assert!(output_schema().to_string().contains("\"template\""));
        assert!(assistant.instructions().contains("### 胸部CT"));
        assert_eq!(assistant.name(), "Claude Opus 5.5(高精度)・Claude Code");
    }

    /// 偽の claude で、補完役が自分のシステムプロンプトと答えの形を渡し、答えを読むことを確かめる。
    #[test]
    fn claude_code_completion_passes_its_own_prompt_and_shape() {
        let dir = std::env::temp_dir().join(format!("openspeech-fake-complete-{}", std::process::id()));
        let reply = r#"{"is_error":false,"structured_output":{"template":"胸部CT","text":"肺野に異常なし。","reply":null,"replace":false}}"#;
        let fake = proc::fake_claude(&dir, reply);
        let assistant = ClaudeCodeAssistant::new("claude-opus-5-5", &fake, &dir, &dictionary());
        let request = AssistantRequest {
            request: "胸部CT".to_string(),
            previous: None,
        };
        let answer = assistant.complete(&request).unwrap();
        assert_eq!(answer.text.as_deref(), Some("肺野に異常なし。"));
        let log = std::fs::read_to_string(dir.join("claude.log")).unwrap();
        assert!(log.contains("### 胸部CT"), "{log}");
        let schema = log.lines().find(|l| l.starts_with("SCHEMA: ")).unwrap();
        assert!(schema.contains("\"template\""), "{schema}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 補完の失敗の知らせは「補完役」と言う(訂正役の文言を借りない)。
    #[test]
    fn api_errors_name_the_completer() {
        let busy = r#"{"error":{"message":"overloaded"}}"#;
        assert_eq!(parse_api_reply(429, busy).unwrap_err(), "補完役が混み合っています");
    }

    #[test]
    fn replies_are_read_from_the_fixed_shape() {
        let reply = reply_from_value(&json!({
            "template": "前立腺MRI",
            "text": " 前立腺MRI\n推定 64 mL。 ",
            "reply": null,
            "replace": true
        }))
        .unwrap();
        assert_eq!(reply.template.as_deref(), Some("前立腺MRI"));
        assert_eq!(reply.text.as_deref(), Some("前立腺MRI\n推定 64 mL。"));
        assert_eq!(reply.reply, None);
        assert!(reply.replace);

        let none = reply_from_value(&json!({
            "template": null,
            "text": "",
            "reply": "合うテンプレートがありません"
        }))
        .unwrap();
        assert_eq!(none.text, None);
        assert_eq!(none.reply.as_deref(), Some("合うテンプレートがありません"));
        assert!(!none.replace, "replace が無ければ新しい依頼");

        assert!(reply_from_value(&json!({"template": null, "text": null, "reply": null})).is_err());
        assert!(reply_from_value(&json!({"text": "あ".repeat(MAX_COMPLETION_CHARS + 1)})).is_err());
    }

    #[test]
    fn api_and_claude_code_answers_are_read_the_same_way() {
        let api_body = json!({
            "content": [
                {"type": "thinking", "thinking": "…"},
                {"type": "text", "text": "{\"template\":\"胸部CT\",\"text\":\"胸部単純CT\",\"reply\":null}"}
            ],
            "stop_reason": "end_turn"
        })
        .to_string();
        let reply = parse_api_reply(200, &api_body).unwrap();
        assert_eq!(reply.template.as_deref(), Some("胸部CT"));
        assert_eq!(reply.text.as_deref(), Some("胸部単純CT"));
        assert!(parse_api_reply(429, &json!({"error": {"message": "busy"}}).to_string()).is_err());

        let cli_body = json!({
            "is_error": false,
            "result": "{\"template\":null,\"text\":null,\"reply\":\"該当なし\"}",
            "structured_output": {"template": null, "text": null, "reply": "該当なし"}
        })
        .to_string();
        let value = proc::claude_code_structured(&cli_body, "").unwrap();
        let reply = reply_from_value(&value).unwrap();
        assert_eq!(reply.reply.as_deref(), Some("該当なし"));
        assert_eq!(reply.text, None);
    }
}
