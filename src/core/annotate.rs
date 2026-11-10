//! 単位 S2: テンプレートの注釈。`docs/simplify-and-assistant-plan.md` §16.8 の S2。
//!
//! Claude(注釈役。既定は Sonnet)には **分類だけ** をさせる: 本文の各行の種類と鍵、モジュールごとの形
//! (束・まとめて 1 つ・全体差し替え)と変種の対象・置き方・極性・条件・auto・役割・区間。答えは閉じた語彙の
//! enum を持つ JSON Schema で固定する。答えから `TemplateStructure` を組むのはコードで、決定論で書ける物は
//! ここで決める: 列挙否定の区切りと型(文から割り出す)、1 行のモジュールの行番号、全体差し替えを行に割った
//! 印(`split`)と外す変種(どの行にも差し替えられない本文の項目)、尺度の宣言。
//!
//! Claude の答えはそのまま信じない。機械の検査(`review`)の結果を `unresolved` に入れて人に見せる:
//! 使い方の注が本文に無い項目・文を指している(古い注)、注の引用が本文に無い、S1 の構造の検査
//! (`check_structure`)に掛かった、注釈の無いモジュールがある。
//!
//! 書くのは `structure.json` だけ(呼び出す側)。`dictionary.json` は読むだけで書かない。

