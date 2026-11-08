//! 認識結果の訂正役。音声認識の取り違えを、用語集と直前の文脈をもとに
//! 言語モデル(Claude)で直す。仮表示→確定置換の二段構えの「確定」側を担う。
//!
//! 送るのは文字だけで、音声は送らない。所見の追加・削除や否定の反転は指示で禁じ、
//! 戻ってきた文は `accept_correction` で長さと左右の語を確かめてから使う。
//!
//! 接続は 2 通り。`ClaudeCorrector` は API キーで直接送り(商用の条件)、
//! `ClaudeCodeCorrector` はこの Mac の Claude Code のログインで送る(個人向けの条件)。
//! 通信の道具は `claude_api.rs`(API キー)と `proc.rs`(Claude Code)にある。

use super::dictionary::Dictionary;
use super::judge::{accept_correction, changes_laterality};
use super::{claude_api, proc};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// 既定の訂正役。速くて安い。
pub const DEFAULT_MODEL: &str = "claude-haiku-5-5";
/// 設定で選べる訂正役。
pub const MODELS: [(&str, &str); 4] = [
    ("claude-haiku-5-5", "Claude Haiku 5.5(速い・安い)"),
    ("claude-sonnet-5-5", "Claude Sonnet 5.5(より正確)"),
    ("claude-opus-5-5", "Claude Opus 5.5(高精度)"),
    ("claude-fable-5-1", "Claude Fable 5.1(最高精度・遅め)"),
];
/// 用語集に載せる登録単語の上限。
const MAX_GLOSSARY_WORDS: usize = 400;
/// 直し方の指示(`guidance.md`)の上限(字)。Claude Code は毎回指示文全体を送るので、長いほど遅く、利用枠も使う。
pub const MAX_GUIDANCE_CHARS: usize = 4000;
/// 指示の節の前置き。2026-11-08 までは「補足(規則より弱い)」だったが、規則ごとファイルに移した。
const GUIDANCE_HEADER: &str = "直し方の指示:";
/// 命令語とテンプレートの語の節の前置き。文末の語を消されると、訂正のあとの判定が効かない。
const PROTECTED_HEADER: &str =
    "次の語は音声の命令語とテンプレートの語です。話者が言ったものは文末にあっても消さず、言い換えず、そのまま残す:";
/// 直前の文脈として渡す文字数の上限。
pub const MAX_CONTEXT_CHARS: usize = 200;
/// 訂正後の文は短いので、考える分を含めてもこれで足りる。
const MAX_TOKENS: u32 = 1024;
const TIMEOUT_SECONDS: &str = "12";
/// 誤りの知らせに入れる役の名前。
const ROLE_NAME: &str = "訂正役";
/// Claude Code の答えを待つ上限。発話ごとに起動するので API より長く取る。
const CLAUDE_CODE_TIMEOUT: Duration = Duration::from_secs(20);
/// 学習済みの訂正例として渡す組の上限。
const MAX_EXAMPLES: usize = 40;
/// 訂正例ブロックの見出し行。
const EXAMPLES_HEADER: &str =
    "この話者でよくある誤認識(認識したまま → 正しい文)。同じ誤りを見たら同じように直す:";
/// 1 発話分の訂正の依頼。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CorrectionRequest {
    /// 認識した文(フィルター適用後)。
    pub text: String,
    /// あやしい語の範囲(文字位置)。訂正役に「ここを疑え」と伝える。
    pub unsure: Vec<(usize, usize)>,
    /// 直前の文(文脈の手がかり)。
    pub context: String,
}

pub trait Corrector: Send + Sync {
    fn name(&self) -> String;
    fn correct(&self, request: &CorrectionRequest) -> Result<String, String>;
    /// 訂正役に渡している指示文。補足が届いているかを試験で確かめるのに使う。
    fn instructions(&self) -> &str {
        ""
    }
}

/// Claude の Messages API を使う訂正役。
pub struct ClaudeCorrector {
    model: String,
    api_key: String,
    system: String,
    /// 学習した訂正例のブロック(無ければ None)。変わりやすいのでキャッシュしない。
    examples: Option<String>,
}

