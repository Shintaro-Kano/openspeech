//! テンプレートの Markdown 形式(`docs/simplify-and-assistant-plan.md` §16.15)。読み(`parse`)と書き(`write`)。
//!
//! 源泉(キーワード・余分な項目・本文・モジュールの名前・文・使い方)は行単位で無損失に往復し、ハッシュ(§16.5)は
//! 変わらない。`> 条件:`・`> 尺度:`・行末の `《…》` は構造(派生)の注釈で、源泉の行には残さず `Annotations` に分けて
//! 返す(確認表の初期値になる)。前書きは YAML の部分集合を自前で読む: 1 行 1 項目 `鍵: 値`、`keywords` は
//! `[a, b]` か `- a` の列、`meta` のような 1 段の入れ子、`modules_extra` は 2 段(モジュール名 → 項目)。値はすべて
//! 文字列。改行や括弧で始まる値、さらに深い入れ子は JSON の書き方でそのまま持つ。
//!
//! 行頭の `#`・`>`・`-`・`\` は `\` で逃げる(読み込みで外す)。本文・文の末尾の空行は持たない。

use super::dictionary::{Module, Template};
use super::structure::{Condition, Group, Scale, TemplateStructure, Vocabulary};
use serde_json::{Map, Value};
use std::collections::BTreeMap;

pub const BODY_HEADING: &str = "# 正常所見";
pub const NOTE_LABEL: &str = "使い方:";
pub const CONDITION_LABEL: &str = "条件:";
pub const SCALE_LABEL: &str = "尺度:";
pub const ALL_GRADES: &str = "全程度";
pub const FROM_SUFFIX: &str = "以上";

/// 行末の `《…》` の中身。役割か区間、または両方。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LineTag {
    pub role: Option<String>,
    pub grades: Option<Vec<String>>,
}

impl LineTag {
    pub fn is_empty(&self) -> bool {
        self.role.is_none() && self.grades.is_none()
    }
}

/// 1 モジュールの注釈。`lines` は文の行と同じ長さ(無ければ空の札)。
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ModuleAnnotation {
    pub condition: Option<Condition>,
    pub scale: Option<Scale>,
    pub lines: Vec<LineTag>,
}

impl ModuleAnnotation {
    pub fn is_empty(&self) -> bool {
        self.condition.is_none() && self.scale.is_none() && self.lines.iter().all(LineTag::is_empty)
    }
}

/// Markdown の注釈(モジュール名 → 注釈)。源泉ではなく構造へ行く物。
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Annotations {
    pub modules: BTreeMap<String, ModuleAnnotation>,
}

/// 読んだ結果。
#[derive(Debug, Clone, PartialEq)]
pub struct Parsed {
    pub template: Template,
    pub annotations: Annotations,
}

// ------------------------------------------------------------------------------------------------
// 書き
// ------------------------------------------------------------------------------------------------

/// 文章を源泉の行に。末尾の空行だけ落とす(行の中身は触らない)。
fn source_lines(text: &str) -> Vec<&str> {
    let mut lines: Vec<&str> = text.split('\n').collect();
    while lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }
    lines
}

/// 行の逃げ: 行頭の `#`・`>`・`-`・`\` には `\` を前に、行末の `》`(札と読まれる)と `\` には `\` を後に付ける。
fn escape(line: &str) -> String {
    let mut out = String::new();
    if line.starts_with(['#', '>', '-', '\\']) {
        out.push('\\');
    }
    out.push_str(line);
    if line.ends_with(['》', '\\']) {
        out.push('\\');
    }
    out
}

/// 逃げを外す。戻り値の 2 つ目は「行末の札を読んでよいか」(行末に `\` があれば札ではない)。
fn unescape(line: &str) -> (&str, bool) {
    let line = line.strip_prefix('\\').unwrap_or(line);
    match line.strip_suffix('\\') {
        Some(inner) => (inner, false),
        None => (line, true),
    }
}

/// 使い方の行の逃げ: 先頭の `\` と、2 行目以降の `使い方:`・`条件:`・`尺度:` で始まる行に `\` を前に付ける。
fn escape_note(line: &str, first: bool) -> String {
    let needs = line.starts_with('\\')
        || (!first && (line.starts_with(NOTE_LABEL) || line.starts_with(CONDITION_LABEL) || line.starts_with(SCALE_LABEL)));
    if needs {
        format!("\\{line}")
    } else {
        line.to_string()
    }
}

/// 前書きの鍵。`: ` を含む・`- ` や `"`・`\` で始まる鍵は JSON の文字列で書く。
fn key_text(key: &str) -> String {
    if key.contains(": ") || key.ends_with(':') || key.starts_with(['-', '"', '\\', ' ']) || key.is_empty() {
        serde_json::to_string(key).unwrap_or_default()
    } else {
        key.to_string()
    }
}