use super::claude_api;
use super::corrector::model_label;
use super::dictionary::Template;
use super::proc;
use super::structure::{
    canonical_lines, check_structure, module_lines, Condition, Group, LineInfo, LineKind, Mode, TemplateStructure,
    Variant, Vocabulary, WHOLE,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// 注釈の既定のモデル(D7)。保存のときに裏で走るので速さは要らず、Haiku の揺れを避ける。
pub const ANNOTATION_MODEL: &str = "claude-sonnet-5-5";
/// 考える量。分類だけだが、行と項目の対応を見比べるので low より上にする。
pub const ANNOTATION_EFFORT: &str = "medium";
/// 答えのトークンの上限(17 行の本文と 20 個ほどの変種で 4000 前後。考える分を見込む)。
const MAX_TOKENS: u32 = 16_000;
/// 待つ上限(秒)。裏で走るので長めに取る。
const API_TIMEOUT_SECONDS: u64 = 240;
const CLAUDE_CODE_TIMEOUT_SECONDS: u64 = 300;
const ROLE_NAME: &str = "注釈役";

// ------------------------------------------------------------------------------------------------
// Claude への依頼(system prompt・依頼の文・答えの形)
// ------------------------------------------------------------------------------------------------

const PROMPT_HEAD: &str = "\
あなたは読影レポートのテンプレートに「構造」の注釈を付ける係です。テンプレートは、正常所見の本文(行の並び)と、\
所見モジュール(所見が言われたときに本文の文と差し替える・本文に足す文。名前・文・使い方の注)から成ります。\
あなたの仕事は分類だけです。文を書き換えたり新しい文を作ったりせず、指定の JSON の形で答えます。

## 本文の行(lines)
本文の各行について、行番号 i の順に 1 つずつ返します(空行も含めて全部)。
- kind: sentence(普通の文)/ list_negation(2 つ以上の臓器を「、」や「・」で並べてまとめて否定する文。\
例「甲状腺、副甲状腺に異常を認めません。」)/ heading(【…】のような節の見出し)/ blank(空行)。
- slot: その行が述べる項目の鍵を、下の語彙から 1 つ選びます。見出しと空行は null。\
1 行に 2 つの事柄がある行は、語彙にある複合の鍵(2 つの項目を「・」で繋いだ鍵)を使います。\
リンパ節の行は、検査の範囲に合う領域の付いた鍵を使います。同じ項目を 2 行で述べるときは 2 行とも同じ鍵にします。\
合う鍵が無ければ null にします(人が確かめます)。
- organs: list_negation の行だけ、並べてある臓器の語を、文に書かれたとおりの字で、出る順に。ほかの行は空の配列。

## 所見モジュール(modules)
モジュールごとに、名前(name)を依頼に書かれたとおりに写し、次を返します。
- shape:
  - per_line(束): 使い方の注が「言われた所見の行だけを使う」のように、行を選んで使うと言うモジュール。\
行ごとに変種を 1 つずつ返す。
  - single: 注がモジュールの文全体を本文の文と差し替える・足すと言うモジュール(1 行のモジュールと、複数行でも\
行を選ぶと言っていないモジュール)。一部の行が「〜が言われたときだけ」でも single(どの行を残すかは補完のときに\
決める)。変種を 1 つだけ返す(line は null)。
  - whole: 使い方の注が「正常所見全体をこの文に差し替える」と言うモジュール。各行を、本文のどの項目と差し替えるか・\
どこに足すかに割って、行ごとに変種を返す。1 行が本文の複数の項目をまとめて言い直すなら、その行の target に\
すべての項目を入れる。全体差し替えでは本文の文はすべて消えるので、条件(文脈)の無いモジュールでは、本文の\
どの項目もどれかの行の replace の target にする(本文の項目に最も近い行で差し替える)。条件付きのモジュールでは、\
行が述べていない項目を target に入れない(どの行にも差し替えられない項目は、短い形では書かれない項目として\
アプリが外す)。どうしても割れない行があるときは\
variants を空にする(全体を差し替える扱いのまま残る)。
- graded: モジュールの行が、1 つの過程(変性・慢性虚血・炎症など)の進み具合に応じて出たり出なかったりする\
所見群なら true。初期の変化から、構造の中での進行、隣の構造への波及(狭窄・圧排)、障害(神経・実質の損傷)へと\
進む目印の行が並ぶ全体差し替えのモジュールがこれに当たる(軽度・中等度・高度で言い分けられる所見)。\
そのときは各行に role を付ける。ふつうは false。
- sex / age_band / purpose: 使い方の注やモジュールの名前が、文脈(性別・年齢帯・精査の目的)をモジュールを使う\
条件にしているときだけ(「若い男性なら」→ 男・若年、「〜の精査なら」→ その目的)。所見の名前(病名)は条件ではない。\
無ければ null。
- variants: 変種の配列。各変種に:
  - line: per_line・whole では、その変種がモジュールの何行目か(0 から)。single では null。
  - target: その変種が差し替える本文の項目の鍵(本文の行に付けた鍵から選ぶ)。その行が実際に述べている項目だけ。\
複数の項目をまとめて言い直すならすべて。前に足す・後に足す変種では、置き場所の目印になる隣の項目。
  - mode: replace(target の文と差し替える)/ prepend(target の文の前に足す。target の文は残す)/ \
append(target の文の後に足す。target の文は残す)。前に足す・後に足すは、本文の target の文を残したまま並べて\
矛盾しない文だけ(本文に無い項目の文、注が「前に足す」「後に足す」「次に足す」と言う文)。本文の文と食い違う文は\
replace にする。同じ項目について述べる行が続くときも、それぞれ replace にする(同じ項目の差し替えが重なれば、\
アプリが源泉の順に並べて置く)。
  - polarity: negative(異常が無い・年齢相応・正常と言う文)/ benign(所見があると言うが、病的意義の無い・正常範囲と\
言う文。生理的な少量の液体、機能性の嚢胞、瘢痕など)/ abnormal(異常所見)。
  - sex / age_band / purpose: この行だけ条件がモジュールと違うとき。モジュールと同じなら null(モジュールの条件を\
引き継ぐ)。
  - auto: 条件の文脈が合えば、口述されなくても置く陰性の文なら true。使い方の注やモジュールの名前が、\
所見ではなく文脈だけでモジュールを使うと言っている(「若い男性なら全体を差し替える」「〜の精査なら足す」)とき、\
その陰性の文がこれに当たる。異常・正常範囲の文(所見が言われたときだけ置く文)、条件の無い文には付けない。
  - slot: 本文に無い項目を足す変種なら、その項目の鍵(語彙から。本文の行に付けた鍵とは別の鍵)。mode は prepend か\
append にする。それ以外は null。
  - organ: target が列挙否定の行で、変種がそのうち 1 つの臓器についての文なら、その臓器の語(列挙の行に書かれた\
とおりの字)。アプリがその臓器を列挙から外して変種の文を置く。それ以外は null。
  - prefix: 使い方の注が「続く文の文頭に〜を付ける」と言うときの語。無ければ null。
  - role: graded のモジュールの行だけ、その行が過程のどの目印かを下の役割から選ぶ。ほかは null。
  - grades: graded のモジュールの行で、出る程度が役割の既定の区間と違うとき(注が「軽度なら〜」と言う、\
解釈の文が程度で替わる)だけ、出る程度の連続した並び。帰結(解釈の文)の行が 2 行以上あるときは、各行に重ならない\
区間を付ける。ほかは null。
  - list_organs: 変種の文そのものが列挙否定(複数の臓器をまとめて否定する文)のとき、並べてある臓器の語を、\
文に書かれたとおりの字で出る順に。ほかは空の配列。
  - organ_slots: list_organs があるとき、各臓器の語が本文のどの項目に当たるか(本文の行に付けた鍵から。1 つの語が\
本文の複数の項目に当たることもある)。ほかは空の配列。
- 使い方の注が本文に無い文や項目を指していても、本文にある項目から最も近い物を選んで構いません(注と本文のずれは\
アプリが別に調べて人に見せます)。
";

/// 注釈役の system prompt。語彙(利用者の追加を含む)を一覧にして添える。テンプレートに依らず同じ文になる。
pub fn system_prompt(vocabulary: &Vocabulary) -> String {
    let mut text = String::from(PROMPT_HEAD);
    text.push_str("\n## 語彙\n項目の鍵(閉じた一覧。これ以外は使えない。括弧の中は意味):\n");
    for group in &vocabulary.slots {
        let keys: Vec<String> = group
            .keys
            .iter()
            .map(|key| match vocabulary.slot_notes.get(key) {
                Some(note) if !note.is_empty() => format!("{key}({note})"),
                _ => key.clone(),
            })
            .collect();
        text.push_str(&format!("- {}: {}\n", group.region, keys.join("、")));
    }
    text.push_str(&format!("文脈の札(精査の目的): {}\n", vocabulary.purposes.join("、")));
    let bands: Vec<String> = vocabulary
        .age_bands
        .iter()
        .map(|(name, (low, high))| format!("{name}({low}〜{high} 歳)"))
        .collect();
    text.push_str(&format!("年齢帯: {}\n", bands.join("、")));
    text.push_str(&format!("程度の段: {}\n役割(意味と既定の区間):\n", vocabulary.default_scale.join("・")));
    for role in &vocabulary.roles {
        text.push_str(&format!("- {}: {}(既定の区間 {})\n", role.name, role.meaning, role.grades.join("・")));
    }
    text
}

/// 依頼の文: テンプレートの名前、行番号の付いた本文、モジュール(名前・使い方・行番号の付いた文)。
pub fn user_message(template: &Template) -> String {
    let name = template.keywords.first().map_or("", String::as_str);
    let mut text = format!("テンプレート「{name}」(キーワード: {})\n\n本文(行番号: 文):\n", template.keywords.join("、"));
    for (i, line) in canonical_lines(&template.body).iter().enumerate() {
        if line.trim().is_empty() {
            text.push_str(&format!("{i}: (空行)\n"));
        } else {
            text.push_str(&format!("{i}: {line}\n"));
        }
    }
    let modules: Vec<_> = template.modules.iter().filter(|m| !module_lines(m).is_empty()).collect();
    if modules.is_empty() {
        text.push_str("\n所見モジュール: なし(modules は空の配列)\n");
        return text;
    }
    text.push_str("\n所見モジュール:\n");
    for module in modules {
        text.push_str(&format!("### {}\n", module.name));
        let note = canonical_lines(&module.note).join(" ");
        if !note.trim().is_empty() {
            text.push_str(&format!("使い方: {note}\n"));
        }
        for (i, line) in module_lines(module).iter().enumerate() {
            text.push_str(&format!("{i}: {line}\n"));
        }
    }
    text
}

fn string_enum(values: &[String]) -> Value {
    json!({"type": "string", "enum": values})
}

fn nullable(schema: Value) -> Value {
    json!({"anyOf": [schema, {"type": "null"}]})
}

fn object(properties: Value) -> Value {
    let required: Vec<String> = properties
        .as_object()
        .map(|map| map.keys().cloned().collect())
        .unwrap_or_default();
    json!({"type": "object", "properties": properties, "required": required, "additionalProperties": false})
}

/// 答えの形。鍵・札・年齢帯・役割・段は語彙の enum にして、閉じた語彙からしか選べないようにする。
/// 項目はすべて必須(無い値は null か空の配列)。
pub fn answer_schema(vocabulary: &Vocabulary) -> Value {
    let keys: Vec<String> = vocabulary.slot_keys().map(str::to_string).collect();
    let key = string_enum(&keys);
    let sexes = vec!["男".to_string(), "女".to_string()];
    let bands: Vec<String> = vocabulary.age_bands.keys().cloned().collect();
    let roles: Vec<String> = vocabulary.roles.iter().map(|r| r.name.clone()).collect();
    let text = json!({"type": "string"});
    let texts = json!({"type": "array", "items": text});
    let line = object(json!({
        "i": {"type": "integer"},
        "kind": string_enum(&["sentence", "list_negation", "heading", "blank"].map(String::from)),
        "slot": nullable(key.clone()),
        "organs": texts.clone(),
    }));
    let organ_slots = object(json!({
        "organ": text.clone(),
        "slots": {"type": "array", "items": key.clone()},
    }));
    let variant = object(json!({
        "line": nullable(json!({"type": "integer"})),
        "target": {"type": "array", "items": key.clone()},
        "mode": string_enum(&["replace", "prepend", "append"].map(String::from)),
        "polarity": string_enum(&["negative", "benign", "abnormal"].map(String::from)),
        "sex": nullable(string_enum(&sexes)),
        "age_band": nullable(string_enum(&bands)),
        "purpose": nullable(string_enum(&vocabulary.purposes)),
        "auto": {"type": "boolean"},
        "slot": nullable(key.clone()),
        "organ": nullable(text.clone()),
        "prefix": nullable(text.clone()),
        "role": nullable(string_enum(&roles)),
        "grades": nullable(json!({"type": "array", "items": string_enum(&vocabulary.default_scale)})),
        "list_organs": texts,
        "organ_slots": {"type": "array", "items": organ_slots},
    }));
    let module = object(json!({
        "name": text,
        "shape": string_enum(&["per_line", "single", "whole"].map(String::from)),
        "graded": {"type": "boolean"},
        "sex": nullable(string_enum(&sexes)),
        "age_band": nullable(string_enum(&bands)),
        "purpose": nullable(string_enum(&vocabulary.purposes)),
        "variants": {"type": "array", "items": variant},
    }));
    object(json!({
        "lines": {"type": "array", "items": line},
        "modules": {"type": "array", "items": module},
    }))
}

// ------------------------------------------------------------------------------------------------
// 答えから構造を組む(決定論)
// ------------------------------------------------------------------------------------------------

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawAnswer {
    lines: Vec<RawLine>,
    modules: Vec<RawModule>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawLine {
    i: Option<usize>,
    kind: String,
    slot: Option<String>,
    organs: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawModule {
    name: String,
    shape: String,
    graded: bool,
    sex: Option<String>,
    age_band: Option<String>,
    purpose: Option<String>,
    variants: Vec<RawVariant>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawVariant {
    line: Option<usize>,
    target: Vec<String>,
    mode: String,
    polarity: Option<String>,
    sex: Option<String>,
    age_band: Option<String>,
    purpose: Option<String>,
    auto: bool,
    slot: Option<String>,
    organ: Option<String>,
    prefix: Option<String>,
    role: Option<String>,
    grades: Option<Vec<String>>,
    list_organs: Vec<String>,
    organ_slots: Vec<RawOrganSlots>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawOrganSlots {
    organ: String,
    slots: Vec<String>,
}

/// 列挙否定の文から区切りと型を割り出す。臓器を区切りで繋いだ並びが文の中にあれば、その並びを `{organs}` に
/// 置いた文が型。見つからなければ None。`rebuild_list` で繋ぎ直すと元の文に戻る。
pub fn list_pattern(text: &str, organs: &[String]) -> Option<(String, String)> {
    if organs.len() < 2 || organs.iter().any(|o| o.is_empty()) {
        return None;
    }
    for sep in ["、", "・", "，", ",", "及び", "および"] {
        let joined = organs.join(sep);
        if let Some(at) = text.find(&joined) {
            let pattern = format!("{}{{organs}}{}", &text[..at], &text[at + joined.len()..]);
            return Some((sep.to_string(), pattern));
        }
    }
    None
}

/// 文脈の札を語彙で確かめて条件にする。知らない値は捨てる。
fn condition_of(
    sex: &Option<String>,
    age_band: &Option<String>,
    purpose: &Option<String>,
    vocabulary: &Vocabulary,
) -> Condition {
    let pick = |value: &Option<String>, ok: &dyn Fn(&str) -> bool| {
        value.as_deref().map(str::trim).filter(|v| !v.is_empty() && ok(v)).map(str::to_string)
    };
    Condition {
        sex: pick(sex, &|v| v == "男" || v == "女"),
        age_band: pick(age_band, &|v| vocabulary.age_bands.contains_key(v)),
        purpose: pick(purpose, &|v| vocabulary.purposes.iter().any(|p| p == v)),
    }
}

/// 名前の比べ方: 空白を除き、全角の英数記号を半角に寄せる。
fn loose(text: &str) -> String {
    fold_width(text).chars().filter(|c| !c.is_whitespace()).collect()
}

/// 全角の英数・記号(U+FF01〜U+FF5E)を半角に寄せる。
fn fold_width(text: &str) -> String {
    text.chars()
        .map(|c| match c as u32 {
            0xFF01..=0xFF5E => char::from_u32(c as u32 - 0xFEE0).unwrap_or(c),
            _ => c,
        })
        .collect()
}

/// 答えの中からモジュールの注釈を探す: 名前が同じ物、ならした名前が同じ物、数が揃っていれば同じ順の物。
fn find_raw_module<'a>(answer: &'a RawAnswer, name: &str, index: usize, count: usize) -> Option<&'a RawModule> {
    answer
        .modules
        .iter()
        .find(|m| m.name == name)
        .or_else(|| answer.modules.iter().find(|m| loose(&m.name) == loose(name)))
        .or_else(|| (answer.modules.len() == count).then(|| &answer.modules[index]))
}

/// 1 つの変種の答えを構造の変種にする。`line` は源泉のモジュールの行(コードが決めた物)。
#[allow(clippy::too_many_arguments)]
fn convert_variant(
    raw: &RawVariant,
    module_name: &str,
    texts: &[&str],
    line: Option<usize>,
    module_condition: &Condition,
    graded: bool,
    body_slots: &[String],
    vocabulary: &Vocabulary,
    problems: &mut Vec<String>,
) -> Variant {
    let at = match line {
        Some(l) => format!("「{module_name}」{l} 行目"),
        None => format!("「{module_name}」"),
    };
    let mut target: Vec<String> = Vec::new();
    for key in &raw.target {
        if !vocabulary.has_slot(key) {
            problems.push(format!("{at}: 対象の鍵「{key}」は語彙に無いので捨てた"));
        } else if !target.contains(key) {
            target.push(key.clone());
        }
    }
    let mode = match raw.mode.as_str() {
        "prepend" => Mode::Prepend,
        "append" => Mode::Append,
        _ => Mode::Replace,
    };
    let polarity = raw
        .polarity
        .as_deref()
        .filter(|p| ["negative", "benign", "abnormal"].contains(p))
        .map(str::to_string);
    let own = condition_of(&raw.sex, &raw.age_band, &raw.purpose, vocabulary);
    let mut condition = if own.is_empty() { module_condition.clone() } else { own };
    // 精査目的の札は陰性の既定文だけに付く(§16.3。異常・正常範囲の所見は目的に依らず口述で置く)。
    // 病名と同じ札(認知症)を条件にした答えがあった。
    if condition.purpose.is_some() && polarity.as_deref() != Some("negative") {
        problems.push(format!(
            "{at}: 陰性でない文に付いた目的の条件「{}」を外した",
            condition.purpose.take().unwrap_or_default()
        ));
    }
    let mut auto = raw.auto;
    if auto && polarity.as_deref() != Some("negative") {
        auto = false;
        problems.push(format!("{at}: 陰性でない文に付いた auto を外した"));
    }
    if auto && condition.is_empty() {
        auto = false;
        problems.push(format!("{at}: 条件の無い auto を外した"));
    }
    let slot = raw.slot.as_deref().map(str::trim).filter(|s| !s.is_empty()).and_then(|s| {
        if vocabulary.has_slot(s) {
            Some(s.to_string())
        } else {
            problems.push(format!("{at}: 足す鍵「{s}」は語彙に無いので捨てた"));
            None
        }
    });
    // 性別の決まった項目を足す変種は、その性別を条件にする(前立腺は男、子宮・付属器は女)。
    if let Some(sex) = slot.as_ref().and_then(|s| vocabulary.slot_sex.get(s)) {
        match &condition.sex {
            None => condition.sex = Some(sex.clone()),
            Some(other) if other != sex => {
                problems.push(format!("{at}: 足す鍵「{}」は{sex}の項目なのに条件が{other}", slot.as_deref().unwrap_or("")))
            }
            Some(_) => {}
        }
    }
    let organ = raw.organ.as_deref().map(str::trim).filter(|s| !s.is_empty()).map(str::to_string);
    let prefix = raw.prefix.as_deref().map(str::trim).filter(|s| !s.is_empty()).map(str::to_string);
    let (role, grades) = if graded {
        let role = raw.role.as_deref().filter(|r| vocabulary.role(r).is_some()).map(str::to_string);
        let mut grades = raw.grades.clone().filter(|g| !g.is_empty());
        // 役割の既定と同じ区間は持たない(既定表の上書きが及ぶように)。帰結は 2 行以上で区間が要るので残す。
        if let (Some(role_name), Some(explicit)) = (&role, &grades) {
            if role_name != "帰結" && vocabulary.role(role_name).is_some_and(|r| &r.grades == explicit) {
                grades = None;
            }
        }
        (role, grades)
    } else {
        (None, None)
    };
    let mut variant = Variant {
        module: module_name.to_string(),
        line,
        target,
        mode,
        polarity,
        condition: (!condition.is_empty()).then_some(condition),
        auto,
        slot,
        organ,
        prefix,
        role,
        grades,
        ..Variant::default()
    };
    if raw.list_organs.len() >= 2 {
        match line.and_then(|l| texts.get(l)).and_then(|text| list_pattern(text, &raw.list_organs)) {
            Some((sep, pattern)) => {
                variant.kind = Some(LineKind::ListNegation);
                variant.organs = raw.list_organs.clone();
                variant.sep = sep;
                variant.pattern = pattern;
                for pair in &raw.organ_slots {
                    // 本文に無い項目は捨てる(列挙の臓器と本文の項目を結ぶ物なので)。
                    let slots: Vec<String> = pair.slots.iter().filter(|s| body_slots.contains(s)).cloned().collect();
                    if !pair.organ.is_empty() && !slots.is_empty() {
                        variant.organ_slots.entry(pair.organ.clone()).or_default().extend(slots);
                    }
                }
            }
            None => problems.push(format!(
                "{at}: 列挙の臓器({})を文の中に見つけられないので普通の文にした",
                raw.list_organs.join("・")
            )),
        }
    }
    variant
}

/// モジュールの形(コードが注で確かめた後)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Shape {
    /// 束: 行ごとの変種。
    PerLine,
    /// まとめて 1 つ。
    Single,
    /// 全体差し替え(行に割れれば割る)。
    Whole,
}

/// 注かモジュールの名前が、正常所見の全体を差し替えると言っているか。
fn note_says_whole(name: &str, note: &str) -> bool {
    ["全体", "全文", "すべて差し替", "全部差し替", "すべてを差し替", "全部を差し替"]
        .iter()
        .any(|w| note.contains(w) || name.contains(w))
}

/// 注が、言われた所見の行だけを選んで使うと言っているか(§16.2 の束の定義)。
fn note_picks_lines(note: &str) -> bool {
    ["行だけ", "行のみ", "文だけ", "文のみ"].iter().any(|w| note.contains(w))
}

fn polarity_rank(polarity: Option<&str>) -> u8 {
    match polarity {
        Some("abnormal") => 3,
        Some("benign") => 2,
        Some("negative") => 1,
        _ => 0,
    }
}

/// 行ごとの変種を 1 つにまとめる(置き方・足す鍵・外す臓器・接頭語・条件・auto が全部の行で同じときだけ)。
/// 対象は本文の順に合わせ、極性は最も強い物(異常 > 正常範囲 > 陰性)。行はモジュール単位(null)。
fn collapse(found: &[Variant], body_slots: &[String]) -> Option<Variant> {
    let first = found.first()?;
    let same = found.iter().all(|v| {
        v.mode == first.mode
            && v.slot == first.slot
            && v.organ == first.organ
            && v.prefix == first.prefix
            && v.condition == first.condition
            && v.auto == first.auto
            && v.kind.is_none()
    });
    if !same {
        return None;
    }
    let mut target: Vec<String> = Vec::new();
    for key in found.iter().flat_map(|v| v.target.iter()) {
        if !target.contains(key) {
            target.push(key.clone());
        }
    }
    target.sort_by_key(|t| body_slots.iter().position(|s| s == t).unwrap_or(usize::MAX));
    let polarity = found
        .iter()
        .map(|v| v.polarity.clone())
        .max_by_key(|p| polarity_rank(p.as_deref()))
        .flatten();
    Some(Variant { line: None, target, polarity, role: None, grades: None, ..first.clone() })
}

/// 全体差し替えの印(源泉のモジュール単位、対象 `*`)。`split` なら行に割った印。
fn whole_marker(module: &str, split: bool) -> Variant {
    Variant {
        module: module.to_string(),
        line: None,
        target: vec![WHOLE.to_string()],
        mode: Mode::Replace,
        polarity: Some("mixed".to_string()),
        split,
        ..Variant::default()
    }
}

/// Claude の答え(`answer_schema` の形)から構造を組む。答えが形として読めなければ Err。
/// 中身の不備は誤りにせず、組めた所まで組んで `unresolved` に理由を書く(人が確認表で直す)。
pub fn structure_from_answer(
    template: &Template,
    vocabulary: &Vocabulary,
    answer: &Value,
    annotated: &str,
) -> Result<TemplateStructure, String> {
    let answer: RawAnswer =
        serde_json::from_value(answer.clone()).map_err(|e| format!("注釈の答えの形が違います: {e}"))?;
    let mut problems: Vec<String> = Vec::new();
    let body = canonical_lines(&template.body);

    // ---- 本文の行 ----
    for raw in &answer.lines {
        if let Some(i) = raw.i.filter(|i| *i >= body.len()) {
            problems.push(format!("答えの行番号 {i} は本文に無い(行の番号がずれている答えかもしれない)"));
        }
    }
    let mut lines: Vec<LineInfo> = Vec::new();
    for (i, text) in body.iter().enumerate() {
        if text.trim().is_empty() {
            lines.push(LineInfo { i, kind: LineKind::Blank, ..LineInfo::default() });
            continue;
        }
        let raw = answer
            .lines
            .iter()
            .find(|l| l.i == Some(i))
            .or_else(|| (answer.lines.len() == body.len()).then(|| &answer.lines[i]));
        let Some(raw) = raw else {
            problems.push(format!("本文 {i} 行目の注釈が無い"));
            lines.push(LineInfo { i, ..LineInfo::default() });
            continue;
        };
        let slot = raw.slot.as_deref().map(str::trim).filter(|s| !s.is_empty()).and_then(|s| {
            if vocabulary.has_slot(s) {
                Some(s.to_string())
            } else {
                problems.push(format!("本文 {i} 行目: 鍵「{s}」は語彙に無いので捨てた"));
                None
            }
        });
        // 列挙否定: 答えの臓器で文を割れなければ、語彙の列挙の鍵の臓器で割る。答えが普通の文でも、列挙の鍵で
        // 割れれば列挙否定にする(臓器を外して置くため。§16.3)。
        let vocabulary_organs = slot.as_ref().and_then(|s| vocabulary.list_slots.get(s));
        let as_list = |organs: &[String]| {
            list_pattern(text, organs).map(|(sep, pattern)| LineInfo {
                i,
                kind: LineKind::ListNegation,
                slot: slot.clone(),
                organs: organs.to_vec(),
                sep,
                pattern,
            })
        };
        let info = match raw.kind.as_str() {
            "heading" => {
                if !text.trim_start().starts_with(['【', '[', '［', '■', '◆', '●']) {
                    problems.push(format!("本文 {i} 行目を見出しとして読んだ(【】で始まらない)"));
                }
                LineInfo { i, kind: LineKind::Heading, ..LineInfo::default() }
            }
            "list_negation" => match as_list(&raw.organs).or_else(|| vocabulary_organs.and_then(|o| as_list(o))) {
                Some(info) => info,
                None => {
                    problems.push(format!(
                        "本文 {i} 行目: 列挙の臓器({})を文の中に見つけられないので普通の文にした",
                        raw.organs.join("・")
                    ));
                    LineInfo { i, slot, ..LineInfo::default() }
                }
            },
            _ => match vocabulary_organs.and_then(|o| as_list(o)) {
                Some(info) => info,
                None => LineInfo { i, slot, ..LineInfo::default() },
            },
        };
        lines.push(info);
    }
    // 本文の項目(源泉の順、重複なし)。
    let mut body_slots: Vec<String> = Vec::new();
    for line in &lines {
        if let Some(slot) = &line.slot {
            if !body_slots.contains(slot) {
                body_slots.push(slot.clone());
            }
        }
    }

    // ---- モジュール ----
    let mut variants: Vec<Variant> = Vec::new();
    let mut groups: Vec<Group> = Vec::new();
    let with_text: Vec<_> = template.modules.iter().filter(|m| !module_lines(m).is_empty()).collect();
    for (index, module) in with_text.iter().enumerate() {
        let name = module.name.as_str();
        let texts = module_lines(module);
        let Some(raw) = find_raw_module(&answer, name, index, with_text.len()) else {
            problems.push(format!("「{name}」の注釈が無い"));
            continue;
        };
        if loose(&raw.name) != loose(name) {
            problems.push(format!("答えの「{}」を、同じ順の「{name}」の注釈として読んだ", raw.name));
        }
        let module_condition = condition_of(&raw.sex, &raw.age_band, &raw.purpose, vocabulary);
        // 形は注の言い方で確かめる(§16.2 の定義): 全体差し替えは注か名前が「全体」と言う物だけ、束は注が
        // 「言われた所見の行だけ」のように行を選ぶ物だけ。Claude の答えはこの範囲で使う。
        let says_whole = note_says_whole(&module.name, &module.note);
        let picks_lines = note_picks_lines(&module.note);
        let shape = match raw.shape.as_str() {
            "whole" if says_whole => Shape::Whole,
            "whole" | "per_line" => Shape::PerLine,
            _ => Shape::Single,
        };
        if says_whole && shape != Shape::Whole {
            problems.push(format!("「{name}」の注は全体を差し替えると言うが、行に割った答えではない"));
        }
        let graded = raw.graded && shape == Shape::Whole;
        if raw.graded && !graded {
            problems.push(format!("「{name}」は全体差し替えではないので、程度の尺度を付けなかった"));
        }
        // 行ごとの変種(束・全体差し替えを割った物)。行番号は源泉のモジュールの範囲だけ、同じ行は最初の物。
        let per_line = |problems: &mut Vec<String>| -> Vec<Variant> {
            let mut found = Vec::new();
            for raw_variant in &raw.variants {
                match raw_variant.line {
                    Some(l) if l < texts.len() => {}
                    other => problems.push(format!("「{name}」: 行 {other:?} はモジュールの文に無いので捨てた")),
                }
            }
            for l in 0..texts.len() {
                let Some(raw_variant) = raw.variants.iter().find(|v| v.line == Some(l)) else {
                    continue;
                };
                let more = raw.variants.iter().filter(|v| v.line == Some(l)).count() - 1;
                if more > 0 {
                    problems.push(format!("「{name}」{l} 行目に変種が {} 個返ったので最初の物を使った", more + 1));
                }
                found.push(convert_variant(
                    raw_variant,
                    name,
                    &texts,
                    Some(l),
                    &module_condition,
                    graded,
                    &body_slots,
                    vocabulary,
                    problems,
                ));
            }
            found
        };
        let mut split_done = false;
        match shape {
            Shape::Whole => {
                let split = per_line(&mut problems);
                if split.is_empty() {
                    variants.push(whole_marker(name, false));
                } else if split.len() < texts.len() {
                    let missing: Vec<String> = (0..texts.len())
                        .filter(|l| !split.iter().any(|v| v.line == Some(*l)))
                        .map(|l| l.to_string())
                        .collect();
                    problems.push(format!(
                        "「{name}」は {} 行目を項目に割れないので、全体差し替えのまま(全文経路)にした",
                        missing.join("・")
                    ));
                    variants.push(whole_marker(name, false));
                } else {
                    // どの行にも差し替えられない本文の項目は、短い形では書かれないので外す(派生)。
                    let covered: BTreeSet<String> = split
                        .iter()
                        .filter(|v| v.mode == Mode::Replace)
                        .flat_map(|v| v.target.iter().cloned())
                        .collect();
                    let uncovered: Vec<&String> = body_slots.iter().filter(|s| !covered.contains(*s)).collect();
                    if !uncovered.is_empty() && (module_condition.is_empty() || covered.is_empty()) {
                        // 外す変種は文脈で当たる(auto)物だけなので、条件の無い全体差し替えでは本文の文が残って
                        // 割った行と食い違う。割らずに全体差し替え(全文経路)のままにする。
                        problems.push(format!(
                            "「{name}」を行に割ると本文の{}に当たる行が無いので、全体差し替えのまま(全文経路)にした",
                            uncovered.iter().map(|s| format!("「{s}」")).collect::<String>()
                        ));
                        variants.push(whole_marker(name, false));
                    } else {
                        variants.extend(split);
                        split_done = true;
                        {
                            for slot in uncovered {
                                variants.push(Variant {
                                    module: name.to_string(),
                                    line: None,
                                    target: vec![slot.clone()],
                                    mode: Mode::Remove,
                                    polarity: None,
                                    condition: Some(module_condition.clone()),
                                    auto: true,
                                    derived: true,
                                    ..Variant::default()
                                });
                            }
                        }
                        variants.push(whole_marker(name, true));
                    }
                }
            }
            Shape::PerLine => {
                let found = per_line(&mut problems);
                if found.len() < texts.len() {
                    let missing: Vec<String> = (0..texts.len())
                        .filter(|l| !found.iter().any(|v| v.line == Some(*l)))
                        .map(|l| l.to_string())
                        .collect();
                    problems.push(format!("「{name}」の {} 行目に変種が無い", missing.join("・")));
                    variants.extend(found);
                } else if picks_lines || texts.len() == 1 {
                    variants.extend(found);
                } else if let Some(single) = collapse(&found, &body_slots) {
                    // 行を選ぶと言っていない複数行のモジュールは、まとめて 1 つ(モジュール単位)。行ごとに別の項目を
                    // 差し替える答えをまとめたときは、1 行だけ言われても全部の項目が差し替わるので知らせる。
                    if found.iter().any(|v| v.target != found[0].target) {
                        problems.push(format!(
                            "「{name}」は行ごとに別の項目({})を差し替える答えだったが、注が行を選ぶと言っていないのでまとめて 1 つにした",
                            single.target.join("・")
                        ));
                    }
                    variants.push(single);
                } else {
                    problems.push(format!(
                        "「{name}」の注は行を選ぶと言っていないが、行ごとに置き方が違うので行ごとの変種のまま"
                    ));
                    variants.extend(found);
                }
            }
            Shape::Single => {
                if picks_lines && texts.len() > 1 {
                    problems.push(format!("「{name}」の注は言われた行だけを使うと言うが、まとめて 1 つの変種になっている"));
                }
                // まとめて 1 つ。1 行のモジュールは行 0、複数行はモジュール単位(null)。
                let Some(first) = raw.variants.first() else {
                    problems.push(format!("「{name}」の変種が無い"));
                    continue;
                };
                if raw.variants.len() > 1 {
                    problems.push(format!(
                        "「{name}」はまとめて 1 つの変種なのに {} 個返ったので最初の物を使った",
                        raw.variants.len()
                    ));
                }
                let line = (texts.len() == 1).then_some(0);
                variants.push(convert_variant(
                    first,
                    name,
                    &texts,
                    line,
                    &module_condition,
                    graded,
                    &body_slots,
                    vocabulary,
                    &mut problems,
                ));
            }
        }
        if graded && split_done {
            groups.push(Group { module: name.to_string(), scale: None });
        }
    }
    for raw in &answer.modules {
        if !with_text.iter().any(|m| m.name == raw.name || loose(&m.name) == loose(&raw.name))
            && answer.modules.len() != with_text.len()
        {
            problems.push(format!("答えの「{}」は源泉に無いモジュールなので捨てた", raw.name));
        }
    }

    infer_organs(template, &lines, &mut variants);
    let mut structure = TemplateStructure {
        name: template.keywords.first().cloned().unwrap_or_default(),
        annotated: annotated.to_string(),
        lines,
        variants,
        groups,
        unresolved: Vec::new(),
    };
    for warning in review(template, &structure, vocabulary) {
        let module = warning.split('」').next().unwrap_or("");
        let explained = warning.contains("は全体差し替えのまま(行に割れていない")
            && problems.iter().any(|p| p.starts_with(module) && p.contains("全文経路"));
        if !explained {
            problems.push(warning);
        }
    }
    structure.unresolved = dedup(problems);
    Ok(structure)
}

/// 文に出る列挙の臓器(長い名前の中に入っている短い名前は数えない。「副腎」の中の「腎」)。1 つに決まれば返す。
fn mentioned_organ(text: &str, organs: &[String]) -> Option<String> {
    let mut found: Vec<&String> = Vec::new();
    for organ in organs.iter().filter(|o| !o.is_empty()) {
        let mut masked = text.to_string();
        for longer in organs.iter().filter(|o| o.len() > organ.len() && o.contains(organ.as_str())) {
            masked = masked.replace(longer.as_str(), &"□".repeat(longer.chars().count()));
        }
        if masked.contains(organ.as_str()) {
            found.push(organ);
        }
    }
    (found.len() == 1).then(|| found[0].clone())
}

/// 列挙否定の行 1 つを差し替える変種に外す臓器が無ければ、変種の文に出る列挙の臓器から決める(1 つに決まる
/// ときだけ)。決まらなければそのまま(`review` が「外す臓器が無い」と出す)。
fn infer_organs(template: &Template, lines: &[LineInfo], variants: &mut [Variant]) {
    for variant in variants.iter_mut() {
        if variant.mode != Mode::Replace
            || variant.organ.is_some()
            || variant.kind.is_some()
            || variant.derived
            || variant.target.len() != 1
        {
            continue;
        }
        let Some(list) = lines
            .iter()
            .find(|l| l.kind == LineKind::ListNegation && l.slot.as_deref() == Some(variant.target[0].as_str()))
        else {
            continue;
        };
        let Some(module) = template.modules.iter().find(|m| m.name == variant.module) else {
            continue;
        };
        let texts = module_lines(module);
        let text = match variant.line {
            Some(l) => texts.get(l).map(|t| t.to_string()).unwrap_or_default(),
            None => texts.join("\n"),
        };
        variant.organ = mentioned_organ(&text, &list.organs);
    }
}

/// 確認表で鍵を選び直したとき: 列挙の鍵(語彙の `list_slots`)を選んだ文の行は、鍵の臓器の並びが文にあれば
/// 列挙否定にする(注釈のときと同じ規則)。
pub fn derive_list_lines(template: &Template, structure: &mut TemplateStructure, vocabulary: &Vocabulary) {
    let body = canonical_lines(&template.body);
    for line in structure.lines.iter_mut().filter(|l| l.kind == LineKind::Sentence) {
        let Some(organs) = line.slot.as_ref().and_then(|s| vocabulary.list_slots.get(s)) else {
            continue;
        };
        if let Some((sep, pattern)) = body.get(line.i).and_then(|text| list_pattern(text, organs)) {
            line.kind = LineKind::ListNegation;
            line.organs = organs.clone();
            line.sep = sep;
            line.pattern = pattern;
        }
    }
}

fn dedup(items: Vec<String>) -> Vec<String> {
    let mut seen = BTreeSet::new();
    items.into_iter().filter(|item| seen.insert(item.clone())).collect()
}

// ------------------------------------------------------------------------------------------------
// 機械の検査(決定論)。注と本文のずれ、構造の lint、変種の無いモジュール。
// ------------------------------------------------------------------------------------------------

/// 人に見せる警告(`unresolved`)を構造から作り直す。確認表で直したあとにも呼ぶ。
pub fn review(template: &Template, structure: &TemplateStructure, vocabulary: &Vocabulary) -> Vec<String> {
    let mut found = note_problems(template, structure);
    found.extend(check_structure(template, structure, vocabulary));
    for module in &template.modules {
        if !module_lines(module).is_empty() && !structure.variants.iter().any(|v| v.module == module.name) {
            found.push(format!("「{}」に変種が無い", module.name));
        }
    }
    for line in structure.lines.iter().filter(|l| l.kind == LineKind::Sentence) {
        if let Some(slot) = line.slot.as_ref().filter(|s| vocabulary.list_slots.contains_key(*s)) {
            found.push(format!("本文 {} 行目は列挙の鍵「{slot}」だが、列挙否定として読めていない", line.i));
        }
    }
    for variant in &structure.variants {
        let at = match variant.line {
            Some(l) => format!("「{}」{l} 行目", variant.module),
            None => format!("「{}」", variant.module),
        };
        if variant.is_whole() && !variant.split {
            found.push(format!("{at}は全体差し替えのまま(行に割れていないので、補完では全文を書かせる)"));
        }
        if let Some(purpose) = variant.condition.as_ref().and_then(|c| c.purpose.as_ref()) {
            if !variant.is_whole() && variant.mode != Mode::Remove && variant.polarity.as_deref() != Some("negative") {
                found.push(format!("{at}: 目的の条件「{purpose}」は陰性の文だけに付ける(所見の文は目的に依らず置く)"));
            }
        }
        let replaces_list = variant.mode == Mode::Replace
            && variant.organ.is_none()
            && variant.kind.is_none()
            && !variant.is_whole()
            && variant.target.len() == 1
            && structure
                .lines
                .iter()
                .any(|l| l.kind == LineKind::ListNegation && l.slot.as_deref() == Some(variant.target[0].as_str()));
        if replaces_list {
            found.push(format!(
                "{at}: 列挙否定の行「{}」を丸ごと差し替える(外す臓器が無いので、ほかの臓器の否定も消える)",
                variant.target[0]
            ));
        }
    }
    dedup(found)
}

/// 項目を指す語句の後ろに付く印(「肝の文」「肺の正常文」「…の否定」)。長い物から見る。
const ITEM_MARKERS: [&str; 5] = ["の正常文", "の否定", "の文", "の行", "と差し替"];
/// 項目でない語(「両方の文」「最後の文」「正常文の否定」「陰性の文」など)。
const NOT_ITEMS: [&str; 18] = [
    "両方", "正常", "正常文", "正常所見", "陰性", "陽性", "異常", "所見", "最後", "先頭", "全体", "各", "他", "次",
    "同", "該当", "文", "行",
];
/// 引用の後ろがこれで始まれば、本文の引用ではなく書き換え後の言い方(「…」を「…」にする)。
const NEW_WORDING: [&str; 11] = [
    "にする", "に直す", "に変える", "に替える", "に置き換える", "に言い換える", "とする", "と書く", "と書き換える",
    "と言い換える", "と直す",
];
/// 引用の後ろが「を(文頭に)付ける・足す・加える・入れる」なら、本文の引用ではなく足す語(「その他、」を文頭に付ける)。
const ADDED_WORDING: [&str; 4] = ["付け", "足", "加え", "入れ"];
/// 足す語の動詞の前に置かれる位置の言い方(「を文頭に付ける」)。
const ADDED_PLACES: [&str; 7] = ["文頭に", "文末に", "先頭に", "末尾に", "前に", "後に", "頭に"];
/// 括弧の中が本文の文の引用と見なせる終わり方(文末か省略)。
const SENTENCE_ENDS: [&str; 10] = [
    "ません", "です", "ず", "…", "・・・", "認めます", "みられます", "られます", "疑います", "考えます",
];
/// 引用の省略の印。ここで割って、割れた断片ごとに本文を探す。
const ELLIPSES: [&str; 4] = ["・・・", "...", "…", "‥"];

/// 項目の語に使う字(漢字・カタカナ・中点・英数)。
fn is_item_char(c: char) -> bool {
    matches!(c,
        '\u{4E00}'..='\u{9FFF}' | '々' | '\u{30A1}'..='\u{30FA}' | 'ー' | '・' | 'Ａ'..='Ｚ' | 'ａ'..='ｚ' | '０'..='９')
        || c.is_ascii_alphanumeric()
}

/// 引用の括弧の組(「」・“”・"")。
const QUOTE_PAIRS: [(char, char); 3] = [('「', '」'), ('“', '”'), ('"', '"')];

/// 括弧で括った引用を取り出す(引用, 後ろの文)と、引用を □ に置いた注。括弧は最初に出た種類から順に見る。
fn bracket_quotes(note: &str) -> (Vec<(String, String)>, String) {
    let mut quotes = Vec::new();
    let mut rest = String::new();
    let mut remaining = note;
    loop {
        let next = QUOTE_PAIRS
            .iter()
            .filter_map(|(open, close)| remaining.find(*open).map(|at| (at, *open, *close)))
            .min_by_key(|(at, _, _)| *at);
        let Some((at, open, close)) = next else {
            break;
        };
        rest.push_str(&remaining[..at]);
        let after_open = &remaining[at + open.len_utf8()..];
        let Some(end) = after_open.find(close) else {
            rest.push_str(&remaining[at..]);
            remaining = "";
            break;
        };
        let quote = &after_open[..end];
        let after = &after_open[end + close.len_utf8()..];
        quotes.push((quote.to_string(), after.to_string()));
        rest.push('□');
        remaining = after;
    }
    rest.push_str(remaining);
    (quotes, rest)
}

/// 引用が本文の引用でなく、新しい言い方か足す語か(後ろの言い回しで決める)。
fn is_new_wording(after: &str) -> bool {
    let after = after.trim_start();
    if NEW_WORDING.iter().any(|w| after.starts_with(w)) {
        return true;
    }
    let Some(verb) = after.strip_prefix('を') else {
        return false;
    };
    let verb = ADDED_PLACES.iter().find_map(|place| verb.strip_prefix(place)).unwrap_or(verb);
    ADDED_WORDING.iter().any(|w| verb.starts_with(w))
}

/// 括弧(半角に寄せた後の `(…)`)の中で、本文の文の引用らしい物(文末か省略で終わる、例示でない)。
fn paren_quotes(note: &str) -> Vec<String> {
    let mut quotes = Vec::new();
    let mut remaining = note;
    while let Some(open) = remaining.find('(') {
        let after_open = &remaining[open + 1..];
        let Some(close) = after_open.find(')') else {
            break;
        };
        let inside = after_open[..close].trim().trim_end_matches('。');
        if !inside.starts_with("例") && SENTENCE_ENDS.iter().any(|end| inside.ends_with(end)) {
            quotes.push(inside.to_string());
        }
        remaining = &after_open[close + 1..];
    }
    quotes
}

/// 注が項目を指す語句(「脾・腎・副腎・膀胱の文」)と、その項目の語(脾・腎・副腎・膀胱)。
fn item_references(note: &str) -> Vec<(String, Vec<String>)> {
    let mut found = Vec::new();
    for marker in ITEM_MARKERS {
        for (at, _) in note.match_indices(marker) {
            let head = &note[..at];
            let start = head
                .char_indices()
                .rev()
                .take_while(|(_, c)| is_item_char(*c))
                .last()
                .map_or(at, |(i, _)| i);
            let item = &note[start..at];
            if item.is_empty() {
                continue;
            }
            let parts: Vec<String> = item
                .split(['・', '、'])
                .map(str::trim)
                .filter(|p| !p.is_empty() && !NOT_ITEMS.contains(p))
                .map(str::to_string)
                .collect();
            if !parts.is_empty() {
                found.push((format!("{item}{marker}"), parts));
            }
        }
    }
    found.sort_by_key(|(phrase, _)| note.find(phrase.as_str()).unwrap_or(usize::MAX));
    found
}

/// 引用の断片(省略の印で割り、端の句読点・括弧を除き、2 字以上の物)。
fn quote_fragments(quote: &str) -> Vec<String> {
    let mut pieces = vec![loose(quote)];
    for mark in ELLIPSES {
        pieces = pieces.iter().flat_map(|p| p.split(mark).map(str::to_string).collect::<Vec<_>>()).collect();
    }
    pieces
        .into_iter()
        .map(|p| p.trim_matches(|c: char| "。、()「」・ ".contains(c)).to_string())
        .filter(|p| p.chars().count() >= 2)
        .collect()
}

/// 使い方の注と本文のずれ(古い注)。注が指す項目が本文(本文の文・項目の鍵・このテンプレートのモジュールの
/// 名前と文)に無い、注の引用(「」と、文の形をした括弧の中)が本文にもモジュールの文にも無い。
/// 新しい言い方を示す引用(「…」を「…」にする、の後ろ)は見ない。
pub fn note_problems(template: &Template, structure: &TemplateStructure) -> Vec<String> {
    let body = loose(&canonical_lines(&template.body).join("\n"));
    let module_texts: Vec<String> = template.modules.iter().map(|m| loose(&m.text)).collect();
    let module_names: Vec<String> = template.modules.iter().map(|m| loose(&m.name)).collect();
    let slots: Vec<String> = structure.lines.iter().filter_map(|l| l.slot.as_deref()).map(loose).collect();
    let in_text = |piece: &str| body.contains(piece) || module_texts.iter().any(|t| t.contains(piece));
    let mut problems = Vec::new();
    for module in &template.modules {
        let note = fold_width(&canonical_lines(&module.note).join("\n"));
        if note.trim().is_empty() {
            continue;
        }
        let (quotes, rest) = bracket_quotes(&note);
        let mut cited: Vec<String> = Vec::new();
        for (quote, after) in &quotes {
            if is_new_wording(after) {
                continue;
            }
            cited.push(quote.clone());
        }
        cited.extend(paren_quotes(&rest));
        for quote in cited {
            if quote_fragments(&quote).iter().any(|piece| !in_text(piece)) {
                problems.push(format!("{}: 注の引用「{quote}」が本文に無い", module.name));
            }
        }
        let mut reported: BTreeSet<String> = BTreeSet::new();
        for (phrase, parts) in item_references(&rest) {
            let missing: Vec<String> = parts
                .into_iter()
                .filter(|part| {
                    let part = loose(part);
                    // 「肝臓」は本文の「肝」で足りる(臓の付く言い方)。
                    let short = part.strip_suffix('臓').filter(|s| !s.is_empty()).unwrap_or(&part).to_string();
                    let known = |piece: &str| {
                        in_text(piece)
                            || module_names.iter().any(|n| n.contains(piece))
                            || slots.iter().any(|s| s == piece || s.contains(piece))
                    };
                    !(known(&part) || known(&short))
                })
                .filter(|part| reported.insert(part.clone()))
                .collect();
            if !missing.is_empty() {
                let names: String = missing.iter().map(|m| format!("「{m}」")).collect();
                problems.push(format!("{}: 注の「{phrase}」の{names}が本文に無い", module.name));
            }
        }
    }
    problems
}

// ------------------------------------------------------------------------------------------------
// 一致率(S2 の完了条件。手書きの構造と、注釈した構造の比べ方)
// ------------------------------------------------------------------------------------------------

/// 一致の数え方: 行は鍵(`slot`)、変種は (モジュール, 行, 種類) で組にして対象の集合と置き方。
/// 種類は 全体差し替えの印・外す(対象の集合で見分ける)・ふつう。手書きに無い変種は「余分」で分母に足す。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Agreement {
    pub lines: usize,
    pub lines_matched: usize,
    pub variants: usize,
    pub variants_matched: usize,
    /// 完了条件の外の参考: 組になった変種(印・外すを除く)の極性と、役割(どちらかにある物)の一致。
    pub polarities: usize,
    pub polarities_matched: usize,
    pub roles: usize,
    pub roles_matched: usize,
    /// 参考: 行の種類(文・列挙否定・見出し・空行)と、組になった変種の外す臓器(`organ`)の一致。
    pub kinds_matched: usize,
    pub organs: usize,
    pub organs_matched: usize,
    pub mismatches: Vec<String>,
}

impl Agreement {
    pub fn total(&self) -> usize {
        self.lines + self.variants
    }

    pub fn matched(&self) -> usize {
        self.lines_matched + self.variants_matched
    }

    /// 一致の割合(0〜1)。比べる物が無ければ 1。
    pub fn rate(&self) -> f64 {
        ratio(self.matched(), self.total())
    }

    pub fn line_rate(&self) -> f64 {
        ratio(self.lines_matched, self.lines)
    }

    pub fn variant_rate(&self) -> f64 {
        ratio(self.variants_matched, self.variants)
    }

    pub fn add(&mut self, other: &Agreement) {
        self.lines += other.lines;
        self.lines_matched += other.lines_matched;
        self.variants += other.variants;
        self.variants_matched += other.variants_matched;
        self.polarities += other.polarities;
        self.polarities_matched += other.polarities_matched;
        self.roles += other.roles;
        self.roles_matched += other.roles_matched;
        self.kinds_matched += other.kinds_matched;
        self.organs += other.organs;
        self.organs_matched += other.organs_matched;
        self.mismatches.extend(other.mismatches.iter().cloned());
    }
}

fn ratio(part: usize, whole: usize) -> f64 {
    if whole == 0 {
        1.0
    } else {
        part as f64 / whole as f64
    }
}

fn variant_key(variant: &Variant) -> (String, Option<usize>, String) {
    let class = if variant.is_whole() {
        "*".to_string()
    } else if variant.mode == Mode::Remove {
        let mut targets = variant.target.clone();
        targets.sort();
        format!("外す:{}", targets.join("・"))
    } else {
        String::new()
    };
    (variant.module.clone(), variant.line, class)
}

/// 置き方の名前(確認表と CLI)。
pub fn mode_label(mode: Mode) -> &'static str {
    match mode {
        Mode::Replace => "差し替え",
        Mode::Prepend => "前に足す",
        Mode::Append => "後に足す",
        Mode::Remove => "外す",
    }
}