impl ClaudeCorrector {
    pub fn new(
        model: &str,
        api_key: &str,
        dictionary: &Dictionary,
        guidance: &str,
        protected: &[String],
        examples: &[(String, String)],
    ) -> Self {
        ClaudeCorrector {
            model: model.to_string(),
            api_key: api_key.to_string(),
            system: build_system_prompt(dictionary, guidance, protected),
            examples: build_examples_block(examples),
        }
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    pub fn system_prompt(&self) -> &str {
        &self.system
    }

    /// 訂正例のブロック(2 つ目の system)。例が無ければ None。
    pub fn examples_prompt(&self) -> Option<&str> {
        self.examples.as_deref()
    }
}

impl Corrector for ClaudeCorrector {
    fn name(&self) -> String {
        MODELS
            .iter()
            .find(|(id, _)| *id == self.model)
            .map_or_else(|| self.model.clone(), |(_, label)| label.to_string())
    }

    fn correct(&self, request: &CorrectionRequest) -> Result<String, String> {
        let body = request_body(&self.model, &self.system, self.examples.as_deref(), request);
        let (status, response) =
            claude_api::post_json(&self.api_key, &body, TIMEOUT_SECONDS, ROLE_NAME)?;
        let corrected = parse_response(status, &response)?;
        finish(&request.text, &corrected)
    }

    fn instructions(&self) -> &str {
        &self.system
    }
}

/// この Mac の Claude Code(サブスクのログイン)を使う訂正役。発話ごとに `claude -p` を起動する。
pub struct ClaudeCodeCorrector {
    model: String,
    binary: PathBuf,
    /// 起動する場所。ここの設定やフックを読まないよう、データのフォルダを使う。
    work_dir: PathBuf,
    /// 規則・用語集に訂正例を続けた文。
    system: String,
}

impl ClaudeCodeCorrector {
    pub fn new(
        model: &str,
        binary: &Path,
        work_dir: &Path,
        dictionary: &Dictionary,
        guidance: &str,
        protected: &[String],
        examples: &[(String, String)],
    ) -> Self {
        let mut system = build_system_prompt(dictionary, guidance, protected);
        if let Some(block) = build_examples_block(examples) {
            system.push('\n');
            system.push_str(&block);
        }
        ClaudeCodeCorrector {
            model: model.to_string(),
            binary: binary.to_path_buf(),
            work_dir: work_dir.to_path_buf(),
            system,
        }
    }
}

impl Corrector for ClaudeCodeCorrector {
    fn name(&self) -> String {
        format!("{}・Claude Code", model_label(&self.model))
    }

    fn correct(&self, request: &CorrectionRequest) -> Result<String, String> {
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
        let corrected = parse_claude_code_output(&stdout, &stderr)?;
        finish(&request.text, &corrected)
    }

