//! テンプレートの構造(派生)と描画。`docs/simplify-and-assistant-plan.md` §16(単位 S)。
//!
//! 三層のうち「構造」と「実体の描画」をここに置く。源泉(`dictionary.rs` の `Template`)は触らない。
//! 構造は源泉のハッシュを鍵に別ファイル(`structure.json`)に持ち、行ごとの鍵(項目)、モジュールごとの
//! 変種(対象・置き方・極性・条件・auto・役割・程度)、尺度のある所見群を記す。描画はコードだけで行い、
//! Claude は「どの変種をどの文で」(選択)だけを返す(§16.6)。法則 L1〜L7(§16.3)は試験で確かめる。
//!
//! このファイルは LLM に触れない。利用者のファイルも書かない(読み書きは呼び出す側)。

use super::dictionary::{strip_template_suffix, template_key, Dictionary, Module, Template};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// アプリ同梱の語彙(§16.7)。利用者の追加は `StructureFile` の `*_extra`。
pub const VOCABULARY_JSON: &str = include_str!("../../assets/vocabulary.json");

/// 〔　〕(充填の空欄)の印。全角括弧で始まる箇所があれば充填の対象とみなす。
pub const PLACEHOLDER_OPEN: char = '〔';

/// 全体差し替えの対象の印(`target: ["*"]`)。
pub const WHOLE: &str = "*";

// ------------------------------------------------------------------------------------------------
// 語彙
// ------------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
pub struct SlotGroup {
    pub region: String,
    pub keys: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RoleDef {
    pub name: String,
    #[serde(default)]
    pub meaning: String,
    /// 既定の区間(尺度の既定 軽度・中等度・高度 の連続した部分列)。
    pub grades: Vec<String>,
}

/// 閉じた語彙。鍵・札・役割と既定の区間・程度語・一般語。
#[derive(Debug, Clone, Deserialize)]
pub struct Vocabulary {
    #[serde(default)]
    pub version: u32,
    pub slots: Vec<SlotGroup>,
    /// 鍵の意味(注釈役に見せる用語集。§16.7 の「今の出どころ」を一般の言い方にした物)。無い鍵もある。
    #[serde(default)]
    pub slot_notes: BTreeMap<String, String>,
    /// 列挙否定の鍵と、その既定の臓器の並び(注釈で行を列挙否定として読むときの手がかり)。
    #[serde(default)]
    pub list_slots: BTreeMap<String, Vec<String>>,
    /// 性別の決まった足す鍵(前立腺は男、子宮・付属器は女)。足す変種の条件の性別になる。
    #[serde(default)]
    pub slot_sex: BTreeMap<String, String>,
    pub purposes: Vec<String>,
    /// 精査目的の札の別の書き方(目眩 → めまい)。依頼の文から文脈を取るときに使う(S3、`context.rs`)。
    #[serde(default)]
    pub purpose_aliases: BTreeMap<String, Vec<String>>,
    #[serde(default)]
    pub age_bands: BTreeMap<String, (u32, u32)>,
    pub default_scale: Vec<String>,
    pub roles: Vec<RoleDef>,
    #[serde(default)]
    pub grade_words: BTreeMap<String, Vec<String>>,
    #[serde(default)]
    pub grade_word_exclusions: Vec<String>,
    #[serde(default)]
    pub modality_words: Vec<String>,
    #[serde(default)]
    pub general_words: Vec<String>,
}

impl Vocabulary {
    /// 同梱の語彙。読めなければ panic(ビルドに入っている JSON なので、試験で必ず通る)。
    pub fn builtin() -> Self {
        serde_json::from_str(VOCABULARY_JSON).expect("assets/vocabulary.json")
    }

    /// 利用者の追加・上書き(`structure.json` の `*_extra`)を重ねる。役割の既定表は名前が同じなら上書き。
    pub fn with_extras(mut self, file: &StructureFile) -> Self {
        if !file.vocabulary_extra.is_empty() {
            self.slots.push(SlotGroup {
                region: "追加".to_string(),
                keys: file.vocabulary_extra.clone(),
            });
        }
        for purpose in &file.purposes_extra {
            if !self.purposes.contains(purpose) {
                self.purposes.push(purpose.clone());
            }
        }
        for (name, grades) in &file.roles_extra {
            match self.roles.iter_mut().find(|r| &r.name == name) {
                Some(role) => role.grades = grades.clone(),
                None => self.roles.push(RoleDef {
                    name: name.clone(),
                    meaning: String::new(),
                    grades: grades.clone(),
                }),
            }
        }
        self.general_words.extend(file.general_words_extra.iter().cloned());
        self
    }

    pub fn has_slot(&self, key: &str) -> bool {
        self.slots.iter().any(|group| group.keys.iter().any(|k| k == key))
    }

    pub fn slot_keys(&self) -> impl Iterator<Item = &str> {
        self.slots.iter().flat_map(|group| group.keys.iter().map(String::as_str))
    }

    pub fn role(&self, name: &str) -> Option<&RoleDef> {
        self.roles.iter().find(|r| r.name == name)
    }

    /// 口述の程度語(対応表の語をすべて)。候補の絞り込みで除くのにも使う。
    pub fn grade_word_list(&self) -> Vec<&str> {
        let mut words: Vec<&str> = self.grade_words.values().flatten().map(String::as_str).collect();
        words.extend(self.grade_words.keys().map(String::as_str));
        words
    }
}

// ------------------------------------------------------------------------------------------------
// 構造(派生)の形。§16.5
// ------------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum LineKind {
    #[default]
    Sentence,
    ListNegation,
    Heading,
    Blank,
}

/// 源泉の 1 行の構造。`i` は本文の行番号。列挙否定は臓器の集合 + 区切り + 型(`{organs}` が臓器の並び)。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct LineInfo {
    pub i: usize,
    pub kind: LineKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub slot: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub organs: Vec<String>,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub sep: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub pattern: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    #[default]
    Replace,
    Prepend,
    Append,
    /// 外す(派生だけ)。全体差し替えを行に割ったとき、短い形に無い既定文を消す。文を置かない。
    Remove,
}

/// 文脈への条件。`sex` は 男/女、`age_band` は 若年/成人/高齢、`purpose` は札(§16.7)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Condition {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sex: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub age_band: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub purpose: Option<String>,
}

impl Condition {
    pub fn is_empty(&self) -> bool {
        self.sex.is_none() && self.age_band.is_none() && self.purpose.is_none()
    }

    /// 文脈に合うか。条件の無い項目は見ない。文脈に無い(不明の)属性は合わない。
    pub fn matches(&self, context: &Context) -> bool {
        self.sex.as_ref().is_none_or(|s| context.sex.as_deref() == Some(s.as_str()))
            && self
                .age_band
                .as_ref()
                .is_none_or(|a| context.age_band.as_deref() == Some(a.as_str()))
            && self
                .purpose
                .as_ref()
                .is_none_or(|p| context.purposes.iter().any(|q| q == p))
    }

    /// 文脈と食い違うか。文脈に無い(不明の)属性は食い違わない。口述で選ばれた変種を退けるのはこのときだけ。
    pub fn contradicts(&self, context: &Context) -> bool {
        self.sex
            .as_ref()
            .is_some_and(|s| context.sex.as_ref().is_some_and(|c| c != s))
            || self
                .age_band
                .as_ref()
                .is_some_and(|a| context.age_band.as_ref().is_some_and(|c| c != a))
            || self
                .purpose
                .as_ref()
                .is_some_and(|p| !context.purposes.is_empty() && !context.purposes.iter().any(|q| q == p))
    }

    /// 「女・若年」「目的=めまい」の形(Markdown の `> 条件:` と知らせ)。
    pub fn label(&self) -> String {
        let mut parts = Vec::new();
        if let Some(sex) = &self.sex {
            parts.push(sex.clone());
        }
        if let Some(age) = &self.age_band {
            parts.push(age.clone());
        }
        if let Some(purpose) = &self.purpose {
            parts.push(format!("目的={purpose}"));
        }
        parts.join("・")
    }
}

/// 一通の補完の文脈。コードが依頼の文から取る(S3)。すべて任意。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Context {
    pub sex: Option<String>,
    pub age_band: Option<String>,
    pub purposes: Vec<String>,
}