/// 極性の名前(確認表と CLI)。
pub fn polarity_label(polarity: Option<&str>) -> &'static str {
    match polarity {
        Some("negative") => "陰性",
        Some("benign") => "正常範囲",
        Some("abnormal") => "異常",
        Some("mixed") => "混在",
        _ => "—",
    }
}

/// 変種の「ほか」の欄: 足す鍵・外す臓器・接頭語・列挙・条件・auto。無ければ空。
pub fn variant_extras(variant: &Variant) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(slot) = &variant.slot {
        parts.push(format!("足す鍵 {slot}"));
    }
    if let Some(organ) = &variant.organ {
        parts.push(format!("列挙から{organ}を外す"));
    }
    if let Some(prefix) = &variant.prefix {
        parts.push(format!("続く文に「{prefix}」"));
    }
    if variant.kind == Some(LineKind::ListNegation) {
        parts.push(format!("列挙({})", variant.organs.join(&variant.sep)));
    }
    if let Some(condition) = variant.condition.as_ref().filter(|c| !c.is_empty()) {
        parts.push(condition.label());
    }
    if variant.auto {
        parts.push("自動".to_string());
    }
    if let Some(role) = &variant.role {
        parts.push(format!("役割 {role}"));
    }
    if let Some(grades) = &variant.grades {
        parts.push(format!("程度 {}", grades.join("・")));
    }
    parts.join("、")
}

