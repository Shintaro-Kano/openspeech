//! 「ヘイクロード」のテンプレート補完を、構造の描画経路に通す(`docs/simplify-and-assistant-plan.md` §16.6、単位 S3)。
//!
//! 流れ: 依頼の下ごしらえ(`prepare`: 文脈と程度語、検査名が無いときの候補と手がかり。決定論)→ Claude(選択だけ、
//! または全文)→ 読み替えと守り(`resolve`: テンプレートの照合、選択ごとの L5・長さ・名詞・文脈・程度の門、`render`、
//! 描画後の L2・L5・長さ。LLM は使わない)→ 置けなければ全文で答え直させる(`run`)。
//!
//! 全文経路(今までどおり Claude が本文を書く)に残るのは、構造の無いテンプレート、全体差し替え、言い直し(D11)と、
//! 描画できなかったとき。どれも `Route` に理由を残し、呼び出す側が診断ログと知らせに出す。

use super::assistant::{Assistant, AssistantReply, AssistantRequest, ReplyKind, MAX_COMPLETION_CHARS, MAX_TARGET_CHARS};
use super::context::{self, Extracted};
use super::dictionary::{template_key, Dictionary, Template, TEMPLATE_SUFFIXES};
use super::judge::{side_counts, side_positions};
use super::structure::{
    bigrams, candidates, canonical_lines, group_levels, has_placeholder, match_template, module_lines, render,
    LineKind, Mode, Selection, StructureFile, TemplateStructure, Variant, Vocabulary,
};
use std::collections::BTreeSet;

/// 選択の文の長さの上限: 源泉の該当行の字数 × 2 + これ(§16.6)。
pub const SELECTION_SLACK_CHARS: usize = 100;

/// 1 回の補完に使う辞書・構造・語彙の控え。補完役の指示文を作ったときと同じ物を使う(置き場所がずれないように)。
#[derive(Debug, Clone)]
pub struct Plan {
    pub dictionary: Dictionary,
    pub structures: StructureFile,
    pub vocabulary: Vocabulary,
}

impl Plan {
    pub fn new(dictionary: Dictionary, structures: StructureFile) -> Self {
        let vocabulary = Vocabulary::builtin().with_extras(&structures);
        Plan { dictionary, structures, vocabulary }
    }

    fn template_name(&self, index: usize) -> String {
        self.dictionary.templates[index].keywords.first().cloned().unwrap_or_default()
    }
}