/// 程度の尺度。既定は語彙の `default_scale`。名前付きは `{"name": "Fazekas", "levels": ["1","2","3"]}`。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Scale {
    Levels(Vec<String>),
    Named { name: String, levels: Vec<String> },
}

impl Scale {
    pub fn levels(&self) -> &[String] {
        match self {
            Scale::Levels(levels) | Scale::Named { levels, .. } => levels,
        }
    }

    pub fn name(&self) -> Option<&str> {
        match self {
            Scale::Levels(_) => None,
            Scale::Named { name, .. } => Some(name),
        }
    }
}

/// 変種 = 今のモジュール(束なら行ごと、束でなければモジュール単位)。§16.3 の組。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Variant {
    pub module: String,
    /// 源泉のモジュールの何行目か。束でない複数行・外す(派生)・全体差し替えの印は null。
    pub line: Option<usize>,
    /// 対象の項目(鍵)。足す鍵のある変種では置き場所の目印。`["*"]` は全体差し替え。
    pub target: Vec<String>,
    pub mode: Mode,
    /// negative / benign / abnormal / mixed。外すは null。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub polarity: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub condition: Option<Condition>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub auto: bool,
    /// 本文に無い項目を足す変種の鍵(足す鍵)。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub slot: Option<String>,
    /// 列挙否定から外す臓器。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub organ: Option<String>,
    /// 続く既定文の文頭に付ける語(「その他、」)。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prefix: Option<String>,
    /// 派生で行に割った源泉の全体差し替えモジュールの印(Claude には割った行を見せる)。
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub split: bool,
    /// 源泉に無い派生だけの変種(外す)。
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub derived: bool,
    /// 変化の役割(初期・進行・波及・障害・帰結・陰性)。区間の既定を与える。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    /// 出る程度の区間(尺度の連続した部分列)。明示したときだけ。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub grades: Option<Vec<String>>,
    /// 変種の文が列挙否定のとき(若い女性の T0)。行と同じ `organs`・`sep`・`pattern` と、臓器→項目の対応。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<LineKind>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub organs: Vec<String>,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub sep: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub pattern: String,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub organ_slots: BTreeMap<String, Vec<String>>,
}

impl Variant {
    /// 全体差し替えの印(源泉のモジュール単位の対象 `*`)。
    pub fn is_whole(&self) -> bool {
        self.target.iter().any(|t| t == WHOLE)
    }
}

/// 尺度のある所見群(モジュール)。`scale` が無ければ語彙の既定。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Group {
    pub module: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scale: Option<Scale>,
}

/// 1 テンプレートの構造。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct TemplateStructure {
    pub name: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub annotated: String,
    pub lines: Vec<LineInfo>,
    pub variants: Vec<Variant>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub groups: Vec<Group>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub unresolved: Vec<String>,
}

pub const STRUCTURE_NOTE: &str =
    "OpenSpeech が付けた構造。手で直さず、辞書の「構造」で直してください。消しても付け直せます。";

/// `structure.json` 全体。鍵は源泉のハッシュ(`source_hash`)。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct StructureFile {
    #[serde(rename = "_note", skip_serializing_if = "String::is_empty")]
    pub note: String,
    pub vocabulary_version: u32,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub vocabulary_extra: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub purposes_extra: Vec<String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub roles_extra: BTreeMap<String, Vec<String>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub general_words_extra: Vec<String>,
    pub templates: BTreeMap<String, TemplateStructure>,
}

impl Default for StructureFile {
    fn default() -> Self {
        StructureFile {
            note: STRUCTURE_NOTE.to_string(),
            vocabulary_version: 1,
            vocabulary_extra: Vec::new(),
            purposes_extra: Vec::new(),
            roles_extra: BTreeMap::new(),
            general_words_extra: Vec::new(),
            templates: BTreeMap::new(),
        }
    }
}

impl StructureFile {
    /// テンプレートの構造(源泉のハッシュで引く)。無ければ未注釈。
    pub fn structure_for(&self, template: &Template) -> Option<&TemplateStructure> {
        self.templates.get(&source_hash(template))
    }

    pub fn insert(&mut self, template: &Template, structure: TemplateStructure) {
        self.templates.insert(source_hash(template), structure);
    }

    /// 合うテンプレートの無い項を消す(保存時)。消した数を返す。
    pub fn prune(&mut self, templates: &[Template]) -> usize {
        let keep: BTreeSet<String> = templates.iter().map(source_hash).collect();
        let before = self.templates.len();
        self.templates.retain(|hash, _| keep.contains(hash));
        before - self.templates.len()
    }
}

// ------------------------------------------------------------------------------------------------
// 源泉のハッシュ。§16.5
// ------------------------------------------------------------------------------------------------

/// 行の末尾の空白(半角空白・タブ・全角空白)を除く。区切りの U+001E は空白ではない。
fn trim_line_end(line: &str) -> &str {
    line.trim_end_matches([' ', '\t', '\u{3000}'])
}

/// 文章を行に揃える: 改行で割り、各行の末尾の空白を除き、末尾の空行を除く。
pub fn canonical_lines(text: &str) -> Vec<&str> {
    // CR は辞書には無いが念のため除く(`\r\n` も `\n` と同じ区切りになる)。
    let mut lines: Vec<&str> = text
        .split('\n')
        .map(|l| trim_line_end(l.trim_end_matches('\r')))
        .collect();
    while lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }
    lines
}

/// ハッシュの元になる正規化した文字列。本文の行を `\n` で繋ぎ、モジュールごとに U+001E、名前、`\n`、
/// 文の行(`\n` 繋ぎ)、`\n`、使い方の行(同じ規則)を続ける。`extra`・`memo`・`keywords` は含めない。
/// Python の参照実装 `scripts/source_hash.py` と同じ値になる。NFC 正規化は行わない(保存された文字のまま)。
pub fn canonical_source(template: &Template) -> String {
    let mut text = canonical_lines(&template.body).join("\n");
    for module in &template.modules {
        text.push('\u{1e}');
        text.push_str(trim_line_end(&module.name));
        text.push('\n');
        text.push_str(&canonical_lines(&module.text).join("\n"));
        text.push('\n');
        text.push_str(&canonical_lines(&module.note).join("\n"));
    }
    text
}

/// 源泉のハッシュ(`sha256:` + 16 進 64 桁)。テンプレートの同一性。
pub fn source_hash(template: &Template) -> String {
    format!("sha256:{}", sha256_hex(canonical_source(template).as_bytes()))
}

