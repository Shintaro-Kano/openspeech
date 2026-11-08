//! 単語・無効単語・記号の読み・テンプレート(定型文)。
//! 利用者が手で編集する辞書ファイル(`dictionary.json`、`store.rs`)の中身でもある。

use super::textio::decode_text;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// 登録単語。`reading` は省略できる(手で書くときも)。`extra` は利用者が足した項目で、保存で残す。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Word {
    pub word: String,
    pub reading: String,
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

impl Word {
    pub fn new(word: &str, reading: &str) -> Self {
        Word {
            word: word.to_string(),
            reading: reading.to_string(),
            extra: BTreeMap::new(),
        }
    }
}

/// テンプレート(定型文)。キーワードに続けて「テンプレート」と話すと本文に置き換わる(「前立腺MRIテンプレート」)。
/// キーワードだけなら普通の語として入る。検査名と正常所見の一対一が基本。
/// `modules` は所見モジュール。話しても入らず、「ヘイクロード」の依頼で Claude に渡す材料
/// (依頼で言われた所見があれば、正常所見の文と差し替える)。
/// `extra` は利用者が辞書ファイルに足した項目(名前・分類・メモなど)。アプリは読まず、保存で残す。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Template {
    /// 呼び出す語(検査名)。先頭が主名で、認識のゆれや別名に備えて複数持てる。
    pub keywords: Vec<String>,
    /// 差し込む文章(正常所見。改行を含んでよい)。ファイルでは 1 行なら文字列、複数行なら行の配列。
    #[serde(with = "lines_or_text")]
    pub body: String,
    /// 所見モジュール(Claude 用)。無ければファイルに書かない。手で `["肝", "胆"]` と名前だけ書いた形や
    /// `null` も読める(名前だけのモジュールは文が無いので Claude には渡さない)。
    #[serde(deserialize_with = "modules_field::deserialize", skip_serializing_if = "Vec::is_empty")]
    pub modules: Vec<Module>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

/// 所見モジュール。依頼でその所見が言われたとき、Claude が正常所見の文と差し替える文。
/// 自由文で、差し替え先は `note`(使い方)に書くか、Claude が同じ臓器・項目の文を選ぶ。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Module {
    /// 名前(「PI-RADS カテゴリー4/5」)。依頼の所見と結び付ける手がかり。
    pub name: String,
    /// 差し替える文(改行を含んでよい)。ファイルでは本文と同じく文字列か行の配列。
    #[serde(with = "lines_or_text")]
    pub text: String,
    /// 使い方(どの文と差し替えるか、全体を差し替えるか、など)。省略できる。
    #[serde(skip_serializing_if = "String::is_empty")]
    pub note: String,
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

/// モジュールの項目の読み方: 配列の要素はモジュールか名前だけの文字列。項目が無いか null なら空。
/// 手で書いた辞書ファイルの 1 項目の形で辞書全体が読めなくならないようにする。
mod modules_field {
    use super::Module;
    use serde::{Deserialize, Deserializer};

    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Entry {
        Full(Module),
        Name(String),
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<Module>, D::Error> {
        let entries: Option<Vec<Entry>> = Option::deserialize(deserializer)?;
        Ok(entries
            .unwrap_or_default()
            .into_iter()
            .map(|entry| match entry {
                Entry::Full(module) => module,
                Entry::Name(name) => Module {
                    name,
                    ..Module::default()
                },
            })
            .collect())
    }
}

/// 記号の読み。発話全体が読みのどれかと一致したとき `text` を入れる。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Symbol {
    /// 読み。認識エンジンの表記のゆれ(漢字・カタカナ)に備えて複数持てる。
    pub readings: Vec<String>,
    /// 入れる文字。
    pub text: String,
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

impl Symbol {
    pub fn new(readings: &[&str], text: &str) -> Self {
        Symbol {
            readings: readings.iter().map(|r| r.to_string()).collect(),
            text: text.to_string(),
            extra: BTreeMap::new(),
        }
    }
}

/// 既定の記号の読み(2026-11-07、利用者の指定の 8 組。漢字の表記は認識エンジンのゆれに備えて足した)。
/// 「にょろ」は全角チルダ(U+FF5E)。Windows のレポートシステムは波ダッシュ(U+301C)を扱えないことがある。
pub fn default_symbols() -> Vec<Symbol> {
    vec![
        Symbol::new(&["まる", "丸"], "。"),
        Symbol::new(&["てん", "点"], "、"),
        Symbol::new(&["かっこ", "括弧"], "（"),
        Symbol::new(&["かっことじる", "括弧閉じる", "かっことじ", "括弧閉じ"], "）"),
        Symbol::new(&["やじるし", "矢印"], "→"),
        Symbol::new(&["かける"], "×"),
        Symbol::new(&["いこーる"], "＝"),
        Symbol::new(&["にょろ"], "～"),
    ]
}

/// 辞書ファイルの先頭に書く説明(JSON にはコメントが書けないので項目にする)。
pub const DICTIONARY_NOTE: &str = "OpenSpeech の辞書。手で編集してよく、保存するとアプリを開いたまま反映されます。\
 words: 単語(word 表記, reading ひらがなの読み。読みは認識エンジンと AI 訂正の手がかりで、省略できる)。\
 disabled: 無効にした単語。\
 symbols: 記号の読み(readings のどれかを単独で話すと text が入る)。\
 templates: テンプレート(keywords のどれかに続けて「テンプレート」と話すと body が入る。「前立腺MRIテンプレート」。body は文字列か行の配列。\
 modules は「ヘイクロード」の依頼で Claude が使う所見モジュール: name 名前, text 差し替える文, note 使い方)。\
 どの項目にも、ほかの名前の項目(メモ・分類など)を自由に足せます(アプリは読まず、書き直しても残します)。";

/// 辞書。`#[serde(default)]` なので、古いファイルや手で書いたファイルに無い項目は既定になる
/// (`symbols` が無ければ既定の記号、空の配列なら記号なし)。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Dictionary {
    #[serde(rename = "_note", skip_serializing_if = "String::is_empty")]
    pub note: String,
    pub words: Vec<Word>,
    pub disabled: Vec<String>,
    pub symbols: Vec<Symbol>,
    pub templates: Vec<Template>,
    /// 利用者が足した項目(ファイルの先頭のメモなど)。保存で残す。
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

impl Default for Dictionary {
    fn default() -> Self {
        Dictionary {
            note: DICTIONARY_NOTE.to_string(),
            words: Vec::new(),
            disabled: Vec::new(),
            symbols: default_symbols(),
            templates: Vec::new(),
            extra: BTreeMap::new(),
        }
    }
}

/// テンプレートの本文。ファイルでは、改行を含む本文を行の配列で書けるようにする(手で編集しやすい)。
mod lines_or_text {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Body {
        Text(String),
        Lines(Vec<String>),
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<String, D::Error> {
        Ok(match Body::deserialize(deserializer)? {
            Body::Text(text) => text,
            Body::Lines(lines) => lines.join("\n"),
        })
    }

