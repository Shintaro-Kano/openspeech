//! 認識結果を整えるフィルター群。
//! 順序: 数値→不要語→表記(単語)→英単語の間隔→全角半角。

use super::dictionary::Dictionary;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum NumeralStyle {
    /// 1万2300
    Mixed,
    /// 12300
    Plain,
    /// 12,300
    Comma,
}

impl NumeralStyle {
    pub const ALL: [NumeralStyle; 3] = [Self::Mixed, Self::Plain, Self::Comma];
    pub fn label(self) -> &'static str {
        match self {
            Self::Mixed => "1万2300",
            Self::Plain => "12300",
            Self::Comma => "12,300",
        }
    }
}

/// 日付の書式(選べる 6 種類)。
pub const DATE_FORMATS: [(&str, &str); 6] = [
    ("g5y1年m1月d1日", "令和1年2月3日 , 2月3日"),
    ("g1y1/m1/d1", "R1/2/3 , 2/3"),
    ("g5y1/m1/d1", "令和1/2/3 , 2/3"),
    ("g5y3年m3月d3日", "令和１年２月３日 , ２月３日"),
    ("g3y3／m3／d3", "Ｒ１／２／３ , ２／３"),
    ("g5y3／m3／d3", "令和１／２／３ , ２／３"),
];

/// 時刻の書式(選べる 6 種類)。
pub const TIME_FORMATS: [(&str, &str); 6] = [
    ("h1時m1分", "13時57分"),
    ("h6:m6", "13:57(9時台は 09:57)"),
    ("h1:m6", "13:57(9時台は 9:57)"),
    ("h3時m3分", "１３時５７分"),
    ("h7：m7", "１３：５７(9時台は ０９：５７)"),
    ("h3：m7", "１３：５７(9時台は ９：５７)"),
];

/// 句読点の種類。(読点, 句点)。
pub const PUNCTUATION_KINDS: [(&str, &str); 3] = [("、", "。"), ("，", "。"), ("，", "．")];

/// 英単語の表現(選べる 2 種類)。
pub const ENGLISH_STYLES: [&str; 2] = [
    "英単語と日本語の間に space を挿入します",
    "英単語と日本語の間にspaceを入れません",
];

pub const WIDTH_STYLES: [&str; 3] = [
    "変換しない",
    "転送時に文字を全角に変換する",
    "転送時に文字を半角に変換する",
];

/// 後ろに来たらカンマや万を入れない語(日付・時刻の単位を含む)。
const NO_COMMA_TOKENS: &[&str] = &["丁目", "番地", "年", "月", "日", "時", "分"];

/// 半角と全角の記号対応。
const SYMBOL_PAIRS: &[(char, char)] = &[
    ('.', '．'),
    (',', '，'),
    ('+', '＋'),
    ('-', '－'),
    ('=', '＝'),
    ('%', '％'),
    ('&', '＆'),
    ('(', '（'),
    (')', '）'),
    ('~', '～'),
    ('<', '＜'),
    ('>', '＞'),
    (';', '；'),
    ('?', '？'),
    ('!', '！'),
    ('/', '／'),
    ('\\', '￥'),
    ('$', '＄'),
    ('#', '＃'),
    ('@', '＠'),
    ('*', '＊'),
    ('"', '”'),
    ('\'', '’'),
    ('[', '［'),
    (']', '］'),
    (':', '：'),
    (' ', '　'),
];

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct FilterSettings {
    pub numeral: NumeralStyle,
    pub date_format: usize,
    pub time_format: usize,
    pub remove_interjections: bool,
    pub auto_punctuation: bool,
    pub punctuation_kind: usize,
    pub english_style: usize,
    pub width_style: usize,
}

impl Default for FilterSettings {
    fn default() -> Self {
        FilterSettings {
            numeral: NumeralStyle::Plain,
            date_format: 0,
            time_format: 0,
            remove_interjections: true,
            auto_punctuation: true,
            punctuation_kind: 0,
            english_style: 0,
            width_style: 0,
        }
    }
}

/// 認識結果に順に掛けるフィルター。音声入力の途中で使う。
pub fn apply_prefilters(text: &str, settings: &FilterSettings, dictionary: &Dictionary) -> String {
    let text = normalize_numbers(text, settings);
    let text = remove_interjections(&text, settings);
    let text = apply_notation(&text, dictionary);
    apply_english_spacing(&text, settings.english_style)
}