/// SHA-256(FIPS 180-4)。外部 crate を足さないために自前で持つ。試験で既知の値と `shasum` に当てる。
pub fn sha256_hex(data: &[u8]) -> String {
    const K: [u32; 64] = [
        0x428a_2f98, 0x7137_4491, 0xb5c0_fbcf, 0xe9b5_dba5, 0x3956_c25b, 0x59f1_11f1, 0x923f_82a4, 0xab1c_5ed5,
        0xd807_aa98, 0x1283_5b01, 0x2431_85be, 0x550c_7dc3, 0x72be_5d74, 0x80de_b1fe, 0x9bdc_06a7, 0xc19b_f174,
        0xe49b_69c1, 0xefbe_4786, 0x0fc1_9dc6, 0x240c_a1cc, 0x2de9_2c6f, 0x4a74_84aa, 0x5cb0_a9dc, 0x76f9_88da,
        0x983e_5152, 0xa831_c66d, 0xb003_27c8, 0xbf59_7fc7, 0xc6e0_0bf3, 0xd5a7_9147, 0x06ca_6351, 0x1429_2967,
        0x27b7_0a85, 0x2e1b_2138, 0x4d2c_6dfc, 0x5338_0d13, 0x650a_7354, 0x766a_0abb, 0x81c2_c92e, 0x9272_2c85,
        0xa2bf_e8a1, 0xa81a_664b, 0xc24b_8b70, 0xc76c_51a3, 0xd192_e819, 0xd699_0624, 0xf40e_3585, 0x106a_a070,
        0x19a4_c116, 0x1e37_6c08, 0x2748_774c, 0x34b0_bcb5, 0x391c_0cb3, 0x4ed8_aa4a, 0x5b9c_ca4f, 0x682e_6ff3,
        0x748f_82ee, 0x78a5_636f, 0x84c8_7814, 0x8cc7_0208, 0x90be_fffa, 0xa450_6ceb, 0xbef9_a3f7, 0xc671_78f2,
    ];
    let mut state: [u32; 8] = [
        0x6a09_e667, 0xbb67_ae85, 0x3c6e_f372, 0xa54f_f53a, 0x510e_527f, 0x9b05_688c, 0x1f83_d9ab, 0x5be0_cd19,
    ];
    let mut message = data.to_vec();
    let bit_length = (data.len() as u64).wrapping_mul(8);
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&bit_length.to_be_bytes());
    for chunk in message.chunks(64) {
        let mut w = [0u32; 64];
        for (i, word) in w.iter_mut().enumerate().take(16) {
            *word = u32::from_be_bytes([chunk[4 * i], chunk[4 * i + 1], chunk[4 * i + 2], chunk[4 * i + 3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16].wrapping_add(s0).wrapping_add(w[i - 7]).wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = state;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ (!e & g);
            let t1 = h.wrapping_add(s1).wrapping_add(ch).wrapping_add(K[i]).wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        for (slot, value) in state.iter_mut().zip([a, b, c, d, e, f, g, h]) {
            *slot = slot.wrapping_add(value);
        }
    }
    state.iter().map(|word| format!("{word:08x}")).collect()
}

// ------------------------------------------------------------------------------------------------
// 選択と描画。§16.3・§16.6・§16.13
// ------------------------------------------------------------------------------------------------

/// Claude が返す選択(§16.6)。充填 `{"line", "text"}`、変種 `{"module", "line"?, "grade"?, "text"}`、
/// 臨時 `{"slot", "organ"?, "text", "adhoc": true}`。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Selection {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub module: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub grade: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub slot: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub organ: Option<String>,
    pub text: String,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub adhoc: bool,
}

impl Selection {
    pub fn fill(line: usize, text: &str) -> Self {
        Selection {
            line: Some(line),
            text: text.to_string(),
            ..Selection::default()
        }
    }

    pub fn variant(module: &str, line: Option<usize>, text: &str) -> Self {
        Selection {
            module: Some(module.to_string()),
            line,
            text: text.to_string(),
            ..Selection::default()
        }
    }

    pub fn graded(module: &str, line: usize, grade: &str, text: &str) -> Self {
        Selection {
            module: Some(module.to_string()),
            line: Some(line),
            grade: Some(grade.to_string()),
            text: text.to_string(),
            ..Selection::default()
        }
    }

    pub fn adhoc(slot: &str, organ: Option<&str>, text: &str) -> Self {
        Selection {
            slot: Some(slot.to_string()),
            organ: organ.map(str::to_string),
            text: text.to_string(),
            adhoc: true,
            ..Selection::default()
        }
    }
}

/// 描画の結果。`notes` は印と知らせ(捨てた選択、補った行、文脈に合わない変種)。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Rendered {
    pub text: String,
    pub notes: Vec<String>,
}

/// 描画できない理由。呼び出す側は全文経路に落とす(§16.6)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RenderError {
    /// 全体差し替えが選ばれた(D9・L6)。
    WholeReplacement(String),
    /// 構造に無いモジュール、本文と合わない構造など。中身は理由。
    Unplaceable(String),
}

impl std::fmt::Display for RenderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RenderError::WholeReplacement(reason) => write!(f, "全体差し替え: {reason}"),
            RenderError::Unplaceable(reason) => write!(f, "置けない: {reason}"),
        }
    }
}

pub fn has_placeholder(text: &str) -> bool {
    text.contains(PLACEHOLDER_OPEN)
}

/// モジュールの文を行に割る(源泉の順。ハッシュと同じく末尾の空白と末尾の空行は除く)。
pub fn module_lines(module: &Module) -> Vec<&str> {
    canonical_lines(&module.text)
}

fn find_module<'a>(template: &'a Template, name: &str) -> Option<(usize, &'a Module)> {
    template.modules.iter().enumerate().find(|(_, m)| m.name == name)
}

/// 列挙否定の文を、残った臓器で繋ぎ直す。全部外れたら None(行ごと消す)。
pub fn rebuild_list(organs: &[String], removed: &BTreeSet<String>, sep: &str, pattern: &str) -> Option<String> {
    let kept: Vec<&str> = organs.iter().filter(|o| !removed.contains(*o)).map(String::as_str).collect();
    if kept.is_empty() {
        return None;
    }
    Some(pattern.replace("{organs}", &kept.join(sep)))
}

/// 描画に入れる 1 つの置き物。`order` は源泉の順(構造の変種の番号。臨時と充填は変種の数 + 本文の行番号)で、
/// 同じ項目の中の並びを決める。`from_list` は列挙否定の変種(T0)自身の文(出力には繋ぎ直した文を使う)。
#[derive(Debug, Clone)]
struct Piece {
    order: usize,
    text: String,
    from_list: bool,
}

/// 行ごとの置き物。
#[derive(Debug, Clone, Default)]
struct LineState {
    prepend: Vec<Piece>,
    replace: Vec<Piece>,
    append: Vec<Piece>,
    fill: Option<Piece>,
    removed: bool,
    /// この行が吸収された先の行(若い女性の T0 のように複数の項目を 1 行で差し替えたとき)。
    absorbed_into: Option<usize>,
    /// 既定文(列挙否定)から外した臓器と、その直後に置く文(臓器, 順, 文)。
    removed_organs: BTreeSet<String>,
    after_list: Vec<(String, usize, String)>,
    /// 続く既定文の文頭に付ける語。
    prefix: Option<String>,
    /// 列挙否定の変種(T0)がこの行に置かれたとき、その臓器の集合と外した臓器。
    list_variant: Option<ListVariant>,
}

#[derive(Debug, Clone)]
struct ListVariant {
    organs: Vec<String>,
    sep: String,
    pattern: String,
    organ_slots: BTreeMap<String, Vec<String>>,
    removed: BTreeSet<String>,
    after: Vec<(usize, usize, String)>,
}

/// 置き方の決まった選択(描画の前の中間の形)。
#[derive(Debug, Clone)]
struct Action {
    order: usize,
    variant: Option<usize>,
    module: Option<String>,
    targets: Vec<String>,
    mode: Mode,
    organ: Option<String>,
    add_slot: Option<String>,
    prefix: Option<String>,
    text: String,
    grade: Option<String>,
}

/// 充填の置き物の `add_slot` に入れる印(`"\0fill:" + 行番号`)。足す鍵と見分ける。
const FILL_MARK: &str = "\u{0}fill:";