    pub fn serialize<S: Serializer>(body: &str, serializer: S) -> Result<S::Ok, S::Error> {
        if body.contains('\n') {
            body.split('\n').collect::<Vec<_>>().serialize(serializer)
        } else {
            body.serialize(serializer)
        }
    }
}

/// 読みに使える文字か(全角ひらがな・長音・変換止めのピリオド)。
fn is_reading_char(c: char) -> bool {
    ('ぁ'..='ゖ').contains(&c) || c == 'ー' || c == '.'
}

/// カタカナをひらがなに直す(読みはひらがなで持つ)。
pub fn to_hiragana(text: &str) -> String {
    text.chars()
        .map(|c| match c {
            'ァ'..='ヶ' => char::from_u32(c as u32 - 0x60).unwrap_or(c),
            _ => c,
        })
        .collect()
}

/// ひらがなをカタカナに直す(認識エンジンの keywords は読みをカタカナで受ける)。
pub fn to_katakana(text: &str) -> String {
    text.chars()
        .map(|c| match c {
            'ぁ'..='ゖ' => char::from_u32(c as u32 + 0x60).unwrap_or(c),
            _ => c,
        })
        .collect()
}

/// キーワードの並びを、画面の欄と同じ分け方(「、」「,」改行)で分け直し、空と重複を除く。
pub fn normalize_keywords(keywords: &[String]) -> Vec<String> {
    split_keywords(&keywords.join("、"))
}

/// 「、」などで区切られたキーワードを分ける。空と重複は除く。
pub fn split_keywords(text: &str) -> Vec<String> {
    let mut keywords: Vec<String> = Vec::new();
    for part in text.split(['、', ',', '，', '\n']) {
        let keyword = part.trim();
        if !keyword.is_empty() && !keywords.iter().any(|k| k == keyword) {
            keywords.push(keyword.to_string());
        }
    }
    keywords
}

fn kanji_digit(c: char) -> Option<u32> {
    "〇一二三四五六七八九"
        .chars()
        .position(|digit| digit == c)
        .map(|value| value as u32)
}

fn kanji_unit(c: char) -> Option<u32> {
    match c {
        '十' => Some(10),
        '百' => Some(100),
        '千' => Some(1000),
        _ => None,
    }
}

fn is_number_char(c: char) -> bool {
    c.is_ascii_digit()
        || ('０'..='９').contains(&c)
        || kanji_digit(c).is_some()
        || kanji_unit(c).is_some()
}

/// 漢数字の並びを算用数字にする(「十二」は 12、「一二」は 12)。
fn kanji_number(run: &[char]) -> String {
    if !run.iter().any(|c| kanji_unit(*c).is_some()) {
        return run
            .iter()
            .filter_map(|c| kanji_digit(*c).and_then(|value| char::from_digit(value, 10)))
            .collect();
    }
    let (mut total, mut digit) = (0, None);
    for c in run {
        match kanji_unit(*c) {
            Some(unit) => total += digit.take().unwrap_or(1) * unit,
            None => digit = kanji_digit(*c),
        }
    }
    (total + digit.unwrap_or(0)).to_string()
}

/// 話した文とキーワードを比べる形にする。認識エンジンごとの表記の差(句読点、空白、全角、
/// カタカナ、漢数字)をならす。数の間の小数点は残す(「1.5」と「15」は別の語)。
/// 音声コマンドの言葉も同じ形で比べる。
pub fn template_key(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut key = String::new();
    let mut index = 0;
    while index < chars.len() {
        let c = chars[index];
        if kanji_digit(c).is_some() || kanji_unit(c).is_some() {
            let length = chars[index..]
                .iter()
                .take_while(|c| kanji_digit(**c).is_some() || kanji_unit(**c).is_some())
                .count();
            key.push_str(&kanji_number(&chars[index..index + length]));
            index += length;
            continue;
        }
        let between_numbers = index > 0
            && is_number_char(chars[index - 1])
            && chars.get(index + 1).is_some_and(|next| is_number_char(*next));
        match c {
            '.' | '．' | '点' if between_numbers => key.push('.'),
            c if c.is_whitespace() || "、。，．,.!！?？・".contains(c) => {}
            'ァ'..='ヶ' => key.extend(char::from_u32(c as u32 - 0x60)),
            '０'..='９' | 'Ａ'..='Ｚ' | 'ａ'..='ｚ' => {
                key.extend(char::from_u32(c as u32 - 0xFEE0).into_iter().flat_map(char::to_lowercase));
            }
            c => key.extend(c.to_lowercase()),
        }
        index += 1;
    }
    key
}

/// 文の末尾の、ならすと `key` になる部分を除いた前半を返す。無ければ None。
/// ならした文からは元に戻せないので、元の文の後ろから 1 文字ずつ伸ばして探す。
/// 数や英単語の途中では切らない(「二十」の「十」、「stab」の「tab」)。
pub fn strip_key_suffix<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    let inside_a_run = |before: char, after: char| {
        (is_number_char(before) && is_number_char(after))
            || (before.is_ascii_alphabetic() && after.is_ascii_alphabetic())
    };
    let (start, first) = text
        .char_indices()
        .rev()
        .find(|(start, _)| template_key(&text[*start..]) == key)?;
    let head = &text[..start];
    let joined = head.chars().next_back().is_some_and(|c| inside_a_run(c, first));
    (!joined).then_some(head)
}

/// テンプレートを呼ぶ接尾辞(ならした形)。キーワードに続けて言う(「前立腺MRIテンプレート」「胸部CTのテンプレ」)。
pub const TEMPLATE_SUFFIXES: [&str; 2] = ["てんぷれーと", "てんぷれ"];
/// 接尾辞の表記。認識エンジンの手がかりと AI 訂正の「そのまま残す語」に渡す。
pub const TEMPLATE_WORD: &str = "テンプレート";

/// ならした発話から接尾辞を除いた、キーワードにあたる部分。接尾辞が無い、または接尾辞だけなら None。
/// 間の「の」は任意なので、呼び出す側は「の」を除いた形でも探す。
pub fn strip_template_suffix(key: &str) -> Option<&str> {
    TEMPLATE_SUFFIXES
        .iter()
        .find_map(|suffix| key.strip_suffix(suffix))
        .filter(|head| !head.is_empty())
}

/// 画面の「所見モジュール」欄の書き方: 「【名前】」の行で 1 つが始まり、次の行から差し替える文。
/// 「使い方: …」で始まる行は使い方。モジュールの間の空行は任意。
pub fn parse_modules(text: &str) -> Result<Vec<Module>, String> {
    fn finish(modules: &mut [Module], lines: &mut Vec<String>, notes: &mut Vec<String>) -> Result<(), String> {
        if let Some(current) = modules.last_mut() {
            let text = lines.join("\n").trim().to_string();
            if text.is_empty() {
                return Err(format!("モジュール「{}」の文がありません", current.name));
            }
            current.text = text;
            current.note = notes.join("\n");
        }
        lines.clear();
        notes.clear();
        Ok(())
    }
    let mut modules: Vec<Module> = Vec::new();
    let mut lines: Vec<String> = Vec::new();
    let mut notes: Vec<String> = Vec::new();
    for raw in text.replace("\r\n", "\n").lines() {
        let line = raw.trim_end();
        let trimmed = line.trim();
        if let Some(name) = trimmed.strip_prefix('【').and_then(|rest| rest.strip_suffix('】')) {
            let name = name.trim();
            if name.is_empty() {
                return Err("モジュールの名前が空です(【名前】)".to_string());
            }
            finish(&mut modules, &mut lines, &mut notes)?;
            if modules.iter().any(|m| m.name == name) {
                return Err(format!("モジュール「{name}」が 2 つあります"));
            }
            modules.push(Module {
                name: name.to_string(),
                ..Module::default()
            });
            continue;
        }
        if modules.is_empty() {
            if trimmed.is_empty() {
                continue;
            }
            return Err("所見モジュールは「【名前】」の行から始めてください".to_string());
        }
        let note = trimmed
            .strip_prefix("使い方:")
            .or_else(|| trimmed.strip_prefix("使い方："));
        if let Some(note) = note {
            let note = note.trim();
            if !note.is_empty() {
                notes.push(note.to_string());
            }
            continue;
        }
        lines.push(line.to_string());
    }
    finish(&mut modules, &mut lines, &mut notes)?;
    Ok(modules)
}

/// テンプレートの欄の検査: キーワードがある、「テンプレート」で終わらない、文章がある。
fn check_template_fields(keywords: &[String], body: &str) -> Result<(), String> {
    if keywords.iter().all(|k| template_key(k).is_empty()) {
        return Err("キーワードを入力してください".to_string());
    }
    if let Some(keyword) = keywords
        .iter()
        .find(|k| TEMPLATE_SUFFIXES.iter().any(|suffix| template_key(k).ends_with(suffix)))
    {
        return Err(format!(
            "「{keyword}」は「テンプレート」で終わっています。キーワードは検査名だけにし、話すときに「テンプレート」を続けます"
        ));
    }
    if body.trim().is_empty() {
        return Err("文章を入力してください".to_string());
    }
    Ok(())
}

/// `parse_modules` の逆。画面の欄と CSV に書く形。
pub fn modules_text(modules: &[Module]) -> String {
    let mut text = String::new();
    for module in modules {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(&format!("【{}】\n{}\n", module.name, module.text.trim_end()));
        for note in module.note.lines().map(str::trim).filter(|n| !n.is_empty()) {
            text.push_str(&format!("使い方: {note}\n"));
        }
    }
    text
}

impl Dictionary {
    pub fn add_word(&mut self, word: &str, reading: &str) -> Result<(), String> {
        let word = word.trim();
        let reading = to_hiragana(reading.trim());
        if word.is_empty() {
            return Err("単語を入力してください".to_string());
        }
        if word.contains('\n') {
            return Err("単語に改行は使えません".to_string());
        }
        if !reading.chars().all(is_reading_char) {
            return Err("読みはひらがなで入力してください(省略もできます)".to_string());
        }
        if self
            .words
            .iter()
            .any(|w| w.word == word && w.reading == reading)
        {
            return Err("同じ単語がすでに登録されています".to_string());
        }
        self.disabled.retain(|d| d != word);
        self.words.push(Word::new(word, &reading));
        Ok(())
    }