/// 前書きの 1 行を鍵と値に割る。鍵が `"` で始まれば JSON の文字列として読む。
fn split_key_value(text: &str) -> Option<(String, Option<&str>)> {
    if text.starts_with('"') {
        let mut escaped = false;
        for (i, c) in text.char_indices().skip(1) {
            if escaped {
                escaped = false;
                continue;
            }
            match c {
                '\\' => escaped = true,
                '"' => {
                    let key: String = serde_json::from_str(&text[..=i]).ok()?;
                    let rest = text[i + 1..].strip_prefix(':')?;
                    return Some((key, rest.strip_prefix(' ').filter(|_| !rest.is_empty())));
                }
                _ => {}
            }
        }
        return None;
    }
    match text.split_once(": ") {
        Some((key, value)) => Some((key.trim().to_string(), Some(value))),
        None => text.strip_suffix(':').map(|key| (key.trim().to_string(), None)),
    }
}

/// 前書きの値。文字列はそのまま、ただし改行・前後の空白・括弧や引用符で始まる物は JSON の文字列で。
fn scalar_text(value: &Value) -> String {
    match value {
        Value::String(s) => {
            let plain = !s.is_empty()
                && !s.contains('\n')
                && !s.starts_with(['"', '[', '{', '\'', ' ', '\t'])
                && !s.ends_with([' ', '\t']);
            if plain {
                s.clone()
            } else {
                serde_json::to_string(s).unwrap_or_default()
            }
        }
        other => serde_json::to_string(other).unwrap_or_default(),
    }
}

fn is_flat_object(map: &Map<String, Value>) -> bool {
    !map.is_empty() && map.values().all(|v| !matches!(v, Value::Object(_) | Value::Array(_)))
}

fn write_entry(out: &mut String, key: &str, value: &Value, indent: usize) {
    let pad = " ".repeat(indent);
    let key = key_text(key);
    match value {
        Value::Object(map) if is_flat_object(map) => {
            out.push_str(&format!("{pad}{key}:\n"));
            for (k, v) in map {
                out.push_str(&format!("{pad}  {}: {}\n", key_text(k), scalar_text(v)));
            }
        }
        other => out.push_str(&format!("{pad}{key}: {}\n", scalar_text(other))),
    }
}

/// キーワードの並び。`,`・括弧・引用符を含む物があれば JSON の配列で。
fn keywords_text(keywords: &[String]) -> String {
    let plain = keywords
        .iter()
        .all(|k| !k.is_empty() && !k.contains([',', '、', '[', ']', '"', '\n']) && k.trim() == k);
    if plain {
        format!("[{}]", keywords.join(", "))
    } else {
        serde_json::to_string(keywords).unwrap_or_default()
    }
}

/// 尺度の `> 尺度:` の書き方。
pub fn scale_label(scale: &Scale) -> String {
    match scale.name() {
        Some(name) => format!("{name} {}", scale.levels().join("・")),
        None => scale.levels().join("・"),
    }
}

/// 区間の書き方: 1 段なら `軽度`、上端まで続くなら `中等度以上`、全段なら `全程度`、ほかは `軽度〜中等度`。
pub fn interval_label(grades: &[String], levels: &[String]) -> String {
    if grades.is_empty() {
        return String::new();
    }
    if grades.len() == levels.len() && grades.iter().zip(levels).all(|(g, l)| g == l) {
        return ALL_GRADES.to_string();
    }
    if grades.len() == 1 {
        return grades[0].clone();
    }
    if grades.last() == levels.last() {
        return format!("{}{FROM_SUFFIX}", grades[0]);
    }
    format!("{}〜{}", grades[0], grades[grades.len() - 1])
}

fn tag_label(tag: &LineTag, levels: &[String]) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(role) = &tag.role {
        parts.push(role.clone());
    }
    if let Some(grades) = &tag.grades {
        parts.push(interval_label(grades, levels));
    }
    parts.join("・")
}