/// 手書きの構造(`gold`)と注釈した構造(`got`)の一致。
pub fn agreement(gold: &TemplateStructure, got: &TemplateStructure) -> Agreement {
    let mut result = Agreement::default();
    let name = &gold.name;
    for (i, expected) in gold.lines.iter().enumerate() {
        result.lines += 1;
        result.kinds_matched += usize::from(got.lines.get(i).is_some_and(|l| l.kind == expected.kind));
        let actual = got.lines.get(i).and_then(|l| l.slot.clone());
        if actual == expected.slot {
            result.lines_matched += 1;
        } else {
            result.mismatches.push(format!(
                "{name} 本文 {i} 行目の鍵: 手書き {} / 注釈 {}",
                expected.slot.as_deref().unwrap_or("なし"),
                actual.as_deref().unwrap_or("なし")
            ));
        }
    }
    let gold_keys: Vec<_> = gold.variants.iter().map(variant_key).collect();
    for (expected, key) in gold.variants.iter().zip(&gold_keys) {
        result.variants += 1;
        let at = format!("{name}「{}」{}", expected.module, expected.line.map_or("全体".to_string(), |l| format!("{l} 行目")));
        match got.variants.iter().find(|v| &variant_key(v) == key) {
            Some(actual) => {
                if key.2.is_empty() {
                    result.polarities += 1;
                    result.polarities_matched += usize::from(actual.polarity == expected.polarity);
                    if expected.role.is_some() || actual.role.is_some() {
                        result.roles += 1;
                        result.roles_matched += usize::from(actual.role == expected.role);
                    }
                    if expected.organ.is_some() || actual.organ.is_some() {
                        result.organs += 1;
                        result.organs_matched += usize::from(actual.organ == expected.organ);
                    }
                }
                let mut a = actual.target.clone();
                let mut e = expected.target.clone();
                a.sort();
                e.sort();
                if a == e && actual.mode == expected.mode {
                    result.variants_matched += 1;
                } else {
                    result.mismatches.push(format!(
                        "{at}{}: 手書き {}・{} / 注釈 {}・{}",
                        if key.2.is_empty() { "" } else { " " },
                        expected.target.join("・"),
                        mode_label(expected.mode),
                        actual.target.join("・"),
                        mode_label(actual.mode)
                    ));
                }
            }
            None => result.mismatches.push(format!(
                "{at}: 手書きの {}・{}{} が注釈に無い",
                expected.target.join("・"),
                mode_label(expected.mode),
                if key.2.is_empty() { "" } else { "(印・外す)" }
            )),
        }
    }
    for actual in &got.variants {
        if !gold_keys.contains(&variant_key(actual)) {
            result.variants += 1;
            result.mismatches.push(format!(
                "{name}「{}」{}: 手書きに無い変種 {}・{}",
                actual.module,
                actual.line.map_or("全体".to_string(), |l| format!("{l} 行目")),
                actual.target.join("・"),
                mode_label(actual.mode)
            ));
        }
    }
    result
}

