//! 依頼の文から文脈(性別・年齢帯・精査目的)と口述の程度語を取る。`docs/simplify-and-assistant-plan.md`
//! §16.6・§16.13(単位 S3)。決定論で、LLM は使わない(D10: Claude に文脈を決めさせない)。
//!
//! 見るのは依頼の文の語だけ(エディターの文章や直前の補完は見ない。§16.9 の「文脈の聞き違い」の守り)。
//! 取れた文脈は依頼に 1 行添えて Claude に知らせ(`assistant::build_user_message`)、描画の門と auto に使う
//! (`structure::render`)。程度語は選択の `grade` の守りに使う(§16.13)。

use super::dictionary::Dictionary;
use super::structure::{Context, Vocabulary};

/// 依頼の文から取った物。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Extracted {
    pub context: Context,
    /// 口述の程度語が指す段(尺度の順。重なりは除く)。言われていなければ空。
    pub grades: Vec<String>,
    /// 文脈に入れなかった理由(男女の語が両方ある、年齢帯が二つ、など)。知らせと `--complete-check` に出す。
    pub notes: Vec<String>,
}

const FEMALE_WORDS: [&str; 3] = ["女性", "女児", "女子"];
const MALE_WORDS: [&str; 3] = ["男性", "男児", "男子"];
/// 年齢帯の語(§16.6: 「若い・若年・高齢・老年」。「成人」も足した。成人男性などの言い方のため)。
const AGE_WORDS: [(&str, &str); 6] = [
    ("若い", "若年"),
    ("若年", "若年"),
    ("高齢", "高齢"),
    ("老年", "高齢"),
    ("老人", "高齢"),
    ("成人", "成人"),
];

fn is_kanji(c: char) -> bool {
    ('\u{4e00}'..='\u{9fff}').contains(&c) || c == '々'
}

/// 半角・全角の数字の値。
fn digit_value(c: char) -> Option<u32> {
    match c {
        '0'..='9' => Some(c as u32 - '0' as u32),
        '０'..='９' => Some(c as u32 - '０' as u32),
        _ => None,
    }
}

/// テンプレートのキーワード(「女性骨盤MRI」「婦人科MRI」など)を伏せる(検査名の中の「女性」を患者の性別と取らない)。
/// `modules` ならモジュールの名前(「直線化・軽度変性」)も伏せる(程度語を取るときだけ。「若い女性」もモジュール名
/// なので、文脈を取るときには伏せない)。
fn mask_keywords(text: &str, dictionary: Option<&Dictionary>, modules: bool) -> String {
    let mut masked = text.to_string();
    let Some(dictionary) = dictionary else {
        return masked;
    };
    let module_names = dictionary
        .templates
        .iter()
        .flat_map(|t| t.modules.iter().map(|m| m.name.as_str()))
        .filter(|_| modules);
    let mut names: Vec<&str> = dictionary
        .template_keywords()
        .chain(module_names)
        .map(str::trim)
        .filter(|k| k.chars().count() >= 2)
        .collect();
    names.sort_by_key(|k| std::cmp::Reverse(k.chars().count()));
    for name in names {
        if masked.contains(name) {
            masked = masked.replace(name, "／");
        }
    }
    masked
}

/// 打ち消しの語。
const NEGATIONS: [&str; 7] = ["ない", "なし", "無し", "無い", "ません", "認めず", "なく"];

/// 名詞(精査目的)が打ち消されているか: 語のすぐ後ろ(「は」「が」「も」を挟んでよい)が打ち消し
/// (「腹痛なし」「腹痛はない」「腹痛は認めない」)。「腹痛精査で異常なし」の「なし」は腹痛を打ち消さない。
fn noun_negated(rest: &str) -> bool {
    let rest = rest.trim_start_matches(['は', 'が', 'も']);
    NEGATIONS.iter().chain(["認めない", "ありません"].iter()).any(|n| rest.starts_with(n))
}

/// 程度語が打ち消されているか: 同じ句の中で、語の後ろ 8 字以内に打ち消しがある(「顕著な狭窄はない」
/// 「顕著でない」)。間に「で」「に」「と」があれば別の句とみなす(「中等度の腰椎症で他に異常なし」)。
fn grade_negated(rest: &str) -> bool {
    let end = rest.find(['、', '。', '，', ',', '\n']).unwrap_or(rest.len());
    // 「軽度ないし中等度」の「ないし」は打ち消しではない。「軽度の狭窄しかない」は程度を言っている。
    let clause = rest[..end].replace("ないし", "／");
    let clause = clause.as_str();
    if clause.contains("しか") {
        return false;
    }
    if ["でない", "ではない", "でなく"].iter().any(|n| clause.starts_with(n)) {
        return true;
    }
    let Some(at) = NEGATIONS.iter().filter_map(|n| clause.find(n)).min() else {
        return false;
    };
    let between = &clause[..at];
    between.chars().count() <= 8 && !between.contains(['で', 'に', 'と'])
}