    fn instructions(&self) -> &str {
        &self.system
    }
}

/// `claude -p --output-format json` の出力から訂正後の文を取り出す({"text": …} の形だけ)。
pub fn parse_claude_code_output(stdout: &str, stderr: &str) -> Result<String, String> {
    proc::claude_code_structured(stdout, stderr)?["text"]
        .as_str()
        .map(str::to_string)
        .filter(|text| !text.trim().is_empty())
        .ok_or_else(|| "訂正役から文が返りませんでした".to_string())
}

pub fn model_label(model: &str) -> String {
    MODELS
        .iter()
        .find(|(id, _)| *id == model)
        .map_or_else(|| model.to_string(), |(_, label)| label.to_string())
}

/// 役割と出力の約束。答えを文としてそのまま取り込む(説明が混ざると捨てる)ので、ここだけはコードで固定する。
/// 直し方の規則は `DEFAULT_GUIDANCE`(利用者が `guidance.md` で書き換える)にある。
const FRAME: &str =
    "あなたは日本語の音声入力の校正係です。音声認識が書き起こした文を受け取り、認識の誤りを下の指示に従って直して返します。\n\
     【 】で囲まれた語は認識の確信度が低く、直す可能性がいちばん高い箇所なので、特に疑って用語集と照らす(出力に【 】は残さない)。\n\
     直前の文はあくまで文脈の手がかりで、出力には含めない。出力は認識文を直したものだけ。\n\
     直すところが無ければ認識文をそのまま返す。短い文でも説明や前置き、断りは書かない。\n";

/// 直し方の指示。利用者が `guidance.md` で自由に書き換える。ファイルが無いか空ならこれをそのまま使い、
/// 「AI への指示を編集…」で初めて作るファイルの中身もこれ(2026-11-08 までは規則をコードで固定していた)。
/// 表記の決まりを読影の文に限っているのは、AI の直しから語の組を学習するので、文体の書き換えを招くと害になるため。
/// 「よく使う語」は認識側の `recognizer::FIELD_WORDS` と同じ(試験で確かめる)。
pub const DEFAULT_GUIDANCE: &str = "\
ここに書いたことは、認識した文を直すときの指示として AI にそのまま渡されます。自由に書き換えられます。
空にするとアプリの既定(初めに書かれていたこの内容)が使われます。
アプリが決めているのは、直した文だけを返して説明を書かないという出力の形と、左右が変わった答えを使わない守りだけです。

# 直してよいもの
- 同音・類似音の語の取り違え。特に医学用語(例: 上波 → 上葉、開業 → 改行)
- 助詞の誤りと、長音の脱落(例: 右肺が結節 → 右肺に結節、脳こそく → 脳梗塞(長音の脱落))。話者の意図が明らかなものだけ
- 句読点の補いと、数値・単位の表記(例: 5ミリ → 5mm、にじゅう → 20)
- 言い直しや「えー」などの不要な音の除去

# してはいけないこと
- 内容の追加・削除・言い換え、否定の反転、語順の入れ替え
- 左右(右・左)を変えること
- 文体の変更や要約。話者が言った通りの内容を保つ
- 数値と単位は話した通りに残す。単位が明らかに聞き違いのときだけ直す
- 判断がつかない語は原文のまま残す

# 分野
主に放射線科の読影レポート: CT・MRI・超音波・核医学。一般の文章のこともある。

# 表記
読影レポートの文だけに当てはめる。一般の文章では数の書き方を変えない(「三種類」「一度」はそのまま)。
- 長さの単位は mm・cm
- 数は算用数字
- 略語は大文字: CT・MRI・T2WI・DWI

# よく使う語
- 胸部CT
- 腹部MRI
- 頭部MRI
- 造影
- 単純
- 拡散強調像
- 肺野
- 縦隔
- 胸水
- 結節影
- 腫瘤
- リンパ節腫大
- 肝嚢胞
- 腎結石
- 脳梗塞
- 白質病変
";

/// 直し方の指示の節。空(ファイルが無い・空)なら既定の指示。上限を超えた分は切る。
fn guidance_section(guidance: &str) -> String {
    let guidance = guidance.trim();
    let guidance = if guidance.is_empty() {
        DEFAULT_GUIDANCE
    } else {
        guidance
    };
    let kept: String = guidance.chars().take(MAX_GUIDANCE_CHARS).collect();
    format!("\n{GUIDANCE_HEADER}\n{kept}\n")
}

/// そのまま残す語の節。語が無ければ節ごと省く。
fn protected_section(words: &[String]) -> String {
    let mut seen = Vec::new();
    for word in words.iter().map(|word| word.trim()) {
        if !word.is_empty() && !seen.contains(&word) {
            seen.push(word);
        }
    }
    if seen.is_empty() {
        return String::new();
    }
    let mut section = format!("\n{PROTECTED_HEADER}\n");
    for word in seen.into_iter().take(MAX_GLOSSARY_WORDS) {
        section.push_str("- ");
        section.push_str(word);
        section.push('\n');
    }
    section
}

/// 登録単語の節。登録が無ければ節ごと省く。テンプレート補完の指示文にも同じ節を載せる。
pub(crate) fn glossary_section(dictionary: &Dictionary) -> String {
    if dictionary.words.is_empty() {
        return String::new();
    }
    let mut section = String::from("\n用語集(表記 / 読み):\n");
    for word in dictionary.words.iter().take(MAX_GLOSSARY_WORDS) {
        section.push_str("- ");
        section.push_str(&word.word);
        if !word.reading.is_empty() {
            section.push_str(" / ");
            section.push_str(&word.reading);
        }
        section.push('\n');
    }
    section
}

/// 規則、分野、利用者の補足、そのまま残す語、用語集をこの順に並べる。規則を先に置くのは、補足が規則より弱いことを文の並びでも示すため。
/// 設定が変わらない限り同じ文なので、API 側のキャッシュが効く。
pub fn build_system_prompt(dictionary: &Dictionary, guidance: &str, protected: &[String]) -> String {
    [
        FRAME,
        &guidance_section(guidance),
        &protected_section(protected),
        &glossary_section(dictionary),
    ]
    .concat()
}

/// 学習した訂正例(認識したまま, 正しい文)を 2 つ目の system に載せる文にする。
/// 空、または有効な組が無ければ None。先頭 40 組まで。同一の組と空の側は除く。
pub fn build_examples_block(examples: &[(String, String)]) -> Option<String> {
    let lines: Vec<String> = examples
        .iter()
        .take(MAX_EXAMPLES)
        .filter(|(from, to)| !from.is_empty() && !to.is_empty() && from != to)
        .map(|(from, to)| format!("・「{from}」→「{to}」"))
        .collect();
    if lines.is_empty() {
        return None;
    }
    Some(format!("{EXAMPLES_HEADER}\n{}", lines.join("\n")))
}

/// あやしい語を【 】で囲む。範囲は重なっていても、順不同でもよい。
pub fn mark_unsure(text: &str, unsure: &[(usize, usize)]) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut ranges: Vec<(usize, usize)> = unsure
        .iter()
        .map(|(s, e)| ((*s).min(chars.len()), (*e).min(chars.len())))
        .filter(|(s, e)| s < e)
        .collect();
    ranges.sort_unstable();
    let mut merged: Vec<(usize, usize)> = Vec::new();
    for (start, end) in ranges {
        match merged.last_mut() {
            Some((_, last_end)) if start <= *last_end => *last_end = (*last_end).max(end),
            _ => merged.push((start, end)),
        }
    }
    let mut out = String::with_capacity(text.len() + merged.len() * 6);
    let mut position = 0;
    for (start, end) in merged {
        out.extend(&chars[position..start]);
        out.push('【');
        out.extend(&chars[start..end]);
        out.push('】');
        position = end;
    }
    out.extend(&chars[position..]);
    out
}