/// 描画。選択(と文脈で当たる auto の変種)を源泉の本文に置く。L1〜L7 の規則はここに集まる。
pub fn render(
    template: &Template,
    structure: &TemplateStructure,
    vocabulary: &Vocabulary,
    context: &Context,
    selections: &[Selection],
) -> Result<Rendered, RenderError> {
    // 本文はハッシュと同じ正規化(末尾の空白・末尾の空行を除く)で行に割る。同じハッシュなら同じに描ける。
    let body: Vec<&str> = canonical_lines(&template.body);
    if body.len() != structure.lines.len() {
        return Err(RenderError::Unplaceable(format!(
            "構造の行数 {} が本文の行数 {} と違う",
            structure.lines.len(),
            body.len()
        )));
    }
    if let Some((index, info)) = structure.lines.iter().enumerate().find(|(i, l)| l.i != *i) {
        return Err(RenderError::Unplaceable(format!("構造の {index} 行目の番号が {}", info.i)));
    }
    let mut notes: Vec<String> = Vec::new();
    let slot_lines = |slot: &str| -> Vec<usize> {
        structure
            .lines
            .iter()
            .filter(|l| l.slot.as_deref() == Some(slot))
            .map(|l| l.i)
            .collect()
    };

    // ---- 1. 選択を置き物に直す ----
    let mut actions: Vec<Action> = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let adhoc_base = structure.variants.len();
    for selection in selections {
        if !seen.insert(serde_json::to_string(selection).unwrap_or_default()) {
            continue; // L4: 同じ選択(全項目が同じ)を二度入れても一度
        }
        if let Some(module) = &selection.module {
            let found: Vec<(usize, &Variant)> = structure
                .variants
                .iter()
                .enumerate()
                .filter(|(_, v)| &v.module == module && !v.is_whole() && !v.derived)
                .collect();
            if found.is_empty() {
                // 源泉の全体差し替え(印だけの物)なら L6・D9 で全文経路。
                if structure.variants.iter().any(|v| &v.module == module && v.is_whole()) {
                    return Err(RenderError::WholeReplacement(module.clone()));
                }
                return Err(RenderError::Unplaceable(format!("モジュール「{module}」が構造に無い")));
            }
            let chosen = match selection.line {
                // 束でないモジュールに行番号を付けて返されても受ける(モジュール単位の変種が 1 つなら)。
                Some(i) => found
                    .iter()
                    .find(|(_, v)| v.line == Some(i))
                    .copied()
                    .or_else(|| (found.len() == 1 && found[0].1.line.is_none()).then(|| found[0])),
                None if found.len() == 1 => Some(found[0]),
                None => found.iter().find(|(_, v)| v.line.is_none()).copied(),
            };
            let Some((index, variant)) = chosen else {
                // 行ごとに割ったモジュールを行なしで選ぶのは、源泉の全体差し替えを選んだのと同じ(D6・L6)。
                if structure.variants.iter().any(|v| &v.module == module && v.is_whole()) {
                    return Err(RenderError::WholeReplacement(module.clone()));
                }
                return Err(RenderError::Unplaceable(format!(
                    "モジュール「{module}」の行 {:?} が構造に無い",
                    selection.line
                )));
            };
            if variant.is_whole() {
                return Err(RenderError::WholeReplacement(module.clone()));
            }
            if let Some(condition) = &variant.condition {
                if condition.contradicts(context) {
                    notes.push(format!(
                        "「{module}」は文脈({})と食い違うので置きませんでした",
                        condition.label()
                    ));
                    continue;
                }
            }
            if let Some(grade) = &selection.grade {
                let levels = group_levels(structure, vocabulary, module);
                match levels {
                    Some(levels) if !levels.contains(grade) => {
                        notes.push(format!("「{module}」の程度「{grade}」は尺度に無いので置きませんでした"));
                        continue;
                    }
                    None => notes.push(format!("「{module}」に尺度は無いので程度「{grade}」は見ませんでした")),
                    _ => {}
                }
            }
            actions.push(Action {
                order: index,
                variant: Some(index),
                module: Some(module.clone()),
                targets: variant.target.clone(),
                mode: variant.mode,
                organ: variant.organ.clone(),
                add_slot: variant.slot.clone(),
                prefix: variant.prefix.clone(),
                text: selection.text.clone(),
                grade: selection.grade.clone(),
            });
        } else if let Some(slot) = &selection.slot {
            // 臨時の充填。鍵は語彙から、項目は本文にある物。
            if !vocabulary.has_slot(slot) {
                notes.push(format!("臨時の鍵「{slot}」は語彙に無いので置きませんでした"));
                continue;
            }
            let Some(&first_line) = slot_lines(slot).first() else {
                notes.push(format!("臨時の鍵「{slot}」は本文に無いので置きませんでした"));
                continue;
            };
            actions.push(Action {
                order: adhoc_base + first_line,
                variant: None,
                module: None,
                targets: vec![slot.clone()],
                mode: Mode::Replace,
                organ: selection.organ.clone(),
                add_slot: None,
                prefix: None,
                text: selection.text.clone(),
                grade: None,
            });
        } else if let Some(line) = selection.line {
            // 本文の行の充填。
            let Some(info) = structure.lines.get(line) else {
                notes.push(format!("充填の行 {line} は本文に無いので置きませんでした"));
                continue;
            };
            if !has_placeholder(body[line]) {
                notes.push(format!("本文 {line} 行目に〔　〕は無いので充填しませんでした"));
                continue;
            }
            // 充填は項目の口述とみなす(auto の差し替えを退ける)が、置くのは行番号で(下の 4a)。
            let slot = info.slot.clone().unwrap_or_default();
            actions.push(Action {
                order: adhoc_base + line,
                variant: None,
                module: None,
                targets: vec![slot],
                mode: Mode::Replace,
                organ: None,
                add_slot: Some(format!("{FILL_MARK}{line}")),
                prefix: None,
                text: selection.text.clone(),
                grade: None,
            });
        } else {
            notes.push("種類の分からない選択を捨てました".to_string());
        }
    }

    // ---- 2. 程度で行を絞る(L7)。所見群ごとに、選択の程度で出ない行を捨て、出るのに無い行を補う ----
    for group in &structure.groups {
        let levels = group_levels(structure, vocabulary, &group.module).unwrap_or_default();
        let grades: Vec<String> = actions
            .iter()
            .filter(|a| a.module.as_deref() == Some(group.module.as_str()))
            .filter_map(|a| a.grade.clone())
            .collect();
        let Some(grade) = grades.first().cloned() else {
            continue; // 程度が言われなければ絞らない
        };
        if grades.iter().any(|g| *g != grade) {
            notes.push(format!("「{}」の程度が揃っていないので「{grade}」で揃えました", group.module));
        }
        let Some((_, module)) = find_module(template, &group.module) else {
            continue;
        };
        let source_lines = module_lines(module);
        let mut kept: Vec<Action> = Vec::new();
        let mut selected_lines: BTreeSet<usize> = BTreeSet::new();
        for action in actions.drain(..) {
            if action.module.as_deref() != Some(group.module.as_str()) {
                kept.push(action);
                continue;
            }
            let variant = &structure.variants[action.variant.unwrap_or(0)];
            let interval = interval_of(variant, vocabulary, &levels);
            if !interval.contains(&grade) {
                notes.push(format!(
                    "「{}」の {} 行目は程度「{grade}」では出ないので捨てました",
                    group.module,
                    variant.line.map_or("?".to_string(), |l| l.to_string())
                ));
                continue;
            }
            if let Some(line) = variant.line {
                selected_lines.insert(line);
            }
            kept.push(action);
        }
        actions = kept;
        for (index, variant) in structure.variants.iter().enumerate() {
            if variant.module != group.module || variant.is_whole() || variant.derived {
                continue;
            }
            let Some(line) = variant.line else { continue };
            if selected_lines.contains(&line) {
                continue;
            }
            if !interval_of(variant, vocabulary, &levels).contains(&grade) {
                continue;
            }
            if let Some(condition) = &variant.condition {
                if !condition.matches(context) {
                    continue;
                }
            }
            let Some(text) = source_lines.get(line) else { continue };
            if has_placeholder(text) {
                notes.push(format!(
                    "「{}」の {line} 行目は程度「{grade}」で出る行ですが〔　〕があるので補えませんでした",
                    group.module
                ));
                continue;
            }
            notes.push(format!("「{}」の {line} 行目を源泉の文のまま補いました", group.module));
            actions.push(Action {
                order: index,
                variant: Some(index),
                module: Some(group.module.clone()),
                targets: variant.target.clone(),
                mode: variant.mode,
                organ: variant.organ.clone(),
                add_slot: variant.slot.clone(),
                prefix: variant.prefix.clone(),
                text: text.to_string(),
                grade: Some(grade.clone()),
            });
        }
    }

    // ---- 3. 文脈で当たる auto の変種 ----
    let dictated_slots: BTreeSet<String> = actions
        .iter()
        .filter(|a| a.add_slot.as_deref().is_none_or(|s| s.starts_with(FILL_MARK)))
        .flat_map(|a| a.targets.iter().cloned())
        .collect();
    for (index, variant) in structure.variants.iter().enumerate() {
        if !variant.auto || variant.is_whole() {
            continue;
        }
        if !variant.condition.as_ref().is_none_or(|c| c.matches(context)) {
            continue;
        }
        let same_text_selected = actions.iter().any(|a| a.variant == Some(index));
        if same_text_selected {
            continue; // 口述でも選ばれていれば一度だけ(L4)
        }
        let is_list = variant.kind == Some(LineKind::ListNegation);
        let displaces = matches!(variant.mode, Mode::Replace | Mode::Remove) && variant.slot.is_none();
        if displaces && !is_list && variant.target.iter().any(|t| dictated_slots.contains(t)) {
            continue; // (i) 口述の選択がある項目の既定文を退ける auto は働かない
        }
        let text = match variant.mode {
            Mode::Remove => String::new(),
            _ => {
                let Some((_, module)) = find_module(template, &variant.module) else { continue };
                let lines = module_lines(module);
                match variant.line {
                    Some(line) => lines.get(line).map(|l| l.to_string()).unwrap_or_default(),
                    None => module.text.clone(),
                }
            }
        };
        actions.push(Action {
            order: index,
            variant: Some(index),
            module: Some(variant.module.clone()),
            targets: variant.target.clone(),
            mode: variant.mode,
            organ: variant.organ.clone(),
            add_slot: variant.slot.clone(),
            prefix: variant.prefix.clone(),
            text,
            grade: None,
        });
    }

    // ---- 4. 行ごとに置く ----
    let mut states: Vec<LineState> = vec![LineState::default(); body.len()];
    // 4a. 列挙否定の変種(T0)と、差し替え・外す・充填。足す物はあとで(吸収先が決まってから)。
    let mut adds: Vec<Action> = Vec::new();
    for action in actions.iter().cloned() {
        if let Some(fill) = action.add_slot.as_deref().and_then(|s| s.strip_prefix(FILL_MARK)) {
            let line: usize = fill.parse().unwrap_or(usize::MAX);
            if let Some(state) = states.get_mut(line) {
                state.fill = Some(Piece {
                    order: action.order,
                    text: action.text.clone(),
                    from_list: false,
                });
            }
            continue;
        }
        if action.add_slot.is_some() {
            adds.push(action);
            continue;
        }
        let variant = action.variant.map(|i| &structure.variants[i]);
        let is_list_variant = variant.is_some_and(|v| v.kind == Some(LineKind::ListNegation));
        let target_lines: Vec<usize> = action.targets.iter().flat_map(|t| slot_lines(t)).collect();
        if target_lines.is_empty() {
            notes.push(format!("対象の項目 {:?} が本文に無いので置きませんでした", action.targets));
            continue;
        }
        let head = *target_lines.iter().min().unwrap();
        match action.mode {
            Mode::Remove => {
                for &line in &target_lines {
                    states[line].removed = true;
                }
            }
            Mode::Replace if is_list_variant => {
                let v = variant.unwrap();
                for &line in &target_lines {
                    if line != head {
                        states[line].absorbed_into = Some(head);
                    }
                }
                let full = rebuild_list(&v.organs, &BTreeSet::new(), &v.sep, &v.pattern).unwrap_or_default();
                if action.text != full {
                    notes.push(format!(
                        "「{}」の列挙否定の文はコードが繋ぎ直すので、選択の文は使いませんでした",
                        v.module
                    ));
                }
                states[head].list_variant = Some(ListVariant {
                    organs: v.organs.clone(),
                    sep: v.sep.clone(),
                    pattern: v.pattern.clone(),
                    organ_slots: v.organ_slots.clone(),
                    removed: BTreeSet::new(),
                    after: Vec::new(),
                });
                states[head].replace.push(Piece {
                    order: action.order,
                    text: action.text.clone(),
                    from_list: true,
                });
            }
            Mode::Replace => {
                if let Some(organ) = &action.organ {
                    // 列挙否定から臓器を外し、その文を直後に置く。
                    let line = head;
                    let info = &structure.lines[line];
                    if info.kind == LineKind::ListNegation && info.organs.iter().any(|o| o == organ) {
                        states[line].removed_organs.insert(organ.clone());
                        states[line].after_list.push((organ.clone(), action.order, action.text.clone()));
                        continue;
                    }
                    notes.push(format!("臓器「{organ}」は列挙に無いので文ごと差し替えました"));
                }
                for &line in &target_lines {
                    if line != head {
                        states[line].absorbed_into = Some(head);
                    }
                }
                states[head].replace.push(Piece {
                    order: action.order,
                    text: action.text.clone(),
                    from_list: false,
                });
            }
            Mode::Prepend | Mode::Append => adds.push(action),
        }
    }
    // 4a'. 複数の項目を 1 つの文で差し替えたとき(直線化・軽度変性)、吸収された行に置かれた差し替え・充填・
    // 臓器外しの文は塊の頭に移す(消さない)。列挙否定の変種(T0)の塊は 4b で臓器ごとに扱う。
    for line in 0..body.len() {
        let Some(head) = states[line].absorbed_into else { continue };
        if states[head].list_variant.is_some() {
            continue;
        }
        let moved = std::mem::take(&mut states[line]);
        states[line].absorbed_into = Some(head);
        states[head].replace.extend(moved.replace);
        if let Some(fill) = moved.fill {
            states[head].replace.push(fill);
        }
        states[head].after_list.extend(moved.after_list);
    }
    // 4b. T0 に吸収された項目への口述の差し替え・臓器外し: T0 の列挙から臓器を外し、直後に置く(§16.3 (ii))。
    let list_heads: Vec<usize> = states
        .iter()
        .enumerate()
        .filter(|(_, s)| s.list_variant.is_some())
        .map(|(i, _)| i)
        .collect();
    for head in list_heads {
        let absorbed: Vec<usize> = (0..body.len())
            .filter(|&i| i == head || states[i].absorbed_into == Some(head))
            .collect();
        let mut moved: Vec<(usize, usize, String)> = Vec::new();
        let mut removed_organs: BTreeSet<String> = BTreeSet::new();
        {
            let list = states[head].list_variant.as_ref().unwrap();
            for &line in &absorbed {
                let slot = structure.lines[line].slot.clone().unwrap_or_default();
                let organs_of_slot: Vec<(usize, String)> = list
                    .organs
                    .iter()
                    .enumerate()
                    .filter(|(_, o)| list.organ_slots.get(*o).is_some_and(|slots| slots.contains(&slot)))
                    .map(|(p, o)| (p, o.clone()))
                    .collect();
                let state = &states[line];
                // 口述の臓器外し(両腎嚢胞 → 腎): その臓器だけ。T0 の列挙に同じ名が無ければ項目の先頭の臓器の所へ。
                let first = organs_of_slot.first().map(|(p, _)| *p).unwrap_or(0);
                for (organ, order, text) in &state.after_list {
                    let position = match organs_of_slot.iter().find(|(_, o)| o == organ) {
                        Some((p, o)) => {
                            removed_organs.insert(o.clone());
                            *p
                        }
                        None => first,
                    };
                    moved.push((position, *order, text.clone()));
                }
                // 口述の差し替え(脂肪肝 → 肝)と充填: 項目に当たる臓器すべて。T0 自身の文は除く。
                let own: Vec<&Piece> = state.replace.iter().filter(|p| !p.from_list).chain(state.fill.iter()).collect();
                if !own.is_empty() {
                    for (_, o) in &organs_of_slot {
                        removed_organs.insert(o.clone());
                    }
                    for piece in own {
                        moved.push((first, piece.order, piece.text.clone()));
                    }
                }
            }
        }
        for &line in &absorbed {
            let state = &mut states[line];
            state.removed_organs.clear();
            state.after_list.clear();
            state.fill = None;
            state.replace.retain(|p| p.from_list);
        }
        let list = states[head].list_variant.as_mut().unwrap();
        list.removed = removed_organs;
        list.after = moved;
    }
    // 4c. 足す物(前に足す・後に足す)。吸収された項目を目印にする物は塊の前後へ(§16.3 (iii))。
    for action in adds {
        let target_lines: Vec<usize> = action.targets.iter().flat_map(|t| slot_lines(t)).collect();
        let Some(&anchor) = (match action.mode {
            Mode::Prepend => target_lines.iter().min(),
            _ => target_lines.iter().max(),
        }) else {
            notes.push(format!("足す先の項目 {:?} が本文に無いので置きませんでした", action.targets));
            continue;
        };
        let anchor = states[anchor].absorbed_into.unwrap_or(anchor);
        let piece = Piece {
            order: action.order,
            text: action.text.clone(),
            from_list: false,
        };
        match action.mode {
            Mode::Prepend => {
                if let Some(prefix) = &action.prefix {
                    states[anchor].prefix = Some(prefix.clone());
                }
                states[anchor].prepend.push(piece);
            }
            _ => states[anchor].append.push(piece),
        }
    }

    // ---- 5. 源泉の順に畳む ----
    let mut out: Vec<String> = Vec::new();
    for (i, info) in structure.lines.iter().enumerate() {
        let state = &states[i];
        if state.absorbed_into.is_some() {
            continue;
        }
        if matches!(info.kind, LineKind::Blank | LineKind::Heading) && state.prepend.is_empty() && state.append.is_empty() {
            out.push(body[i].to_string());
            continue;
        }
        let mut prepend = state.prepend.clone();
        prepend.sort_by_key(|p| p.order);
        out.extend(prepend.into_iter().map(|p| p.text));
        if state.removed {
            // 既定文だけ消す。
        } else if let Some(list) = &state.list_variant {
            if let Some(text) = rebuild_list(&list.organs, &list.removed, &list.sep, &list.pattern) {
                out.push(text);
            }
            let mut after = list.after.clone();
            after.sort();
            out.extend(after.into_iter().map(|(_, _, t)| t));
        } else if !state.replace.is_empty() || state.fill.is_some() {
            // 差し替え(併置)。同じ行に充填もあれば併置して知らせる。臓器外しの文も消さずに続ける。
            let mut pieces = state.replace.clone();
            if let Some(fill) = &state.fill {
                if !pieces.is_empty() {
                    notes.push(format!("本文 {i} 行目に充填と差し替えの両方があるので併置しました"));
                }
                pieces.push(fill.clone());
            }
            pieces.sort_by_key(|p| p.order);
            out.extend(pieces.into_iter().map(|p| p.text));
            out.extend(organ_texts(info, &state.after_list));
        } else {
            let default = if info.kind == LineKind::ListNegation && !state.removed_organs.is_empty() {
                rebuild_list(&info.organs, &state.removed_organs, &info.sep, &info.pattern)
            } else {
                Some(body[i].to_string())
            };
            if let Some(text) = default {
                match &state.prefix {
                    Some(prefix) => out.push(format!("{prefix}{text}")),
                    None => out.push(text),
                }
            }
            out.extend(organ_texts(info, &state.after_list));
        }
        let mut append = state.append.clone();
        append.sort_by_key(|p| p.order);
        out.extend(append.into_iter().map(|p| p.text));
    }
    Ok(Rendered {
        text: out.join("\n"),
        notes,
    })
}

