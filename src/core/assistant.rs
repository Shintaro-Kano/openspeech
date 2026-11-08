//! 「ヘイ、クロード」の依頼(このアプリ独自、計画書の単位 B と 2026-11-09 の §7)。
//!
//! 呼びかけ語(既定は「ヘイクロード」)で始まる発話を Claude への依頼にする。依頼は 2 種類:
//! テンプレートの補完(辞書ファイルのテンプレートから 1 つ選び、言われた値を入れた本文を返す)と、
//! 文章の手直し(添えた「対象の文章」を直した全文を返す)。どちらでもなければ短い返事だけ。
//! モードは持たず、呼びかけ 1 回 = 依頼 1 回。呼びかけだけの発話なら、次の発話を依頼として待つ
//! (`app/assistant_flow.rs` の `call_waiting`)。
//!
//! 答え方の規則は利用者が `request.md` で書き換える(無いか空なら `DEFAULT_REQUEST_GUIDANCE`)。コードに残すのは
//! 答えの形の約束(`REQUEST_FRAME`)と、手直しを当てる前の門(`check_edit`: 左右・長さ・同じ文)だけ。
//!
//! 接続は訂正役と同じ 2 通り(API キー、この Mac の Claude Code)で、通信の道具は `claude_api.rs`・`proc.rs` のものを使う。
//! Claude に渡るのは依頼の文、対象の文章(選択範囲か全文の先頭 3000 字)、テンプレートの一覧、用語集、
//! 直前に補完した文(言い直し用)。音声は送らない。

use super::claude_api;
use super::corrector::{self, model_label};
use super::dictionary::{template_key, Dictionary, Module};
use super::guidance;
use super::judge::{adds_laterality, changed_ranges};
use super::proc;
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
/// 依頼に添える対象の文章の上限(字)。全文がこれより長ければ先頭だけ添える。
pub const MAX_TARGET_CHARS: usize = 3000;
/// 依頼の指示(`request.md`)の上限(字)。訂正の指示と同じ。
pub const MAX_REQUEST_GUIDANCE_CHARS: usize = 4000;
/// 手直しの答えは、対象の字数の 2 倍にこれを足した字数まで(内容を足さない約束なので、それより長い答えは使わない)。
pub const EDIT_SLACK_CHARS: usize = 100;
/// 補完した本文は長くても数百トークン。考える分を含めてもこれで足りる。
const MAX_TOKENS: u32 = 2048;
/// 対象の文章を添えるときの土台。選択範囲の手直しは考える量が medium なので、考える分を多めに取り、
/// 全文を書き直せるよう、さらに対象の字数の 2 倍を足す。
const MAX_TOKENS_WITH_TARGET: u32 = 4096;
/// API の応答を待つ上限(curl の --max-time)。訂正より長い出力なので訂正役より長く取る。
const API_TIMEOUT_SECONDS: u64 = 15;
/// Claude Code の答えを待つ上限。起動の分だけ API より長く取る。
const CLAUDE_CODE_TIMEOUT_SECONDS: u64 = 25;
/// 対象の文章を添えるときに待ちを延ばす割合(この字数ごとに 1 秒)。手直しは全文を書き直すので長くかかる。
const TARGET_CHARS_PER_SECOND: usize = 40;
/// 誤りの知らせに入れる役の名前。
const ROLE_NAME: &str = "補完役";
/// 指示の節の前置き。
const GUIDANCE_HEADER: &str = "依頼への答え方の指示:";

/// 役割と答えの形の約束。答えを本文としてそのまま取り込むので、ここだけはコードで固定する。
/// 答え方の規則は `DEFAULT_REQUEST_GUIDANCE`(利用者が `request.md` で書き換える)にある。
const REQUEST_FRAME: &str = "あなたは音声入力アプリの中で、話者の依頼に答える係です。下の指示に従って依頼に答えます。\n\
答えは決められた形で書く: kind に依頼の種類(template / edit / none)、text に本文、reply に短い返事、\
template に選んだテンプレートの先頭のキーワード(template のときだけ)、replace に言い直しかどうか。\n\
text は本文だけで、説明や前置き、Markdown を書かない。\n";