pub fn build_user_message(request: &CorrectionRequest) -> String {
    let mut message = String::new();
    let context = request.context.trim();
    if !context.is_empty() {
        message.push_str("直前の文:\n");
        message.push_str(context);
        message.push_str("\n\n");
    }
    message.push_str("認識文:\n");
    message.push_str(&mark_unsure(&request.text, &request.unsure));
    message
}

/// 訂正役の答えの形。
fn output_schema() -> Value {
    json!({
        "type": "object",
        "properties": {"text": {"type": "string"}},
        "required": ["text"],
        "additionalProperties": false
    })
}

/// Messages API に送る本文。考える量は最小(effort low)、出力は {"text": …} に固定する。
/// system は 1 つ目(規則・用語集)だけキャッシュし、変わりやすい訂正例は 2 つ目に分けて載せる。
pub fn request_body(
    model: &str,
    system: &str,
    examples: Option<&str>,
    request: &CorrectionRequest,
) -> Value {
    claude_api::messages_body(
        model,
        MAX_TOKENS,
        system,
        examples,
        build_user_message(request),
        output_schema(),
    )
}

/// API の応答から訂正後の文を取り出す。
pub fn parse_response(status: u16, body: &str) -> Result<String, String> {
    let text = claude_api::response_text(status, body, ROLE_NAME)?;
    // 構造化出力なら {"text": "…"}。そうでなければ本文をそのまま使う。
    let corrected = serde_json::from_str::<Value>(&text)
        .ok()
        .and_then(|value| value["text"].as_str().map(str::to_string))
        .unwrap_or(text);
    Ok(corrected)
}