/// 外した臓器の文を、列挙の臓器の順(同じ臓器なら源泉の順)に並べる。
fn organ_texts(info: &LineInfo, after_list: &[(String, usize, String)]) -> Vec<String> {
    let mut after: Vec<(usize, usize, String)> = after_list
        .iter()
        .map(|(organ, order, text)| {
            let position = info.organs.iter().position(|o| o == organ).unwrap_or(0);
            (position, *order, text.clone())
        })
        .collect();
    after.sort();
    after.into_iter().map(|(_, _, t)| t).collect()
}

/// 所見群の尺度の段。尺度の無いモジュールなら None。
pub fn group_levels(structure: &TemplateStructure, vocabulary: &Vocabulary, module: &str) -> Option<Vec<String>> {
    let group = structure.groups.iter().find(|g| g.module == module)?;
    Some(match &group.scale {
        Some(scale) => scale.levels().to_vec(),
        None => vocabulary.default_scale.clone(),
    })
}

/// 行(変種)の出る程度の区間: 明示があればそれ、無ければ役割の既定、どちらも無ければ全段。
pub fn interval_of(variant: &Variant, vocabulary: &Vocabulary, levels: &[String]) -> Vec<String> {
    if let Some(grades) = &variant.grades {
        return grades.clone();
    }
    if let Some(role) = variant.role.as_deref().and_then(|r| vocabulary.role(r)) {
        // 既定の区間は既定の尺度で書いてある。名前付きの尺度には既定が無いので全段。
        if levels == vocabulary.default_scale.as_slice() {
            return role.grades.clone();
        }
    }
    levels.to_vec()
}