    /// 短すぎる読みの注意文。誤認識の原因になる。
    pub fn reading_warning(reading: &str) -> Option<&'static str> {
        (!reading.is_empty() && reading.chars().count() <= 2)
            .then_some("短い読みは誤変換の原因になります。なるべく3文字以上にしてください")
    }

    /// 単語を消す。消したら true。
    pub fn remove_word(&mut self, index: usize) -> bool {
        if index < self.words.len() {
            self.words.remove(index);
            true
        } else {
            false
        }
    }

    pub fn disable_word(&mut self, word: &str) {
        let word = word.trim();
        if word.is_empty() || self.disabled.iter().any(|d| d == word) {
            return;
        }
        self.words.retain(|w| w.word != word);
        self.disabled.push(word.to_string());
    }

    // ---- テンプレート ----

    /// テンプレートを追加する。`editing` があればその番号を置き換える。戻り値は保存した番号。
    pub fn save_template(
        &mut self,
        editing: Option<usize>,
        keywords: &str,
        body: &str,
    ) -> Result<usize, String> {
        let keywords = split_keywords(keywords);
        let body = body.replace("\r\n", "\n");
        check_template_fields(&keywords, &body)?;
        if let Some(keyword) = self.taken_keyword(&keywords, editing) {
            return Err(format!("「{keyword}」はほかのテンプレートで使っています"));
        }
        // 画面から直しても、所見モジュールと利用者がファイルに足した項目は残す。ただし共有ライブラリの
        // `meta`(題名・投稿者など)は外す: 直した時点でその人のテンプレートになり、共有の題名に古い物を使わない。
        let (modules, mut extra) = editing
            .and_then(|index| self.templates.get(index))
            .map(|template| (template.modules.clone(), template.extra.clone()))
            .unwrap_or_default();
        extra.remove("meta");
        let template = Template {
            keywords,
            body,
            modules,
            extra,
        };
        match editing.filter(|index| *index < self.templates.len()) {
            Some(index) => {
                self.templates[index] = template;
                Ok(index)
            }
            None => {
                self.templates.push(template);
                Ok(self.templates.len() - 1)
            }
        }
    }