/// テンプレートと注釈を Markdown に。
pub fn write(template: &Template, annotations: &Annotations, vocabulary: &Vocabulary) -> String {
    let mut out = String::new();
    out.push_str("---\n");
    out.push_str(&format!("keywords: {}\n", keywords_text(&template.keywords)));
    for (key, value) in &template.extra {
        write_entry(&mut out, key, value, 0);
    }
    let with_extra: Vec<&Module> = template.modules.iter().filter(|m| !m.extra.is_empty()).collect();
    if !with_extra.is_empty() {
        out.push_str("modules_extra:\n");
        for module in with_extra {
            out.push_str(&format!("  {}:\n", key_text(&module.name)));
            for (key, value) in &module.extra {
                write_entry(&mut out, key, value, 4);
            }
        }
    }
    out.push_str("---\n");
    out.push_str(BODY_HEADING);
    out.push('\n');
    for line in source_lines(&template.body) {
        out.push_str(&escape(line));
        out.push('\n');
    }
    for module in &template.modules {
        out.push('\n');
        out.push_str(&format!("## {}\n", module.name));
        let annotation = annotations.modules.get(&module.name);
        if let Some(condition) = annotation.and_then(|a| a.condition.as_ref()) {
            out.push_str(&format!("> {CONDITION_LABEL} {}\n", condition.label()));
        }
        let scale = annotation.and_then(|a| a.scale.as_ref());
        if let Some(scale) = scale {
            out.push_str(&format!("> {SCALE_LABEL} {}\n", scale_label(scale)));
        }
        let levels: Vec<String> = scale.map(|s| s.levels().to_vec()).unwrap_or_else(|| vocabulary.default_scale.clone());
        for (i, line) in source_lines(&module.text).iter().enumerate() {
            out.push_str(&escape(line));
            if let Some(tag) = annotation.and_then(|a| a.lines.get(i)) {
                let label = tag_label(tag, &levels);
                if !label.is_empty() {
                    out.push_str(&format!("《{label}》"));
                }
            }
            out.push('\n');
        }
        for (i, line) in source_lines(&module.note).iter().enumerate() {
            let line = escape_note(line, i == 0);
            if i == 0 {
                out.push_str(&format!("> {NOTE_LABEL} {line}\n"));
            } else {
                out.push_str(&format!("> {line}\n"));
            }
        }
    }
    out
}

// ------------------------------------------------------------------------------------------------
// 読み
// ------------------------------------------------------------------------------------------------

struct FrontLine {
    number: usize,
    indent: usize,
    text: String,
}

fn parse_scalar(text: &str) -> Value {
    let text = text.trim();
    if text.starts_with(['"', '{']) || (text.starts_with('[') && text.ends_with(']')) {
        if let Ok(value) = serde_json::from_str::<Value>(text) {
            return value;
        }
    }
    if text.starts_with('[') && text.ends_with(']') {
        let items: Vec<Value> = text[1..text.len() - 1]
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| Value::String(s.to_string()))
            .collect();
        return Value::Array(items);
    }
    Value::String(text.to_string())
}

/// `indent` の深さの項目の並びを読む。`- 項目` の列か `鍵: 値`(値が無ければ下の深い行が入れ子)。
fn parse_block(lines: &[FrontLine], mut index: usize, indent: usize) -> Result<(Value, usize), String> {
    let mut object: Map<String, Value> = Map::new();
    let mut list: Vec<Value> = Vec::new();
    let mut is_list = None;
    while index < lines.len() && lines[index].indent == indent {
        let line = &lines[index];
        if let Some(item) = line.text.strip_prefix("- ") {
            if is_list == Some(false) {
                return Err(format!("{} 行目: 項目の列と鍵が混ざっている", line.number));
            }
            is_list = Some(true);
            list.push(parse_scalar(item));
            index += 1;
            continue;
        }
        if is_list == Some(true) {
            return Err(format!("{} 行目: 項目の列と鍵が混ざっている", line.number));
        }
        is_list = Some(false);
        let Some((key, value)) = split_key_value(&line.text) else {
            return Err(format!("{} 行目: 「鍵: 値」の形ではない", line.number));
        };
        if key.is_empty() {
            return Err(format!("{} 行目: 鍵が無い", line.number));
        }
        index += 1;
        let value = match value {
            Some(value) => parse_scalar(value),
            None if index < lines.len() && lines[index].indent > indent => {
                let (nested, next) = parse_block(lines, index, lines[index].indent)?;
                index = next;
                nested
            }
            None => Value::String(String::new()),
        };
        object.insert(key, value);
    }
    if index < lines.len() && lines[index].indent > indent {
        return Err(format!("{} 行目: 字下げが深すぎる", lines[index].number));
    }
    Ok((if is_list == Some(true) { Value::Array(list) } else { Value::Object(object) }, index))
}

fn string_items(value: &Value) -> Vec<String> {
    match value {
        Value::Array(items) => items
            .iter()
            .map(|v| match v {
                Value::String(s) => s.trim().to_string(),
                other => other.to_string(),
            })
            .filter(|s| !s.is_empty())
            .collect(),
        Value::String(s) => s
            .split([',', '、'])
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect(),
        _ => Vec::new(),
    }
}