/// 依頼の下ごしらえの結果。`request` は Claude に送る形(文脈・程度語・候補を添えた物)。
#[derive(Debug, Clone, Default)]
pub struct Prepared {
    pub request: AssistantRequest,
    pub extracted: Extracted,
    /// 依頼で検査名(モダリティの語を含むキーワード、または「〜テンプレート」)が言われたテンプレート。
    pub named: Option<usize>,
    /// 検査名が無いときの候補(テンプレートの番号, 点数)。点数順。検査名があれば空。
    pub candidates: Vec<(usize, usize)>,
    /// 候補が複数のとき、手がかりで絞った 1 つと出どころ(「エディターの文章から」「直前の補完から」)。
    pub hint: Option<(usize, &'static str)>,
    /// 名詞の門で見る依頼の文(テンプレートのキーワードと「テンプレート」を伏せた物。検査名の字「腹」「部」で
    /// 生理的な腹水の行が通らないように。QA 再確認 低)。
    pub noun_request: String,
}

/// 依頼の文で検査名が言われたテンプレート。キーワードがモダリティの語(CT・MRI…)を含み依頼の文に現れるか、
/// キーワードに「テンプレート」が続くとき。前回・比較・既往の語がある節は見ない。依頼の中で先に現れる物、
/// 同じ位置なら長いキーワードを優先する。「前立腺」「胸部」のような別名だけでは検査名と見ない(「前立腺の腫大」を
/// 前立腺MRI の呼び出しと取らない)。
pub fn named_template(request: &str, dictionary: &Dictionary, vocabulary: &Vocabulary) -> Option<usize> {
    let kept: Vec<&str> = request
        .split(['、', '。', '\n'])
        .filter(|clause| !["前回", "比較", "既往"].iter().any(|w| clause.contains(w)))
        .collect();
    let key = template_key(&kept.join("、"));
    let modalities: Vec<String> = vocabulary.modality_words.iter().map(|m| template_key(m)).collect();
    // (位置, 長さの逆, 番号) の最小。
    let mut best: Option<(usize, std::cmp::Reverse<usize>, usize)> = None;
    for (index, template) in dictionary.templates.iter().enumerate() {
        for keyword in &template.keywords {
            let k = template_key(keyword);
            if k.chars().count() < 2 {
                continue;
            }
            let modality = modalities.iter().any(|m| !m.is_empty() && k.contains(m.as_str()));
            let called = TEMPLATE_SUFFIXES
                .iter()
                .filter_map(|suffix| key.find(&format!("{k}{suffix}")).or_else(|| key.find(&format!("{k}の{suffix}"))))
                .min();
            let at = if modality { key.find(&k).or(called) } else { called };
            if let Some(at) = at {
                let candidate = (at, std::cmp::Reverse(k.chars().count()), index);
                if best.is_none_or(|b| candidate < b) {
                    best = Some(candidate);
                }
            }
        }
    }
    best.map(|(_, _, index)| index)
}

/// キーワードから (部位, モダリティ) の組を作る(「腹部骨盤単純CT」→ ("腹部骨盤", "ct"))。ならした形。
fn region_pairs(template: &Template, modalities: &[String]) -> Vec<(String, String)> {
    let mut pairs = Vec::new();
    for keyword in &template.keywords {
        let k = template_key(keyword);
        let Some(modality) = modalities
            .iter()
            .filter(|m| !m.is_empty() && k.contains(m.as_str()))
            .max_by_key(|m| m.chars().count())
        else {
            continue;
        };
        let region = k.replace(modality.as_str(), "").replace("単純", "").replace("造影", "");
        if region.chars().count() >= 2 {
            pairs.push((region, modality.clone()));
        }
    }
    pairs
}

/// エディターの文章の手がかり(§16.14 の決定 2): 文ごとに、候補のテンプレートの部位の語とモダリティの語が
/// 一緒に現れるか。前回・比較・既往の語がある文は見ない。当たる候補がちょうど 1 つならそれ。
fn editor_hint(text: &str, candidates: &[(usize, usize)], dictionary: &Dictionary, vocabulary: &Vocabulary) -> Option<usize> {
    let modalities: Vec<String> = vocabulary.modality_words.iter().map(|m| template_key(m)).collect();
    let head: String = text.chars().take(MAX_TARGET_CHARS).collect();
    let sentences: Vec<String> = head
        .split(['。', '\n'])
        .filter(|s| !["前回", "比較", "既往"].iter().any(|w| s.contains(w)))
        .map(template_key)
        .filter(|s| !s.is_empty())
        .collect();
    let hits: Vec<usize> = candidates
        .iter()
        .map(|(index, _)| *index)
        .filter(|index| {
            let pairs = region_pairs(&dictionary.templates[*index], &modalities);
            sentences
                .iter()
                .any(|s| pairs.iter().any(|(region, modality)| s.contains(region.as_str()) && s.contains(modality.as_str())))
        })
        .collect();
    (hits.len() == 1).then(|| hits[0])
}

/// 名詞の門で見る依頼の文: 検査名(モダリティの語を含むキーワード。長い物から)と「テンプレート」「テンプレ」を
/// 伏せる。「前立腺」「胸部」のような別名は所見の語でもあるので伏せない(「前立腺の腫大」)。
fn noun_request(request: &str, dictionary: &Dictionary, vocabulary: &Vocabulary) -> String {
    let modalities: Vec<String> = vocabulary.modality_words.iter().map(|m| template_key(m)).collect();
    let mut keywords: Vec<&str> = dictionary
        .template_keywords()
        .map(str::trim)
        .filter(|k| k.chars().count() >= 2)
        .filter(|k| {
            let key = template_key(k);
            modalities.iter().any(|m| !m.is_empty() && key.contains(m.as_str()))
        })
        .collect();
    keywords.sort_by_key(|k| std::cmp::Reverse(k.chars().count()));
    let mut text = request.to_string();
    for word in keywords.into_iter().chain(["テンプレート", "テンプレ"]) {
        text = text.replace(word, "／");
    }
    text
}

/// 依頼の下ごしらえ: 依頼の文から文脈と程度語を取り、検査名が無ければ候補を絞って手がかりを探し、Claude に送る形に
/// 添える。`previous_template` は直前の補完のテンプレート名(候補が複数のときの手がかり)。
pub fn prepare(mut request: AssistantRequest, previous_template: Option<&str>, plan: &Plan) -> Prepared {
    let extracted = context::extract(&request.request, &plan.vocabulary, Some(&plan.dictionary));
    request.context = context::context_label(&extracted.context);
    request.grades = extracted.grades.clone();
    let named = named_template(&request.request, &plan.dictionary, &plan.vocabulary);
    let mut found = Vec::new();
    let mut hint = None;
    if named.is_none() {
        found = candidates(&request.request, &plan.dictionary, &plan.vocabulary);
        // 手がかりは点数がいちばん高い候補どうしの同点(CT か MRI か)を解くときだけ使う(§16.14 の決定 2)。
        // 点数の低い候補を手がかりで選ぶと、内容に合う候補が依頼から消える(QA M1)。
        let top_score = found.first().map_or(0, |(_, score)| *score);
        let top: Vec<(usize, usize)> = found.iter().copied().filter(|(_, score)| *score == top_score).collect();
        if top.len() > 1 {
            hint = request
                .target
                .as_ref()
                .and_then(|t| editor_hint(&t.text, &top, &plan.dictionary, &plan.vocabulary))
                .map(|index| (index, "エディターの文章から"))
                .or_else(|| {
                    previous_template
                        .and_then(|name| match_template(&plan.dictionary, name))
                        .filter(|index| top.iter().any(|(i, _)| i == index))
                        .map(|index| (index, "直前の補完から"))
                });
        }
        request.candidates = match hint {
            Some((index, from)) => format!("{}({from})", plan.template_name(index)),
            None => found.iter().map(|(index, _)| plan.template_name(*index)).collect::<Vec<_>>().join("・"),
        };
        if !request.candidates.is_empty() {
            // 手直しの依頼を補完と取り違えないよう、テンプレートの補完のときの話だと添える。
            request.candidates.insert_str(0, "(テンプレートの補完なら)");
        }
    }
    let noun_request = noun_request(&request.request, &plan.dictionary, &plan.vocabulary);
    Prepared { request, extracted, named, candidates: found, hint, noun_request }
}

/// どの経路で本文を作ったか。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Route {
    /// テンプレートの補完ではない(手直し・該当なし)。答えをそのまま使う。
    NotTemplate,
    /// 構造の描画(S3)。
    Rendered,
    /// 全文経路(Claude が本文を書いた)。中身は理由(構造の無いテンプレート、言い直し、Claude が全文で答えた…)。
    FullText(String),
    /// 描画できなかったので全文で答え直させた(または答えにあった全文を使った)。中身は理由。「落ちた」に数える。
    Fallback(String),
    /// 指示に従わない答え(構造のあるテンプレートに本文)だったので、選択で答え直させて描画した(または本文の源泉の
    /// 行を選択に読み替えて描画した)。中身は理由。「落ちた」に数える。
    Retried(String),
}

impl Route {
    pub fn label(&self) -> String {
        match self {
            Route::NotTemplate => "補完ではない".to_string(),
            Route::Rendered => "構造の描画".to_string(),
            Route::FullText(why) => format!("全文経路({why})"),
            Route::Fallback(why) => format!("全文経路に落ちた({why})"),
            Route::Retried(why) => format!("構造の描画(答え直させた: {why})"),
        }
    }
}

/// `resolve` の結果(Claude を呼ばずに決まること)。
#[derive(Debug, Clone, PartialEq)]
pub enum Resolution {
    NotTemplate,
    /// 答えの全文を使う。
    Text { template: Option<usize>, why: String },
    /// 描画した。
    Rendered {
        template: usize,
        text: String,
        kept: Vec<Selection>,
        /// 描画の知らせのうち、利用者に見せる物(捨てた行・補えなかった行・程度で絞らなかった選択など)。
        warnings: Vec<String>,
        /// それ以外の知らせ(補った行、自動の行に任せた選択など)。`--complete-check` に出す。
        notes: Vec<String>,
        dropped: Vec<String>,
        /// 本文の答えを選択に読み替えて描いた(指示に従わない答えを直した。「落ちた」に数える)。
        read_from_text: bool,
    },
    /// 描画できない。全文で答え直させる。
    NeedFullText { template: Option<usize>, why: String, dropped: Vec<String> },
    /// 構造のあるテンプレートに、源泉に無い文を含む本文で答えた(指示に従わない答え)。選択で答え直させる。
    NeedSelections { template: usize, why: String },
}

/// 選択 1 つの門の結果。
enum Verdict {
    Keep(Selection),
    /// 〔　〕の無い本文の行の直し。項目ごとにまとめてから臨時の充填にする(2 行の項目の両方を直したとき、
    /// 元の文と直した文が並ばないように。QA H3)。
    LineEdit { slot: String, line: usize, text: String },
    /// 使わないが落としたのではない(変わらない本文の行)。中身は知らせ。
    Ignore(String),
    /// 退けた(§16.6 の守り: 文脈と食い違う、束の行の取り違え)。中身は理由。
    Drop(String),
    /// この選択があると構造では正しく描けない(全体差し替え、構造に無いモジュール、依頼に無い左右、長すぎる文、
    /// 置き場所の分からない本文の行)。全文で答え直させる(口述した所見を黙って落とさない。QA H4)。
    Fallback(String),
}