    /// テンプレートをそのまま(所見モジュールと `extra` ごと)足す。共有ライブラリからの取り込みに使う。
    /// 画面の保存と同じ検査をする。キーワードがほかのテンプレートと重なるとき、`replace` なら重なる物を
    /// 置き換え(最初の物の位置に入れ、ほかは消す)、でなければ Err。戻り値は入れた番号。
    pub fn add_template(&mut self, mut template: Template, replace: bool) -> Result<usize, String> {
        template.keywords = normalize_keywords(&template.keywords);
        template.body = template.body.replace("\r\n", "\n");
        check_template_fields(&template.keywords, &template.body)?;
        let conflicts = self.conflicting_templates(&template.keywords, None);
        let Some(&first) = conflicts.first() else {
            self.templates.push(template);
            return Ok(self.templates.len() - 1);
        };
        if !replace {
            let keyword = self.taken_keyword(&template.keywords, None).cloned().unwrap_or_default();
            return Err(format!("「{keyword}」はほかのテンプレートで使っています"));
        }
        for &index in conflicts.iter().skip(1).rev() {
            self.templates.remove(index);
        }
        self.templates[first] = template;
        Ok(first)
    }

    /// キーワードのどれかがほかのテンプレート(`except` の番号は除く)と重なるテンプレートの番号(小さい順)。
    /// 比べ方は呼び出しと同じ(`template_key`)。
    pub fn conflicting_templates(&self, keywords: &[String], except: Option<usize>) -> Vec<usize> {
        let keys: Vec<String> = keywords.iter().map(|k| template_key(k)).filter(|k| !k.is_empty()).collect();
        self.templates
            .iter()
            .enumerate()
            .filter(|(index, template)| {
                Some(*index) != except
                    && template.keywords.iter().any(|k| keys.contains(&template_key(k)))
            })
            .map(|(index, _)| index)
            .collect()
    }