// ------------------------------------------------------------------------------------------------
// 構造の検査(保存時・試験)。§16.13 の被覆・連続、鍵の語彙、対象の実在。
// ------------------------------------------------------------------------------------------------

pub fn check_structure(template: &Template, structure: &TemplateStructure, vocabulary: &Vocabulary) -> Vec<String> {
    let mut errors = Vec::new();
    let body: Vec<&str> = canonical_lines(&template.body);
    if body.len() != structure.lines.len() {
        errors.push(format!("行数: 構造 {} / 本文 {}", structure.lines.len(), body.len()));
        return errors;
    }
    let mut slots: BTreeSet<&str> = BTreeSet::new();
    for (i, info) in structure.lines.iter().enumerate() {
        if info.i != i {
            errors.push(format!("{i} 行目の番号が {}", info.i));
        }
        let text = body[i];
        match info.kind {
            LineKind::Blank if !text.trim().is_empty() => errors.push(format!("{i} 行目は空行ではない")),
            LineKind::Heading if text.trim().is_empty() => errors.push(format!("{i} 行目は見出しなのに空")),
            LineKind::Heading | LineKind::Blank if info.slot.is_some() => {
                errors.push(format!("{i} 行目(見出し・空行)に鍵がある"))
            }
            LineKind::Sentence | LineKind::ListNegation => match &info.slot {
                None => errors.push(format!("{i} 行目に鍵が無い")),
                Some(slot) if !vocabulary.has_slot(slot) => errors.push(format!("{i} 行目の鍵「{slot}」は語彙に無い")),
                Some(slot) => {
                    slots.insert(slot);
                }
            },
            _ => {}
        }
        if info.kind == LineKind::ListNegation {
            if info.organs.len() < 2 || !info.pattern.contains("{organs}") {
                errors.push(format!("{i} 行目の列挙否定に臓器か型が無い"));
            } else if rebuild_list(&info.organs, &BTreeSet::new(), &info.sep, &info.pattern).as_deref() != Some(text) {
                errors.push(format!("{i} 行目の列挙否定を繋ぎ直すと本文と違う"));
            }
        }
    }
    let mut seen_lines: BTreeSet<(String, Option<usize>)> = BTreeSet::new();
    for (index, variant) in structure.variants.iter().enumerate() {
        let where_ = format!("変種 {index}「{}」", variant.module);
        let Some((_, module)) = find_module(template, &variant.module) else {
            errors.push(format!("{where_}: モジュールが源泉に無い"));
            continue;
        };
        if let Some(line) = variant.line {
            if line >= module_lines(module).len() {
                errors.push(format!("{where_}: 行 {line} はモジュールの文に無い"));
            }
        }
        if variant.is_whole() {
            if variant.polarity.is_some() && variant.polarity.as_deref() != Some("mixed") {
                errors.push(format!("{where_}: 全体差し替えの極性は mixed"));
            }
            continue;
        }
        if !variant.derived && !seen_lines.insert((variant.module.clone(), variant.line)) {
            errors.push(format!("{where_}: 同じモジュールの同じ行に変種が二つ"));
        }
        if variant.target.is_empty() {
            errors.push(format!("{where_}: 対象が無い"));
        }
        for target in &variant.target {
            if !slots.contains(target.as_str()) {
                errors.push(format!("{where_}: 対象「{target}」が本文に無い"));
            }
        }
        if let Some(slot) = &variant.slot {
            if !vocabulary.has_slot(slot) {
                errors.push(format!("{where_}: 足す鍵「{slot}」は語彙に無い"));
            }
            if slots.contains(slot.as_str()) {
                errors.push(format!("{where_}: 足す鍵「{slot}」は本文にある項目(差し替えにする)"));
            }
            if !matches!(variant.mode, Mode::Prepend | Mode::Append) {
                errors.push(format!("{where_}: 足す鍵は前に足す・後に足すだけ"));
            }
        }
        if let Some(organ) = &variant.organ {
            if variant.mode != Mode::Replace {
                errors.push(format!("{where_}: 臓器を外すのは差し替えだけ"));
            }
            let ok = variant.target.iter().any(|t| {
                structure
                    .lines
                    .iter()
                    .any(|l| l.slot.as_deref() == Some(t) && l.kind == LineKind::ListNegation && l.organs.contains(organ))
            });
            if !ok {
                errors.push(format!("{where_}: 臓器「{organ}」は対象の列挙に無い"));
            }
        }
        if variant.mode == Mode::Remove {
            if !variant.derived || !variant.auto {
                errors.push(format!("{where_}: 外すは派生(derived)かつ auto"));
            }
            if variant.polarity.is_some() {
                errors.push(format!("{where_}: 外すに極性は無い"));
            }
            if variant.line.is_some() {
                errors.push(format!("{where_}: 外すに源泉の行は無い(line は null)"));
            }
        } else if let Some(polarity) = &variant.polarity {
            if !["negative", "benign", "abnormal"].contains(&polarity.as_str()) {
                errors.push(format!("{where_}: 極性「{polarity}」(mixed は全体差し替えだけ)"));
            }
        } else {
            errors.push(format!("{where_}: 極性が無い"));
        }
        if variant.auto && matches!(variant.polarity.as_deref(), Some("abnormal") | Some("benign")) {
            errors.push(format!("{where_}: 異常・正常範囲の変種に auto は付けない(D4)"));
        }
        if variant.auto && variant.condition.as_ref().is_none_or(Condition::is_empty) {
            errors.push(format!("{where_}: auto には条件が要る"));
        }
        if let Some(condition) = &variant.condition {
            if let Some(sex) = &condition.sex {
                if sex != "男" && sex != "女" {
                    errors.push(format!("{where_}: 性別「{sex}」"));
                }
            }
            if let Some(age) = &condition.age_band {
                if !vocabulary.age_bands.contains_key(age) {
                    errors.push(format!("{where_}: 年齢帯「{age}」"));
                }
            }
            if let Some(purpose) = &condition.purpose {
                if !vocabulary.purposes.contains(purpose) {
                    errors.push(format!("{where_}: 目的の札「{purpose}」は一覧に無い"));
                }
            }
        }
        if let Some(role) = &variant.role {
            if vocabulary.role(role).is_none() {
                errors.push(format!("{where_}: 役割「{role}」は語彙に無い"));
            }
            if !structure.groups.iter().any(|g| g.module == variant.module) {
                errors.push(format!("{where_}: 役割があるのに所見群(尺度)が無い"));
            }
        }
        if variant.kind == Some(LineKind::ListNegation) {
            if variant.organs.len() < 2 || !variant.pattern.contains("{organs}") {
                errors.push(format!("{where_}: 列挙否定の変種に臓器か型が無い"));
            }
            if variant.mode != Mode::Replace {
                errors.push(format!("{where_}: 列挙否定の変種は差し替え"));
            }
            for target in &variant.target {
                if !variant.organ_slots.values().any(|slots_of| slots_of.contains(target)) {
                    errors.push(format!("{where_}: organ_slots が対象「{target}」を覆っていない"));
                }
            }
            let text = variant
                .line
                .and_then(|l| module_lines(module).get(l).map(|t| t.to_string()))
                .unwrap_or_default();
            if rebuild_list(&variant.organs, &BTreeSet::new(), &variant.sep, &variant.pattern).as_deref() != Some(text.as_str()) {
                errors.push(format!("{where_}: 列挙否定を繋ぎ直すとモジュールの文と違う"));
            }
            for (organ, slots_of) in &variant.organ_slots {
                if !variant.organs.contains(organ) {
                    errors.push(format!("{where_}: organ_slots の「{organ}」は列挙に無い"));
                }
                for slot in slots_of {
                    if !variant.target.contains(slot) {
                        errors.push(format!("{where_}: organ_slots の「{slot}」は対象に無い"));
                    }
                }
            }
        }
    }
    // 所見群: 尺度、区間の連続、被覆、帰結の区間。
    for group in &structure.groups {
        let levels = group_levels(structure, vocabulary, &group.module).unwrap_or_default();
        let named = group.scale.as_ref().is_some_and(|s| s.name().is_some());
        let members: Vec<&Variant> = structure
            .variants
            .iter()
            .filter(|v| v.module == group.module && !v.is_whole() && !v.derived)
            .collect();
        if members.is_empty() {
            errors.push(format!("所見群「{}」に行が無い", group.module));
            continue;
        }
        if !members.iter().all(|v| v.line.is_some())
            || !structure.variants.iter().any(|v| v.module == group.module && v.is_whole() && v.split)
        {
            errors.push(format!("所見群「{}」は行ごとに割る(split の印と行ごとの変種)", group.module));
        }
        if named {
            for variant in members.iter().filter(|v| v.grades.is_none()) {
                errors.push(format!(
                    "所見群「{}」の行 {:?}: 名前付きの尺度では区間を明示する(役割の既定表は既定の尺度の物)",
                    group.module, variant.line
                ));
            }
        }
        let mut covered: BTreeSet<&str> = BTreeSet::new();
        let mut conclusions: Vec<Vec<String>> = Vec::new();
        for variant in &members {
            let interval = interval_of(variant, vocabulary, &levels);
            let positions: Vec<usize> = interval.iter().filter_map(|g| levels.iter().position(|l| l == g)).collect();
            if positions.len() != interval.len() || interval.is_empty() {
                errors.push(format!("所見群「{}」の行 {:?} の区間に尺度に無い段がある", group.module, variant.line));
            } else if positions.windows(2).any(|w| w[1] != w[0] + 1) {
                errors.push(format!("所見群「{}」の行 {:?} の区間が連続していない", group.module, variant.line));
            }
            for g in &interval {
                covered.insert(levels.iter().find(|l| *l == g).map_or("", String::as_str));
            }
            if variant.role.as_deref() == Some("帰結") {
                if variant.grades.is_none() {
                    conclusions.push(Vec::new());
                } else {
                    conclusions.push(interval.clone());
                }
            }
        }
        for level in &levels {
            if !covered.contains(level.as_str()) {
                errors.push(format!("所見群「{}」は程度「{level}」で出る行が無い", group.module));
            }
        }
        if conclusions.len() >= 2 {
            if conclusions.iter().any(Vec::is_empty) {
                errors.push(format!("所見群「{}」の帰結が 2 行以上あるのに区間の無い行がある", group.module));
            }
            for (a, x) in conclusions.iter().enumerate() {
                for y in conclusions.iter().skip(a + 1) {
                    if x.iter().any(|g| y.contains(g)) {
                        errors.push(format!("所見群「{}」の帰結の区間が重なる", group.module));
                    }
                }
            }
        }
    }
    errors
}