/// 語が打ち消されずに現れるか(`grade` なら程度語の打ち消し、そうでなければ名詞の打ち消しで見る)。
fn mentions(text: &str, word: &str, grade: bool) -> bool {
    !word.is_empty()
        && text.match_indices(word).any(|(at, w)| {
            let rest = &text[at + w.len()..];
            !(if grade { grade_negated(rest) } else { noun_negated(rest) })
        })
}

/// 単独の「女」「男」か(「若い女、」「40歳男」)。後ろに漢字が続く語(「女性化」など)は語の一部として見ない。
fn has_single(text: &str, mark: char) -> bool {
    let chars: Vec<char> = text.chars().collect();
    chars
        .iter()
        .enumerate()
        .any(|(i, c)| *c == mark && chars.get(i + 1).is_none_or(|next| !is_kanji(*next)))
}

fn sex_of(text: &str) -> (bool, bool) {
    // 「女性化乳房」は性別の語ではない(男性の所見)。
    let text = &text.replace("女性化", "／");
    let female = FEMALE_WORDS.iter().any(|w| text.contains(w)) || has_single(text, '女');
    let male = MALE_WORDS.iter().any(|w| text.contains(w)) || has_single(text, '男');
    (female, male)
}

/// 年齢帯の境(語彙の `age_bands`)で年齢を帯にする。
fn band_of(age: u32, vocabulary: &Vocabulary) -> Option<String> {
    vocabulary
        .age_bands
        .iter()
        .find(|(_, (low, high))| *low <= age && age <= *high)
        .map(|(name, _)| name.clone())
}

/// 「35歳」「３５才」「30代」から年齢帯を取る。「30代」は 10 年の幅がひとつの帯に収まるときだけ。
fn bands_from_numbers(text: &str, vocabulary: &Vocabulary) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut bands = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        if digit_value(chars[i]).is_none() {
            i += 1;
            continue;
        }
        let start = i;
        let mut value: u32 = 0;
        while i < chars.len() {
            let Some(d) = digit_value(chars[i]) else { break };
            value = value.saturating_mul(10).saturating_add(d);
            i += 1;
        }
        // 小数の一部(5.5 歳のような言い方は無いが、寸法の「5.5」の後に「歳」は来ない)は見ない。
        let after_point = start > 0 && matches!(chars[start - 1], '.' | '．');
        if after_point || i - start > 3 {
            continue;
        }
        match chars.get(i) {
            Some('歳') | Some('才') => {
                if let Some(band) = band_of(value, vocabulary) {
                    bands.push(band);
                }
            }
            Some('代') => {
                let (low, high) = (band_of(value, vocabulary), band_of(value + 9, vocabulary));
                if let (Some(low), Some(high)) = (low, high) {
                    if low == high {
                        bands.push(low);
                    }
                }
            }
            _ => {}
        }
    }
    bands
}

/// 依頼の文から文脈と程度語を取る。`dictionary` があれば、テンプレートのキーワードの中の語(「女性骨盤MRI」の
/// 「女性」)は性別に数えない。
pub fn extract(request: &str, vocabulary: &Vocabulary, dictionary: Option<&Dictionary>) -> Extracted {
    let mut notes = Vec::new();
    let text = mask_keywords(request, dictionary, false);

    // 性別。男女の語が両方あれば取らない(聞き違いの疑い。左右と同じ重さ、§16.9)。
    let sex = match sex_of(&text) {
        (true, false) => Some("女".to_string()),
        (false, true) => Some("男".to_string()),
        (true, true) => {
            notes.push("男女の語が両方あるので、性別は文脈に入れませんでした".to_string());
            None
        }
        (false, false) => None,
    };

    // 年齢帯。数と語の両方から集め、食い違えば取らない。
    let mut bands = bands_from_numbers(&text, vocabulary);
    for (word, band) in AGE_WORDS {
        if text.contains(word) && vocabulary.age_bands.contains_key(band) {
            bands.push(band.to_string());
        }
    }
    bands.sort();
    bands.dedup();
    let age_band = match bands.len() {
        0 => None,
        1 => bands.pop(),
        _ => {
            notes.push(format!("年齢帯が {} と食い違うので、文脈に入れませんでした", bands.join("・")));
            None
        }
    };

    // 精査目的。札(と別の書き方)が依頼の文に現れれば。
    let mut purposes = Vec::new();
    for purpose in &vocabulary.purposes {
        let aliases = vocabulary.purpose_aliases.get(purpose).map(Vec::as_slice).unwrap_or(&[]);
        if mentions(&text, purpose, false) || aliases.iter().any(|a| mentions(&text, a, false)) {
            purposes.push(purpose.clone());
        }
    }

    Extracted {
        context: Context { sex, age_band, purposes },
        grades: grades_in(&mask_keywords(request, dictionary, true), vocabulary),
        notes,
    }
}