/// 転送時だけ掛けるフィルター。
pub fn apply_transfer_filters(text: &str, settings: &FilterSettings) -> String {
    match settings.width_style {
        1 => to_full_width(text),
        2 => to_half_width(text),
        _ => text.to_string(),
    }
}

// ---- 不要語と句読点 ----

fn is_break(c: Option<char>) -> bool {
    c.is_none_or(|c| c.is_whitespace() || "、。，．".contains(c))
}

/// 不要語を取り除く。語の途中を削らないよう、文や句の切れ目にあるものだけを対象にする。
fn strip_fillers(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    'outer: while i < chars.len() {
        for word in ["えーと", "えー"] {
            let pattern: Vec<char> = word.chars().collect();
            let end = i + pattern.len();
            if chars.get(i..end) != Some(&pattern[..]) {
                continue;
            }
            let before = out.chars().last();
            let after = chars.get(end).copied();
            if is_break(before) || matches!(after, Some('、' | '，')) {
                i = end;
                if matches!(chars.get(i), Some('、' | '，')) {
                    i += 1;
                }
                continue 'outer;
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

/// 句読点を取り除く。数字の間の読点(L3、4 など)は意味が変わるので残す。
fn strip_punctuation_marks(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let digit = |index: Option<usize>| {
        index
            .and_then(|i| chars.get(i))
            .is_some_and(char::is_ascii_digit)
    };
    chars
        .iter()
        .enumerate()
        .filter(|(i, c)| match c {
            '。' => false,
            '、' => digit(i.checked_sub(1)) && digit(Some(i + 1)),
            _ => true,
        })
        .map(|(_, c)| *c)
        .collect()
}

fn remove_interjections(text: &str, settings: &FilterSettings) -> String {
    let out = if settings.remove_interjections {
        strip_fillers(text)
    } else {
        text.to_string()
    };
    if settings.auto_punctuation {
        let (comma, period) = PUNCTUATION_KINDS[settings.punctuation_kind.min(2)];
        out.replace('、', comma).replace('。', period)
    } else {
        strip_punctuation_marks(&out)
    }
}

// ---- 表記(ユーザー辞書と無効単語) ----

/// 発話全体が登録単語の読みなら表記に直す。文の途中は別の語の一部かもしれないので触らない。
fn apply_notation(text: &str, dictionary: &Dictionary) -> String {
    let spoken = text.trim_matches(|c: char| c.is_whitespace() || "、。".contains(c));
    let normalized = normalize_reading(spoken);
    dictionary
        .words
        .iter()
        .find(|word| {
            word.reading.chars().count() >= 3 && normalize_reading(&word.reading) == normalized
        })
        .map_or_else(|| text.to_string(), |word| word.word.clone())
}

/// 無効単語の位置(文字単位)。認識結果を語の単位で除けないため、
/// 語の一部を誤って消さないよう、消さずに印を付けて利用者に判断してもらう。
pub fn disabled_ranges(text: &str, dictionary: &Dictionary) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    for word in dictionary.disabled.iter().filter(|w| !w.is_empty()) {
        for (byte, _) in text.match_indices(word.as_str()) {
            let start = text[..byte].chars().count();
            ranges.push((start, start + word.chars().count()));
        }
    }
    ranges
}

// ---- 英単語の間隔 ----

#[derive(Debug, Clone, Copy, PartialEq)]
enum CharKind {
    Alpha,
    Kanji,
    Numeric,
    Symbol,
}

fn char_kind(c: char) -> CharKind {
    if c.is_ascii_alphabetic() || ('Ａ'..='Ｚ').contains(&c) || ('ａ'..='ｚ').contains(&c) {
        CharKind::Alpha
    } else if c.is_ascii_digit() || ('０'..='９').contains(&c) {
        CharKind::Numeric
    } else if ('ぁ'..='ゖ').contains(&c)
        || ('ァ'..='ヶ').contains(&c)
        || ('一'..='龠').contains(&c)
        || c == 'ー'
        || c == '々'
    {
        CharKind::Kanji
    } else {
        CharKind::Symbol
    }
}

/// 日本語と英字の境目の間隔だけを設定に合わせて揃える。それ以外の空白は変えない
/// (「血圧 120 80」の数字の間などは意味を持つ)。
fn apply_english_spacing(text: &str, style: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    for (i, &c) in chars.iter().enumerate() {
        if c == ' ' {
            let previous = out.chars().last();
            let next = chars[i + 1..].iter().find(|n| **n != ' ').copied();
            let at_boundary = matches!(
                (previous.map(char_kind), next.map(char_kind)),
                (Some(CharKind::Kanji), Some(CharKind::Alpha))
                    | (Some(CharKind::Alpha), Some(CharKind::Kanji))
            );
            if !at_boundary {
                out.push(' ');
            }
            continue;
        }
        if style == 0 {
            if let Some(previous) = out.chars().last() {
                let boundary = matches!(
                    (char_kind(previous), char_kind(c)),
                    (CharKind::Kanji, CharKind::Alpha) | (CharKind::Alpha, CharKind::Kanji)
                );
                if boundary {
                    out.push(' ');
                }
            }
        }
        out.push(c);
    }
    out
}

// ---- 全角と半角 ----

pub fn to_full_width(text: &str) -> String {
    text.chars()
        .map(|c| {
            if let Some((_, full)) = SYMBOL_PAIRS.iter().find(|(half, _)| *half == c) {
                *full
            } else if c.is_ascii_alphanumeric() {
                char::from_u32(c as u32 + 0xFEE0).unwrap_or(c)
            } else {
                c
            }
        })
        .collect()
}

pub fn to_half_width(text: &str) -> String {
    text.chars()
        .map(|c| {
            if let Some((half, _)) = SYMBOL_PAIRS.iter().find(|(_, full)| *full == c) {
                *half
            } else if ('０'..='９').contains(&c)
                || ('Ａ'..='Ｚ').contains(&c)
                || ('ａ'..='ｚ').contains(&c)
            {
                char::from_u32(c as u32 - 0xFEE0).unwrap_or(c)
            } else {
                c
            }
        })
        .collect()
}

// ---- 数値・日付・時刻 ----

/// 数の表記を整える。漢数字は日付と時刻の中だけ直す。
/// 文字列だけでは「十二指腸」「五十嵐」と区別できないため、それ以外の漢数字には触らない。
fn normalize_numbers(text: &str, settings: &FilterSettings) -> String {
    let text = format_dates(text, settings.date_format);
    let text = format_times(&text, settings.time_format);
    let text = convert_measurements(&text);
    format_plain_numbers(&text, settings.numeral)
}

/// 漢数字のすぐ後ろに続けば算用数字に直す単位(寸法・個数)。
/// 認識エンジンが数を漢数字で出すことがある(「五ミリ」「三点五センチ」)ので、単位つきのものだけ直す。
const MEASUREMENT_UNITS: &[&str] = &[
    "ミリ",
    "mm",
    "センチ",
    "cm",
    "メートル",
    "パーセント",
    "％",
    "%",
    "個",
    "本",
    "回",
    "倍",
    "枚",
    "例",
    "箇所",
    "か所",
    "ヶ所",
    "カ所",
    "層",
];

/// 「五ミリ」→「5ミリ」、「三点五センチ」→「3.5センチ」。単位が続くときだけ直すので「十二指腸」には触らない。
fn convert_measurements(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < chars.len() {
        if !is_kanji_number_char(chars[i]) {
            out.push(chars[i]);
            i += 1;
            continue;
        }
        let start = i;
        while i < chars.len()
            && (is_kanji_number_char(chars[i])
                || (chars[i] == '点'
                    && i > start
                    && chars.get(i + 1).is_some_and(|c| kanji_digit(*c).is_some())))
        {
            i += 1;
        }
        let run: String = chars[start..i].iter().collect();
        let rest: String = chars[i..].iter().take(6).collect();
        let followed_by_unit = MEASUREMENT_UNITS.iter().any(|unit| rest.starts_with(unit));
        match followed_by_unit.then(|| measurement_value(&run)).flatten() {
            Some(value) => out.push_str(&value),
            None => out.push_str(&run),
        }
    }
    out
}

/// 漢数字(小数点「点」つきも可)を算用数字の文字列にする。
fn measurement_value(run: &str) -> Option<String> {
    match run.split_once('点') {
        Some((whole, fraction)) => {
            let whole = kanji_number_value(whole)?;
            if fraction.is_empty() {
                return None;
            }
            let fraction: String = fraction
                .chars()
                .map(|c| kanji_digit(c).map(|d| char::from(b'0' + d as u8)))
                .collect::<Option<String>>()?;
            Some(format!("{whole}.{fraction}"))
        }
        None => kanji_number_value(run).map(|value| value.to_string()),
    }
}

fn kanji_digit(c: char) -> Option<u64> {
    "〇一二三四五六七八九"
        .chars()
        .position(|d| d == c)
        .map(|p| p as u64)
}

fn kanji_small_unit(c: char) -> Option<u64> {
    match c {
        '十' => Some(10),
        '百' => Some(100),
        '千' => Some(1000),
        _ => None,
    }
}

fn kanji_large_unit(c: char) -> Option<u64> {
    match c {
        '万' => Some(10_000),
        '億' => Some(100_000_000),
        '兆' => Some(1_000_000_000_000),
        _ => None,
    }
}

fn is_kanji_number_char(c: char) -> bool {
    kanji_digit(c).is_some() || kanji_small_unit(c).is_some() || kanji_large_unit(c).is_some()
}

/// 漢数字の並びを値にする。二〇二六 のような桁読みと、二千二十六 のような単位読みの両方に対応する。
fn kanji_number_value(run: &str) -> Option<u64> {
    if run.chars().all(|c| kanji_digit(c).is_some()) {
        return run.chars().try_fold(0u64, |acc, c| {
            acc.checked_mul(10)?.checked_add(kanji_digit(c)?)
        });
    }
    let (mut total, mut section, mut digit) = (0u64, 0u64, None::<u64>);
    for c in run.chars() {
        if let Some(value) = kanji_digit(c) {
            digit = Some(digit.unwrap_or(0).checked_mul(10)?.checked_add(value)?);
        } else if let Some(unit) = kanji_small_unit(c) {
            section = section.checked_add(digit.take().unwrap_or(1).checked_mul(unit)?)?;
        } else {
            let unit = kanji_large_unit(c)?;
            let block = section + digit.take().unwrap_or(0);
            total = total.checked_add(block.max(1).checked_mul(unit)?)?;
            section = 0;
        }
    }
    total.checked_add(section)?.checked_add(digit.unwrap_or(0))
}

fn digits(value: u32, kind: u32) -> String {
    let plain = value.to_string();
    match kind {
        1 => plain,
        3 => to_full_width(&plain),
        6 => format!("{value:02}"),
        7 => to_full_width(&format!("{value:02}")),
        _ => plain,
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct Era {
    name: &'static str,
    letter: &'static str,
    first_year: i32,
}

const ERAS: [(i32, u32, u32, Era); 3] = [
    (
        2019,
        5,
        1,
        Era {
            name: "令和",
            letter: "R",
            first_year: 2019,
        },
    ),
    (
        1989,
        1,
        8,
        Era {
            name: "平成",
            letter: "H",
            first_year: 1989,
        },
    ),
    (
        1926,
        12,
        25,
        Era {
            name: "昭和",
            letter: "S",
            first_year: 1926,
        },
    ),
];

fn era_for(year: i32, month: u32, day: u32) -> Option<Era> {
    ERAS.iter()
        .find(|(y, m, d, _)| (year, month, day) >= (*y, *m, *d))
        .map(|(_, _, _, era)| *era)
}

fn era_by_name(name: &str) -> Option<Era> {
    ERAS.iter().map(|e| e.3).find(|era| era.name == name)
}

/// `g5y1年m1月d1日` のような書式文字列を展開する。`g` と `y` が無い短い形にも使う。
fn render_format(format: &str, values: &[(char, u32)], era: Option<Era>) -> String {
    let chars: Vec<char> = format.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        let kind = chars.get(i + 1).and_then(|c| c.to_digit(10));
        match (chars[i], kind) {
            ('g', Some(kind)) => {
                if let Some(era) = era {
                    match kind {
                        1 => out.push_str(era.letter),
                        3 => out.push_str(&to_full_width(era.letter)),
                        _ => out.push_str(era.name),
                    }
                }
                i += 2;
            }
            (letter, Some(kind)) if values.iter().any(|(k, _)| *k == letter) => {
                let value = values.iter().find(|(k, _)| *k == letter).unwrap().1;
                out.push_str(&digits(value, kind));
                i += 2;
            }
            (c, _) => {
                out.push(c);
                i += 1;
            }
        }
    }
    out
}

/// 年月日の書式から「月日」だけの短い書式を作る(月の前までを落とす)。
fn short_date_format(format: &str) -> &str {
    format.find('m').map_or(format, |p| &format[p..])
}

/// 文字列中の 1〜4 桁の数字、または漢数字を読む。(値, 次の位置)
fn read_number(chars: &[char], start: usize) -> Option<(u32, usize)> {
    let kanji_end = chars[start.min(chars.len())..]
        .iter()
        .take_while(|c| is_kanji_number_char(**c))
        .count();
    if kanji_end > 0 {
        let run: String = chars[start..start + kanji_end].iter().collect();
        let value = u32::try_from(kanji_number_value(&run)?).ok()?;
        return (value <= 9999).then_some((value, start + kanji_end));
    }
    let mut i = start;
    let mut value = 0u32;
    while i < chars.len() && i - start < 4 {
        let digit = chars[i].to_digit(10).or_else(|| {
            ('０'..='９')
                .contains(&chars[i])
                .then(|| chars[i] as u32 - '０' as u32)
        });
        match digit {
            Some(d) => value = value * 10 + d,
            None => break,
        }
        i += 1;
    }
    (i > start).then_some((value, i))
}

fn is_digit_at(chars: &[char], index: Option<usize>) -> bool {
    index.and_then(|i| chars.get(i)).is_some_and(|c| {
        c.is_ascii_digit() || ('０'..='９').contains(c) || is_kanji_number_char(*c)
    })
}

/// 「○年○月○日」「○月○日」を探して選択中の書式に直す。
fn format_dates(text: &str, format_index: usize) -> String {
    let format = DATE_FORMATS[format_index.min(5)].0;
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < chars.len() {
        let previous_is_digit = i > 0 && is_digit_at(&chars, Some(i - 1));
        let era_name = ["令和", "平成", "昭和"]
            .into_iter()
            .find(|name| chars[i..].iter().take(2).collect::<String>() == *name);
        let number_at = if era_name.is_some() { i + 2 } else { i };
        if !previous_is_digit {
            if let Some((rendered, next)) = parse_date(&chars, number_at, era_name, format) {
                out.push_str(&rendered);
                i = next;
                continue;
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

fn parse_date(
    chars: &[char],
    start: usize,
    era_name: Option<&str>,
    format: &str,
) -> Option<(String, usize)> {
    let (first, after_first) = read_number(chars, start)?;
    match chars.get(after_first) {
        Some('年') => {
            let (month, after_month) = read_number(chars, after_first + 1)?;
            if chars.get(after_month) != Some(&'月') {
                return None;
            }
            let (day, after_day) = read_number(chars, after_month + 1)?;
            if chars.get(after_day) != Some(&'日') || !valid_month_day(month, day) {
                return None;
            }
            let (era, era_year) = match era_name.and_then(era_by_name) {
                Some(era) => (era, first),
                None => {
                    let era = era_for(first as i32, month, day)?;
                    (era, (first as i32 - era.first_year + 1) as u32)
                }
            };
            let values = [('y', era_year), ('m', month), ('d', day)];
            Some((render_format(format, &values, Some(era)), after_day + 1))
        }
        Some('月') if era_name.is_none() => {
            let (day, after_day) = read_number(chars, after_first + 1)?;
            if chars.get(after_day) != Some(&'日') || !valid_month_day(first, day) {
                return None;
            }
            let values = [('m', first), ('d', day)];
            Some((
                render_format(short_date_format(format), &values, None),
                after_day + 1,
            ))
        }
        _ => None,
    }
}

fn valid_month_day(month: u32, day: u32) -> bool {
    (1..=12).contains(&month) && (1..=31).contains(&day)
}

/// 「○時○分」を選択中の書式に直す。
fn format_times(text: &str, format_index: usize) -> String {
    let format = TIME_FORMATS[format_index.min(5)].0;
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < chars.len() {
        if !(i > 0 && is_digit_at(&chars, Some(i - 1))) {
            if let Some((hour, after_hour)) = read_number(&chars, i) {
                if chars.get(after_hour) == Some(&'時') {
                    if let Some((minute, after_minute)) = read_number(&chars, after_hour + 1) {
                        if chars.get(after_minute) == Some(&'分') && hour < 24 && minute < 60 {
                            let values = [('h', hour), ('m', minute)];
                            out.push_str(&render_format(format, &values, None));
                            i = after_minute + 1;
                            continue;
                        }
                    }
                }
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

/// 3 桁以上の数字の並びを数値の表現に合わせて書き直す。
fn format_plain_numbers(text: &str, style: NumeralStyle) -> String {
    if style == NumeralStyle::Plain {
        return text.to_string();
    }
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < chars.len() {
        if !chars[i].is_ascii_digit() {
            out.push(chars[i]);
            i += 1;
            continue;
        }
        let start = i;
        while i < chars.len() && chars[i].is_ascii_digit() {
            i += 1;
        }
        let run: String = chars[start..i].iter().collect();
        let previous = start.checked_sub(1).and_then(|p| chars.get(p)).copied();
        let next = chars.get(i).copied();
        let rest: String = chars[i..].iter().take(2).collect();
        let joined = matches!(previous, Some('.' | ':' | '/' | ',' | '-'))
            || matches!(next, Some('.' | ':' | '/' | ',' | '-'))
            || run.starts_with('0')
            || NO_COMMA_TOKENS.iter().any(|t| rest.starts_with(t));
        if joined {
            out.push_str(&run);
            continue;
        }
        match style {
            NumeralStyle::Comma => out.push_str(&with_commas(&run)),
            NumeralStyle::Mixed => out.push_str(&with_units(&run)),
            NumeralStyle::Plain => out.push_str(&run),
        }
    }
    out
}

fn with_commas(run: &str) -> String {
    let bytes = run.as_bytes();
    let mut out = String::new();
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 && (bytes.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(*b as char);
    }
    out
}

fn with_units(run: &str) -> String {
    let Ok(value) = run.parse::<u64>() else {
        return run.to_string();
    };
    if value < 10_000 {
        return run.to_string();
    }
    let mut remaining = value;
    let mut out = String::new();
    for (unit, name) in [
        (1_000_000_000_000u64, "兆"),
        (100_000_000, "億"),
        (10_000, "万"),
    ] {
        let count = remaining / unit;
        if count > 0 {
            out.push_str(&count.to_string());
            out.push_str(name);
            remaining %= unit;
        }
    }
    if remaining > 0 {
        out.push_str(&remaining.to_string());
    }
    out
}

/// ひらがな読みを、決まった揃え方に直す。
/// 片仮名はひらがなに、お段+う・え段+い は長音に変える。ピリオドは変換を止める印。
pub fn normalize_reading(reading: &str) -> String {
    let chars: Vec<char> = reading
        .chars()
        .filter(|c| !c.is_whitespace())
        .map(|c| {
            if ('ァ'..='ヶ').contains(&c) {
                char::from_u32(c as u32 - 0x60).unwrap_or(c)
            } else {
                c
            }
        })
        .collect();
    let mut out = String::new();
    for (i, &c) in chars.iter().enumerate() {
        if c == '.' {
            continue;
        }
        let previous = i.checked_sub(1).map(|p| chars[p]);
        let long_o = c == 'う' && previous.is_some_and(is_o_row);
        let long_e = c == 'い' && previous.is_some_and(is_e_row);
        out.push(if long_o || long_e { 'ー' } else { c });
    }
    out
}

fn is_o_row(c: char) -> bool {
    "おこごそぞとどのほぼぽもよろをょぉ".contains(c)
}

fn is_e_row(c: char) -> bool {
    "えけげせぜてでねへべぺめれぇ".contains(c)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings() -> FilterSettings {
        FilterSettings::default()
    }

    /// 選択肢は見本の文字でしか見分けられない。
    #[test]
    fn format_examples_can_be_told_apart() {
        for examples in [
            DATE_FORMATS.map(|(_, example)| example),
            TIME_FORMATS.map(|(_, example)| example),
        ] {
            for (index, example) in examples.iter().enumerate() {
                assert!(!examples[index + 1..].contains(example), "{example}");
            }
        }
    }

    /// 見本は実際の出力と同じ形。
    #[test]
    fn format_examples_show_what_the_filters_write() {
        for (index, (_, example)) in TIME_FORMATS.iter().enumerate() {
            let afternoon = format_times("13時57分", index);
            assert!(example.starts_with(&afternoon), "{example} / {afternoon}");
            if example.contains("9時台") {
                let morning = format_times("9時57分", index);
                assert!(example.contains(&morning), "{example} / {morning}");
            }
        }
        for (index, (_, example)) in DATE_FORMATS.iter().enumerate() {
            let full = format_dates("令和1年2月3日", index);
            assert!(example.starts_with(&full), "{example} / {full}");
            let short = format_dates("2月3日", index);
            assert!(example.ends_with(&short), "{example} / {short}");
        }
        let numeral = |style| {
            let filters = FilterSettings {
                numeral: style,
                ..settings()
            };
            normalize_numbers("12300", &filters)
        };
        for style in NumeralStyle::ALL {
            assert_eq!(numeral(style), style.label());
        }
    }

    #[test]
    fn removes_interjections_only_when_enabled() {
        let empty = Dictionary::default();
        let text = apply_prefilters("えーと、右肺に結節", &settings(), &empty);
        assert_eq!(text, "右肺に結節");
        let keep = FilterSettings {
            remove_interjections: false,
            ..settings()
        };
        assert_eq!(apply_prefilters("えー結節", &keep, &empty), "えー結節");
    }

    #[test]
    fn strips_punctuation_when_auto_insert_is_off() {
        let off = FilterSettings {
            auto_punctuation: false,
            ..settings()
        };
        assert_eq!(remove_interjections("結節、あり。", &off), "結節あり");
    }

    #[test]
    fn swaps_punctuation_kind() {
        let comma = FilterSettings {
            punctuation_kind: 1,
            ..settings()
        };
        assert_eq!(remove_interjections("あ、い。", &comma), "あ，い。");
        let dots = FilterSettings {
            punctuation_kind: 2,
            ..settings()
        };
        assert_eq!(remove_interjections("あ、い。", &dots), "あ，い．");
    }

    #[test]
    fn kanji_numbers_inside_dates_and_times_are_read() {
        assert_eq!(format_dates("二千二十六年六月四日", 0), "令和8年6月4日");
        assert_eq!(format_dates("十二月十五日", 0), "12月15日");
        assert_eq!(format_times("九時五分", 1), "09:05");
    }

    /// レビューで見つかった、医療文を変えてしまう例。すべて入力のまま残ること。
    #[test]
    fn medical_words_with_kanji_numerals_stay_untouched() {
        let empty = Dictionary::default();
        for text in [
            "十二指腸",
            "不十分",
            "一時的",
            "百日咳",
            "四五日前",
            "右第二三肋骨",
            "五十嵐",
            "万一",
            "〇九〇",
            "一過性",
            "一般的",
            "十二分に",
        ] {
            assert_eq!(apply_prefilters(text, &settings(), &empty), text);
        }
    }

    #[test]
    fn spaces_between_numbers_stay() {
        let empty = Dictionary::default();
        for text in ["血圧 120 80", "T2 3.5", "L4 5", "10μg", "deep learning 2"] {
            assert_eq!(apply_prefilters(text, &settings(), &empty), text);
        }
    }

    #[test]
    fn fillers_are_only_removed_at_breaks() {
        assert_eq!(strip_fillers("まえーと"), "まえーと");
        assert_eq!(strip_fillers("えー右肺、えーと左肺"), "右肺、左肺");
        assert_eq!(strip_fillers("所見はえーと、なし"), "所見はなし");
    }

    #[test]
    fn digits_keep_their_comma_when_punctuation_is_off() {
        assert_eq!(
            strip_punctuation_marks("L3、4の高さ、あり。"),
            "L3、4の高さあり"
        );
    }

    #[test]
    fn numeral_styles_leave_years_alone() {
        assert_eq!(
            format_plain_numbers("2026年に15000円", NumeralStyle::Comma),
            "2026年に15,000円"
        );
        assert_eq!(
            format_plain_numbers("20000年", NumeralStyle::Mixed),
            "20000年"
        );
    }

    #[test]
    fn numeral_styles() {
        assert_eq!(
            format_plain_numbers("15000円", NumeralStyle::Mixed),
            "1万5000円"
        );
        assert_eq!(
            format_plain_numbers("15000円", NumeralStyle::Comma),
            "15,000円"
        );
        assert_eq!(
            format_plain_numbers("15000円", NumeralStyle::Plain),
            "15000円"
        );
    }

    #[test]
    fn no_comma_before_address_tokens() {
        assert_eq!(
            format_plain_numbers("1234番地", NumeralStyle::Comma),
            "1234番地"
        );
    }

    #[test]
    fn date_formats_follow_examples() {
        assert_eq!(format_dates("2009年6月4日", 0), "平成21年6月4日");
        assert_eq!(format_dates("2009年6月4日", 1), "H21/6/4");
        assert_eq!(format_dates("2009年6月4日", 2), "平成21/6/4");
        assert_eq!(format_dates("2009年6月4日", 3), "平成２１年６月４日");
        assert_eq!(format_dates("2009年6月4日", 4), "Ｈ２１／６／４");
        assert_eq!(format_dates("2009年6月4日", 5), "平成２１／６／４");
    }

    #[test]
    fn short_date_formats_follow_examples() {
        assert_eq!(format_dates("2月14日", 0), "2月14日");
        assert_eq!(format_dates("2月14日", 1), "2/14");
        assert_eq!(format_dates("2月14日", 3), "２月１４日");
        assert_eq!(format_dates("2月14日", 4), "２／１４");
    }

    #[test]
    fn dates_use_the_current_era() {
        assert_eq!(format_dates("2026年6月4日", 0), "令和8年6月4日");
        assert_eq!(format_dates("平成21年6月4日", 1), "H21/6/4");
    }

    #[test]
    fn time_formats_follow_examples() {
        assert_eq!(format_times("9時5分", 0), "9時5分");
        assert_eq!(format_times("9時5分", 1), "09:05");
        assert_eq!(format_times("9時5分", 2), "9:05");
        assert_eq!(format_times("9時5分", 3), "９時５分");
        assert_eq!(format_times("9時5分", 4), "０９：０５");
        assert_eq!(format_times("9時5分", 5), "９：０５");
    }

    #[test]
    fn spaces_around_english_words() {
        assert_eq!(apply_english_spacing("胸部CTで", 0), "胸部 CT で");
        assert_eq!(apply_english_spacing("胸部 CT で", 1), "胸部CTで");
        assert_eq!(apply_english_spacing("deep learning", 1), "deep learning");
    }

    #[test]
    fn width_conversion_round_trips() {
        assert_eq!(to_full_width("CT 5mm"), "ＣＴ　５ｍｍ");
        assert_eq!(to_half_width("ＣＴ　５ｍｍ"), "CT 5mm");
    }

    #[test]
    fn readings_become_words_only_when_spoken_alone() {
        let mut dictionary = Dictionary::default();
        dictionary.add_word("肝腺", "かんせん").unwrap();
        assert_eq!(apply_notation("かんせん。", &dictionary), "肝腺");
        assert_eq!(
            apply_notation("ウイルスかんせん", &dictionary),
            "ウイルスかんせん"
        );
    }

    #[test]
    fn disabled_words_are_marked_not_removed() {
        let mut dictionary = Dictionary::default();
        dictionary.disabled.push("がん".to_string());
        let text = "胃がん検診";
        assert_eq!(apply_prefilters(text, &settings(), &dictionary), text);
        assert_eq!(disabled_ranges(text, &dictionary), vec![(1, 3)]);
    }

    #[test]
    fn readings_are_normalized_like_the_help_describes() {
        assert_eq!(normalize_reading("とうきょう"), "とーきょー");
        assert_eq!(normalize_reading("とーきょー"), "とーきょー");
        assert_eq!(normalize_reading("すぺいん"), "すぺーん");
        assert_eq!(normalize_reading("すぺ.いん"), "すぺいん");
        assert_eq!(normalize_reading("カイギョウ"), "かいぎょー");
    }
}

#[cfg(test)]
mod measurement_tests {
    use super::*;

    #[test]
    fn kanji_numbers_before_units_become_digits() {
        assert_eq!(
            convert_measurements("右肺上葉に五ミリの結節を認めます"),
            "右肺上葉に5ミリの結節を認めます"
        );
        assert_eq!(
            convert_measurements("三点五センチの腫瘤と十三mmの結節、二個"),
            "3.5センチの腫瘤と13mmの結節、2個"
        );
        assert_eq!(
            convert_measurements("百二十パーセント、一例、第五腰椎"),
            "120パーセント、1例、第五腰椎"
        );
    }

    #[test]
    fn kanji_numbers_without_units_are_left_alone() {
        assert_eq!(
            convert_measurements("十二指腸と五十嵐さん、三角形"),
            "十二指腸と五十嵐さん、三角形"
        );
        assert_eq!(convert_measurements("点ミリ"), "点ミリ");
    }
}