fn char_count(text: &str) -> usize {
    text.chars().count()
}

/// 〔　〕の中身を除いた文。
fn without_placeholders(text: &str) -> String {
    let mut out = String::new();
    let mut depth = 0usize;
    for c in text.chars() {
        match c {
            '〔' => depth += 1,
            '〕' if depth > 0 => depth -= 1,
            _ if depth == 0 => out.push(c),
            _ => {}
        }
    }
    out
}

/// 名詞の門の前の表記のならし(認識は「のう胞」と書くことがある。実物の学習の組「膵のう胞→膵嚢胞」)。
fn normalize_nouns(text: &str) -> String {
    text.replace("のう胞", "嚢胞").replace("のう腫", "嚢腫").replace('囊', "嚢")
}

/// どの文にもよく出る漢字(共有していても同じ所見とは言えない)。名詞の門の字の比べ方で除く。
const COMMON_KANJI: &str = "認的性部所見異常明様特記程度疑思有無両側左右上下内外大小高低";

/// 源泉の行の名詞が依頼の文に現れるか: 2-gram が 1 つ重なるか、よく出る字を除いた漢字が 2 字以上重なる
/// (「胆石」と「胆嚢結石」、「両側腎のう胞」と「両腎嚢胞」)。〔　〕の中と一般語・程度語の 2-gram は見ない。
/// 名詞が 1 つも無い行は判断できないので通す。
fn noun_overlap(source: &str, request: &str, vocabulary: &Vocabulary) -> bool {
    let source = normalize_nouns(&without_placeholders(source));
    let request = normalize_nouns(request);
    let stop: Vec<&str> = vocabulary
        .general_words
        .iter()
        .map(String::as_str)
        .chain(vocabulary.grade_word_list())
        .collect();
    let grams: Vec<String> = bigrams(&source)
        .into_iter()
        .filter(|g| !stop.iter().any(|w| w.contains(g.as_str())))
        .collect();
    if grams.is_empty() {
        return true;
    }
    if grams.iter().any(|g| request.contains(g.as_str())) {
        return true;
    }
    let kanji = |text: &str| -> BTreeSet<char> {
        text.chars()
            .filter(|c| ('\u{4e00}'..='\u{9fff}').contains(c) && !COMMON_KANJI.contains(*c))
            .collect()
    };
    kanji(&source).intersection(&kanji(&request)).count() >= 2
}

/// 名詞の門(§16.6): 束の行の取り違え(脂肪肝と言ったのに肝嚢胞の行)を見る。名詞の無い行は通す。
pub fn nouns_in_request(source: &str, request: &str, vocabulary: &Vocabulary) -> bool {
    noun_overlap(source, request, vocabulary)
}

/// 打ち消しの述語(左右の語の後ろで最初に出る述語がこれなら、その左右は所見の左右ではない)。長い物から。
const NEGATIVE_PREDICATES: [&str; 12] = [
    "認められません", "認めません", "みられません", "指摘できません", "ありません", "はっきりしません",
    "認めず", "なく", "なし", "無し", "ない", "ません",
];
/// 肯定の述語。
const POSITIVE_PREDICATES: [&str; 16] = [
    "認め", "あり", "有り", "みられ", "見られ", "思われ", "疑", "考え", "伴", "示し", "示す", "広が", "生じ", "消失",
    "偏位", "狭窄し",
];

/// 左右の語のうち、所見の左右の数(右, 左)。語の後ろで最初に出る述語が打ち消しなら数えない(「右上顎洞、両側の
/// 篩骨洞…には明らかな粘膜肥厚は認められません」「左右差なく」)。肯定なら数える(「左前頭葉に陳旧性梗塞を
/// 認めるほか、…指摘できません」の左は数える。QA 再確認 高 2)。述語が見つからなければ数える。
fn positive_sides(text: &str) -> (usize, usize) {
    let negative_at = |rest: &str| -> bool {
        let chars: Vec<(usize, char)> = rest.char_indices().collect();
        for (at, _) in chars {
            let tail = &rest[at..];
            if NEGATIVE_PREDICATES.iter().any(|p| tail.starts_with(p)) {
                return true;
            }
            if POSITIVE_PREDICATES.iter().any(|p| tail.starts_with(p)) {
                return false;
            }
        }
        false
    };
    side_positions(text)
        .into_iter()
        .filter(|(after, _)| !negative_at(&text[*after..]))
        .fold((0, 0), |(r, l), (_, side)| if side == '右' { (r + 1, l) } else { (r, l + 1) })
}

/// 左右の門(§16.3 の L5): 依頼に左右の語があれば、選択の文の所見の左右の数は依頼の数以下(左右それぞれ)。無ければ
/// 源泉の行の所見の左右の数以下(数え方は `adds_laterality` と同じ語)。打ち消しの述語に掛かる左右は、選択の文でも
/// 源泉でも数えない(めまいの源泉の「左右差なく」が、依頼に無い「右小脳橋角部に…」を通す枠にならないように)。
pub fn sides_ok(request: &str, source: &str, text: &str) -> bool {
    let (request_right, request_left) = side_counts(request);
    let (right, left) = positive_sides(text);
    if request_right + request_left > 0 {
        right <= request_right && left <= request_left
    } else {
        let (source_right, source_left) = positive_sides(source);
        right <= source_right && left <= source_left
    }
}

/// 束(1 モジュールの独立した行を、言われた行だけ選ぶ物。§16.2)か。尺度のある所見群は束ではない(程度で行が決まる)。
fn is_bundle(structure: &TemplateStructure, module: &str) -> bool {
    let lines = structure
        .variants
        .iter()
        .filter(|v| v.module == module && !v.derived && !v.is_whole() && v.line.is_some())
        .count();
    lines >= 2 && !structure.groups.iter().any(|g| g.module == module)
}

/// 選んだ変種(render と同じ選び方)。
fn chosen_variant<'a>(structure: &'a TemplateStructure, module: &str, line: Option<usize>) -> Option<&'a Variant> {
    let found: Vec<&Variant> = structure
        .variants
        .iter()
        .filter(|v| v.module == module && !v.is_whole() && !v.derived)
        .collect();
    match line {
        Some(i) => found
            .iter()
            .find(|v| v.line == Some(i))
            .copied()
            .or_else(|| (found.len() == 1 && found[0].line.is_none()).then(|| found[0])),
        None if found.len() == 1 => Some(found[0]),
        None => found.iter().find(|v| v.line.is_none()).copied(),
    }
}

fn module_source(template: &Template, variant: &Variant) -> Option<String> {
    let module = template.modules.iter().find(|m| m.name == variant.module)?;
    match variant.line {
        Some(line) => module_lines(module).get(line).map(|l| l.to_string()),
        None => Some(canonical_lines(&module.text).join("\n")),
    }
}