// ------------------------------------------------------------------------------------------------
// テンプレートの照合と、検査名が無いときの候補。§16.6・§16.14
// ------------------------------------------------------------------------------------------------

/// Claude が返したテンプレート名を、全テンプレートの全キーワードを `template_key` でならして引く。
/// 「〜テンプレート」と返しても、間の「の」があっても受ける。見つからなければ None(全文経路へ)。
pub fn match_template(dictionary: &Dictionary, name: &str) -> Option<usize> {
    let key = template_key(name);
    if key.is_empty() {
        return None;
    }
    let mut heads: Vec<String> = vec![key.clone()];
    if let Some(head) = strip_template_suffix(&key) {
        heads.push(head.to_string());
        if let Some(h) = head.strip_suffix('の') {
            heads.push(h.to_string());
        }
    }
    dictionary.templates.iter().position(|template| {
        template
            .keywords
            .iter()
            .any(|k| heads.iter().any(|h| template_key(k) == *h))
    })
}

fn is_gram_char(c: char) -> bool {
    // 漢字・々・カタカナ(長音「ー」は語の中なので含める。中黒「・」は区切りなので含めない)。
    ('\u{4e00}'..='\u{9fff}').contains(&c) || c == '々' || (('\u{30a1}'..='\u{30fc}').contains(&c) && c != '・')
}

/// 漢字・カタカナの連なりから 2 字の連なり(2-gram)を取る。ひらがな・記号・英数で切る。
pub fn bigrams(text: &str) -> BTreeSet<String> {
    let mut grams = BTreeSet::new();
    let mut run: Vec<char> = Vec::new();
    let mut flush = |run: &mut Vec<char>| {
        for pair in run.windows(2) {
            grams.insert(pair.iter().collect::<String>());
        }
        run.clear();
    };
    for c in text.chars() {
        if is_gram_char(c) {
            run.push(c);
        } else {
            flush(&mut run);
        }
    }
    flush(&mut run);
    grams
}