/// 依頼への答え方の指示。利用者が `request.md` で自由に書き換える。ファイルが無いか空ならこれをそのまま使い、
/// 「依頼の指示を編集…」で初めて作るファイルの中身もこれ。先頭の説明は `<!-- -->` の注釈で、送らない。
pub const DEFAULT_REQUEST_GUIDANCE: &str = "\
<!--
ここに書いたことは、「ヘイクロード」の依頼を Claude に渡すときの指示としてそのまま送られます。自由に書き換えられます。
空にするとアプリの既定(初めに書かれていたこの内容)が使われます。
アプリが決めているのは、答えの形(依頼の種類・本文・返事)と、長すぎる本文や左右が変わった手直しを使わない守りだけです。
この <!-- --> で囲んだ部分は Claude に送られません。
-->
# 役割
音声入力アプリの中で、放射線科医の所見のテンプレートを補完し、頼まれれば文章を手直しする係。話者は放射線科医で、依頼は音声認識の文なので、誤りや読みのままの語を含むことがある。
テンプレートは検査名ごとの正常所見で、所見モジュール(異常があるときに差し替える文)が付くことがある。

# 依頼の種類
- template: 検査名やテンプレートの内容を言い、所見の文章を作ってほしい依頼(例: 「胸部CT。右上葉にすりガラス結節」)
- edit: 添えられた「対象の文章」を直してほしい依頼(例: 「文章を意味が一貫するように整えて」「誤字を直して」「矛盾がないか確認して」)。対象の文章が無ければ none にし、reply に「対象の文章がありません」と書く
- none: どちらでもない、または合うテンプレートが無い。text は null にし、reply に理由を 30 字以内で書く

# テンプレートの補完(template)
1. 下の「テンプレート一覧」から依頼に合うものを 1 つ選ぶ。検査名(別名を含む)で言われればそれ、無ければ内容で選ぶ。
2. 依頼で言われた値(部位・大きさ・個数・有無など)を正常所見の該当する箇所に入れる。言われていない箇所は文のまま残し、所見を作らない。〔　〕は言われなければ〔　〕のまま。
3. テンプレートが計算値を含むときだけ計算し、式を括弧で併記する(例: 約 64 mL(0.52×5.5×4.5×5.0))。
4. 所見モジュールは、依頼でその所見が言われたときだけ使う。使うときは正常所見のうち同じ臓器・項目の文をモジュールの文と差し替え、ほかの文は残す。「使い方」があればそれに従う(全体を差し替える、文を足す、など)。言われていないモジュールは使わない。
5. 左右・否定は言われた通り。言い換え・要約・説明文・Markdown は書かない。
6. 「直前に補完した文章」が添えられていて、依頼がその文章の言い直し(値の変更・追記・削除)なら replace を true にし、text にはその文章を直した全文を書く。新しい依頼なら replace は false。
7. text は補完した本文だけ。reply は空(null)にする。

# 文章の手直し(edit)
- text には「対象の文章」を直した全文を書く(対象の全体を置き換えるので、直さない部分もそのまま含める)。直すところが無ければ対象の文章をそのまま書く。
- 直してよいもの: 誤字・同音の取り違え、助詞、重複、文のつながり、前後で食い違う左右・有無・数(どちらが正しいか文脈から明らかなとき)。依頼で言われたこと(「〜を〜に直して」など)に従う。
- してはいけないこと: 内容の追加・削除、否定の反転、依頼で言われていない左右の変更、数値と単位の変更、文体の変更、要約。
- どちらが正しいか決められない矛盾(左右・有無・数など)は直さずに残し、reply に 60 字以内で指摘する。直すところが無ければ reply に「直すところはありません」と書く。
";

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