// ------------------------------------------------------------------------------------------------
// 注釈役(API・Claude Code)
// ------------------------------------------------------------------------------------------------

/// 注釈役。テンプレート 1 件の答え(`answer_schema` の形の JSON)を返す。
pub trait Annotator: Send + Sync {
    fn name(&self) -> String;
    fn answer(&self, template: &Template, vocabulary: &Vocabulary) -> Result<Value, String>;
}

/// API キーで直接送る注釈役。
pub struct ApiAnnotator {
    model: String,
    api_key: String,
}

impl ApiAnnotator {
    pub fn new(model: &str, api_key: &str) -> Self {
        ApiAnnotator { model: model.to_string(), api_key: api_key.to_string() }
    }
}

/// Messages API に送る本文。system(語彙と規則)はテンプレートに依らないのでキャッシュが効く。
pub fn request_body(model: &str, template: &Template, vocabulary: &Vocabulary) -> Value {
    claude_api::messages_body(
        model,
        MAX_TOKENS,
        ANNOTATION_EFFORT,
        &system_prompt(vocabulary),
        None,
        user_message(template),
        answer_schema(vocabulary),
    )
}

impl Annotator for ApiAnnotator {
    fn name(&self) -> String {
        model_label(&self.model)
    }

    fn answer(&self, template: &Template, vocabulary: &Vocabulary) -> Result<Value, String> {
        let body = request_body(&self.model, template, vocabulary);
        let (status, response) =
            claude_api::post_json(&self.api_key, &body, &API_TIMEOUT_SECONDS.to_string(), ROLE_NAME)?;
        let text = claude_api::response_text(status, &response, ROLE_NAME)?;
        serde_json::from_str(&text).map_err(|_| format!("{ROLE_NAME}の答えが読めません"))
    }
}

/// この Mac の Claude Code(`claude -p`)を使う注釈役。
pub struct ClaudeCodeAnnotator {
    model: String,
    binary: PathBuf,
    /// 起動する場所(データのフォルダ。ここの設定やフックを読まない)。
    work_dir: PathBuf,
}

impl ClaudeCodeAnnotator {
    pub fn new(model: &str, binary: &Path, work_dir: &Path) -> Self {
        ClaudeCodeAnnotator {
            model: model.to_string(),
            binary: binary.to_path_buf(),
            work_dir: work_dir.to_path_buf(),
        }
    }
}

impl Annotator for ClaudeCodeAnnotator {
    fn name(&self) -> String {
        format!("{}・Claude Code", model_label(&self.model))
    }

    fn answer(&self, template: &Template, vocabulary: &Vocabulary) -> Result<Value, String> {
        let (stdout, stderr) = proc::run_claude_code(
            &self.binary,
            &self.work_dir,
            &self.model,
            &system_prompt(vocabulary),
            proc::Answer { schema: &answer_schema(vocabulary), effort: ANNOTATION_EFFORT },
            &user_message(template),
            Duration::from_secs(CLAUDE_CODE_TIMEOUT_SECONDS),
        )?;
        proc::claude_code_structured(&stdout, &stderr)
    }
}