/// 列挙否定の行の直しで外す臓器。`organ` があれば列挙の名前に寄せる(「右腎」→ 腎、「脾臓」→ 脾)。無ければ文に
/// 出る臓器から決める(「副腎」の中の「腎」は数えない)。Ok(Some) は 1 臓器、Ok(None) は行ごとの直し(列挙の臓器を
/// 全部書き直した)、Err は決められない(臓器が 0 か、全部でない複数)。
fn list_organ(organs: &[String], organ: Option<&str>, text: &str) -> Result<Option<String>, ()> {
    let named_in = |haystack: &str| -> Vec<String> {
        let mut found: Vec<&String> = organs.iter().filter(|o| haystack.contains(o.as_str())).collect();
        // 長い名前に含まれる短い名前は、長い名前の所にしか無ければ数えない(副腎の中の腎)。
        found.retain(|o| {
            let longer: Vec<&String> = organs.iter().filter(|l| l.len() > o.len() && l.contains(o.as_str())).collect();
            longer.is_empty() || haystack.replace(longer[0].as_str(), "").contains(o.as_str())
        });
        found.into_iter().cloned().collect()
    };
    // 文に出る臓器を先に見る(organ が「腎」でも文が「左副腎に腺腫」なら副腎。QA 再確認 低)。
    let found = named_in(text);
    match found.len() {
        1 => return Ok(Some(found[0].clone())),
        n if n == organs.len() && n > 0 => return Ok(None),
        n if n > 1 => return Err(()),
        _ => {}
    }
    if let Some(organ) = organ {
        if organs.iter().any(|o| o == organ) {
            return Ok(Some(organ.to_string()));
        }
        if let [only] = named_in(organ).as_slice() {
            return Ok(Some(only.clone()));
        }
    }
    Err(())
}

/// 文が列挙否定の変種の型で、臓器が列挙の部分列(順も同じ)か(Claude が臓器を外しただけの書き換え)。
fn list_subset(text: &str, variant: &Variant) -> bool {
    let Some((head, tail)) = variant.pattern.split_once("{organs}") else {
        return false;
    };
    let Some(middle) = text.strip_prefix(head).and_then(|t| t.strip_suffix(tail)) else {
        return false;
    };
    let mut at = 0;
    for organ in middle.split(variant.sep.as_str()) {
        match variant.organs[at..].iter().position(|o| o == organ) {
            Some(found) => at += found + 1,
            None => return false,
        }
    }
    !middle.is_empty()
}

/// 選択 1 つの門(§16.6): 自動の行・文脈・程度・左右・長さ・名詞。本文の行の直しは充填か臨時に読み替える。
fn guard(
    selection: &Selection,
    template: &Template,
    structure: &TemplateStructure,
    prepared: &Prepared,
    plan: &Plan,
    notes: &mut Vec<String>,
    warnings: &mut Vec<String>,
) -> Verdict {
    let request = prepared.request.request.as_str();
    let body = canonical_lines(&template.body);
    if let Some(module) = &selection.module {
        let line_label = selection.line.map_or(String::new(), |l| format!(" {l} 行目"));
        if !template.modules.iter().any(|m| &m.name == module) {
            return Verdict::Fallback(format!("このテンプレートに無いモジュール「{module}」"));
        }
        let Some(variant) = chosen_variant(structure, module, selection.line) else {
            if structure.variants.iter().any(|v| &v.module == module && v.is_whole()) {
                return Verdict::Fallback(format!("全体差し替えのモジュール「{module}」"));
            }
            return Verdict::Fallback(format!("構造に無いモジュール「{module}」{line_label}"));
        };
        let line_label = variant.line.map_or(String::new(), |l| format!(" {l} 行目"));
        let source = module_source(template, variant).unwrap_or_default();
        // 列挙否定の変種(若い女性の T0)の文はコードが繋ぎ直す。Claude が臓器を外しただけの文に書き換えたなら
        // (「胆、膵、脾、副腎に…」)元の文として扱い、外し方はほかの選択からコードが決める。それ以外の書き換えは
        // 描けない(所見が消える)ので全文で答え直させる(QA 再確認 中 3)。
        let mut selection = selection.clone();
        if variant.kind == Some(LineKind::ListNegation) && selection.text != source {
            if !list_subset(&selection.text, variant) {
                return Verdict::Fallback(format!("「{module}」{line_label}(列挙否定)の文が書き換えられた"));
            }
            notes.push(format!("「{module}」{line_label}の列挙は、ほかの選択から臓器を外して繋ぎ直します"));
            selection.text = source.clone();
        }
        let selection = &selection;
        // S1 で決めた意味(§16.3): Claude が返した選択は、自動の変種でも口述として扱い、退けるのは条件が文脈と
        // 食い違うときだけ(外傷の精査で「C5 椎体に骨髄浮腫」と言われたら、陰性の自動文ではなく口述の文。QA H1)。
        // 自動の変種を源泉の文のまま選び、条件が文脈に合う物は、自動で置かれる物と同じ(描画が 1 回にまとめる)。
        let same_as_auto = variant.auto
            && selection.text == source
            && variant.condition.as_ref().is_none_or(|c| c.matches(&prepared.extracted.context));
        if let Some(condition) = &variant.condition {
            if condition.contradicts(&prepared.extracted.context) {
                return Verdict::Drop(format!("「{module}」{line_label}: 条件({})が文脈と食い違う", condition.label()));
            }
        }
        let mut grade = selection.grade.clone();
        match group_levels(structure, &plan.vocabulary, module) {
            None => {
                if grade.take().is_some() {
                    notes.push(format!("「{module}」に尺度は無いので程度は見ません"));
                }
            }
            Some(levels) => {
                // 程度で行を絞るのは、選択の段が依頼の程度語の段のどれかのときだけ(§16.6)。違えば絞らない(選んだ
                // 行をそのまま置く。別の所見の程度語を所見群に当てて行を落とさない。QA H2)。
                if let Some(g) = &grade {
                    let dictated = prepared.extracted.grades.iter().any(|d| d == g && levels.contains(d));
                    if !dictated {
                        warnings.push(format!("「{module}」の程度「{g}」は依頼の程度の語に無いので、程度で行を絞りませんでした"));
                        grade = None;
                    }
                }
            }
        }
        if !sides_ok(request, &source, &selection.text) {
            return Verdict::Fallback(format!("「{module}」{line_label}: 依頼に無い左右"));
        }
        if char_count(&selection.text) > char_count(&source) * 2 + SELECTION_SLACK_CHARS {
            return Verdict::Fallback(format!("「{module}」{line_label}: 文が長すぎる"));
        }
        // 束の行(行に割った若い女性の自動の行も含む)は名詞の門を通す: 言われていない虫垂の行・生理的な腹水の行を
        // 選ばれたら退けて知らせる(黙って置かない・黙って捨てない)。Claude が行を書き換えて、その文の名詞が依頼に
        // あれば通す(若い女性の肺の行に「右肺下葉に 5 mm の結節」と書いた。QA 再確認 高 1)。
        let nouns = &prepared.noun_request;
        let dictated = nouns_in_request(&source, nouns, &plan.vocabulary)
            || (selection.text != source && nouns_in_request(&selection.text, nouns, &plan.vocabulary));
        if is_bundle(structure, module) && !same_as_auto && !dictated {
            return Verdict::Drop(format!("「{module}」{line_label}: 行の名詞が依頼に無い(行の取り違えの疑い)"));
        }
        return Verdict::Keep(Selection {
            module: Some(module.clone()),
            line: variant.line,
            grade,
            text: selection.text.clone(),
            ..Selection::default()
        });
    }
    let Some(line) = selection.line else {
        return Verdict::Fallback("モジュールも行も無い選択".to_string());
    };
    let (Some(info), Some(source)) = (structure.lines.get(line), body.get(line)) else {
        return Verdict::Fallback(format!("本文に {line} 行目は無い"));
    };
    let Some(slot) = info.slot.clone().filter(|_| matches!(info.kind, LineKind::Sentence | LineKind::ListNegation)) else {
        return Verdict::Fallback(format!("本文 {line} 行目は見出しか空行"));
    };
    if selection.text == *source {
        return Verdict::Ignore(format!("本文 {line} 行目は変わっていないので置き直しません"));
    }
    if !sides_ok(request, source, &selection.text) {
        return Verdict::Fallback(format!("本文 {line} 行目: 依頼に無い左右"));
    }
    if char_count(&selection.text) > char_count(source) * 2 + SELECTION_SLACK_CHARS {
        return Verdict::Fallback(format!("本文 {line} 行目: 文が長すぎる"));
    }
    if info.kind == LineKind::ListNegation {
        // 列挙否定の行: 1 臓器の文なら列挙から外して直後に置く(ほかの臓器の陰性文を消さない。QA M5)。
        return match list_organ(&info.organs, selection.organ.as_deref(), &selection.text) {
            Ok(Some(organ)) => Verdict::Keep(Selection::adhoc(&slot, Some(&organ), &selection.text)),
            Ok(None) => Verdict::LineEdit { slot, line, text: selection.text.clone() },
            Err(()) => Verdict::Fallback(format!("本文 {line} 行目(列挙)のどの臓器の文か決められない")),
        };
    }
    if selection.organ.is_some() {
        notes.push(format!("本文 {line} 行目は列挙の行ではないので、臓器の指定は見ません"));
    }
    if has_placeholder(source) {
        return Verdict::Keep(Selection::fill(line, &selection.text));
    }
    Verdict::LineEdit { slot, line, text: selection.text.clone() }
}