/// 依頼に添える対象の文章。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Target {
    pub text: String,
    /// 利用者が選んだ範囲か(選んでいなければ全文)。選択は手直しの明示の合図なので、考える量を増やす。
    pub selected: bool,
    /// 長いので先頭 `MAX_TARGET_CHARS` 字だけ添えたか。
    pub truncated: bool,
}

/// 1 回の依頼。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AssistantRequest {
    /// 依頼の文(呼びかけ語を除き、整形済み)。
    pub request: String,
    /// 直前に補完した本文がエディターに手付かずで残っていれば添える(「体積を 70 に直して」のような言い直し用)。
    pub previous: Option<String>,
    /// 対象の文章(選択範囲、無ければ全文の先頭)。エディターが空なら None。
    pub target: Option<Target>,
}

/// 依頼の種類(Claude の見立て)。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ReplyKind {
    /// テンプレートの補完。
    #[default]
    Template,
    /// 対象の文章の手直し。
    Edit,
    /// どちらでもない(返事だけ)。
    None,
}

/// Claude の答え。`text` があれば本文(補完した本文か、手直しした全文)、無ければ `reply` が理由。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AssistantReply {
    pub kind: ReplyKind,
    /// 選んだテンプレートの先頭のキーワード(知らせと試験に使う)。
    pub template: Option<String>,
    pub text: Option<String>,
    /// 返事。手直しでは、直さずに残した矛盾の指摘。
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

/// 依頼への答え方の指示の節。`<!-- -->` の注釈は送らない。空(ファイルが無い・空・注釈だけ)なら既定の指示。
fn guidance_section(guidance: &str) -> String {
    let kept = guidance::prepare(guidance, DEFAULT_REQUEST_GUIDANCE, MAX_REQUEST_GUIDANCE_CHARS);
    format!("\n{GUIDANCE_HEADER}\n{kept}\n")
}

/// 答えの形の約束、答え方の指示、テンプレート一覧、用語集をこの順に並べる。辞書と指示が変わらない限り同じ文なので、
/// API 側のキャッシュが効く。
pub fn build_system_prompt(dictionary: &Dictionary, guidance: &str) -> String {
    [
        REQUEST_FRAME,
        &guidance_section(guidance),
        &template_section(dictionary),
        &corrector::glossary_section(dictionary),
    ]
    .concat()
}

/// 対象の文章の見出し(選択範囲か、全文か、全文の先頭か)。
fn target_heading(target: &Target) -> String {
    match (target.selected, target.truncated) {
        (true, false) => "対象の文章(選択範囲):".to_string(),
        (true, true) => format!("対象の文章(選択範囲の先頭 {MAX_TARGET_CHARS} 字):"),
        (false, false) => "対象の文章(全文):".to_string(),
        (false, true) => format!("対象の文章(先頭 {MAX_TARGET_CHARS} 字):"),
    }
}

pub fn build_user_message(request: &AssistantRequest) -> String {
    let mut message = String::from("依頼:\n");
    message.push_str(request.request.trim());
    if let Some(target) = request.target.as_ref().filter(|t| !t.text.trim().is_empty()) {
        message.push_str("\n\n");
        message.push_str(&target_heading(target));
        message.push('\n');
        message.push_str(&target.text);
    }
    if let Some(previous) = request.previous.as_deref().map(str::trim).filter(|p| !p.is_empty()) {
        message.push_str("\n\n直前に補完した文章(言い直しの依頼ならこれを直す):\n");
        message.push_str(previous);
    }
    message
}

/// 考える量。選択範囲の手直しは利用者の明示の合図なので medium、ほか(テンプレートの補完の所要を増やさない)は low。
pub fn effort(request: &AssistantRequest) -> &'static str {
    if request.target.as_ref().is_some_and(|t| t.selected) {
        "medium"
    } else {
        "low"
    }
}

/// 添えた対象の文章の字数(無ければ 0)。
fn target_chars(request: &AssistantRequest) -> usize {
    request.target.as_ref().map_or(0, |t| t.text.chars().count())
}