/// `> 条件:` の中身(「女・若年」「目的=めまい」)を条件に。
pub fn parse_condition(text: &str, vocabulary: &Vocabulary) -> Result<Condition, String> {
    let mut condition = Condition::default();
    for part in text.split('・').map(str::trim).filter(|p| !p.is_empty()) {
        if part == "男" || part == "女" {
            if condition.sex.is_some() {
                return Err(format!("条件「{text}」に性別が二つ"));
            }
            condition.sex = Some(part.to_string());
        } else if vocabulary.age_bands.contains_key(part) {
            if condition.age_band.is_some() {
                return Err(format!("条件「{text}」に年齢帯が二つ"));
            }
            condition.age_band = Some(part.to_string());
        } else {
            let purpose = part.strip_prefix("目的=").unwrap_or(part);
            if !vocabulary.purposes.iter().any(|p| p == purpose) {
                return Err(format!("条件「{text}」の「{part}」は性別・年齢帯・目的の札のどれでもない"));
            }
            if condition.purpose.is_some() {
                return Err(format!("条件「{text}」に目的が二つ"));
            }
            condition.purpose = Some(purpose.to_string());
        }
    }
    if condition.is_empty() {
        return Err("条件が空".to_string());
    }
    Ok(condition)
}

/// `> 尺度:` の中身(「軽度・中等度・高度」「Fazekas 1・2・3」)を尺度に。
pub fn parse_scale(text: &str) -> Result<Scale, String> {
    let tokens: Vec<&str> = text.split_whitespace().collect();
    let (name, levels) = match tokens.as_slice() {
        [levels] => (None, *levels),
        [name, levels] => (Some(name.to_string()), *levels),
        _ => return Err(format!("尺度「{text}」は「段・段・段」か「名前 段・段・段」")),
    };
    let levels: Vec<String> = levels.split('・').map(str::trim).filter(|s| !s.is_empty()).map(str::to_string).collect();
    if levels.len() < 2 {
        return Err(format!("尺度「{text}」の段が 2 つに満たない"));
    }
    Ok(match name {
        Some(name) => Scale::Named { name, levels },
        None => Scale::Levels(levels),
    })
}

/// 区間の読み: `全程度`・`軽度`・`中等度以上`・`軽度〜中等度`(`～`・`~` も可)。
pub fn parse_interval(text: &str, levels: &[String]) -> Result<Vec<String>, String> {
    let position = |level: &str| {
        levels
            .iter()
            .position(|l| l == level)
            .ok_or_else(|| format!("「{level}」は尺度({})に無い", levels.join("・")))
    };
    if text == ALL_GRADES {
        return Ok(levels.to_vec());
    }
    if let Some(from) = text.strip_suffix(FROM_SUFFIX) {
        let start = position(from)?;
        return Ok(levels[start..].to_vec());
    }
    if let Some((from, to)) = text.split_once(['〜', '～', '~']) {
        let start = position(from.trim())?;
        let end = position(to.trim())?;
        if end < start {
            return Err(format!("区間「{text}」の向きが逆"));
        }
        return Ok(levels[start..=end].to_vec());
    }
    Ok(vec![levels[position(text)?].clone()])
}

/// `《…》` の中身を札に。役割と区間は `・` で繋ぐ。
pub fn parse_tag(text: &str, levels: &[String], vocabulary: &Vocabulary) -> Result<LineTag, String> {
    let mut tag = LineTag::default();
    for part in text.split('・').map(str::trim).filter(|p| !p.is_empty()) {
        if vocabulary.role(part).is_some() {
            if tag.role.is_some() {
                return Err(format!("《{text}》に役割が二つ"));
            }
            tag.role = Some(part.to_string());
        } else {
            if tag.grades.is_some() {
                return Err(format!("《{text}》に区間が二つ"));
            }
            tag.grades = Some(parse_interval(part, levels)?);
        }
    }
    if tag.is_empty() {
        return Err("《》が空".to_string());
    }
    Ok(tag)
}

/// 行末の `《…》` を外す。無ければ札は None。
fn split_tag(line: &str) -> (&str, Option<&str>) {
    if let Some(body) = line.strip_suffix('》') {
        if let Some(open) = body.rfind('《') {
            let tag = &body[open + '《'.len_utf8()..];
            return (&line[..open], Some(tag));
        }
    }
    (line, None)
}

struct RawModule {
    number: usize,
    name: String,
    text: Vec<String>,
    tags: Vec<Option<(usize, String)>>,
    note: Vec<String>,
    condition: Option<(usize, String)>,
    scale: Option<(usize, String)>,
}