/// 文脈で当たる auto の変種。
fn applying_autos<'a>(structure: &'a TemplateStructure, prepared: &Prepared) -> Vec<&'a Variant> {
    structure
        .variants
        .iter()
        .filter(|v| v.auto && !v.is_whole() && v.condition.as_ref().is_none_or(|c| c.matches(&prepared.extracted.context)))
        .collect()
}

/// 選ばれた所見群(尺度のあるモジュール)の変種すべて(程度で補われる行も含む)。
fn group_variants<'a>(structure: &'a TemplateStructure, kept: &[Selection]) -> Vec<&'a Variant> {
    structure
        .variants
        .iter()
        .filter(|v| !v.is_whole() && !v.derived)
        .filter(|v| structure.groups.iter().any(|g| g.module == v.module))
        .filter(|v| kept.iter().any(|s| s.module.as_deref() == Some(v.module.as_str())))
        .collect()
}

/// 描画の後の局所性(§16.3 の L2): 選択(auto を含む)の対象の行・充填した行・接頭語の及ぶ行以外の本文の行は、
/// 源泉の順のまま一字一句残っている。
fn locality_holds(template: &Template, structure: &TemplateStructure, kept: &[Selection], autos: &[&Variant], output: &str) -> bool {
    let body = canonical_lines(&template.body);
    let mut slots: BTreeSet<String> = BTreeSet::new();
    let mut lines: BTreeSet<usize> = BTreeSet::new();
    let touch = |variant: &Variant, slots: &mut BTreeSet<String>| {
        if matches!(variant.mode, Mode::Replace | Mode::Remove) || variant.prefix.is_some() {
            slots.extend(variant.target.iter().cloned());
        }
    };
    for selection in kept {
        if let Some(module) = &selection.module {
            if let Some(variant) = chosen_variant(structure, module, selection.line) {
                touch(variant, &mut slots);
            }
        } else if let Some(slot) = &selection.slot {
            slots.insert(slot.clone());
        } else if let Some(line) = selection.line {
            lines.insert(line);
        }
    }
    // 尺度のある所見群は、選ばれていない行も程度で補われる(L7)ので、所見群の項目すべてを対象に数える。
    for variant in group_variants(structure, kept) {
        touch(variant, &mut slots);
    }
    for variant in autos {
        touch(variant, &mut slots);
    }
    let untouched: Vec<&str> = structure
        .lines
        .iter()
        .filter(|l| !lines.contains(&l.i) && l.slot.as_ref().is_none_or(|s| !slots.contains(s)))
        .filter_map(|l| body.get(l.i).copied())
        .collect();
    let out: Vec<&str> = output.split('\n').collect();
    let mut at = 0;
    for line in &out {
        if at < untouched.len() && untouched[at] == *line {
            at += 1;
        }
    }
    at == untouched.len()
}

/// 描画の知らせのうち、利用者に見せる物(捨てた・補えなかった・置かなかった・使わなかった・併置した)。
fn is_warning(note: &str) -> bool {
    ["捨てました", "補えませんでした", "置きませんでした", "使いませんでした", "併置しました", "文ごと差し替えました", "揃えました"]
        .iter()
        .any(|w| note.contains(w))
}