/// テンプレートの語(本文・モジュールの文と名前・キーワードの 2-gram。程度語と一般語を除く)。
pub fn template_grams(template: &Template, vocabulary: &Vocabulary) -> BTreeSet<String> {
    let mut grams = bigrams(&template.body);
    for module in &template.modules {
        grams.extend(bigrams(&module.text));
        grams.extend(bigrams(&module.name));
    }
    for keyword in &template.keywords {
        grams.extend(bigrams(keyword));
    }
    let stop: Vec<&str> = vocabulary
        .general_words
        .iter()
        .map(String::as_str)
        .chain(vocabulary.grade_word_list())
        .collect();
    grams.retain(|g| !stop.iter().any(|w| w.contains(g.as_str())));
    grams
}

/// 検査名の無い依頼の候補: テンプレートの 2-gram が依頼の文にいくつ現れるかを点数にし(等重み)、1 以上を
/// 点数順に返す(同点は辞書の順)。戻り値は (テンプレートの番号, 点数)。モジュールの名前の 2-gram に重みを付ける
/// 案は、実物の辞書の順で「肝嚢胞」が副鼻腔CT(貯留嚢胞)を先頭にしたので採らない(QA4 H3)。
pub fn candidates(request: &str, dictionary: &Dictionary, vocabulary: &Vocabulary) -> Vec<(usize, usize)> {
    let mut scored: Vec<(usize, usize)> = dictionary
        .templates
        .iter()
        .enumerate()
        .map(|(index, template)| {
            let score = template_grams(template, vocabulary)
                .iter()
                .filter(|g| request.contains(g.as_str()))
                .count();
            (index, score)
        })
        .filter(|(_, score)| *score > 0)
        .collect();
    scored.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    scored
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_matches_known_vectors() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        // 2 ブロックにまたがる長さ(56 バイト)。
        assert_eq!(
            sha256_hex(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
    }

    /// macOS の shasum と一致する(日本語・改行入り)。shasum が無ければ飛ばす。
    #[test]
    fn sha256_matches_shasum_for_japanese_text() {
        let text = "肝に明らかな占拠性病変を認めません。\n\u{1e}肝\n軽度の脂肪肝を認めます。\n言われた所見";
        let output = std::process::Command::new("/usr/bin/shasum")
            .args(["-a", "256"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .and_then(|mut child| {
                use std::io::Write;
                child.stdin.take().unwrap().write_all(text.as_bytes())?;
                child.wait_with_output()
            });
        let Ok(output) = output else { return };
        let expected = String::from_utf8_lossy(&output.stdout).split_whitespace().next().unwrap().to_string();
        assert_eq!(sha256_hex(text.as_bytes()), expected);
    }

    #[test]
    fn the_hash_ignores_trailing_blanks_keywords_and_extras() {
        let a: Template = serde_json::from_str(
            r#"{"keywords": ["腹部CT"], "body": ["肝は正常。", "脾は正常。"], "modules": [{"name": "肝", "text": "脂肪肝。", "note": "肝の文と"}], "memo": "x"}"#,
        )
        .unwrap();
        let b: Template = serde_json::from_str(
            r#"{"keywords": ["別名"], "body": ["肝は正常。 ", "脾は正常。　", ""], "modules": [{"name": "肝", "text": ["脂肪肝。", ""], "note": "肝の文と\t", "diagnosis": "y"}]}"#,
        )
        .unwrap();
        assert_eq!(source_hash(&a), source_hash(&b));
        let mut c = a.clone();
        c.body.push_str("\n胆嚢は正常。");
        assert_ne!(source_hash(&a), source_hash(&c));
        let mut d = a.clone();
        d.modules[0].note = "別の使い方".to_string();
        assert_ne!(source_hash(&a), source_hash(&d));
        assert!(source_hash(&a).starts_with("sha256:"));
        assert_eq!(source_hash(&a).len(), 7 + 64);
    }

    #[test]
    fn the_builtin_vocabulary_loads_and_is_consistent() {
        let vocabulary = Vocabulary::builtin();
        assert!(vocabulary.slot_keys().count() >= 55);
        assert_eq!(vocabulary.default_scale, ["軽度", "中等度", "高度"]);
        assert_eq!(vocabulary.roles.len(), 6);
        for role in &vocabulary.roles {
            assert!(role.grades.iter().all(|g| vocabulary.default_scale.contains(g)), "{}", role.name);
        }
        assert_eq!(vocabulary.role("障害").unwrap().grades, ["高度"]);
        assert!(vocabulary.purposes.contains(&"腹痛".to_string()));
        let keys: Vec<&str> = vocabulary.slot_keys().collect();
        let unique: BTreeSet<&str> = keys.iter().copied().collect();
        assert_eq!(keys.len(), unique.len(), "鍵が重複");
    }

    #[test]
    fn roles_extra_overrides_the_default_interval_everywhere() {
        let mut file = StructureFile::default();
        file.roles_extra.insert("波及".to_string(), vec!["高度".to_string()]);
        let vocabulary = Vocabulary::builtin().with_extras(&file);
        assert_eq!(vocabulary.role("波及").unwrap().grades, ["高度"]);
        let variant = Variant {
            role: Some("波及".to_string()),
            ..Variant::default()
        };
        assert_eq!(interval_of(&variant, &vocabulary, &vocabulary.default_scale), ["高度"]);
        let explicit = Variant {
            role: Some("波及".to_string()),
            grades: Some(vec!["中等度".to_string(), "高度".to_string()]),
            ..Variant::default()
        };
        assert_eq!(interval_of(&explicit, &vocabulary, &vocabulary.default_scale), ["中等度", "高度"]);
    }

    #[test]
    fn conditions_match_only_known_context() {
        let condition = Condition {
            sex: Some("女".to_string()),
            age_band: Some("若年".to_string()),
            purpose: None,
        };
        let young_woman = Context {
            sex: Some("女".to_string()),
            age_band: Some("若年".to_string()),
            purposes: vec![],
        };
        assert!(condition.matches(&young_woman));
        assert!(!condition.matches(&Context::default()), "不明は合わない");
        let pain = Condition {
            purpose: Some("腹痛".to_string()),
            ..Condition::default()
        };
        assert!(!pain.matches(&young_woman));
        assert!(pain.matches(&Context {
            purposes: vec!["腹痛".to_string()],
            ..Context::default()
        }));
        assert_eq!(condition.label(), "女・若年");
        assert_eq!(pain.label(), "目的=腹痛");
    }

    #[test]
    fn bigrams_split_on_kana_and_symbols() {
        let grams = bigrams("右上葉に8ミリのすりガラス結節のみ");
        assert!(grams.contains("右上") && grams.contains("上葉") && grams.contains("ガラ") && grams.contains("結節"));
        assert!(!grams.contains("葉に") && !grams.contains("のす"));
        assert!(bigrams("肝").is_empty());
    }

    #[test]
    fn rebuilding_a_list_drops_removed_organs() {
        let organs: Vec<String> = ["脾", "腎", "副腎"].iter().map(|s| s.to_string()).collect();
        let removed: BTreeSet<String> = ["腎".to_string()].into_iter().collect();
        assert_eq!(
            rebuild_list(&organs, &removed, "、", "{organs}に特記所見ありません。"),
            Some("脾、副腎に特記所見ありません。".to_string())
        );
        let all: BTreeSet<String> = organs.iter().cloned().collect();
        assert_eq!(rebuild_list(&organs, &all, "、", "{organs}"), None);
    }

    #[test]
    fn matching_a_template_name_tolerates_the_suffix() {
        let mut dictionary = Dictionary::default();
        dictionary.save_template(None, "腹部骨盤CT、腹部CT、abdomen CT", "本文").unwrap();
        dictionary.save_template(None, "胸部CT", "本文").unwrap();
        assert_eq!(match_template(&dictionary, "腹部CT"), Some(0));
        assert_eq!(match_template(&dictionary, "腹部骨盤CTテンプレート"), Some(0));
        assert_eq!(match_template(&dictionary, "腹部骨盤CTのテンプレート"), Some(0));
        assert_eq!(match_template(&dictionary, "Abdomen CT"), Some(0));
        assert_eq!(match_template(&dictionary, "胸部 CT"), Some(1));
        assert_eq!(match_template(&dictionary, "頭部CT"), None);
        assert_eq!(match_template(&dictionary, ""), None);
    }
}