/// Markdown を読む。誤りは行番号付きの理由。
pub fn parse(text: &str, vocabulary: &Vocabulary) -> Result<Parsed, String> {
    let lines: Vec<&str> = text.split('\n').map(|l| l.strip_suffix('\r').unwrap_or(l)).collect();
    let mut index = 0;
    while index < lines.len() && lines[index].trim().is_empty() {
        index += 1;
    }
    if lines.get(index).map(|l| l.trim_end()) != Some("---") {
        return Err("先頭に前書き(--- で囲んだ keywords)が無い".to_string());
    }
    index += 1;
    let mut front: Vec<FrontLine> = Vec::new();
    loop {
        let Some(line) = lines.get(index) else {
            return Err("前書きの終わりの --- が無い".to_string());
        };
        if line.trim_end() == "---" {
            index += 1;
            break;
        }
        if !line.trim().is_empty() {
            let indent = line.len() - line.trim_start_matches(' ').len();
            front.push(FrontLine {
                number: index + 1,
                indent,
                text: line.trim_start_matches(' ').trim_end().to_string(),
            });
        }
        index += 1;
    }
    let (front_value, _) = parse_block(&front, 0, 0)?;
    let Value::Object(mut front_map) = front_value else {
        return Err("前書きは「鍵: 値」の並び".to_string());
    };
    let keywords = string_items(&front_map.remove("keywords").unwrap_or(Value::Null));
    if keywords.is_empty() {
        return Err("前書きに keywords が無い".to_string());
    }
    let modules_extra = front_map.remove("modules_extra");
    let extra: BTreeMap<String, Value> = front_map.into_iter().collect();

    while index < lines.len() && lines[index].trim().is_empty() {
        index += 1;
    }
    if lines.get(index).map(|l| l.trim_end()) != Some(BODY_HEADING) {
        return Err(format!("{} 行目: 「{BODY_HEADING}」が無い", index + 1));
    }
    index += 1;
    let mut body: Vec<String> = Vec::new();
    while index < lines.len() && !lines[index].starts_with("## ") {
        let line = lines[index];
        if line.starts_with('#') {
            return Err(format!("{} 行目: 本文に見出しがある(行頭の # は \\ で逃がす)", index + 1));
        }
        if !(body.is_empty() && line.trim().is_empty()) {
            body.push(unescape(line).0.to_string());
        }
        index += 1;
    }
    while body.last().is_some_and(|l| l.trim().is_empty()) {
        body.pop();
    }

    let mut raw_modules: Vec<RawModule> = Vec::new();
    while index < lines.len() {
        let heading = lines[index];
        let name = heading.strip_prefix("## ").map(str::trim).unwrap_or_default();
        if name.is_empty() {
            return Err(format!("{} 行目: モジュール名が無い", index + 1));
        }
        let mut module = RawModule {
            number: index + 1,
            name: name.to_string(),
            text: Vec::new(),
            tags: Vec::new(),
            note: Vec::new(),
            condition: None,
            scale: None,
        };
        index += 1;
        let mut in_note = false;
        while index < lines.len() && !lines[index].starts_with("## ") {
            let number = index + 1;
            let line = lines[index];
            index += 1;
            if let Some(quote) = line.strip_prefix('>') {
                let content = quote.strip_prefix(' ').unwrap_or(quote);
                if let Some(escaped) = content.strip_prefix('\\') {
                    // 逃がした使い方の続きの行(`条件:` などで始まる行、`\` で始まる行)。
                    if !in_note {
                        return Err(format!("{number} 行目: 使い方の前に逃がした行がある"));
                    }
                    module.note.push(escaped.trim_end().to_string());
                } else if let Some(note) = content.strip_prefix(NOTE_LABEL) {
                    let note = note.strip_prefix(' ').unwrap_or(note);
                    let note = note.strip_prefix('\\').unwrap_or(note);
                    module.note.push(note.trim_end().to_string());
                    in_note = true;
                } else if let Some(condition) = content.strip_prefix(CONDITION_LABEL) {
                    module.condition = Some((number, condition.trim().to_string()));
                } else if let Some(scale) = content.strip_prefix(SCALE_LABEL) {
                    module.scale = Some((number, scale.trim().to_string()));
                } else if in_note {
                    module.note.push(content.trim_end().to_string());
                } else {
                    return Err(format!(
                        "{number} 行目: 「> 」の行は 使い方・条件・尺度 のどれかで始める(文の行頭の > は \\ で逃がす)"
                    ));
                }
                continue;
            }
            if line.trim().is_empty() {
                if !in_note && !module.text.is_empty() {
                    module.text.push(String::new());
                    module.tags.push(None);
                }
                continue;
            }
            if in_note {
                return Err(format!("{number} 行目: 使い方の後に文がある(文は使い方の前に)"));
            }
            if line.starts_with('#') {
                return Err(format!("{number} 行目: 文に見出しがある(行頭の # は \\ で逃がす)"));
            }
            let (unescaped, tag_allowed) = unescape(line);
            let (text, tag) = if tag_allowed { split_tag(unescaped) } else { (unescaped, None) };
            module.text.push(text.to_string());
            module.tags.push(tag.map(|t| (number, t.to_string())));
        }
        while module.text.last().is_some_and(|l| l.trim().is_empty()) {
            module.text.pop();
            module.tags.pop();
        }
        while module.note.last().is_some_and(|l| l.trim().is_empty()) {
            module.note.pop();
        }
        raw_modules.push(module);
    }

    let mut modules: Vec<Module> = Vec::new();
    let mut annotations = Annotations::default();
    for raw in raw_modules {
        if modules.iter().any(|m| m.name == raw.name) {
            return Err(format!("{} 行目: モジュール「{}」が二つある", raw.number, raw.name));
        }
        let mut annotation = ModuleAnnotation::default();
        if let Some((number, text)) = &raw.condition {
            annotation.condition = Some(parse_condition(text, vocabulary).map_err(|e| format!("{number} 行目: {e}"))?);
        }
        if let Some((number, text)) = &raw.scale {
            annotation.scale = Some(parse_scale(text).map_err(|e| format!("{number} 行目: {e}"))?);
        }
        let levels: Vec<String> = annotation
            .scale
            .as_ref()
            .map(|s| s.levels().to_vec())
            .unwrap_or_else(|| vocabulary.default_scale.clone());
        for tag in &raw.tags {
            annotation.lines.push(match tag {
                Some((number, text)) => parse_tag(text, &levels, vocabulary).map_err(|e| format!("{number} 行目: {e}"))?,
                None => LineTag::default(),
            });
        }
        if !annotation.is_empty() {
            annotations.modules.insert(raw.name.clone(), annotation);
        }
        modules.push(Module {
            name: raw.name,
            text: raw.text.join("\n"),
            note: raw.note.join("\n"),
            extra: BTreeMap::new(),
        });
    }
    if let Some(Value::Object(per_module)) = modules_extra {
        for (name, extra) in per_module {
            let Some(module) = modules.iter_mut().find(|m| m.name == name) else {
                return Err(format!("前書きの modules_extra の「{name}」に当たるモジュールが無い"));
            };
            match extra {
                Value::Object(map) => module.extra = map.into_iter().collect(),
                other => return Err(format!("modules_extra の「{name}」は「鍵: 値」の並び(今は {other})")),
            }
        }
    } else if let Some(other) = modules_extra {
        return Err(format!("前書きの modules_extra は「モジュール名: 項目」の入れ子(今は {other})"));
    }
    Ok(Parsed {
        template: Template {
            keywords,
            body: body.join("\n"),
            modules,
            extra,
        },
        annotations,
    })
}