/// 答えのトークンの上限。対象の文章を添えたら、手直しで全文を書き直せるよう、その字数の 2 倍を足す。
fn max_tokens(request: &AssistantRequest) -> u32 {
    if request.target.is_none() {
        return MAX_TOKENS;
    }
    let extra = u32::try_from(target_chars(request) * 2).unwrap_or(u32::MAX);
    MAX_TOKENS_WITH_TARGET.saturating_add(extra)
}

/// 答えを待つ上限(秒)。手直しは長くかかるので、対象の字数に応じて延ばす。
fn timeout_seconds(base: u64, request: &AssistantRequest) -> u64 {
    base + (target_chars(request) / TARGET_CHARS_PER_SECOND) as u64
}

/// 答えの形。5 つとも必ず書かせ、無い文は null にする。
fn output_schema() -> Value {
    let nullable_text = json!({"anyOf": [{"type": "string"}, {"type": "null"}]});
    json!({
        "type": "object",
        "properties": {
            "kind": {"type": "string", "enum": ["template", "edit", "none"]},
            "template": nullable_text,
            "text": nullable_text,
            "reply": nullable_text,
            "replace": {"type": "boolean"}
        },
        "required": ["kind", "template", "text", "reply", "replace"],
        "additionalProperties": false
    })
}

/// Messages API に送る本文。system はキャッシュし、考える量は `effort`、出力は形を固定する。
pub fn request_body(model: &str, system: &str, request: &AssistantRequest) -> Value {
    claude_api::messages_body(
        model,
        max_tokens(request),
        effort(request),
        system,
        None,
        build_user_message(request),
        output_schema(),
    )
}