/// 本文の答えを選択に読み替える。本文の行はそのまま、行ごとのモジュールの行はその行の選択、まとめて 1 つの
/// モジュールは全行が続けて現れたときだけその選択にする。源泉に無い行や、まとめて 1 つのモジュールの一部の行が
/// あれば None(読み替えない)。
fn selections_from_text(text: &str, template: &Template, structure: &TemplateStructure) -> Option<Vec<Selection>> {
    let body = canonical_lines(&template.body);
    let lines: Vec<&str> = text.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    let mut read: Vec<Selection> = Vec::new();
    let mut at = 0;
    'next: while at < lines.len() {
        let line = lines[at];
        for module in &template.modules {
            let module_text = module_lines(module);
            let variants: Vec<&Variant> = structure
                .variants
                .iter()
                .filter(|v| v.module == module.name && !v.derived && !v.is_whole())
                .collect();
            if let Some(unit) = variants.iter().find(|v| v.line.is_none()) {
                let n = module_text.len();
                if n > 0 && lines.len() >= at + n && lines[at..at + n] == module_text[..] {
                    read.push(Selection::variant(&unit.module, None, &module_text.join("\n")));
                    at += n;
                    continue 'next;
                }
                if module_text.contains(&line) {
                    return None;
                }
                continue;
            }
            if let Some(index) = module_text.iter().position(|l| *l == line) {
                if variants.iter().any(|v| v.line == Some(index)) {
                    read.push(Selection::variant(&module.name, Some(index), line));
                    at += 1;
                    continue 'next;
                }
            }
        }
        if !body.contains(&line) {
            return None;
        }
        at += 1;
    }
    Some(read)
}

/// Claude の答えのテンプレート名を照合する。名前に飾り(「(構造あり)」)が付いていても受ける。
fn match_reply_template(dictionary: &Dictionary, name: &str) -> Option<usize> {
    match_template(dictionary, name).or_else(|| {
        let cut = name.find(['(', '（']).map_or(name, |at| &name[..at]);
        (cut != name).then(|| match_template(dictionary, cut)).flatten()
    })
}

/// 答えを読み替えて守りを掛け、描画する(Claude は呼ばない)。
pub fn resolve(prepared: &Prepared, reply: &AssistantReply, plan: &Plan) -> Resolution {
    if reply.kind != ReplyKind::Template {
        return Resolution::NotTemplate;
    }
    let index = reply
        .template
        .as_deref()
        .and_then(|name| match_reply_template(&plan.dictionary, name))
        .or(prepared.named)
        .or_else(|| (prepared.candidates.len() == 1).then(|| prepared.candidates[0].0));
    let has_text = reply.text.is_some();
    let text_or_fallback = |template: Option<usize>, why: &str| {
        if has_text {
            Resolution::Text { template, why: why.to_string() }
        } else {
            Resolution::NeedFullText { template, why: why.to_string(), dropped: Vec::new() }
        }
    };
    if reply.replace {
        return text_or_fallback(index, "言い直し");
    }
    let Some(index) = index else {
        return text_or_fallback(None, "テンプレート名が照合できない");
    };
    let template = &plan.dictionary.templates[index];
    let Some(structure) = plan.structures.structure_for(template) else {
        return text_or_fallback(Some(index), "構造の無いテンプレート");
    };
    let mut notes = Vec::new();
    let mut selections = reply.selections.clone();
    let mut read_from_text = false;
    if selections.is_empty() && has_text {
        // 構造のあるテンプレートに本文で答えた。全文で答えてよい理由(全体差し替えか構造の無いモジュール)があれば
        // その本文を使う。理由が無く、本文の行がどれも源泉の行(本文かモジュールの文)なら、モジュールの行を選択に
        // 読み替えて、ほかの答えと同じ守りと描画に通す(Claude が本文を書き写すとき行を落とすことがある: 若い女性の
        // 異常なしで消化管と腹水の行が抜けた)。源泉に無い行(値を入れた文・新しい所見)があれば、その本文を使う。
        let legitimate = template.modules.iter().any(|m| {
            let variants: Vec<&Variant> = structure.variants.iter().filter(|v| v.module == m.name && !v.derived).collect();
            !m.text.trim().is_empty() && (variants.is_empty() || variants.iter().any(|v| v.is_whole() && !v.split))
        });
        match (legitimate, selections_from_text(reply.text.as_deref().unwrap_or_default(), template, structure)) {
            (false, Some(read)) => {
                notes.push(format!("Claude が本文で答えたので、源泉の行を選択 {} 個に読み替えました", read.len()));
                selections = read;
                read_from_text = true;
            }
            // 源泉に無い文を含む本文: 守りの無い本文を黙って使わず、選択で答え直させる(行を落とした本文が全文経路で
            // 入った。QA 再確認 中 2)。答え直しでも本文なら、その本文を使う。
            (false, None) if prepared.request.selections_only.is_none() => {
                return Resolution::NeedSelections { template: index, why: "構造のあるテンプレートに本文で答えた".to_string() };
            }
            _ => return Resolution::Text { template: Some(index), why: "Claude が全文で答えた".to_string() },
        }
    }
    let mut warnings = Vec::new();
    let mut dropped = Vec::new();
    let mut kept = Vec::new();
    let mut edits: Vec<(String, usize, String)> = Vec::new();
    for selection in &selections {
        match guard(selection, template, structure, prepared, plan, &mut notes, &mut warnings) {
            Verdict::Keep(selection) => kept.push(selection),
            Verdict::LineEdit { slot, line, text } => edits.push((slot, line, text)),
            Verdict::Ignore(note) => notes.push(note),
            Verdict::Drop(reason) => dropped.push(reason),
            Verdict::Fallback(why) => return Resolution::NeedFullText { template: Some(index), why, dropped },
        }
    }
    // 本文の行の直しのうち、選ばれた所見群の 1 つの行だけが差し替える項目の物は、その行の選択にする(程度で
    // 補われる行と並ばないように。中等度の頸椎症で髄内を本文の行で直したとき。QA M6)。
    // 読み替えるのは、程度が効いていて(補いが起きうる)、Claude がその行を自分では選んでいないときだけ(選んでいれば
    // 両方の文を併置する。椎間板の変性の行と、本文の行に書いた椎間板ヘルニア。QA 再確認 中 1)。
    let groups = group_variants(structure, &kept);
    edits.retain(|(slot, _, text)| {
        let owners: Vec<&&Variant> = groups
            .iter()
            .filter(|v| v.mode == Mode::Replace && v.slot.is_none() && v.target.len() == 1 && &v.target[0] == slot)
            .collect();
        let [owner] = owners.as_slice() else { return true };
        let grade = kept
            .iter()
            .filter(|s| s.module.as_deref() == Some(owner.module.as_str()))
            .find_map(|s| s.grade.clone());
        let selected = kept.iter().any(|s| s.module.as_deref() == Some(owner.module.as_str()) && s.line == owner.line);
        if grade.is_none() || selected {
            return true;
        }
        kept.push(Selection { module: Some(owner.module.clone()), line: owner.line, grade, text: text.clone(), ..Selection::default() });
        notes.push(format!("本文の「{slot}」の直しを「{}」の {} 行目として置きます", owner.module, owner.line.unwrap_or(0)));
        false
    });
    // 〔　〕の無い本文の行の直しは、項目ごとに 1 つの臨時の充填にまとめる。直さない行は源泉の文、同じ項目の充填は
    // その文で(QA H3: 膵の 2 行を両方直すと元の文と直した文が並んでいた)。
    let body = canonical_lines(&template.body);
    let mut slots_done: Vec<String> = Vec::new();
    for (slot, _, _) in &edits {
        if slots_done.contains(slot) {
            continue;
        }
        slots_done.push(slot.clone());
        let mut lines: Vec<String> = Vec::new();
        for info in structure.lines.iter().filter(|l| l.slot.as_deref() == Some(slot.as_str())) {
            let edited = edits.iter().rev().find(|(s, l, _)| s == slot && *l == info.i).map(|(_, _, t)| t.clone());
            let filled = kept
                .iter()
                .position(|s| s.module.is_none() && s.slot.is_none() && s.line == Some(info.i))
                .map(|at| kept.remove(at).text);
            lines.push(edited.or(filled).unwrap_or_else(|| body.get(info.i).map(|t| t.to_string()).unwrap_or_default()));
        }
        kept.push(Selection::adhoc(slot, None, &lines.join("\n")));
    }
    let rendered = match render(template, structure, &plan.vocabulary, &prepared.extracted.context, &kept) {
        Ok(rendered) => rendered,
        Err(error) => {
            return Resolution::NeedFullText { template: Some(index), why: error.to_string(), dropped };
        }
    };
    for note in rendered.notes {
        if is_warning(&note) {
            warnings.push(note);
        } else {
            notes.push(note);
        }
    }
    let autos = applying_autos(structure, prepared);
    if !locality_holds(template, structure, &kept, &autos, &rendered.text) {
        return Resolution::NeedFullText { template: Some(index), why: "描画の後の局所性(L2)が破れた".to_string(), dropped };
    }
    // 描画は源泉の本文・auto の文・所見群の行・選択の文を並べるだけなので、左右の数はその合計を超えない(L5)。
    let mut allowed = side_counts(&template.body);
    for text in autos
        .iter()
        .chain(group_variants(structure, &kept).iter())
        .filter_map(|v| module_source(template, v))
        .chain(kept.iter().map(|s| s.text.clone()))
    {
        let (right, left) = side_counts(&text);
        allowed = (allowed.0 + right, allowed.1 + left);
    }
    let (right, left) = side_counts(&rendered.text);
    if right > allowed.0 || left > allowed.1 {
        return Resolution::NeedFullText { template: Some(index), why: "描画の後の左右(L5)が増えた".to_string(), dropped };
    }
    if char_count(&rendered.text) > MAX_COMPLETION_CHARS {
        return Resolution::NeedFullText { template: Some(index), why: "描画した本文が長すぎる".to_string(), dropped };
    }
    let mut seen = BTreeSet::new();
    warnings.retain(|w| seen.insert(w.clone()));
    Resolution::Rendered { template: index, text: rendered.text, kept, warnings, notes, dropped, read_from_text }
}