    /// ほかのテンプレート(`except` の番号は除く)で使っているキーワードの最初の 1 つ。
    fn taken_keyword<'a>(&self, keywords: &'a [String], except: Option<usize>) -> Option<&'a String> {
        keywords.iter().find(|keyword| {
            !self
                .conflicting_templates(std::slice::from_ref(*keyword), except)
                .is_empty()
        })
    }

    /// 所見モジュールの欄(`parse_modules` の書き方)と一緒に保存する。欄が読めなければ何も変えない。
    /// 欄に無い項目(利用者がファイルで足した `diagnosis` など)は、同じ名前のモジュールから引き継ぐ。
    pub fn save_template_with_modules(
        &mut self,
        editing: Option<usize>,
        keywords: &str,
        body: &str,
        modules: &str,
    ) -> Result<usize, String> {
        let mut modules = parse_modules(modules)?;
        if let Some(existing) = editing.and_then(|index| self.templates.get(index)) {
            for module in &mut modules {
                if let Some(old) = existing.modules.iter().find(|m| m.name == module.name) {
                    module.extra = old.extra.clone();
                }
            }
        }
        let index = self.save_template(editing, keywords, body)?;
        self.templates[index].modules = modules;
        Ok(index)
    }

    /// 話した文全体が「キーワード+テンプレート」(間の「の」は任意)と一致するテンプレート。
    /// キーワードだけ、文の途中の語では発動しない。
    pub fn find_template(&self, spoken: &str) -> Option<&Template> {
        let key = template_key(spoken);
        let head = strip_template_suffix(&key)?;
        let by_key = |head: &str| {
            self.templates
                .iter()
                .find(|template| template.keywords.iter().any(|k| template_key(k) == head))
        };
        by_key(head).or_else(|| {
            head.strip_suffix('の')
                .filter(|h| !h.is_empty())
                .and_then(by_key)
        })
    }

    /// すべてのキーワード(認識エンジンの手がかりに渡す)。
    pub fn template_keywords(&self) -> impl Iterator<Item = &str> {
        self.templates
            .iter()
            .flat_map(|template| template.keywords.iter().map(String::as_str))
    }

    // ---- インポートとエクスポート ----

    /// 「語句,読み」の CSV を読む(3 列目があれば無視する)。戻り値は取り込んだ数と、失敗した行の説明。
    pub fn import_words_csv(&mut self, bytes: &[u8]) -> (usize, Vec<String>) {
        let mut added = 0;
        let mut errors = Vec::new();
        for (index, line) in decode_text(bytes).lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let fields = parse_csv_line(line);
            let word = fields.first().map_or("", String::as_str);
            let reading = fields.get(1).map_or("", String::as_str);
            match self.add_word(word, reading) {
                Ok(()) => added += 1,
                Err(message) => errors.push(format!("{} 行目: {message}", index + 1)),
            }
        }
        (added, errors)
    }

    pub fn export_words_csv(&self) -> String {
        self.words
            .iter()
            .map(|w| format!("{},{}\r\n", csv_field(&w.word), csv_field(&w.reading)))
            .collect()
    }

    pub fn import_disabled_csv(&mut self, bytes: &[u8]) -> usize {
        let before = self.disabled.len();
        for line in decode_text(bytes).lines() {
            if let Some(word) = parse_csv_line(line).first() {
                self.disable_word(word);
            }
        }
        self.disabled.len() - before
    }

    pub fn export_disabled_csv(&self) -> String {
        self.disabled
            .iter()
            .map(|w| format!("{}\r\n", csv_field(w)))
            .collect()
    }

    /// 「キーワード,文章,所見モジュール」の CSV を読む(3 列目は無くてよい)。キーワードは「、」区切りで
    /// 複数書ける。文章とモジュール(画面の欄と同じ書き方)には改行を含められる。
    pub fn import_templates_csv(&mut self, bytes: &[u8]) -> (usize, Vec<String>) {
        let mut added = 0;
        let mut errors = Vec::new();
        for (index, record) in parse_csv_records(&decode_text(bytes)).iter().enumerate() {
            let keywords = record.first().map_or("", String::as_str);
            let body = record.get(1).map_or("", String::as_str);
            let modules = record.get(2).map_or("", String::as_str);
            match self.save_template_with_modules(None, keywords, body, modules) {
                Ok(_) => added += 1,
                Err(message) => {
                    // 囲みのないカンマで文章が 3 列目へこぼれたときの手がかり。
                    let spilled = !modules.trim().is_empty() && !modules.trim_start().starts_with('【');
                    let hint = if spilled {
                        "(3 列目は所見モジュールです。文章にカンマがあるなら引用符で囲みます)"
                    } else {
                        ""
                    };
                    errors.push(format!("{} 件目: {message}{hint}", index + 1));
                }
            }
        }
        (added, errors)
    }

    pub fn export_templates_csv(&self) -> String {
        self.templates
            .iter()
            .map(|t| {
                format!(
                    "{},{},{}\r\n",
                    csv_field(&t.keywords.join("、")),
                    csv_field(&t.body),
                    csv_field(&modules_text(&t.modules))
                )
            })
            .collect()
    }
}

/// CSV の 1 行を項目に分ける。ダブルクォートの囲みに対応する。
pub fn parse_csv_line(line: &str) -> Vec<String> {
    let mut fields = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' if quoted && chars.peek() == Some(&'"') => {
                current.push('"');
                chars.next();
            }
            '"' => quoted = !quoted,
            ',' if !quoted => fields.push(std::mem::take(&mut current)),
            c => current.push(c),
        }
    }
    fields.push(current);
    fields
}

/// 改行を含む項目がある CSV を、全体から読む。空の行は飛ばす。
pub fn parse_csv_records(text: &str) -> Vec<Vec<String>> {
    let mut records = Vec::new();
    let mut fields = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    let mut chars = text.chars().peekable();
    let finish = |fields: &mut Vec<String>, current: &mut String, records: &mut Vec<Vec<String>>| {
        fields.push(std::mem::take(current));
        let record = std::mem::take(fields);
        if record.iter().any(|field| !field.trim().is_empty()) {
            records.push(record);
        }
    };
    while let Some(c) = chars.next() {
        match c {
            '"' if quoted && chars.peek() == Some(&'"') => {
                current.push('"');
                chars.next();
            }
            '"' => quoted = !quoted,
            ',' if !quoted => fields.push(std::mem::take(&mut current)),
            '\r' if !quoted && chars.peek() == Some(&'\n') => {}
            '\n' if !quoted => finish(&mut fields, &mut current, &mut records),
            c => current.push(c),
        }
    }
    finish(&mut fields, &mut current, &mut records);
    records
}