/// 形を指定した答え({"kind", "template", "text", "reply", "replace"})を読む。空の文字列は null と同じに扱う。
/// kind が無いか知らない値なら、本文があれば補完、無ければ該当なしと見なす。本文の無い手直しは該当なし。
/// 本文も返事も無い答え、長すぎる本文は失敗にする(手直しの長さは対象の字数で `check_edit` が見る)。
pub fn reply_from_value(value: &Value) -> Result<AssistantReply, String> {
    let field = |name: &str| {
        value[name]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    let text = field("text");
    // 本文も返事も無い答えだけを失敗にする(kind が none の本文は捨てるが、該当なしの答えとしては読める)。
    let empty = text.is_none() && field("reply").is_none();
    let kind = match (value["kind"].as_str(), &text) {
        (_, None) => ReplyKind::None,
        (Some("edit"), Some(_)) => ReplyKind::Edit,
        (Some("none"), Some(_)) => ReplyKind::None,
        (_, Some(_)) => ReplyKind::Template,
    };
    let reply = AssistantReply {
        kind,
        template: field("template"),
        text: if kind == ReplyKind::None { None } else { text },
        reply: field("reply"),
        replace: value["replace"].as_bool().unwrap_or(false),
    };
    if let Some(text) = &reply.text {
        let limit = match kind {
            ReplyKind::Edit => MAX_TARGET_CHARS * 2 + EDIT_SLACK_CHARS,
            _ => MAX_COMPLETION_CHARS,
        };
        if text.chars().count() > limit {
            return Err(if kind == ReplyKind::Edit {
                TOO_LONG.to_string()
            } else {
                "補完した文が長すぎるので使いませんでした".to_string()
            });
        }
    }
    if empty {
        return Err("Claude から答えが返りませんでした".to_string());
    }
    Ok(reply)
}

/// 手直しの答えが長すぎるときの知らせ。
pub const TOO_LONG: &str = "答えが長すぎるので使いませんでした";
/// 依頼に左右の語が無いのに、手直しで左右が変わったときの知らせ。
pub const LATERALITY_CHANGED: &str = "左右が変わった答えなので使いませんでした";

/// 手直しの答えを当ててよいか。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditCheck {
    /// 当てる。中身は当てる文(対象の前後の空白を残した答え)。
    Apply(String),
    /// 答えが対象と同じ(直すところが無い)。
    Unchanged,
    /// 使わない。中身は知らせの文。
    Refused(&'static str),
}

/// 依頼の文に左右の語があるか(あれば、左右を直す依頼かもしれないので左右の門を掛けない)。
fn mentions_side(request: &str) -> bool {
    ["右", "左", "みぎ", "ひだり"].iter().any(|side| request.contains(side))
}

/// 手直しの答えの門。順に: 依頼に左右の語が無いのに右か左の語が増えた(`adds_laterality`。重複の削除や
/// 並べ替えは通す)、対象の字数 × 2 + 100 字より長い、対象と同じ。答えは前後の空白を落として届くので、
/// 対象の前後の空白(改行など)を付け直してから比べて当てる。
pub fn check_edit(request: &str, target: &str, answer: &str) -> EditCheck {
    let body = target.trim();
    let lead = &target[..target.len() - target.trim_start().len()];
    let tail = &target[target.trim_end().len()..];
    let answer = answer.trim();
    if !mentions_side(request) && adds_laterality(body, answer) {
        return EditCheck::Refused(LATERALITY_CHANGED);
    }
    if answer.chars().count() > target.chars().count() * 2 + EDIT_SLACK_CHARS {
        return EditCheck::Refused(TOO_LONG);
    }
    if answer == body {
        return EditCheck::Unchanged;
    }
    EditCheck::Apply(format!("{lead}{answer}{tail}"))
}

/// 手直しで変わった箇所の数。足した・置き換えた字の続き(答えの側)と、消した字の続き(対象の側)の多い方
/// (消しただけの直しも 1 箇所と数える)。
pub fn edit_changes(target: &str, answer: &str) -> usize {
    changed_ranges(target, answer)
        .len()
        .max(changed_ranges(answer, target).len())
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
    pub fn new(model: &str, api_key: &str, dictionary: &Dictionary, guidance: &str) -> Self {
        ApiAssistant {
            model: model.to_string(),
            api_key: api_key.to_string(),
            system: build_system_prompt(dictionary, guidance),
        }
    }
}

impl Assistant for ApiAssistant {
    fn name(&self) -> String {
        model_label(&self.model)
    }

    fn complete(&self, request: &AssistantRequest) -> Result<AssistantReply, String> {
        let body = request_body(&self.model, &self.system, request);
        let timeout = timeout_seconds(API_TIMEOUT_SECONDS, request).to_string();
        let (status, response) = claude_api::post_json(&self.api_key, &body, &timeout, ROLE_NAME)?;
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
    pub fn new(
        model: &str,
        binary: &Path,
        work_dir: &Path,
        dictionary: &Dictionary,
        guidance: &str,
    ) -> Self {
        ClaudeCodeAssistant {
            model: model.to_string(),
            binary: binary.to_path_buf(),
            work_dir: work_dir.to_path_buf(),
            system: build_system_prompt(dictionary, guidance),
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
            proc::Answer { schema: &output_schema(), effort: effort(request) },
            &message,
            Duration::from_secs(timeout_seconds(CLAUDE_CODE_TIMEOUT_SECONDS, request)),
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
        let system = build_system_prompt(&dictionary(), "");
        assert!(system.starts_with(REQUEST_FRAME));
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
        assert!(build_system_prompt(&Dictionary::default(), "").contains("(登録なし)"));
    }

    /// 並びは 答えの形の約束 → 答え方の指示 → テンプレート一覧 → 用語集。
    #[test]
    fn the_frame_comes_first_then_the_guidance_the_templates_and_the_glossary() {
        let system = build_system_prompt(&dictionary(), "胸部は 5 mm 単位で書く");
        let at = |part: &str| system.find(part).unwrap_or_else(|| panic!("{part}: {system}"));
        assert_eq!(at("あなたは音声入力アプリの中で"), 0);
        assert!(at("text は本文だけ") < at(GUIDANCE_HEADER));
        assert!(at(GUIDANCE_HEADER) < at("胸部は 5 mm 単位で書く"));
        assert!(at("胸部は 5 mm 単位で書く") < at("テンプレート一覧:"));
        assert!(at("テンプレート一覧:") < at("用語集(表記 / 読み):"));
        assert_eq!(GUIDANCE_HEADER, "依頼への答え方の指示:");
        assert!(!system.contains("# 役割"), "利用者の指示が既定に置き換わる");
    }

    /// 指示が無いか空(空白・注釈だけ)なら既定を使う。注釈は送らない。上限で切る。
    #[test]
    fn the_request_guidance_defaults_strips_comments_and_is_cut() {
        let plain = build_system_prompt(&dictionary(), "");
        for empty in [" \n", "<!-- メモだけ -->\n"] {
            assert_eq!(build_system_prompt(&dictionary(), empty), plain);
        }
        assert!(plain.contains(&format!("{GUIDANCE_HEADER}\n# 役割\n")), "{plain}");
        assert!(plain.contains("# 文章の手直し(edit)"));
        assert!(DEFAULT_REQUEST_GUIDANCE.starts_with("<!--\nここに書いたことは"));
        assert!(!plain.contains("<!--") && !plain.contains("-->") && !plain.contains("ここに書いたことは"));
        let own = build_system_prompt(&dictionary(), "<!-- 自分用 -->\n短く答える<!-- 理由 -->");
        assert!(own.contains("短く答える") && !own.contains("自分用") && !own.contains("理由"));
        let long = format!("{}余", "彙".repeat(MAX_REQUEST_GUIDANCE_CHARS));
        let cut = build_system_prompt(&dictionary(), &long);
        assert_eq!(cut.matches('彙').count(), MAX_REQUEST_GUIDANCE_CHARS);
        assert!(!cut.contains('余') && cut.contains("テンプレート一覧:"));
    }

    #[test]
    fn the_user_message_carries_the_request_and_the_previous_text() {
        let request = AssistantRequest {
            request: "前立腺MRI。サイズは 5.5×4.5×5.0".to_string(),
            ..AssistantRequest::default()
        };
        assert_eq!(build_user_message(&request), "依頼:\n前立腺MRI。サイズは 5.5×4.5×5.0");
        let again = AssistantRequest {
            request: "体積を 70 に直して".to_string(),
            previous: Some("前立腺MRI\n推定 64 mL。".to_string()),
            target: None,
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

    /// 対象の文章は依頼の後、直前の補完の前。見出しは選択範囲・全文・先頭 3000 字で変わる。
    #[test]
    fn the_target_follows_the_request_with_its_heading() {
        let target = |selected: bool, truncated: bool| AssistantRequest {
            request: "誤字を直して".to_string(),
            previous: Some("直前の補完".to_string()),
            target: Some(Target { text: "右肺に結節を認めまる。".to_string(), selected, truncated }),
        };
        assert_eq!(
            build_user_message(&target(true, false)),
            "依頼:\n誤字を直して\n\n対象の文章(選択範囲):\n右肺に結節を認めまる。\n\n直前に補完した文章(言い直しの依頼ならこれを直す):\n直前の補完"
        );
        assert!(build_user_message(&target(false, false)).contains("\n\n対象の文章(全文):\n右肺に"));
        assert!(build_user_message(&target(false, true)).contains("\n\n対象の文章(先頭 3000 字):\n右肺に"));
        assert!(build_user_message(&target(true, true)).contains("\n\n対象の文章(選択範囲の先頭 3000 字):\n"));
    }

    /// 考える量は選択範囲を添えたときだけ medium(API の effort、Claude Code の --effort)。ほかは low。
    #[test]
    fn only_a_selected_target_raises_the_effort() {
        let request = |target: Option<Target>| AssistantRequest {
            request: "整えて".to_string(),
            previous: None,
            target,
        };
        let selected = request(Some(Target { text: "文。".to_string(), selected: true, truncated: false }));
        let whole = request(Some(Target { text: "文。".to_string(), selected: false, truncated: false }));
        assert_eq!(request_body("m", "規則", &selected)["output_config"]["effort"], "medium");
        assert_eq!(request_body("m", "規則", &whole)["output_config"]["effort"], "low");
        assert_eq!(request_body("m", "規則", &request(None))["output_config"]["effort"], "low");
        assert_eq!(request_body("m", "規則", &request(None))["max_tokens"], MAX_TOKENS);
        assert_eq!(request_body("m", "規則", &whole)["max_tokens"], MAX_TOKENS_WITH_TARGET + 4, "手直しで書き直す分を足す");
        assert_eq!(request_body("m", "規則", &selected)["max_tokens"], 4100);

        let dir = std::env::temp_dir().join(format!("openspeech-fake-effort-{}", std::process::id()));
        let reply = r#"{"is_error":false,"structured_output":{"kind":"edit","template":null,"text":"文。","reply":null,"replace":false}}"#;
        let fake = proc::fake_claude(&dir, reply);
        let assistant = ClaudeCodeAssistant::new("claude-opus-5-5", &fake, &dir, &dictionary(), "");
        assistant.complete(&selected).unwrap();
        assistant.complete(&whole).unwrap();
        let log = std::fs::read_to_string(dir.join("claude.log")).unwrap();
        let efforts: Vec<&str> = log.lines().filter(|l| l.starts_with("EFFORT: ")).collect();
        assert_eq!(efforts, ["EFFORT: medium", "EFFORT: low"], "{log}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_request_body_caches_the_system_and_fixes_the_shape() {
        let body = request_body(
            "claude-opus-5-5",
            "規則",
            &AssistantRequest {
                request: "胸部CT".to_string(),
                ..AssistantRequest::default()
            },
        );
        assert_eq!(body["model"], "claude-opus-5-5");
        assert_eq!(body["system"][0]["cache_control"]["type"], "ephemeral");
        assert_eq!(body["output_config"]["effort"], "low");
        let schema = &body["output_config"]["format"]["schema"];
        assert_eq!(schema["required"], json!(["kind", "template", "text", "reply", "replace"]));
        assert_eq!(schema["properties"]["kind"]["enum"], json!(["template", "edit", "none"]));
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
            "",
        );
        assert!(output_schema().to_string().contains("\"template\""));
        assert!(assistant.instructions().contains("### 胸部CT"));
        assert_eq!(assistant.name(), "Claude Opus 5.5(高精度)・Claude Code");
    }

    /// 偽の claude で、補完役が自分のシステムプロンプトと答えの形を渡し、答えを読むことを確かめる。
    #[test]
    fn claude_code_completion_passes_its_own_prompt_and_shape() {
        let dir = std::env::temp_dir().join(format!("openspeech-fake-complete-{}", std::process::id()));
        let reply = r#"{"is_error":false,"structured_output":{"kind":"template","template":"胸部CT","text":"肺野に異常なし。","reply":null,"replace":false}}"#;
        let fake = proc::fake_claude(&dir, reply);
        let assistant = ClaudeCodeAssistant::new("claude-opus-5-5", &fake, &dir, &dictionary(), "");
        let request = AssistantRequest {
            request: "胸部CT".to_string(),
            ..AssistantRequest::default()
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

    /// kind を読む。無いか知らない値なら本文の有無で補い、本文の無い手直しは該当なし。
    /// 手直しの本文はテンプレートの上限より長くてよい(長さは対象の字数で門が見る)。
    #[test]
    fn the_kind_is_read_and_filled_in_from_the_text() {
        let read = |value: Value| reply_from_value(&value).unwrap();
        assert_eq!(read(json!({"kind": "edit", "text": "直した文。", "reply": "指摘"})).kind, ReplyKind::Edit);
        assert_eq!(read(json!({"kind": "template", "text": "本文"})).kind, ReplyKind::Template);
        assert_eq!(read(json!({"text": "本文"})).kind, ReplyKind::Template, "kind が無ければ本文の有無");
        assert_eq!(read(json!({"kind": "rewrite", "text": "本文"})).kind, ReplyKind::Template, "知らない値も同じ");
        let edit_without_text = read(json!({"kind": "edit", "text": null, "reply": "対象の文章がありません"}));
        assert_eq!(edit_without_text.kind, ReplyKind::None);
        let none = read(json!({"kind": "none", "text": "余計な本文", "reply": "該当なし"}));
        assert_eq!((none.kind, none.text), (ReplyKind::None, None));
        let silent = read(json!({"kind": "none", "text": "余計な本文", "reply": null}));
        assert_eq!((silent.kind, silent.text, silent.reply), (ReplyKind::None, None, None), "該当なしとして読む(失敗にしない)");
        let long_edit = "あ".repeat(MAX_COMPLETION_CHARS + 1);
        assert_eq!(read(json!({"kind": "edit", "text": long_edit})).text.unwrap().chars().count(), MAX_COMPLETION_CHARS + 1);
        let too_long = "あ".repeat(MAX_TARGET_CHARS * 2 + EDIT_SLACK_CHARS + 1);
        assert_eq!(reply_from_value(&json!({"kind": "edit", "text": too_long})).unwrap_err(), TOO_LONG);
    }

    /// 手直しの門: 依頼に左右が無いのに左右が変わった・長すぎる・同じ文。対象の前後の空白は残して当てる。
    #[test]
    fn edits_pass_the_gates_in_order() {
        let target = "右肺上葉に結節を認めまる。\n";
        assert_eq!(
            check_edit("誤字を直して", target, "右肺上葉に結節を認めます。"),
            EditCheck::Apply("右肺上葉に結節を認めます。\n".to_string())
        );
        assert_eq!(
            check_edit("誤字を直して", target, "左肺上葉に結節を認めます。"),
            EditCheck::Refused(LATERALITY_CHANGED)
        );
        for asked in ["右を左に直して", "みぎをひだりに", "左右を直して"] {
            assert!(matches!(check_edit(asked, target, "左肺上葉に結節を認めます。"), EditCheck::Apply(_)), "{asked}");
        }
        // 重複の削除・文の並べ替えは、左右の並びが変わっても語が増えないので通す。
        let twice = "右肺に結節。右肺に結節。左腎に嚢胞。";
        assert!(matches!(check_edit("重複を消して", twice, "右肺に結節。左腎に嚢胞。"), EditCheck::Apply(_)));
        assert!(matches!(check_edit("並べ替えて", "右肺に結節。左腎に嚢胞。", "左腎に嚢胞。右肺に結節。"), EditCheck::Apply(_)));
        assert_eq!(check_edit("整えて", twice, "右肺に結節。左肺に結節。左腎に嚢胞。"), EditCheck::Refused(LATERALITY_CHANGED));
        let limit = target.chars().count() * 2 + EDIT_SLACK_CHARS;
        let long = |chars: usize| format!("右{}", "あ".repeat(chars - 1));
        assert_eq!(check_edit("整えて", target, &long(limit + 1)), EditCheck::Refused(TOO_LONG));
        assert!(matches!(check_edit("整えて", target, &long(limit)), EditCheck::Apply(_)));
        assert_eq!(check_edit("整えて", target, " 右肺上葉に結節を認めまる。 "), EditCheck::Unchanged);
    }

    #[test]
    fn edit_changes_count_insertions_and_deletions() {
        assert_eq!(edit_changes("結節を認めまる。", "結節を認めます。"), 1);
        assert_eq!(edit_changes("肺に結節結節を認めます。胸水なし。", "肺に結節を認めます。胸水なし"), 2);
        assert_eq!(edit_changes("重複重複です。", "重複です。"), 1, "消しただけの直しも数える");
    }

    #[test]
    fn api_and_claude_code_answers_are_read_the_same_way() {
        let api_body = json!({
            "content": [
                {"type": "thinking", "thinking": "…"},
                {"type": "text", "text": "{\"kind\":\"template\",\"template\":\"胸部CT\",\"text\":\"胸部単純CT\",\"reply\":null}"}
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