/// 注釈した時刻(`2026-11-10T09:00:00+09:00` の形)。
pub fn now_stamp() -> String {
    chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, false)
}

/// テンプレート 1 件を注釈する。接続の失敗と読めない答えは Err(未注釈のまま)。
pub fn annotate(annotator: &dyn Annotator, template: &Template, vocabulary: &Vocabulary) -> Result<TemplateStructure, String> {
    let answer = annotator.answer(template, vocabulary)?;
    structure_from_answer(template, vocabulary, &answer, &now_stamp())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::dictionary::Module;
    use crate::core::structure::{render, Context, Selection};

    fn vocabulary() -> Vocabulary {
        Vocabulary::builtin()
    }

    fn module(name: &str, text: &str, note: &str) -> Module {
        Module { name: name.to_string(), text: text.to_string(), note: note.to_string(), ..Module::default() }
    }

    /// 小さな腹部のテンプレート(試験用。実物ではない)。
    fn abdomen() -> Template {
        Template {
            keywords: vec!["試験腹部CT".to_string()],
            body: "肝に占拠性病変を認めません。\n脾、腎、副腎に特記所見ありません。\n\n腹水はみられません。\n腹部に病的なリンパ節腫大はみられません。".to_string(),
            modules: vec![
                module("肝", "軽度の脂肪肝を認めます。\n肝嚢胞を認めます。", "言われた所見の行だけを肝の文と差し替える"),
                module("腎", "右腎に結石を認めます。", "脾・腎・副腎の文から腎を外して置く"),
                module("若年女性", "肝、脾、腎、副腎に異常を認めません。\n子宮および両付属器は年齢相応です。\n腹水なし。", "若い女性なら正常所見全体をこの文に差し替える"),
                module("腹痛精査", "虫垂は正常です。", "腹痛の精査なら腹水の文の前に足す(陰性の文)"),
            ],
            extra: Default::default(),
        }
    }

    /// 決まった答え(Claude の代わり)。
    fn abdomen_answer() -> Value {
        json!({
            "lines": [
                {"i": 0, "kind": "sentence", "slot": "肝", "organs": []},
                {"i": 1, "kind": "list_negation", "slot": "脾腎副腎", "organs": ["脾", "腎", "副腎"]},
                {"i": 2, "kind": "blank", "slot": null, "organs": []},
                {"i": 3, "kind": "sentence", "slot": "腹水", "organs": []},
                {"i": 4, "kind": "sentence", "slot": "腹部骨盤リンパ節", "organs": []}
            ],
            "modules": [
                {"name": "肝", "shape": "per_line", "graded": false, "sex": null, "age_band": null, "purpose": null,
                 "variants": [
                    {"line": 0, "target": ["肝"], "mode": "replace", "polarity": "abnormal", "sex": null, "age_band": null, "purpose": null, "auto": false, "slot": null, "organ": null, "prefix": null, "role": null, "grades": null, "list_organs": [], "organ_slots": []},
                    {"line": 1, "target": ["肝"], "mode": "replace", "polarity": "benign", "sex": null, "age_band": null, "purpose": null, "auto": true, "slot": null, "organ": null, "prefix": null, "role": "初期", "grades": null, "list_organs": [], "organ_slots": []}
                 ]},
                {"name": "腎", "shape": "single", "graded": false, "sex": null, "age_band": null, "purpose": null,
                 "variants": [
                    {"line": 3, "target": ["脾腎副腎"], "mode": "replace", "polarity": "abnormal", "sex": null, "age_band": null, "purpose": null, "auto": false, "slot": null, "organ": "腎", "prefix": null, "role": null, "grades": null, "list_organs": [], "organ_slots": []}
                 ]},
                {"name": "若年女性", "shape": "whole", "graded": false, "sex": "女", "age_band": "若年", "purpose": null,
                 "variants": [
                    {"line": 0, "target": ["肝", "脾腎副腎"], "mode": "replace", "polarity": "negative", "sex": null, "age_band": null, "purpose": null, "auto": true, "slot": null, "organ": null, "prefix": null, "role": null, "grades": null,
                     "list_organs": ["肝", "脾", "腎", "副腎"], "organ_slots": [{"organ": "肝", "slots": ["肝"]}, {"organ": "脾", "slots": ["脾腎副腎"]}, {"organ": "腎", "slots": ["脾腎副腎"]}, {"organ": "副腎", "slots": ["脾腎副腎"]}]},
                    {"line": 1, "target": ["脾腎副腎"], "mode": "append", "polarity": "negative", "sex": "女", "age_band": null, "purpose": null, "auto": true, "slot": "子宮・付属器", "organ": null, "prefix": null, "role": null, "grades": null, "list_organs": [], "organ_slots": []},
                    {"line": 2, "target": ["腹水"], "mode": "replace", "polarity": "negative", "sex": null, "age_band": null, "purpose": null, "auto": true, "slot": null, "organ": null, "prefix": null, "role": null, "grades": null, "list_organs": [], "organ_slots": []}
                 ]},
                {"name": "腹痛精査", "shape": "single", "graded": false, "sex": null, "age_band": null, "purpose": "腹痛",
                 "variants": [
                    {"line": null, "target": ["腹水"], "mode": "prepend", "polarity": "negative", "sex": null, "age_band": null, "purpose": null, "auto": true, "slot": "虫垂", "organ": null, "prefix": null, "role": null, "grades": null, "list_organs": [], "organ_slots": []}
                 ]}
            ]
        })
    }

    #[test]
    fn every_slot_note_names_a_vocabulary_key() {
        let vocabulary = vocabulary();
        for key in vocabulary.slot_notes.keys() {
            assert!(vocabulary.has_slot(key), "slot_notes の「{key}」は語彙に無い");
        }
    }

    #[test]
    fn the_schema_is_closed_and_every_field_is_required() {
        let vocabulary = vocabulary();
        let schema = answer_schema(&vocabulary);
        fn walk(value: &Value, path: &str) {
            if value["type"] == "object" {
                assert_eq!(value["additionalProperties"], false, "{path}");
                let properties = value["properties"].as_object().unwrap();
                let required: Vec<&str> = value["required"].as_array().unwrap().iter().map(|v| v.as_str().unwrap()).collect();
                for name in properties.keys() {
                    assert!(required.contains(&name.as_str()), "{path}.{name} が必須でない");
                    walk(&properties[name], &format!("{path}.{name}"));
                }
            }
            if let Some(items) = value.get("items") {
                walk(items, &format!("{path}[]"));
            }
            if let Some(any) = value["anyOf"].as_array() {
                for option in any {
                    walk(option, path);
                }
            }
        }
        walk(&schema, "answer");
        let slot_enum = &schema["properties"]["lines"]["items"]["properties"]["slot"]["anyOf"][0]["enum"];
        assert_eq!(slot_enum.as_array().unwrap().len(), vocabulary.slot_keys().count());
        let variant = &schema["properties"]["modules"]["items"]["properties"]["variants"]["items"]["properties"];
        assert_eq!(variant["mode"]["enum"], json!(["replace", "prepend", "append"]), "外すはコードが決める");
        assert_eq!(variant["role"]["anyOf"][0]["enum"].as_array().unwrap().len(), 6);
        assert_eq!(variant["purpose"]["anyOf"][0]["enum"], json!(vocabulary.purposes));
    }

    #[test]
    fn the_prompt_lists_the_vocabulary_and_the_request_numbers_the_lines() {
        let vocabulary = vocabulary();
        let system = system_prompt(&vocabulary);
        for key in vocabulary.slot_keys() {
            assert!(system.contains(key), "語彙の「{key}」が system に無い");
        }
        assert!(system.contains("脾腎副腎(脾・腎・副腎をまとめて述べる行)"));
        assert!(system.contains("障害: 実質・神経・骨の損傷、合併症(既定の区間 高度)"));
        let message = user_message(&abdomen());
        assert!(message.starts_with("テンプレート「試験腹部CT」"));
        assert!(message.contains("1: 脾、腎、副腎に特記所見ありません。\n2: (空行)\n3: 腹水はみられません。"));
        assert!(message.contains("### 若年女性\n使い方: 若い女性なら正常所見全体をこの文に差し替える\n0: 肝、脾、腎、副腎に異常を認めません。"));
        let bare = Template { modules: Vec::new(), ..abdomen() };
        assert!(user_message(&bare).contains("所見モジュール: なし"));
    }

    #[test]
    fn the_api_body_uses_sonnet_by_default_with_the_closed_schema() {
        let vocabulary = vocabulary();
        let body = request_body(ANNOTATION_MODEL, &abdomen(), &vocabulary);
        assert_eq!(body["model"], "claude-sonnet-5-5");
        assert_eq!(body["output_config"]["effort"], ANNOTATION_EFFORT);
        assert_eq!(body["output_config"]["format"]["schema"], answer_schema(&vocabulary));
        assert_eq!(body["system"][0]["cache_control"]["type"], "ephemeral");
        assert!(body["messages"][0]["content"].as_str().unwrap().contains("0: 肝に占拠性病変を認めません。"));
    }

    #[test]
    fn list_patterns_are_found_in_the_sentence() {
        let organs = |list: &[&str]| list.iter().map(|o| o.to_string()).collect::<Vec<_>>();
        assert_eq!(
            list_pattern("脾、腎、副腎に特記所見ありません。", &organs(&["脾", "腎", "副腎"])),
            Some(("、".to_string(), "{organs}に特記所見ありません。".to_string()))
        );
        assert_eq!(
            list_pattern("精嚢・膀胱に明らかな異常を認めません。", &organs(&["精嚢", "膀胱"])),
            Some(("・".to_string(), "{organs}に明らかな異常を認めません。".to_string()))
        );
        assert_eq!(list_pattern("脾、腎に異常なし。", &organs(&["腎", "脾"])), None, "順が違えば型にならない");
        assert_eq!(list_pattern("肝に異常なし。", &organs(&["肝"])), None);
    }

    #[test]
    fn the_answer_becomes_a_structure_with_the_derived_parts_filled_in() {
        let vocabulary = vocabulary();
        let template = abdomen();
        let structure = structure_from_answer(&template, &vocabulary, &abdomen_answer(), "2026-11-10T09:00:00+09:00").unwrap();
        assert_eq!(structure.name, "試験腹部CT");
        assert_eq!(structure.annotated, "2026-11-10T09:00:00+09:00");
        // 行: 列挙否定の区切りと型は文から、空行は答えに依らず空行。
        assert_eq!(structure.lines.len(), 5);
        assert_eq!(structure.lines[1].kind, LineKind::ListNegation);
        assert_eq!(structure.lines[1].sep, "、");
        assert_eq!(structure.lines[1].pattern, "{organs}に特記所見ありません。");
        assert_eq!(structure.lines[2].kind, LineKind::Blank);
        assert_eq!(structure.lines[2].slot, None);
        let find = |module: &str, line: Option<usize>, mode: Mode| {
            structure
                .variants
                .iter()
                .find(|v| v.module == module && v.line == line && v.mode == mode)
                .unwrap_or_else(|| panic!("{module} {line:?} {mode:?} が無い"))
        };
        // 束: 行ごと。正常範囲に付いた auto は外し、尺度の無い所見群の役割は捨てる。
        let cyst = find("肝", Some(1), Mode::Replace);
        assert_eq!(cyst.polarity.as_deref(), Some("benign"));
        assert!(!cyst.auto);
        assert_eq!(cyst.role, None);
        assert!(structure.unresolved.iter().any(|u| u.contains("陰性でない文に付いた auto を外した")));
        // まとめて 1 つの 1 行のモジュールは行 0(答えの行番号は見ない)。
        let kidney = find("腎", Some(0), Mode::Replace);
        assert_eq!(kidney.organ.as_deref(), Some("腎"));
        // 全体差し替えは行に割り、どの行にも差し替えられない項目(リンパ節)を外す。印は最後。
        let t0 = find("若年女性", Some(0), Mode::Replace);
        assert_eq!(t0.kind, Some(LineKind::ListNegation));
        assert_eq!(t0.pattern, "{organs}に異常を認めません。");
        assert_eq!(t0.organ_slots["腎"], vec!["脾腎副腎".to_string()]);
        assert_eq!(t0.condition.as_ref().unwrap().label(), "女・若年", "モジュールの条件を引き継ぐ");
        let t1 = find("若年女性", Some(1), Mode::Append);
        assert_eq!(t1.slot.as_deref(), Some("子宮・付属器"));
        assert_eq!(t1.condition.as_ref().unwrap().label(), "女", "行の条件が優先");
        let removed = find("若年女性", None, Mode::Remove);
        assert_eq!(removed.target, vec!["腹部骨盤リンパ節".to_string()]);
        assert!(removed.derived && removed.auto && removed.polarity.is_none());
        let marker = structure.variants.iter().rfind(|v| v.module == "若年女性").unwrap();
        assert!(marker.is_whole() && marker.split);
        // 目的の札だけのモジュール: 複数行でなく 1 行なので行 0、足す鍵、auto。
        let appendix = find("腹痛精査", Some(0), Mode::Prepend);
        assert_eq!(appendix.slot.as_deref(), Some("虫垂"));
        assert!(appendix.auto);
        assert_eq!(appendix.condition.as_ref().unwrap().purpose.as_deref(), Some("腹痛"));
        // 組んだ構造は S1 の検査を通る(外した auto の知らせ以外に警告が無い)。
        assert_eq!(check_structure(&template, &structure, &vocabulary), Vec::<String>::new());
        assert_eq!(structure.unresolved.len(), 1, "{:?}", structure.unresolved);
        // そのまま描ける: 若い女性の文脈で、T0 が肝と脾腎副腎を 1 行にし、リンパ節の文が消える。
        let context = Context { sex: Some("女".to_string()), age_band: Some("若年".to_string()), purposes: Vec::new() };
        let rendered = render(&template, &structure, &vocabulary, &context, &[]).unwrap();
        assert_eq!(
            rendered.text,
            "肝、脾、腎、副腎に異常を認めません。\n子宮および両付属器は年齢相応です。\n\n腹水なし。"
        );
        let rendered = render(&template, &structure, &vocabulary, &Context::default(), &[Selection::variant("肝", Some(0), "軽度の脂肪肝を認めます。")]).unwrap();
        assert!(rendered.text.starts_with("軽度の脂肪肝を認めます。\n脾、腎、副腎に特記所見ありません。"));
    }

    #[test]
    fn a_whole_replacement_that_cannot_be_split_stays_whole() {
        let vocabulary = vocabulary();
        let template = abdomen();
        let mut answer = abdomen_answer();
        // 1 行だけ割れない(変種が無い)と、全体差し替えのまま(全文経路)。
        answer["modules"][2]["variants"].as_array_mut().unwrap().remove(2);
        let structure = structure_from_answer(&template, &vocabulary, &answer, "").unwrap();
        let young: Vec<&Variant> = structure.variants.iter().filter(|v| v.module == "若年女性").collect();
        assert_eq!(young.len(), 1);
        assert!(young[0].is_whole() && !young[0].split);
        assert!(structure.unresolved.iter().any(|u| u.contains("2 行目を項目に割れない")));
        // 条件の無い全体差し替えを割ってどの行にも当たらない項目が残れば、外す変種は文脈でしか当たらないので
        // 割らずに全体差し替え(全文経路)のままにして知らせる(QA M1: 割ると本文の文と割った行が並んで食い違う)。
        let mut answer = abdomen_answer();
        answer["modules"][2]["sex"] = Value::Null;
        answer["modules"][2]["age_band"] = Value::Null;
        for variant in answer["modules"][2]["variants"].as_array_mut().unwrap() {
            variant["auto"] = json!(false);
            variant["sex"] = Value::Null;
        }
        let structure = structure_from_answer(&template, &vocabulary, &answer, "").unwrap();
        assert!(!structure.variants.iter().any(|v| v.mode == Mode::Remove));
        let young: Vec<&Variant> = structure.variants.iter().filter(|v| v.module == "若年女性").collect();
        assert_eq!(young.len(), 1);
        assert!(young[0].is_whole() && !young[0].split);
        assert!(structure.unresolved.iter().any(|u| u
            == "「若年女性」を行に割ると本文の「腹部骨盤リンパ節」に当たる行が無いので、全体差し替えのまま(全文経路)にした"));
        assert!(!structure.unresolved.iter().any(|u| u.contains("「若年女性」は全体差し替えのまま(行に割れていない")), "理由を書いた所に重ねない");
    }

    #[test]
    fn missing_and_unknown_parts_are_reported_not_trusted() {
        let vocabulary = vocabulary();
        let template = abdomen();
        let mut answer = abdomen_answer();
        answer["lines"][0]["slot"] = json!("肝臓");
        answer["lines"][1]["organs"] = json!(["脾", "膵"]);
        answer["lines"][3]["kind"] = json!("list_negation");
        answer["lines"][3]["organs"] = json!(["腹水", "胸水"]);
        answer["lines"].as_array_mut().unwrap().remove(4);
        answer["modules"].as_array_mut().unwrap().remove(3);
        answer["modules"][0]["variants"].as_array_mut().unwrap().remove(1);
        let structure = structure_from_answer(&template, &vocabulary, &answer, "").unwrap();
        let unresolved = structure.unresolved.join("\n");
        assert!(unresolved.contains("本文 0 行目: 鍵「肝臓」は語彙に無いので捨てた"), "{unresolved}");
        assert!(unresolved.contains("本文 3 行目: 列挙の臓器(腹水・胸水)を文の中に見つけられないので普通の文にした"));
        assert!(!unresolved.contains("本文 1 行目: 列挙"), "列挙の鍵なら語彙の臓器で割り直す");
        assert!(unresolved.contains("本文 4 行目の注釈が無い"));
        assert!(unresolved.contains("「腹痛精査」の注釈が無い"));
        assert!(unresolved.contains("「肝」の 1 行目に変種が無い"));
        assert!(unresolved.contains("0 行目に鍵が無い"), "S1 の検査も入る");
        assert_eq!(structure.lines[1].kind, LineKind::ListNegation);
        assert_eq!(structure.lines[1].organs, vec!["脾".to_string(), "腎".to_string(), "副腎".to_string()]);
        assert_eq!(structure.lines[3].kind, LineKind::Sentence);
        // 形として読めない答えは Err(未注釈のまま)。
        assert!(structure_from_answer(&template, &vocabulary, &json!({"lines": "x"}), "").is_err());
    }

    /// 空の変種の答え(試験で項目を上書きして使う)。
    fn raw_variant(line: Value, target: &[&str], mode: &str, polarity: &str) -> Value {
        json!({"line": line, "target": target, "mode": mode, "polarity": polarity, "sex": null, "age_band": null,
               "purpose": null, "auto": false, "slot": null, "organ": null, "prefix": null, "role": null,
               "grades": null, "list_organs": [], "organ_slots": []})
    }

    /// QA H1(a): 列挙の鍵の行を文と答えても、語彙の臓器で列挙否定に読み直し、列挙の行を差し替える変種の
    /// 外す臓器は変種の文から決める。描くと精嚢の陰性文が残る(注「精嚢が正常ならその旨は残す」)。
    #[test]
    fn list_keys_become_list_negations_and_the_removed_organ_is_found_in_the_text() {
        let vocabulary = vocabulary();
        let template = Template {
            keywords: vec!["試験前立腺".to_string()],
            body: "精嚢・膀胱に明らかな異常を認めません。\n病的なリンパ節腫大はみられません。".to_string(),
            modules: vec![
                module("肉柱形成", "膀胱壁は肥厚し、肉柱形成もみられます。", "精嚢・膀胱の文と差し替える(精嚢が正常ならその旨は残す)"),
                module("両方", "精嚢と膀胱に浸潤を疑います。", "精嚢・膀胱の文と差し替える"),
            ],
            extra: Default::default(),
        };
        let answer = json!({
            "lines": [
                {"i": 0, "kind": "sentence", "slot": "精嚢・膀胱", "organs": []},
                {"i": 1, "kind": "sentence", "slot": "骨盤リンパ節", "organs": []}
            ],
            "modules": [
                {"name": "肉柱形成", "shape": "single", "graded": false, "sex": null, "age_band": null, "purpose": null,
                 "variants": [raw_variant(Value::Null, &["精嚢・膀胱"], "replace", "abnormal")]},
                {"name": "両方", "shape": "single", "graded": false, "sex": null, "age_band": null, "purpose": null,
                 "variants": [raw_variant(Value::Null, &["精嚢・膀胱"], "replace", "abnormal")]}
            ]
        });
        let structure = structure_from_answer(&template, &vocabulary, &answer, "").unwrap();
        assert_eq!(structure.lines[0].kind, LineKind::ListNegation);
        assert_eq!(structure.lines[0].sep, "・");
        assert_eq!(structure.variants[0].organ.as_deref(), Some("膀胱"));
        assert_eq!(structure.variants[1].organ, None, "両方の臓器が出る文は決めない");
        assert!(structure.unresolved.iter().any(|u| u.contains("「両方」0 行目: 列挙否定の行「精嚢・膀胱」を丸ごと差し替える")));
        let rendered = render(&template, &structure, &vocabulary, &Context::default(), &[Selection::variant("肉柱形成", Some(0), "膀胱壁は肥厚し、肉柱形成もみられます。")]).unwrap();
        assert_eq!(rendered.text, "精嚢に明らかな異常を認めません。\n膀胱壁は肥厚し、肉柱形成もみられます。\n病的なリンパ節腫大はみられません。");
        // 確認表で列挙の鍵を選び直したときも同じ規則で列挙否定になる。
        let mut edited = structure.clone();
        edited.lines[0] = LineInfo { i: 0, slot: Some("精嚢・膀胱".to_string()), ..LineInfo::default() };
        derive_list_lines(&template, &mut edited, &vocabulary);
        assert_eq!(edited.lines[0], structure.lines[0]);
        // 列挙の鍵なのに割れない行は警告。
        let mut broken = structure.clone();
        broken.lines[0] = LineInfo { i: 0, slot: Some("精嚢・膀胱".to_string()), ..LineInfo::default() };
        assert!(review(&template, &broken, &vocabulary).iter().any(|u| u.contains("本文 0 行目は列挙の鍵「精嚢・膀胱」だが")));
    }

    #[test]
    fn the_organ_in_a_sentence_ignores_shorter_names_inside_longer_ones() {
        let organs: Vec<String> = ["脾", "腎", "副腎"].iter().map(|o| o.to_string()).collect();
        assert_eq!(mentioned_organ("両腎嚢胞あり。", &organs).as_deref(), Some("腎"));
        assert_eq!(mentioned_organ("左副腎に結節を認めます。", &organs).as_deref(), Some("副腎"));
        assert_eq!(mentioned_organ("右腎と左副腎に結節。", &organs), None, "2 つ出れば決めない");
        assert_eq!(mentioned_organ("肝嚢胞あり。", &organs), None);
    }

    /// QA H1(b)(d): 目的の札は陰性の文だけ(病名と同じ札を異常の文に付けた答えは外して知らせる)。性別の決まった
    /// 項目を足す変種は、その性別を条件にする。
    #[test]
    fn purposes_stay_on_negative_sentences_and_sexed_items_get_their_sex() {
        let vocabulary = vocabulary();
        let template = Template {
            keywords: vec!["試験".to_string()],
            body: "脳の形態は年齢相応です。\n脾、腎、副腎に特記所見ありません。".to_string(),
            modules: vec![
                module("認知症", "萎縮を認めます。", "脳の形態の文と差し替える"),
                module("前立腺", "前立腺の腫大がみられます。", "脾・腎・副腎の文の後に足す"),
            ],
            extra: Default::default(),
        };
        let mut prostate = raw_variant(Value::Null, &["脾腎副腎"], "append", "abnormal");
        prostate["slot"] = json!("前立腺");
        let answer = json!({
            "lines": [
                {"i": 0, "kind": "sentence", "slot": "脳形態", "organs": []},
                {"i": 1, "kind": "list_negation", "slot": "脾腎副腎", "organs": ["脾", "腎", "副腎"]}
            ],
            "modules": [
                {"name": "認知症", "shape": "single", "graded": false, "sex": null, "age_band": null, "purpose": "認知症",
                 "variants": [raw_variant(Value::Null, &["脳形態"], "replace", "abnormal")]},
                {"name": "前立腺", "shape": "single", "graded": false, "sex": null, "age_band": null, "purpose": null,
                 "variants": [prostate]}
            ]
        });
        let structure = structure_from_answer(&template, &vocabulary, &answer, "").unwrap();
        assert_eq!(structure.variants[0].condition, None);
        assert!(structure.unresolved.iter().any(|u| u == "「認知症」0 行目: 陰性でない文に付いた目的の条件「認知症」を外した"));
        assert_eq!(structure.variants[1].condition.as_ref().and_then(|c| c.sex.as_deref()), Some("男"));
    }

    /// QA M2: 注が行を選ぶと言っていないのに、行ごとに別の項目を差し替える答えをまとめたら知らせる。注が
    /// 「言われた所見の文だけ」と言うなら束のまま。
    #[test]
    fn collapsing_lines_across_items_is_reported_and_sentence_bundles_stay_per_line() {
        let vocabulary = vocabulary();
        let template = |note: &str| Template {
            keywords: vec!["試験".to_string()],
            body: "胆嚢に異常なし。\n総胆管の拡張はありません。".to_string(),
            modules: vec![module("胆", "胆嚢結石あり。\n総胆管結石あり。", note)],
            extra: Default::default(),
        };
        let answer = json!({
            "lines": [
                {"i": 0, "kind": "sentence", "slot": "胆嚢", "organs": []},
                {"i": 1, "kind": "sentence", "slot": "総胆管", "organs": []}
            ],
            "modules": [{"name": "胆", "shape": "per_line", "graded": false, "sex": null, "age_band": null, "purpose": null,
                "variants": [raw_variant(json!(0), &["胆嚢"], "replace", "abnormal"), raw_variant(json!(1), &["総胆管"], "replace", "abnormal")]}]
        });
        let bundled = structure_from_answer(&template("言われた所見の文だけを差し替える"), &vocabulary, &answer, "").unwrap();
        assert_eq!(bundled.variants.len(), 2, "文だけ = 束");
        let collapsed = structure_from_answer(&template("胆の文と差し替える"), &vocabulary, &answer, "").unwrap();
        assert_eq!(collapsed.variants.len(), 1);
        assert_eq!(collapsed.variants[0].target, vec!["胆嚢".to_string(), "総胆管".to_string()]);
        assert!(collapsed.unresolved.iter().any(|u| u.contains("行ごとに別の項目(胆嚢・総胆管)を差し替える答えだった")));
    }

    /// QA M3: ありそうな注で誤検出しない(足す語・言い換え・丁寧体の括弧・「肝臓」)。「の文」と書かない参照と
    /// "…" の引用は見つける。
    #[test]
    fn note_checks_skip_added_words_and_catch_other_references() {
        let template = Template {
            keywords: vec!["試験".to_string()],
            body: "肝に占拠性病変を認めません。\n脾、腎、副腎に特記所見ありません。".to_string(),
            modules: vec![
                module("a", "脂肪肝あり。", "「その他、」を文頭に付ける。肝臓の文と差し替える(言われたら直します)"),
                module("b", "肝嚢胞あり。", "「肝嚢胞あり」と言い換える"),
                module("c", "壁肥厚あり。", "膀胱と差し替える。門脈の行を残す"),
                module("d", "肝腫瘤あり。", "\"肝に明らかな異常を認めず\"と差し替える"),
            ],
            extra: Default::default(),
        };
        let structure = TemplateStructure {
            lines: vec![
                LineInfo { i: 0, slot: Some("肝".to_string()), ..LineInfo::default() },
                LineInfo { i: 1, slot: Some("脾腎副腎".to_string()), ..LineInfo::default() },
            ],
            ..TemplateStructure::default()
        };
        assert_eq!(
            note_problems(&template, &structure),
            vec![
                "c: 注の「膀胱と差し替」の「膀胱」が本文に無い".to_string(),
                "c: 注の「門脈の行」の「門脈」が本文に無い".to_string(),
                "d: 注の引用「肝に明らかな異常を認めず」が本文に無い".to_string(),
            ]
        );
    }

    /// QA 再確認の低 4 件: 「…」を残して後に足す は引用として見る、括弧の「…あります」は指示文、全文経路に戻した
    /// 理由と「全体差し替えのまま」を重ねない、表で付けた目的の条件を知らせる。
    #[test]
    fn second_review_findings_stay_fixed() {
        let vocabulary = vocabulary();
        let template = Template {
            keywords: vec!["試験".to_string()],
            body: "両側の上顎洞に粘膜肥厚を認めません。\n中耳の含気は良好です。".to_string(),
            modules: vec![
                module("嚢胞", "上顎洞に貯留嚢胞を認めます。", "「副鼻腔に明らかな粘膜肥厚や液体貯留は認めません」を残して後に足す"),
                module("中耳炎", "中耳に液体貯留を認めます。", "中耳の文と差し替える(言われなければ省くことがあります)"),
            ],
            extra: Default::default(),
        };
        let structure = TemplateStructure {
            lines: vec![
                LineInfo { i: 0, slot: Some("副鼻腔".to_string()), ..LineInfo::default() },
                LineInfo { i: 1, slot: Some("中耳".to_string()), ..LineInfo::default() },
            ],
            ..TemplateStructure::default()
        };
        assert_eq!(
            note_problems(&template, &structure),
            vec!["嚢胞: 注の引用「副鼻腔に明らかな粘膜肥厚や液体貯留は認めません」が本文に無い".to_string()]
        );
        // 全文経路に戻した理由だけを出す(同じ事柄を 2 回言わない)。
        let mut answer = abdomen_answer();
        answer["modules"][2]["sex"] = Value::Null;
        answer["modules"][2]["age_band"] = Value::Null;
        for variant in answer["modules"][2]["variants"].as_array_mut().unwrap() {
            variant["auto"] = json!(false);
            variant["sex"] = Value::Null;
        }
        let whole = structure_from_answer(&abdomen(), &vocabulary, &answer, "").unwrap();
        assert_eq!(whole.unresolved.iter().filter(|u| u.starts_with("「若年女性」")).count(), 1, "{:?}", whole.unresolved);
        // 表で異常の変種に目的の条件を付けると知らせる。
        let mut edited = structure_from_answer(&abdomen(), &vocabulary, &abdomen_answer(), "").unwrap();
        let index = edited.variants.iter().position(|v| v.module == "肝" && v.line == Some(0)).unwrap();
        edited.variants[index].condition = Some(Condition { purpose: Some("腹痛".to_string()), ..Condition::default() });
        assert!(review(&abdomen(), &edited, &vocabulary)
            .iter()
            .any(|u| u == "「肝」0 行目: 目的の条件「腹痛」は陰性の文だけに付ける(所見の文は目的に依らず置く)"));
    }

    /// QA 低: 黙って捨てない(同じ行の 2 つ目の変種、本文に無い行番号、【】の無い見出し、名前の違う答えを順で読む)。
    #[test]
    fn silent_fallbacks_are_reported() {
        let vocabulary = vocabulary();
        let template = Template {
            keywords: vec!["試験".to_string()],
            body: "肝に異常なし。\n腎に異常なし。".to_string(),
            modules: vec![module("肝", "脂肪肝。\n肝嚢胞。", "言われた所見の行だけを肝の文と差し替える")],
            extra: Default::default(),
        };
        let answer = json!({
            "lines": [
                {"i": 0, "kind": "heading", "slot": null, "organs": []},
                {"i": 2, "kind": "sentence", "slot": "脾腎副腎", "organs": []}
            ],
            "modules": [{"name": "肝モジュール", "shape": "per_line", "graded": false, "sex": null, "age_band": null, "purpose": null,
                "variants": [raw_variant(json!(0), &["肝"], "replace", "abnormal"), raw_variant(json!(0), &["肝"], "prepend", "abnormal"),
                             raw_variant(json!(1), &["肝"], "replace", "benign")]}]
        });
        let structure = structure_from_answer(&template, &vocabulary, &answer, "").unwrap();
        let unresolved = structure.unresolved.join("\n");
        assert!(unresolved.contains("答えの行番号 2 は本文に無い"), "{unresolved}");
        assert!(unresolved.contains("本文 0 行目を見出しとして読んだ(【】で始まらない)"));
        assert!(unresolved.contains("答えの「肝モジュール」を、同じ順の「肝」の注釈として読んだ"));
        assert!(unresolved.contains("「肝」0 行目に変種が 2 個返ったので最初の物を使った"));
    }

    #[test]
    fn graded_groups_keep_roles_and_drop_intervals_equal_to_the_default() {
        let vocabulary = vocabulary();
        let template = Template {
            keywords: vec!["試験腰椎".to_string()],
            body: "椎体は保たれています。\n椎間板は保たれています。\n脊柱管に狭窄はありません。".to_string(),
            modules: vec![module(
                "変性",
                "椎体に骨棘を認めます。\n椎間板の膨隆を認めます。\n脊柱管が狭窄しています。\n慢性変化と考えます。",
                "正常所見全体をこの文に差し替える。軽度なら最後の文は使わない",
            )],
            extra: Default::default(),
        };
        let variant = |line: usize, target: &str, role: &str, grades: Value| {
            json!({"line": line, "target": [target], "mode": "replace", "polarity": "abnormal", "sex": null, "age_band": null, "purpose": null, "auto": false, "slot": null, "organ": null, "prefix": null, "role": role, "grades": grades, "list_organs": [], "organ_slots": []})
        };
        let answer = json!({
            "lines": [
                {"i": 0, "kind": "sentence", "slot": "椎体", "organs": []},
                {"i": 1, "kind": "sentence", "slot": "椎間板", "organs": []},
                {"i": 2, "kind": "sentence", "slot": "脊柱管・椎間孔", "organs": []}
            ],
            "modules": [{"name": "変性", "shape": "whole", "graded": true, "sex": null, "age_band": null, "purpose": null, "variants": [
                variant(0, "椎体", "初期", json!(["軽度", "中等度", "高度"])),
                variant(1, "椎間板", "初期", Value::Null),
                variant(2, "脊柱管・椎間孔", "波及", json!(["中等度", "高度"])),
                variant(3, "脊柱管・椎間孔", "帰結", json!(["中等度", "高度"]))
            ]}]
        });
        let structure = structure_from_answer(&template, &vocabulary, &answer, "").unwrap();
        assert_eq!(structure.groups, vec![Group { module: "変性".to_string(), scale: None }]);
        assert_eq!(structure.variants[0].grades, None, "役割の既定と同じ区間は持たない");
        assert_eq!(structure.variants[2].grades, None);
        assert_eq!(structure.variants[3].grades, Some(vec!["中等度".to_string(), "高度".to_string()]), "帰結は残す");
        assert!(structure.variants.last().unwrap().split);
        assert_eq!(check_structure(&template, &structure, &vocabulary), Vec::<String>::new());
    }

    #[test]
    fn old_notes_are_found_by_the_machine() {
        let template = Template {
            keywords: vec!["試験".to_string()],
            body: "脾、腎、副腎に特記所見ありません。\nT2WIで頭蓋内に異常信号を認めません。\n脳の形態は年齢相応です。".to_string(),
            modules: vec![
                module("腎", "両腎嚢胞あり。\n前立腺の腫大がみられます。", "脾・腎・副腎・膀胱の文のところに置く(前立腺の文は膀胱の文の後)"),
                module("白質", "白質に高信号を認めます。", "「頭蓋内に明らかな異常信号を認めず…」と差し替える"),
                module("萎縮", "萎縮を認めます。", "「脳の形態は年齢相応です」と差し替える(水頭症も言われたら両方の文を置く)"),
                module("信号", "高信号です。", "頭蓋内の文と差し替える。言われていない所は正常文の否定(明らかな浮腫は認められません)を残す"),
                module("言い換え", "萎縮が強い。", "「萎縮を認めます」を「萎縮が強く進んでいます」にする。続く文（脳の形態は・・・）は残す"),
            ],
            extra: Default::default(),
        };
        let structure = TemplateStructure {
            lines: vec![
                LineInfo { i: 0, kind: LineKind::ListNegation, slot: Some("脾腎副腎".to_string()), ..LineInfo::default() },
                LineInfo { i: 1, slot: Some("頭蓋内".to_string()), ..LineInfo::default() },
                LineInfo { i: 2, slot: Some("脳形態".to_string()), ..LineInfo::default() },
            ],
            ..TemplateStructure::default()
        };
        assert_eq!(
            note_problems(&template, &structure),
            vec![
                "腎: 注の「脾・腎・副腎・膀胱の文」の「膀胱」が本文に無い".to_string(),
                "白質: 注の引用「頭蓋内に明らかな異常信号を認めず…」が本文に無い".to_string(),
                "信号: 注の引用「明らかな浮腫は認められません」が本文に無い".to_string(),
            ]
        );
    }
}