fn csv_field(text: &str) -> String {
    if text.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", text.replace('"', "\"\""))
    } else {
        text.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn readings_are_hiragana_or_empty() {
        let mut dictionary = Dictionary::default();
        assert!(dictionary.add_word("東京", "toukyou").is_err());
        assert!(dictionary.add_word("東京", "トウキョウ").is_ok());
        assert_eq!(dictionary.words[0].reading, "とうきょう");
        assert!(dictionary.add_word("東京", "とうきょう").is_err());
        assert!(dictionary.add_word("MRI", "").is_ok());
        assert!(dictionary.add_word("", "えむあーるあい").is_err());
    }

    #[test]
    fn period_is_allowed_in_readings() {
        let mut dictionary = Dictionary::default();
        assert!(dictionary.add_word("スペイン", "すぺ.いん").is_ok());
    }

    #[test]
    fn disabling_removes_registered_word() {
        let mut dictionary = Dictionary::default();
        dictionary.add_word("肺", "はいぞう").unwrap();
        dictionary.disable_word("肺");
        assert!(dictionary.words.is_empty());
        assert_eq!(dictionary.disabled, vec!["肺"]);
    }

    #[test]
    fn words_csv_round_trip() {
        let mut dictionary = Dictionary::default();
        dictionary
            .add_word("Gd造影", "がどりにうむぞうえい")
            .unwrap();
        let mut copy = Dictionary::default();
        let (added, errors) = copy.import_words_csv(dictionary.export_words_csv().as_bytes());
        assert_eq!((added, errors.len()), (1, 0));
        assert_eq!(copy.words, dictionary.words);
    }

    #[test]
    fn csv_import_reports_bad_lines() {
        let mut dictionary = Dictionary::default();
        let (added, errors) =
            dictionary.import_words_csv("肺,はいぞう,\n腎臓,jinzou,\n".as_bytes());
        assert_eq!(added, 1);
        assert_eq!(errors.len(), 1);
        assert!(errors[0].starts_with("2 行目"));
    }

    #[test]
    fn old_words_with_a_category_still_load() {
        let json = r#"{"words":[{"word":"肺","reading":"はい","category":"病名"}],
            "attention":[["右","左"]]}"#;
        let dictionary: Dictionary = serde_json::from_str(json).unwrap();
        assert_eq!(dictionary.words[0].word, "肺");
        let mut copy = Dictionary::default();
        let (added, errors) = copy.import_words_csv("腺癌,せんがん,病名\r\n".as_bytes());
        assert_eq!((added, errors.len()), (1, 0));
    }

    fn with_template() -> Dictionary {
        let mut dictionary = Dictionary::default();
        dictionary
            .save_template(None, "胸部正常、胸部せいじょう", "肺野に異常なし。\n胸水なし。")
            .unwrap();
        dictionary
    }

    /// キーワードに続けて「テンプレート」(「テンプレ」、間の「の」は任意)と話したときだけ。
    #[test]
    fn templates_need_the_suffix_after_the_keyword() {
        let dictionary = with_template();
        assert!(dictionary.find_template("胸部正常テンプレート").is_some());
        assert!(dictionary.find_template("胸部 正常、テンプレート。").is_some());
        assert!(dictionary.find_template("胸部せいじょうのテンプレート").is_some());
        assert!(dictionary.find_template("胸部正常のテンプレ").is_some());
        assert!(dictionary.find_template("胸部正常").is_none(), "キーワードだけは普通の語");
        assert!(dictionary.find_template("胸部正常テンプレートです").is_none());
        assert!(dictionary.find_template("所見は胸部正常テンプレート").is_none());
        assert!(dictionary.find_template("テンプレート").is_none());
        assert!(dictionary.find_template("のテンプレート").is_none());
        assert!(dictionary.find_template("。").is_none());
    }

    /// キーワード自体が「の」で終わっても、「の」を落とさずに探すので見つかる。
    #[test]
    fn a_keyword_ending_in_no_still_matches() {
        let mut dictionary = Dictionary::default();
        dictionary.save_template(None, "腹部の", "本文").unwrap();
        assert!(dictionary.find_template("腹部のテンプレート").is_some());
        assert!(dictionary.find_template("腹部テンプレート").is_none());
    }

    #[test]
    fn template_keywords_ignore_how_the_engine_writes_them() {
        let mut dictionary = Dictionary::default();
        dictionary
            .save_template(None, "テンプレート1、CT正常", "本文")
            .unwrap();
        assert!(dictionary.find_template("てんぷれーと一テンプレート").is_some());
        assert!(dictionary.find_template("テンプレート１てんぷれーと").is_some());
        assert!(dictionary.find_template("ct 正常 テンプレート").is_some());
        assert!(dictionary.find_template("ＣＴ正常テンプレート").is_some());
    }

    #[test]
    fn template_keywords_read_numbers_however_they_are_written() {
        let mut dictionary = Dictionary::default();
        dictionary
            .save_template(None, "テンプレート12、所見105、CT1.5", "本文")
            .unwrap();
        assert!(dictionary.find_template("テンプレート十二テンプレート").is_some());
        assert!(dictionary.find_template("テンプレート一二のテンプレート").is_some());
        assert!(dictionary.find_template("テンプレート１２テンプレート").is_some());
        assert!(dictionary.find_template("所見百五テンプレート").is_some());
        assert!(dictionary.find_template("CT1.5テンプレート").is_some());
        assert!(dictionary.find_template("CT一点五のテンプレート").is_some());
        assert!(dictionary.find_template("CT15テンプレート").is_none(), "小数点は別の語");
        assert!(dictionary.find_template("テンプレート二十一テンプレート").is_none());
    }

    #[test]
    fn a_trailing_key_is_found_in_the_original_text() {
        assert_eq!(strip_key_suffix("結節を認めます。改行", "改行"), Some("結節を認めます。"));
        assert_eq!(strip_key_suffix("所見 ここで、改行。", "ここで改行"), Some("所見 "));
        assert_eq!(strip_key_suffix("結節 テンソウ", "てんそう"), Some("結節 "));
        assert_eq!(strip_key_suffix("改行です", "改行"), None);
    }

    #[test]
    fn a_trailing_key_never_splits_a_number_or_a_word() {
        assert_eq!(strip_key_suffix("二十", "10"), None);
        assert_eq!(strip_key_suffix("stab", "tab"), None);
        assert_eq!(strip_key_suffix("所見 tab", "tab"), Some("所見 "));
    }

    #[test]
    fn template_keywords_must_be_unique_and_filled_in() {
        let mut dictionary = with_template();
        assert!(dictionary.save_template(None, "胸部 正常", "別").is_err());
        assert!(dictionary.save_template(None, "、", "本文").is_err());
        assert!(dictionary.save_template(None, "腹部正常", " \n").is_err());
        assert_eq!(dictionary.templates.len(), 1);
        // 自分自身のキーワードは編集で残せる。
        dictionary
            .save_template(Some(0), "胸部正常", "書き換えた文")
            .unwrap();
        assert_eq!(dictionary.templates[0].body, "書き換えた文");
        assert_eq!(dictionary.templates[0].keywords, vec!["胸部正常"]);
    }

    #[test]
    fn templates_csv_round_trip_keeps_line_breaks_and_modules() {
        let mut dictionary = with_template();
        dictionary
            .save_template_with_modules(
                None,
                "腹部正常",
                "「肝」に異常なし, 脾腫なし",
                "【脂肪肝】\n軽度の脂肪肝を認めます。\n使い方: 肝の文と差し替える\n",
            )
            .unwrap();
        let text = dictionary.export_templates_csv();
        assert!(text.contains("【脂肪肝】"), "{text}");
        let mut copy = Dictionary::default();
        let (added, errors) = copy.import_templates_csv(text.as_bytes());
        assert_eq!((added, errors.len()), (2, 0), "{errors:?}");
        assert_eq!(copy.templates, dictionary.templates);
        // 3 列目の無い旧い CSV も読める。
        let (added, errors) = Dictionary::default().import_templates_csv("胸部正常,本文\r\n".as_bytes());
        assert_eq!((added, errors.len()), (1, 0), "{errors:?}");
    }

    #[test]
    fn templates_csv_reports_bad_records() {
        let mut dictionary = Dictionary::default();
        let (added, errors) =
            dictionary.import_templates_csv("a,本文\r\n\r\nb,\r\nc,本文,文だけ\r\n".as_bytes());
        assert_eq!(added, 1);
        assert_eq!(errors.len(), 2, "{errors:?}");
        assert!(errors[0].starts_with("2 件目"), "{errors:?}");
        assert!(errors[1].contains("【名前】") && errors[1].contains("引用符"), "{errors:?}");
    }

    /// キーワードは検査名だけ。「テンプレート」で終わる語は 1 回の発話で呼べないので断る。
    #[test]
    fn a_keyword_may_not_end_with_the_suffix() {
        let mut dictionary = Dictionary::default();
        let error = dictionary.save_template(None, "胸部テンプレート", "本文").unwrap_err();
        assert!(error.contains("「テンプレート」で終わって"), "{error}");
        assert!(dictionary.save_template(None, "胸部、腹部のテンプレ", "本文").is_err());
        assert!(dictionary.save_template(None, "テンプレート1", "本文").is_ok(), "終わりでなければよい");
    }

    /// 画面の欄から保存し直しても、欄に無い項目(diagnosis など)は同じ名前のモジュールから引き継ぐ。
    #[test]
    fn editing_in_the_tool_keeps_module_extras() {
        let json = r#"{"keywords": ["腰椎MRI"], "body": "本文",
            "modules": [{"name": "変形性腰椎症", "text": "文", "note": "全体を差し替える", "diagnosis": "変形性腰椎症。"}]}"#;
        let mut dictionary = Dictionary::default();
        dictionary.templates.push(serde_json::from_str(json).unwrap());
        let text = modules_text(&dictionary.templates[0].modules);
        assert!(!text.contains("変形性腰椎症。"), "欄には出ない: {text}");
        let edited = format!("{text}\n【圧迫骨折】\n骨折の文\n");
        dictionary
            .save_template_with_modules(Some(0), "腰椎MRI", "新しい本文", &edited)
            .unwrap();
        let modules = &dictionary.templates[0].modules;
        assert_eq!(modules.len(), 2);
        assert_eq!(modules[0].extra["diagnosis"], "変形性腰椎症。");
        assert_eq!(modules[0].note, "全体を差し替える");
        assert!(modules[1].extra.is_empty());
    }

    /// 手で名前だけ書いた modules(`["肝", "胆"]`)や null でも辞書全体は読める。
    #[test]
    fn modules_written_as_names_still_load() {
        let names: Template =
            serde_json::from_str(r#"{"keywords": ["腹部CT"], "body": "本文", "modules": ["肝", "胆"]}"#).unwrap();
        assert_eq!(names.modules.len(), 2);
        assert_eq!(names.modules[1].name, "胆");
        assert_eq!(names.modules[1].text, "");
        let null: Template =
            serde_json::from_str(r#"{"keywords": ["腹部CT"], "body": "本文", "modules": null}"#).unwrap();
        assert!(null.modules.is_empty());
        let dictionary: Dictionary =
            serde_json::from_str(r#"{"templates": [{"keywords": ["a"], "body": "b", "modules": ["肝"]}]}"#).unwrap();
        assert_eq!(dictionary.templates[0].modules[0].name, "肝");
    }

    /// 所見モジュールの欄: 【名前】で始まり、文、任意の「使い方:」。書いた形に戻せる。
    #[test]
    fn modules_are_parsed_and_written_back() {
        let text = "\n【PI-RADS カテゴリー4/5】\n〔部位〕に結節を認めます。\n被膜外浸潤はありません。\n使い方: 正常文の「辺縁域…」と差し替える\n\n【肉柱形成】\n膀胱壁は肥厚しています。\n";
        let modules = parse_modules(text).unwrap();
        assert_eq!(modules.len(), 2);
        assert_eq!(modules[0].name, "PI-RADS カテゴリー4/5");
        assert_eq!(modules[0].text, "〔部位〕に結節を認めます。\n被膜外浸潤はありません。");
        assert_eq!(modules[0].note, "正常文の「辺縁域…」と差し替える");
        assert_eq!(modules[1].note, "");
        let written = modules_text(&modules);
        assert_eq!(parse_modules(&written).unwrap(), modules);
        assert!(written.starts_with("【PI-RADS カテゴリー4/5】\n〔部位〕"), "{written}");
        assert_eq!(parse_modules("  \n").unwrap(), Vec::new());
        assert_eq!(modules_text(&[]), "");
        for bad in ["文だけ", "【】\n文", "【a】\n", "【a】\n文\n【a】\n文"] {
            assert!(parse_modules(bad).is_err(), "{bad}");
        }
    }

    /// ファイルの形: モジュールの文も行の配列でよく、使い方が無ければ書かない。知らない項目は残る。
    #[test]
    fn modules_read_as_text_or_lines_and_keep_extra_fields() {
        let json = r#"{"keywords": ["前立腺MRI"], "body": "本文",
            "modules": [{"name": "肉柱形成", "text": ["膀胱壁は肥厚し、", "肉柱形成もみられます。"], "memo": "x"},
                        {"name": "PI-RADS 4/5", "text": "結節を認めます。", "note": "差し替える"}]}"#;
        let template: Template = serde_json::from_str(json).unwrap();
        assert_eq!(template.modules[0].text, "膀胱壁は肥厚し、\n肉柱形成もみられます。");
        assert_eq!(template.modules[0].note, "");
        assert_eq!(template.modules[0].extra["memo"], "x");
        assert_eq!(template.modules[1].note, "差し替える");
        let written = serde_json::to_value(&template).unwrap();
        assert!(written["modules"][0].get("note").is_none(), "{written}");
        assert_eq!(written["modules"][1]["note"], "差し替える");
        assert_eq!(written["modules"][0]["memo"], "x");
        let plain: Template = serde_json::from_str(r#"{"keywords": ["a"], "body": "b"}"#).unwrap();
        assert!(plain.modules.is_empty());
        assert!(serde_json::to_value(&plain).unwrap().get("modules").is_none(), "空なら書かない");
        // 画面から本文だけ直してもモジュールは残る。
        let mut dictionary = Dictionary::default();
        dictionary.templates.push(template);
        assert_eq!(dictionary.save_template(Some(0), "前立腺MRI", "新しい本文"), Ok(0));
        assert_eq!(dictionary.templates[0].modules.len(), 2);
        assert_eq!(dictionary.save_template(None, "胸部CT", "本文"), Ok(1));
    }

    /// 辞書ファイルの形: 本文は文字列でも行の配列でもよく、書くときは複数行なら配列。知らない項目は残す。
    #[test]
    fn template_body_reads_as_text_or_lines_and_keeps_extra_fields() {
        let json = r#"{"keywords": ["胸部正常"], "body": ["異常なし。", "以上。"], "memo": "自由項目", "group": 2}"#;
        let template: Template = serde_json::from_str(json).unwrap();
        assert_eq!(template.body, "異常なし。\n以上。");
        assert_eq!(template.extra["memo"], "自由項目");
        assert_eq!(template.extra["group"], 2);
        let written = serde_json::to_value(&template).unwrap();
        assert_eq!(written["body"], serde_json::json!(["異常なし。", "以上。"]));
        assert_eq!(written["memo"], "自由項目");
        let one_line: Template = serde_json::from_str(r#"{"keywords": ["a"], "body": "一行"}"#).unwrap();
        assert_eq!(one_line.body, "一行");
        assert_eq!(serde_json::to_value(&one_line).unwrap()["body"], "一行");
        // 画面から本文を直しても自由項目は残る。
        let mut dictionary = Dictionary::default();
        dictionary.templates.push(template);
        dictionary.save_template(Some(0), "胸部正常", "新しい本文").unwrap();
        assert_eq!(dictionary.templates[0].extra["memo"], "自由項目");
    }

    /// 項目が欠けた辞書ファイルも読める。空のファイルは既定の記号入りになる。
    #[test]
    fn a_sparse_dictionary_file_gets_defaults() {
        let dictionary: Dictionary = serde_json::from_str("{}").unwrap();
        assert_eq!(dictionary, Dictionary::default());
        assert_eq!(dictionary.symbols.len(), 8);
        assert_eq!(dictionary.note, DICTIONARY_NOTE);
        let no_symbols: Dictionary = serde_json::from_str(r#"{"symbols": []}"#).unwrap();
        assert!(no_symbols.symbols.is_empty(), "空にしたなら空のまま");
        let symbol: Symbol = serde_json::from_str(r#"{"readings": ["ぷらす"], "text": "＋"}"#).unwrap();
        assert_eq!(symbol, Symbol::new(&["ぷらす"], "＋"));
    }

    #[test]
    fn default_symbols_cover_the_eight_readings() {
        let symbols = default_symbols();
        let find = |reading: &str| {
            symbols
                .iter()
                .find(|s| s.readings.iter().any(|r| r == reading))
                .map(|s| s.text.as_str())
        };
        for (reading, text) in [
            ("まる", "。"),
            ("てん", "、"),
            ("かっこ", "（"),
            ("かっことじる", "）"),
            ("やじるし", "→"),
            ("かける", "×"),
            ("いこーる", "＝"),
            ("にょろ", "～"),
        ] {
            assert_eq!(find(reading), Some(text), "{reading}");
        }
        assert_eq!(to_katakana("いこーる"), "イコール");
    }

    #[test]
    fn the_suffix_is_stripped_from_the_normalized_key() {
        assert_eq!(strip_template_suffix("胸部ctてんぷれーと"), Some("胸部ct"));
        assert_eq!(strip_template_suffix("胸部ctのてんぷれ"), Some("胸部ctの"));
        assert_eq!(strip_template_suffix("てんぷれーと"), None);
        assert_eq!(strip_template_suffix("胸部ct"), None);
    }
}