/// 1 回の補完の結果。`reply.text` が当てる本文。
#[derive(Debug, Clone)]
pub struct Outcome {
    pub reply: AssistantReply,
    pub route: Route,
    /// 照合したテンプレートの先頭のキーワード(照合できなければ Claude の答えの名前)。
    pub template: Option<String>,
    /// Claude の最初の答えの選択(読み替えの前)。
    pub claude_selections: Vec<Selection>,
    /// 描画に使った選択(守りと読み替えの後)。
    pub kept: Vec<Selection>,
    /// 退けた選択と理由。
    pub dropped: Vec<String>,
    /// 描画の知らせのうち利用者に見せる物(捨てた行・補えなかった行・程度で絞らなかった選択など)。知らせに出す。
    pub warnings: Vec<String>,
    /// それ以外の描画の知らせ(補った行、自動の行に任せた選択など)。
    pub notes: Vec<String>,
    /// Claude を呼んだ回数(全文で答え直させたら 2)。
    pub calls: u32,
    /// 検査名が無いときの、選んだ以外の候補(知らせに出す)。点数がいちばん高い候補だけ(低い点の当たりは知らせない)。
    pub other_candidates: Vec<String>,
    /// 依頼に添えた文脈の表示(知らせに出す。文脈は本文に出ないので。§16.9)。
    pub context: String,
    /// 文脈に入れなかった理由(男女の語が両方ある、など)。
    pub context_notes: Vec<String>,
    /// 選んだテンプレートが候補に無かった(候補があったときだけ)。
    pub off_candidates: bool,
}

impl Outcome {
    /// 「落ちた」(描画できず全文経路に逃げた・選択を退けた)の数。
    pub fn failures(&self) -> usize {
        self.dropped.len() + usize::from(matches!(self.route, Route::Fallback(_) | Route::Retried(_)))
    }
}