/// 口述の程度語が指す段(§16.13、D20)。「最も強い」「比較的強い」はレベルの比較なので先に除く。打ち消された語
/// (「顕著な狭窄はない」)は数えない。散在・多発などは段ではないので対応表に無い。並びは既定の尺度の順。
pub fn grades_in(request: &str, vocabulary: &Vocabulary) -> Vec<String> {
    let mut text = request.to_string();
    let mut exclusions: Vec<&str> = vocabulary.grade_word_exclusions.iter().map(String::as_str).collect();
    exclusions.sort_by_key(|w| std::cmp::Reverse(w.chars().count()));
    for word in exclusions {
        text = text.replace(word, "／");
    }
    let mut found: Vec<String> = vocabulary
        .grade_words
        .iter()
        .filter(|(level, words)| mentions(&text, level, true) || words.iter().any(|w| mentions(&text, w, true)))
        .map(|(level, _)| level.clone())
        .collect();
    let position = |level: &String| vocabulary.default_scale.iter().position(|l| l == level).unwrap_or(usize::MAX);
    found.sort_by_key(position);
    found
}

/// 文脈の表示(「女性・若年・目的=腹痛」)。何も無ければ空。依頼への添え書きと知らせに使う。
pub fn context_label(context: &Context) -> String {
    let mut parts = Vec::new();
    match context.sex.as_deref() {
        Some("女") => parts.push("女性".to_string()),
        Some("男") => parts.push("男性".to_string()),
        Some(other) => parts.push(other.to_string()),
        None => {}
    }
    if let Some(age) = &context.age_band {
        parts.push(age.clone());
    }
    for purpose in &context.purposes {
        parts.push(format!("目的={purpose}"));
    }
    parts.join("・")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::dictionary::Template;

    fn vocabulary() -> Vocabulary {
        Vocabulary::builtin()
    }

    fn context(request: &str) -> Context {
        extract(request, &vocabulary(), None).context
    }

    #[test]
    fn sex_comes_from_the_words_of_the_request() {
        assert_eq!(context("若い女性、腹部骨盤CTテンプレート").sex.as_deref(), Some("女"));
        assert_eq!(context("成人男性、少量の腹水").sex.as_deref(), Some("男"));
        assert_eq!(context("40歳女、腹痛").sex.as_deref(), Some("女"));
        assert_eq!(context("65歳男").sex.as_deref(), Some("男"));
        assert_eq!(context("肝嚢胞のみ").sex, None);
        let both = extract("男性、女性", &vocabulary(), None);
        assert_eq!(both.context.sex, None, "両方なら取らない");
        assert_eq!(both.notes.len(), 1);
    }

    #[test]
    fn template_keywords_do_not_count_as_the_patient_sex() {
        let mut dictionary = Dictionary::default();
        dictionary.templates.push(Template {
            keywords: vec!["女性骨盤MRI".to_string(), "婦人科MRI".to_string()],
            body: "子宮に異常なし。".to_string(),
            ..Template::default()
        });
        let found = extract("女性骨盤MRIで子宮筋腫", &vocabulary(), Some(&dictionary));
        assert_eq!(found.context.sex, None);
        let found = extract("若い女性、女性骨盤MRIで機能性嚢胞", &vocabulary(), Some(&dictionary));
        assert_eq!(found.context.sex.as_deref(), Some("女"));
    }

    #[test]
    fn age_bands_come_from_ages_decades_and_words() {
        assert_eq!(context("35歳女性").age_band.as_deref(), Some("若年"));
        assert_eq!(context("３５才").age_band.as_deref(), Some("若年"));
        assert_eq!(context("40歳").age_band.as_deref(), Some("成人"), "境は 40 から成人(D2)");
        assert_eq!(context("72歳男性").age_band.as_deref(), Some("高齢"));
        assert_eq!(context("30代女性").age_band.as_deref(), Some("若年"));
        assert_eq!(context("60代").age_band, None, "成人と高齢にまたがる 10 年は取らない");
        assert_eq!(context("若い女性").age_band.as_deref(), Some("若年"));
        assert_eq!(context("高齢男性").age_band.as_deref(), Some("高齢"));
        assert_eq!(context("成人男性、少量の腹水").age_band.as_deref(), Some("成人"));
        assert_eq!(context("サイズは5.5かける4.5かける6センチメートル").age_band, None);
        let clash = extract("若い 70歳", &vocabulary(), None);
        assert_eq!(clash.context.age_band, None);
        assert_eq!(clash.notes.len(), 1);
    }

    #[test]
    fn purposes_come_from_the_closed_list_and_its_aliases() {
        assert_eq!(context("若い女性、腹痛精査").purposes, ["腹痛"]);
        assert_eq!(context("めまいの精査で頭部MRI").purposes, ["めまい"]);
        assert_eq!(context("目眩精査").purposes, ["めまい"]);
        assert_eq!(context("外傷、頸椎MRI").purposes, ["外傷"]);
        assert!(context("肝嚢胞のみ").purposes.is_empty());
    }

    #[test]
    fn grade_words_map_to_levels_and_comparisons_are_excluded() {
        let v = vocabulary();
        assert_eq!(grades_in("中等度の変形性腰椎症", &v), ["中等度"]);
        assert_eq!(grades_in("軽度の脂肪肝と高度の腰椎症", &v), ["軽度", "高度"]);
        assert_eq!(grades_in("わずかな胸水", &v), ["軽度"]);
        assert_eq!(grades_in("著明な狭窄", &v), ["高度"]);
        assert!(grades_in("変形性腰椎症でL4/5が最も強い狭窄、L3/4も比較的強い", &v).is_empty(), "§14.5 の例は程度ではない");
        assert!(grades_in("多発結腸憩室", &v).is_empty(), "多発は段ではない");
        assert!(grades_in("肝嚢胞のみ", &v).is_empty());
    }

    /// QA の指摘(S3): 打ち消された語・検査名やモジュール名の中の語・別の書き方。
    #[test]
    fn negated_words_and_names_are_not_context_or_grades() {
        let v = vocabulary();
        assert!(context("若い女性、腹痛なし").purposes.is_empty(), "腹痛なし");
        assert!(context("腹痛はない").purposes.is_empty());
        assert_eq!(context("若い女性、腹痛精査で異常なし").purposes, ["腹痛"], "異常なしは腹痛を打ち消さない");
        assert_eq!(grades_in("中等度の変形性腰椎症で他に異常なし", &v), ["中等度"]);
        assert_eq!(grades_in("軽度ないし中等度の狭窄", &v), ["軽度", "中等度"], "ないしは打ち消しではない");
        assert_eq!(grades_in("軽度の狭窄しかない", &v), ["軽度"]);
        assert!(grades_in("変形性頸椎症、顕著な狭窄はない", &v).is_empty(), "顕著な狭窄はない");
        assert!(grades_in("顕著でない", &v).is_empty());
        assert_eq!(grades_in("顕著な狭窄あり", &v), ["高度"]);
        assert_eq!(context("若い女性、右下腹部痛").purposes, ["腹痛"], "下腹部痛も腹痛");
        assert_eq!(context("目まいの精査").purposes, ["めまい"]);
        assert_eq!(context("乳腺女性化、65歳男性").sex.as_deref(), Some("男"), "女性化は性別の語ではない");
        let mut dictionary = Dictionary::default();
        dictionary.templates.push(Template {
            keywords: vec!["頸椎MRI".to_string()],
            body: "配列は保たれています。".to_string(),
            modules: vec![crate::core::dictionary::Module {
                name: "直線化・軽度変性".to_string(),
                text: "直線化しています。".to_string(),
                ..Default::default()
            }],
            ..Template::default()
        });
        assert!(extract("頸椎MRI、直線化・軽度変性", &v, Some(&dictionary)).grades.is_empty(), "モジュール名の中の軽度");
    }

    #[test]
    fn the_label_reads_like_the_plan() {
        assert_eq!(context_label(&context("若い女性、腹痛精査")), "女性・若年・目的=腹痛");
        assert_eq!(context_label(&Context::default()), "");
    }
}