// ------------------------------------------------------------------------------------------------
// 注釈と構造の行き来
// ------------------------------------------------------------------------------------------------

impl Annotations {
    /// 構造から注釈を起こす(書き出し用)。条件はモジュールの変種がみな同じ条件のときだけ(`> 条件:` は
    /// モジュール単位)。所見群は尺度を必ず書く。名前付きの尺度で区間の無い行は構造の検査(`check_structure`)が
    /// 誤りにするので、ここでは区間をそのまま写す。
    pub fn from_structure(template: &Template, structure: &TemplateStructure, vocabulary: &Vocabulary) -> Self {
        let mut annotations = Annotations::default();
        for module in &template.modules {
            let members: Vec<_> = structure
                .variants
                .iter()
                .filter(|v| v.module == module.name && !v.is_whole() && !v.derived)
                .collect();
            let mut annotation = ModuleAnnotation::default();
            if let Some(first) = members.first() {
                if first.condition.is_some() && members.iter().all(|v| v.condition == first.condition) {
                    annotation.condition = first.condition.clone();
                }
            }
            let group = structure.groups.iter().find(|g| g.module == module.name);
            if let Some(group) = group {
                annotation.scale = Some(
                    group
                        .scale
                        .clone()
                        .unwrap_or_else(|| Scale::Levels(vocabulary.default_scale.clone())),
                );
            }
            let line_count = source_lines(&module.text).len();
            for i in 0..line_count {
                let tag = members
                    .iter()
                    .find(|v| v.line == Some(i))
                    .map(|v| LineTag {
                        role: v.role.clone(),
                        grades: v.grades.clone(),
                    })
                    .unwrap_or_default();
                annotation.lines.push(tag);
            }
            if !annotation.is_empty() {
                annotations.modules.insert(module.name.clone(), annotation);
            }
        }
        annotations
    }