/// 補完を 1 回行う: Claude に送り、構造があれば描画し、描画できなければ全文で答え直させる。
pub fn run(assistant: &dyn Assistant, prepared: &Prepared, plan: &Plan) -> Result<Outcome, String> {
    let first = assistant.complete(&prepared.request)?;
    let claude_selections = first.selections.clone();
    let mut outcome = Outcome {
        reply: first.clone(),
        route: Route::NotTemplate,
        template: first.template.clone(),
        claude_selections,
        kept: Vec::new(),
        dropped: Vec::new(),
        warnings: Vec::new(),
        notes: Vec::new(),
        calls: 1,
        other_candidates: Vec::new(),
        off_candidates: false,
        context: prepared.request.context.clone(),
        context_notes: prepared.extracted.notes.clone(),
    };
    let mut resolution = resolve(prepared, &first, plan);
    let mut retried: Option<String> = None;
    if let Resolution::NeedSelections { template, why } = &resolution {
        // 選択で答え直させる。描けなければ 1 回目の本文を使う(全文経路に落ちた)。
        let mut again = prepared.clone();
        again.request.selections_only = Some(plan.template_name(*template));
        let second = assistant.complete(&again.request)?;
        outcome.calls = 2;
        outcome.claude_selections = second.selections.clone();
        let template = *template;
        retried = Some(why.clone());
        resolution = match resolve(&again, &second, plan) {
            rendered @ Resolution::Rendered { .. } => rendered,
            _ => Resolution::Text { template: Some(template), why: format!("{why}。選択で答え直させても描けなかった") },
        };
    }
    let chosen = match resolution {
        Resolution::NeedSelections { .. } => unreachable!("答え直しのあとは描画か本文"),
        Resolution::NotTemplate => return Ok(outcome),
        Resolution::Text { template, why } => {
            outcome.route = if retried.is_some() { Route::Fallback(why) } else { Route::FullText(why) };
            template
        }
        Resolution::Rendered { template, text, kept, warnings, notes, dropped, read_from_text } => {
            outcome.route = match retried.take() {
                Some(why) => Route::Retried(why),
                None if read_from_text => Route::Retried("本文の答えの源泉の行を選択に読み替えた".to_string()),
                None => Route::Rendered,
            };
            outcome.reply.text = Some(text);
            outcome.kept = kept;
            outcome.warnings = warnings;
            outcome.notes = notes;
            outcome.dropped = dropped;
            Some(template)
        }
        Resolution::NeedFullText { template, why, dropped } => {
            outcome.dropped = dropped;
            outcome.route = Route::Fallback(why.clone());
            if first.text.is_none() {
                // 全文で答え直させる(同じ依頼に「全文で答える」を添える)。
                let name = template
                    .map(|i| plan.template_name(i))
                    .or_else(|| first.template.clone())
                    .unwrap_or_else(|| "選んだテンプレート".to_string());
                let mut again = prepared.request.clone();
                again.full_text = Some(name);
                let second = assistant.complete(&again)?;
                outcome.calls = 2;
                let Some(text) = second.text.clone() else {
                    return Err(format!("構造で置けず、全文でも補完できませんでした({why})"));
                };
                outcome.reply.text = Some(text);
                outcome.reply.reply = second.reply.clone();
                if outcome.template.is_none() {
                    outcome.template = second.template.clone();
                }
            }
            template
        }
    };
    if let Some(index) = chosen {
        outcome.template = Some(plan.template_name(index));
        if !prepared.candidates.is_empty() {
            outcome.off_candidates = !prepared.candidates.iter().any(|(i, _)| *i == index);
            let top = prepared.candidates.iter().map(|(_, score)| *score).max().unwrap_or(0);
            outcome.other_candidates = prepared
                .candidates
                .iter()
                .filter(|(i, score)| *i != index && *score == top)
                .map(|(i, _)| plan.template_name(*i))
                .collect();
        }
    }
    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholders_are_removed_before_looking_for_nouns() {
        assert_eq!(without_placeholders("左副腎に〔　〕mm程度の結節"), "左副腎にmm程度の結節");
        let v = Vocabulary::builtin();
        assert!(nouns_in_request("両腎嚢胞あり。", "肝嚢胞、腎嚢胞のみ", &v), "2-gram で当たる");
        assert!(!nouns_in_request("肝嚢胞を認めます。", "軽度の脂肪肝", &v), "取り違え");
        assert!(!nouns_in_request("生理的な少量の腹水を認めます。", "若い女性、異常なし", &v));
        assert!(nouns_in_request("生理的な少量の腹水を認めます。", "若い女性、少量の腹水", &v));
        // QA H4: 言い方の違い・認識の表記(のう胞・囊)でも当たる。
        assert!(nouns_in_request("胆嚢結石あり。", "腹部骨盤CTテンプレート、胆石", &v), "胆石");
        assert!(nouns_in_request("両腎嚢胞あり。", "両側腎のう胞", &v));
        assert!(nouns_in_request("肝嚢胞を認めます。", "肝囊胞のみ", &v));
        assert!(!nouns_in_request("両腎嚢胞あり。", "軽度の脂肪肝と左副腎に12ミリの腺腫疑い", &v), "腎の 1 字だけでは当たらない");
        assert!(!nouns_in_request("生理的な少量の腹水を認めます。", "若い女性、腹部骨盤CTテンプレート、異常なし", &v));
    }

    #[test]
    fn list_organs_are_found_in_the_organ_or_the_text() {
        let organs: Vec<String> = ["脾", "腎", "副腎"].iter().map(|s| s.to_string()).collect();
        assert_eq!(list_organ(&organs, Some("右腎"), "右腎に結石を認めます。"), Ok(Some("腎".to_string())));
        assert_eq!(list_organ(&organs, Some("脾臓"), "脾臓に嚢胞。"), Ok(Some("脾".to_string())));
        assert_eq!(list_organ(&organs, None, "右腎に結石を認めます。"), Ok(Some("腎".to_string())));
        assert_eq!(list_organ(&organs, None, "左副腎に結節。"), Ok(Some("副腎".to_string())), "副腎の中の腎は数えない");
        assert_eq!(list_organ(&organs, None, "脾、腎、副腎に異常なし。"), Ok(None), "全部書き直したら行ごと");
        assert_eq!(list_organ(&organs, None, "右腎結石と左副腎腺腫。"), Err(()), "全部でない複数は決めない");
        assert_eq!(list_organ(&organs, None, "結石を認めます。"), Err(()));
        assert_eq!(list_organ(&organs, Some("腎"), "左副腎に腺腫を疑います。"), Ok(Some("副腎".to_string())), "文の臓器を先に見る");
        assert_eq!(list_organ(&organs, Some("腎"), "結石を認めます。"), Ok(Some("腎".to_string())), "文に無ければ organ");
    }

    #[test]
    fn sides_follow_the_request_or_the_source() {
        assert!(sides_ok("右上葉に8ミリ", "〔　〕に〔　〕mm", "右上葉に8mm"));
        assert!(!sides_ok("右上葉に8ミリ", "〔　〕に〔　〕mm", "左上葉に8mm"));
        assert!(sides_ok("副腎腺腫", "左副腎に〔　〕mm", "左副腎に12mm"), "依頼に無ければ源泉の左右は残してよい");
        assert!(!sides_ok("肝嚢胞", "肝嚢胞を認めます。", "右葉に肝嚢胞を認めます。"));
        // 打ち消しの文の左右は数えない(QA H4: 左上顎洞炎で、注どおり「右上顎洞…は認められません」を書く)。
        assert!(sides_ok(
            "副鼻腔CTで左上顎洞炎",
            "〔　〕上顎洞に…",
            "左上顎洞に軟部濃度域が広がり、上顎洞炎と思われます。
右上顎洞、両側の篩骨洞には明らかな粘膜肥厚は認められません。"
        ));
        assert!(!sides_ok("副鼻腔CTで左上顎洞炎", "〔　〕上顎洞に…", "右上顎洞炎と思われます。"));
        // QA 再確認 高 2: 打ち消しで終わる文の中の肯定の左右は数える。源泉の打ち消しの左右は枠にならない。
        assert!(!sides_ok("右前頭葉に陳旧性梗塞", "大脳白質に…", "左前頭葉に陳旧性梗塞を認めるほか、明らかな異常信号は指摘できません。"));
        assert!(!sides_ok(
            "副鼻腔CTで左上顎洞炎",
            "〔　〕上顎洞に…",
            "右上顎洞に軟部濃度域が広がり上顎洞炎と思われるが、左上顎洞には粘膜肥厚は認められません。"
        ));
        assert!(!sides_ok(
            "めまいの精査、小脳橋角部に腫瘍疑い",
            "両側の小脳橋角部にも、聴神経腫瘍などを考慮する占拠性病変は認められません。\n内耳の形態に左右差なく、明らかな異常を認めません。",
            "右小脳橋角部に占拠性病変を認めます。"
        ));
        assert_eq!(positive_sides("内耳の形態に左右差なく、明らかな異常を認めません。"), (0, 0));
        assert_eq!(positive_sides("左副腎に〔　〕mm程度の結節状構造がみられ、皮質腺腫を疑います。"), (0, 1));
    }
}