/// 訂正後の文を整え、妥当なら返す。
pub fn finish(original: &str, corrected: &str) -> Result<String, String> {
    let mut corrected = corrected.trim().to_string();
    if !original.contains('\n') {
        corrected = corrected.replace(['\r', '\n'], "");
    }
    if changes_laterality(original, &corrected) {
        return Err("訂正役が左右の語を変えたので、認識したままの文を残しました".to_string());
    }
    if accept_correction(original, &corrected) {
        Ok(corrected)
    } else {
        Err("訂正役の答えが元の文からかけ離れているため使いませんでした".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::dictionary::Word;

    fn request(text: &str) -> CorrectionRequest {
        CorrectionRequest {
            text: text.to_string(),
            unsure: Vec::new(),
            context: String::new(),
        }
    }

    #[test]
    fn unsure_words_are_bracketed_in_order() {
        assert_eq!(
            mark_unsure("右肺上波に結節", &[(3, 4), (0, 1)]),
            "【右】肺上【波】に結節"
        );
        assert_eq!(
            mark_unsure("右肺上波", &[(2, 4), (3, 4), (9, 12)]),
            "右肺【上波】"
        );
        assert_eq!(mark_unsure("", &[(0, 1)]), "");
    }

    #[test]
    fn user_message_has_context_then_text() {
        let mut req = request("右肺上波に結節");
        req.context = "胸部CT。".to_string();
        req.unsure = vec![(3, 4)];
        assert_eq!(
            build_user_message(&req),
            "直前の文:\n胸部CT。\n\n認識文:\n右肺上【波】に結節"
        );
        assert_eq!(build_user_message(&request("肺")), "認識文:\n肺");
    }

    #[test]
    fn system_prompt_lists_glossary_and_profile() {
        let dictionary = Dictionary {
            words: vec![Word {
                word: "上葉".to_string(),
                reading: "じょうよう".to_string(),
                ..Word::default()
            }],
            ..Dictionary::default()
        };
        let prompt = build_system_prompt(&dictionary, "", &[]);
        assert!(prompt.contains("主に放射線科の読影レポート"), "指示が空なら既定の指示");
        assert!(prompt.contains("- 上葉 / じょうよう"));
        let plain = build_system_prompt(&Dictionary::default(), "", &[]);
        assert!(!plain.contains("用語集(表記 / 読み)"));
    }

    fn glossary() -> Dictionary {
        Dictionary {
            words: vec![Word {
                word: "上葉".to_string(),
                reading: "じょうよう".to_string(),
                ..Word::default()
            }],
            ..Dictionary::default()
        }
    }

    /// 指示は役割と出力の約束(固定)の後ろ、用語集の前。利用者の文がそのまま入る。
    #[test]
    fn guidance_sits_between_the_frame_and_the_glossary() {
        let prompt = build_system_prompt(&glossary(), "単位は mm で書く", &[]);
        let at = |text: &str| prompt.find(text).unwrap_or_else(|| panic!("{text}: {prompt}"));
        assert!(at("校正係") < at("出力は認識文を直したものだけ"));
        assert!(at("出力は認識文を直したものだけ") < at(GUIDANCE_HEADER));
        assert!(at(GUIDANCE_HEADER) < at("単位は mm で書く"));
        assert!(at("単位は mm で書く") < at("用語集(表記 / 読み)"));
        assert!(!prompt.contains("してはいけないこと"), "利用者の指示があれば既定の規則は入れない");
        assert_eq!(GUIDANCE_HEADER, "直し方の指示:");
    }

    /// 命令語とテンプレートの語は補足の後ろ、用語集の前。重なる語は 1 回だけ載せる。
    #[test]
    fn protected_words_follow_the_guidance() {
        let words = ["改行", "胸部正常", " 改行 ", ""].map(String::from);
        let prompt = build_system_prompt(&glossary(), "補足の文", &words);
        let at = |text: &str| prompt.find(text).unwrap_or_else(|| panic!("{text}: {prompt}"));
        assert!(at("補足の文") < at(PROTECTED_HEADER));
        assert!(at(PROTECTED_HEADER) < at("- 改行"));
        assert!(at("- 胸部正常") < at("用語集(表記 / 読み)"));
        assert_eq!(prompt.matches("- 改行\n").count(), 1);
    }

    #[test]
    fn no_protected_words_leave_no_section() {
        let plain = build_system_prompt(&glossary(), "", &[]);
        assert!(!plain.contains(PROTECTED_HEADER));
        let blank = [" ".to_string()];
        assert_eq!(build_system_prompt(&glossary(), "", &blank), plain);
    }

    /// 指示が無ければ既定の指示(雛形と同じ文)を使う。空白だけも無いのと同じ。
    #[test]
    fn empty_guidance_uses_the_default_rules() {
        let plain = build_system_prompt(&glossary(), "", &[]);
        assert_eq!(build_system_prompt(&glossary(), " \n\t\n", &[]), plain);
        assert!(plain.contains(GUIDANCE_HEADER));
        assert!(plain.contains("してはいけないこと"));
        assert!(plain.contains("左右(右・左)を変えること"));
        assert!(plain.contains(DEFAULT_GUIDANCE.trim()));
    }

    /// 字数で数えるので、日本語の途中で切れても壊れない。
    #[test]
    fn guidance_is_cut_at_the_limit() {
        let long = format!("{}余", "彙".repeat(MAX_GUIDANCE_CHARS));
        let prompt = build_system_prompt(&glossary(), &long, &[]);
        assert_eq!(prompt.matches('彙').count(), MAX_GUIDANCE_CHARS);
        assert!(!prompt.contains('余'));
        assert!(prompt.contains("用語集(表記 / 読み)"), "後ろの節は残る");
        let exact = "彙".repeat(MAX_GUIDANCE_CHARS);
        assert!(build_system_prompt(&glossary(), &exact, &[]).contains(&exact));
    }

    /// 2 つの接続方法は同じ文を使い、学習した例はどちらも補足と用語集の後ろに来る。
    #[test]
    fn both_correctors_share_the_guidance_and_keep_examples_last() {
        let examples = [("上波".to_string(), "上葉".to_string())];
        let api = ClaudeCorrector::new(DEFAULT_MODEL, "k", &glossary(), "補足の文", &[], &examples);
        let code = ClaudeCodeCorrector::new(
            DEFAULT_MODEL,
            Path::new("/opt/homebrew/bin/claude"),
            Path::new("/tmp"),
            &glossary(),
            "補足の文",
            &[],
            &examples,
        );
        assert!(api.system_prompt().contains("補足の文"));
        assert!(!api.system_prompt().contains(EXAMPLES_HEADER), "例はキャッシュの外");
        assert!(code.system.starts_with(api.system_prompt()));
        assert!(code.system.ends_with(api.examples_prompt().unwrap()));
        let body = request_body(
            DEFAULT_MODEL,
            api.system_prompt(),
            api.examples_prompt(),
            &request("肺"),
        );
        assert!(body["system"][0]["text"].as_str().unwrap().contains("補足の文"));
        assert_eq!(body["system"][0]["cache_control"]["type"], "ephemeral");
        assert!(body["system"][1].get("cache_control").is_none());
    }

    /// 並びは速い順で、既定は変えない。
    #[test]
    fn four_models_can_be_chosen() {
        let ids: Vec<&str> = MODELS.iter().map(|(id, _)| *id).collect();
        assert_eq!(
            ids,
            [
                "claude-haiku-5-5",
                "claude-sonnet-5-5",
                "claude-opus-5-5",
                "claude-fable-5-1"
            ]
        );
        assert_eq!(DEFAULT_MODEL, ids[0]);
        assert_eq!(model_label("claude-opus-5-5"), "Claude Opus 5.5(高精度)");
        assert_eq!(model_label("claude-fable-5-1"), "Claude Fable 5.1(最高精度・遅め)");
        assert_eq!(model_label("claude-opus-9"), "claude-opus-9", "未知の ID はそのまま");
    }

    #[test]
    fn request_body_uses_cache_and_fixed_output() {
        let body = request_body("claude-haiku-5-5", "役割", None, &request("肺"));
        assert_eq!(body["model"], "claude-haiku-5-5");
        assert_eq!(body["system"][0]["cache_control"]["type"], "ephemeral");
        assert_eq!(body["output_config"]["effort"], "low");
        assert_eq!(body["output_config"]["format"]["type"], "json_schema");
        assert_eq!(body["messages"][0]["content"], "認識文:\n肺");
        assert!(
            body.get("thinking").is_none(),
            "thinking は既定(adaptive)に任せる"
        );
    }

    #[test]
    fn request_body_adds_uncached_examples_block() {
        let pairs = vec![("上波".to_string(), "上葉".to_string())];
        let block = build_examples_block(&pairs).unwrap();
        let body = request_body("m", "役割", Some(&block), &request("肺"));
        let system = body["system"].as_array().unwrap();
        assert_eq!(system.len(), 2);
        assert_eq!(system[0]["cache_control"]["type"], "ephemeral");
        assert!(system[1].get("cache_control").is_none());
        assert!(system[1]["text"]
            .as_str()
            .unwrap()
            .contains("・「上波」→「上葉」"));
    }

    #[test]
    fn request_body_without_examples_has_one_system_block() {
        let body = request_body("m", "役割", None, &request("肺"));
        assert_eq!(body["system"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn examples_block_caps_and_skips_useless_pairs() {
        assert_eq!(build_examples_block(&[]), None);
        let useless = vec![
            ("同じ".to_string(), "同じ".to_string()),
            (String::new(), "x".to_string()),
            ("y".to_string(), String::new()),
        ];
        assert_eq!(build_examples_block(&useless), None);
        let many: Vec<(String, String)> = (0..60)
            .map(|i| (format!("誤{i}"), format!("正{i}")))
            .collect();
        let block = build_examples_block(&many).unwrap();
        assert!(block.starts_with(EXAMPLES_HEADER));
        assert_eq!(block.lines().count(), 1 + 40);
        assert!(block.contains("「誤39」") && !block.contains("「誤40」"));
    }

    #[test]
    fn structured_answer_is_read_from_the_text_block() {
        let body = r#"{"content":[{"type":"thinking","thinking":""},{"type":"text","text":"{\"text\":\"右肺上葉に結節。\"}"}],"stop_reason":"end_turn"}"#;
        assert_eq!(parse_response(200, body).unwrap(), "右肺上葉に結節。");
        let plain =
            r#"{"content":[{"type":"text","text":" 右肺上葉に結節。 "}],"stop_reason":"end_turn"}"#;
        assert_eq!(parse_response(200, plain).unwrap(), "右肺上葉に結節。");
    }

    #[test]
    fn api_errors_become_plain_messages() {
        let body = r#"{"type":"error","error":{"type":"authentication_error","message":"invalid x-api-key"}}"#;
        assert_eq!(
            parse_response(401, body).unwrap_err(),
            "API キーが正しくありません"
        );
        let body = r#"{"type":"error","error":{"type":"not_found_error","message":"model: x"}}"#;
        assert!(parse_response(404, body).unwrap_err().contains("model: x"));
        assert!(parse_response(500, "not json").unwrap_err().contains("500"));
        let refusal = r#"{"content":[],"stop_reason":"refusal"}"#;
        assert!(parse_response(200, refusal).is_err());
        let empty = r#"{"content":[{"type":"text","text":""}],"stop_reason":"end_turn"}"#;
        assert!(parse_response(200, empty).is_err());
    }

    #[test]
    fn far_off_answers_are_rejected() {
        assert!(accept_correction("右肺上波に結節", "右肺上葉に結節。"));
        assert!(accept_correction("えーっと、右肺", "右肺"));
        assert!(!accept_correction("右肺", ""));
        assert!(!accept_correction("右肺", &"長".repeat(30)));
        assert!(!accept_correction(&"文".repeat(40), "短"));
        assert_eq!(finish("右肺上波", " 右肺\n上葉 ").unwrap(), "右肺上葉");
        assert!(finish("右肺", "").is_err());
    }

    #[test]
    fn answers_that_are_commentary_are_not_used() {
        assert!(!accept_correction("肺に結節", "修正の必要はありません"));
        assert!(!accept_correction("はい", "認識文が短いため訂正できません。"));
        assert!(finish("肺に結節", "申し訳ありません").is_err());
        assert!(accept_correction("術後修正は不要", "術後修正は不要です。"));
        for reply in [
            "変更はありません",
            "誤りはありません",
            "問題ありません",
            "特に直す箇所はありません",
            "No changes needed.",
            "I cannot correct this",
        ] {
            assert!(!accept_correction("肺に結節", reply), "{reply}");
        }
        for reply in ["承知しました", "どの文を直しますか", "入力された文が短すぎます"] {
            assert!(!accept_correction("はい", reply), "{reply}");
        }
    }

    /// 説明と紛らわしい語を含んでいても、元の文に近ければ正しい訂正として使う。
    #[test]
    fn corrections_that_contain_commentary_words_are_kept() {
        for (raw, fixed) in [
            ("秀成大血管転位症", "修正大血管転位症"),
            ("しゅうせい月齢", "修正月齢"),
            ("ていせい報告", "訂正報告"),
            ("線量計のこうせい", "線量計の校正"),
            ("げんぶんどおり", "原文どおり"),
            ("恐怖ダンジェンシティ", "胸部単純CT"),
            ("結節を認めません", "結節を認めません。"),
        ] {
            assert!(accept_correction(raw, fixed), "{fixed}");
        }
    }

    #[test]
    fn answers_that_flip_laterality_are_not_used() {
        assert!(!accept_correction("右肺上葉に結節", "左肺上葉に結節"));
        let error = finish("右肺上葉に結節", "左肺上葉に結節").unwrap_err();
        assert!(error.contains("左右"), "{error}");
        assert_eq!(
            finish("みぎ肺上波", "右肺上葉").unwrap(),
            "右肺上葉",
            "左右が同じなら使う"
        );
    }

    fn cli_corrector() -> ClaudeCodeCorrector {
        ClaudeCodeCorrector::new(
            "claude-haiku-5-5",
            Path::new("/opt/homebrew/bin/claude"),
            Path::new("/tmp"),
            &Dictionary::default(),
            "",
            &[],
            &[("上波".to_string(), "上葉".to_string())],
        )
    }

    /// Claude Code に渡すシステムプロンプト(ファイルで渡す)は、規則・用語集に訂正例を続けたもの。
    #[test]
    fn claude_code_gets_the_rules_and_the_examples_as_its_system_prompt() {
        let corrector = cli_corrector();
        assert!(corrector.instructions().contains("校正係"));
        assert!(corrector.instructions().contains("・「上波」→「上葉」"));
        assert!(corrector.name().ends_with("Claude Code"));
    }

    /// 偽の claude で、訂正役が自分のシステムプロンプトと答えの形を渡し、答えを読むことを確かめる。
    #[test]
    fn claude_code_correction_passes_its_own_prompt_and_shape() {
        let dir = std::env::temp_dir().join(format!("openspeech-fake-correct-{}", std::process::id()));
        let reply = r#"{"type":"result","is_error":false,"result":"","structured_output":{"text":"右肺上葉に結節"}}"#;
        let fake = proc::fake_claude(&dir, reply);
        let corrector = ClaudeCodeCorrector::new(
            "claude-haiku-5-5",
            &fake,
            &dir,
            &Dictionary::default(),
            "",
            &[],
            &[("上波".to_string(), "上葉".to_string())],
        );
        assert_eq!(corrector.correct(&request("右肺上波に結節")).unwrap(), "右肺上葉に結節");
        let log = std::fs::read_to_string(dir.join("claude.log")).unwrap();
        assert!(log.contains("校正係") && log.contains("・「上波」→「上葉」"), "{log}");
        let schema = log.lines().find(|l| l.starts_with("SCHEMA: ")).unwrap();
        assert!(schema.contains("\"text\"") && !schema.contains("template"), "{schema}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn claude_code_answers_are_read_from_the_fixed_shape_only() {
        let structured = r#"{"type":"result","is_error":false,"result":"{\"text\":\"右肺上葉\"}",
            "structured_output":{"text":"右肺上葉"}}"#;
        assert_eq!(parse_claude_code_output(structured, "").unwrap(), "右肺上葉");
        let in_result = r#"{"is_error":false,"result":"{\"text\":\"右肺上葉\"}"}"#;
        assert_eq!(parse_claude_code_output(in_result, "").unwrap(), "右肺上葉");
        // 形の決まっていない答えは、説明が混ざっていることがあるので使わない。
        let free = r#"{"is_error":false,"result":"左肺下葉に…\n\nWait — I should check"}"#;
        assert!(parse_claude_code_output(free, "").is_err());
        let empty = r#"{"is_error":false,"result":"","structured_output":{"text":" "}}"#;
        assert!(parse_claude_code_output(empty, "").is_err());
    }

    #[test]
    fn claude_code_failures_say_what_to_do() {
        let login = r#"{"is_error":true,"result":"Not logged in · Please run /login"}"#;
        assert!(parse_claude_code_output(login, "").unwrap_err().contains("/login"));
        let limit = r#"{"is_error":true,"result":"Claude usage limit reached"}"#;
        assert!(parse_claude_code_output(limit, "").unwrap_err().contains("上限"));
        let crash = parse_claude_code_output("", "panic: something broke").unwrap_err();
        assert!(crash.contains("something broke"), "{crash}");
        let plain = parse_claude_code_output("right lung nodule was found", "").unwrap_err();
        assert!(!plain.contains("nodule"), "{plain}");
        let subtype = r#"{"is_error":true,"subtype":"error_during_execution"}"#;
        assert!(parse_claude_code_output(subtype, "")
            .unwrap_err()
            .contains("error_during_execution"));
    }

    /// 知らせはログにも残るので、話した文を載せない。
    #[test]
    fn claude_code_failures_do_not_repeat_the_dictated_text() {
        let error = parse_claude_code_output("右肺に結節 not json", "").unwrap_err();
        assert!(!error.contains("右肺"), "{error}");
        let error = parse_claude_code_output(r#"{"is_error":true,"result":"右肺に結節"}"#, "");
        assert!(!error.unwrap_err().contains("右肺"));
    }

    #[test]
    fn corrector_name_uses_the_label() {
        let engine = ClaudeCorrector::new(
            "claude-haiku-5-5",
            "k",
            &Dictionary::default(),
            "",
            &[],
            &[],
        );
        assert_eq!(engine.name(), "Claude Haiku 5.5(速い・安い)");
        assert_eq!(model_label("other"), "other");
    }

    /// 実際の API を叩く。`ANTHROPIC_API_KEY` があるときだけ手で走らせる。
    #[test]
    #[ignore = "本物の Anthropic API を呼ぶ(ANTHROPIC_API_KEY が要る)"]
    fn claude_corrects_the_sample_sentence() {
        let key = std::env::var("ANTHROPIC_API_KEY").expect("ANTHROPIC_API_KEY");
        let dictionary = Dictionary {
            words: vec![Word {
                word: "上葉".to_string(),
                reading: "じょうよう".to_string(),
                ..Word::default()
            }],
            ..Dictionary::default()
        };
        let engine = ClaudeCorrector::new(DEFAULT_MODEL, &key, &dictionary, "", &[], &[]);
        let mut req = request("右肺上波に5ミリの結節を認めます");
        req.unsure = vec![(3, 4)];
        let began = std::time::Instant::now();
        let corrected = engine.correct(&req).unwrap();
        println!("{:.2}s {corrected}", began.elapsed().as_secs_f32());
        assert!(corrected.contains("上葉"), "{corrected}");
    }
}