    /// 注釈を構造に入れる(読み込み用。確認表の初期値)。モジュール単位の条件はそのモジュールの変種すべてに、
    /// 尺度は所見群に(既定の尺度なら `scale` は持たない)、札は同じ行の変種の `role`・`grades` に。
    pub fn apply(&self, structure: &mut TemplateStructure, vocabulary: &Vocabulary) {
        for (name, annotation) in &self.modules {
            if let Some(condition) = &annotation.condition {
                for variant in structure.variants.iter_mut().filter(|v| &v.module == name && !v.is_whole() && !v.derived) {
                    variant.condition = Some(condition.clone());
                }
            }
            let has_tags = annotation.lines.iter().any(|t| !t.is_empty());
            if annotation.scale.is_some() || has_tags {
                let scale = annotation.scale.clone().filter(|s| s.levels() != vocabulary.default_scale.as_slice() || s.name().is_some());
                match structure.groups.iter_mut().find(|g| &g.module == name) {
                    Some(group) => group.scale = scale,
                    None => structure.groups.push(Group {
                        module: name.clone(),
                        scale,
                    }),
                }
            }
            if has_tags || annotation.scale.is_some() {
                for (i, tag) in annotation.lines.iter().enumerate() {
                    for variant in structure
                        .variants
                        .iter_mut()
                        .filter(|v| &v.module == name && v.line == Some(i) && !v.is_whole())
                    {
                        variant.role = tag.role.clone();
                        variant.grades = tag.grades.clone();
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vocabulary() -> Vocabulary {
        Vocabulary::builtin()
    }

    #[test]
    fn the_example_from_the_plan_parses_with_annotations() {
        let text = "---\nkeywords: [腰椎MRI, 腰椎]\nmeta:\n  modality: MRI\nmodules_extra:\n  変形性腰椎症:\n    diagnosis: 変形性腰椎症。\n---\n# 正常所見\n腰仙椎は概ね保たれてみられます。\n\n## 変形性腰椎症\n> 尺度: 軽度・中等度・高度\n腰椎の変形や骨棘形成を認めます。\n黄色靭帯の肥厚や、椎間関節の過形成を認めます。《進行》\nこれらにより、〔　〕にかけて脊柱管狭窄がみられます。《波及・高度》\n脊柱管は〔　〕レベルで最も強く狭窄しています。《中等度以上》\n> 使い方: レベルは言われた通り。\n> 二行目の使い方\n\n## 若い女性\n> 条件: 女・若年\n生理的な少量の腹水を認めます。\n";
        let parsed = parse(text, &vocabulary()).unwrap();
        let t = &parsed.template;
        assert_eq!(t.keywords, ["腰椎MRI", "腰椎"]);
        assert_eq!(t.body, "腰仙椎は概ね保たれてみられます。");
        assert_eq!(t.extra["meta"]["modality"], "MRI");
        assert_eq!(t.modules.len(), 2);
        assert_eq!(t.modules[0].name, "変形性腰椎症");
        assert_eq!(
            t.modules[0].text,
            "腰椎の変形や骨棘形成を認めます。\n黄色靭帯の肥厚や、椎間関節の過形成を認めます。\nこれらにより、〔　〕にかけて脊柱管狭窄がみられます。\n脊柱管は〔　〕レベルで最も強く狭窄しています。"
        );
        assert_eq!(t.modules[0].note, "レベルは言われた通り。\n二行目の使い方");
        assert_eq!(t.modules[0].extra["diagnosis"], "変形性腰椎症。");
        let a = &parsed.annotations.modules["変形性腰椎症"];
        assert_eq!(a.scale, Some(Scale::Levels(vec!["軽度".into(), "中等度".into(), "高度".into()])));
        assert_eq!(a.lines[0], LineTag::default());
        assert_eq!(a.lines[1].role.as_deref(), Some("進行"));
        assert_eq!(a.lines[1].grades, None);
        assert_eq!(a.lines[2].role.as_deref(), Some("波及"));
        assert_eq!(a.lines[2].grades, Some(vec!["高度".to_string()]));
        assert_eq!(a.lines[3].role, None);
        assert_eq!(a.lines[3].grades, Some(vec!["中等度".to_string(), "高度".to_string()]));
        let w = &parsed.annotations.modules["若い女性"];
        assert_eq!(w.condition.as_ref().unwrap().label(), "女・若年");
        assert!(w.scale.is_none());
    }

    #[test]
    fn writing_then_reading_keeps_the_source_and_the_annotations() {
        let template: Template = serde_json::from_str(
            r##"{"keywords": ["頭部CT", "脳CT"], "body": ["# で始まる行", "> で始まる行", "- 箇条書き", "\\ で始まる行", "", "空行のあとの行"],
                "modules": [{"name": "白質病変", "text": "大脳白質に低濃度域。", "note": "頭蓋内の文の前に足す"}, {"name": "空の使い方", "text": ["一行目", "二行目"]}],
                "meta": {"id": "head-ct", "updated": "2026-11-09"}, "memo": "値に: コロン"}"##,
        )
        .unwrap();
        let vocabulary = vocabulary();
        let markdown = write(&template, &Annotations::default(), &vocabulary);
        assert!(markdown.contains("\\# で始まる行\n\\> で始まる行\n\\- 箇条書き\n\\\\ で始まる行\n\n空行のあとの行\n"));
        let parsed = parse(&markdown, &vocabulary).unwrap();
        assert_eq!(parsed.template, template);
        assert!(parsed.annotations.modules.is_empty());
        // 行末の 》 と \、使い方の続きの行の 条件:/尺度:/使い方:、先頭の空白と \、`: ` を含む鍵とモジュール名。
        let tricky: Template = serde_json::from_str(
            r##"{"keywords": ["x, y", "z"], "body": ["《参考》で終わる文《初期》", "バックスラッシュで終わる\\", "\\両端\\"],
                "modules": [{"name": "名: 前", "text": ["文《全程度》", "普通の文"], "note": "\\先頭\n条件: 続き\n尺度: 続き\n使い方: 続き\n 空白で始まる", "k: v": "1"}],
                "a: b": "c", "- d": "e"}"##,
        )
        .unwrap();
        let markdown = write(&tricky, &Annotations::default(), &vocabulary);
        let parsed = parse(&markdown, &vocabulary).unwrap_or_else(|e| panic!("{e}\n{markdown}"));
        assert_eq!(parsed.template, tricky, "{markdown}");
        assert!(parsed.annotations.modules.is_empty(), "{markdown}");
        assert_eq!(
            super::super::structure::source_hash(&parsed.template),
            super::super::structure::source_hash(&tricky)
        );
    }

    #[test]
    fn front_matter_values_that_are_not_plain_strings_survive() {
        let template: Template = serde_json::from_str(
            r#"{"keywords": ["x"], "body": "本文", "memo": "二行\nある", "count": "3", "tags": ["a", "b"], "nested": {"deep": {"k": "v"}}, "empty": ""}"#,
        )
        .unwrap();
        let vocabulary = vocabulary();
        let parsed = parse(&write(&template, &Annotations::default(), &vocabulary), &vocabulary).unwrap();
        assert_eq!(parsed.template, template);
    }

    #[test]
    fn intervals_are_written_in_the_short_forms_and_read_back() {
        let levels: Vec<String> = ["軽度", "中等度", "高度"].iter().map(|s| s.to_string()).collect();
        let cases: [(&[&str], &str); 5] = [
            (&["軽度"], "軽度"),
            (&["中等度", "高度"], "中等度以上"),
            (&["軽度", "中等度"], "軽度〜中等度"),
            (&["軽度", "中等度", "高度"], "全程度"),
            (&["高度"], "高度"),
        ];
        for (grades, label) in cases {
            let grades: Vec<String> = grades.iter().map(|s| s.to_string()).collect();
            assert_eq!(interval_label(&grades, &levels), label);
            assert_eq!(parse_interval(label, &levels).unwrap(), grades);
        }
        let named: Vec<String> = ["1", "2", "3"].iter().map(|s| s.to_string()).collect();
        assert_eq!(parse_interval("2以上", &named).unwrap(), ["2", "3"]);
        assert_eq!(parse_interval("1～2", &named).unwrap(), ["1", "2"]);
        assert!(parse_interval("4", &named).is_err());
        assert!(parse_interval("高度〜軽度", &levels).is_err());
    }

    #[test]
    fn bad_tags_and_quotes_are_errors_with_line_numbers() {
        let vocabulary = vocabulary();
        let base = "---\nkeywords: [x]\n---\n# 正常所見\n本文\n\n## m\n";
        let err = parse(&format!("{base}文《強度》\n"), &vocabulary).unwrap_err();
        assert!(err.starts_with("8 行目"), "{err}");
        let err = parse(&format!("{base}> 条件: 女・中年\n文\n"), &vocabulary).unwrap_err();
        assert!(err.contains("中年"), "{err}");
        let err = parse(&format!("{base}> ただの引用\n文\n"), &vocabulary).unwrap_err();
        assert!(err.contains("使い方・条件・尺度"), "{err}");
        let err = parse(&format!("{base}> 使い方: 先\n文\n"), &vocabulary).unwrap_err();
        assert!(err.contains("使い方の後"), "{err}");
        assert!(parse("---\nmemo: x\n---\n# 正常所見\n本文\n", &vocabulary).unwrap_err().contains("keywords"));
        assert!(parse("# 正常所見\n本文\n", &vocabulary).unwrap_err().contains("前書き"));
        // 名前付きの尺度では段の名前がそのまま区間になる。
        let parsed = parse(&format!("{base}> 尺度: Fazekas 1・2・3\n文《2以上》\n"), &vocabulary).unwrap();
        let a = &parsed.annotations.modules["m"];
        assert_eq!(a.scale, Some(Scale::Named { name: "Fazekas".into(), levels: vec!["1".into(), "2".into(), "3".into()] }));
        assert_eq!(a.lines[0].grades, Some(vec!["2".to_string(), "3".to_string()]));
    }
}
