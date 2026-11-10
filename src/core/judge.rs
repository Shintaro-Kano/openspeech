//! 文と文の比べ方。訂正役・学習・辞書の画面が、AI の答えを使ってよいかを決めるのに使う
//! (左右・否定・対語が変わっていないか、説明文でないか、どこが変わったか)。

/// 差分計算の上限(文字数の積)。超えたら全体を変更扱いにする。
const MAX_DIFF_CELLS: usize = 4_000_000;

/// 左右を表す語。英字は語の途中を拾わないよう、前後が英字でないときだけ数える。
/// RT は放射線治療や RT-PCR と紛れるので入れない。
const LATERALITY_FORMS: [(&str, char); 18] = [
    ("右", '右'),
    ("みぎ", '右'),
    ("ミギ", '右'),
    ("ﾐｷﾞ", '右'),
    ("Rt", '右'),
    ("Ｒｔ", '右'),
    ("right", '右'),
    ("Right", '右'),
    ("RIGHT", '右'),
    ("左", '左'),
    ("ひだり", '左'),
    ("ヒダリ", '左'),
    ("ﾋﾀﾞﾘ", '左'),
    ("Lt", '左'),
    ("Ｌｔ", '左'),
    ("left", '左'),
    ("Left", '左'),
    ("LEFT", '左'),
];

/// 左右の語を、出てきた順に並べる。
fn laterality_signature(text: &str) -> Vec<char> {
    side_positions(text).into_iter().map(|(_, side)| side).collect()
}

/// 左右の語と、その語の直後の位置(バイト)。出てきた順。数え方は `laterality_signature` と同じ。
pub fn side_positions(text: &str) -> Vec<(usize, char)> {
    let is_letter = |c: Option<char>| c.is_some_and(|c| c.is_ascii_alphabetic());
    let mut found_sides = Vec::new();
    let mut previous = None;
    let mut at = 0;
    while let Some(c) = text[at..].chars().next() {
        let rest = &text[at..];
        let found = LATERALITY_FORMS.iter().find(|(form, _)| {
            rest.starts_with(form)
                && (!form.is_ascii()
                    || !(is_letter(previous) || is_letter(rest[form.len()..].chars().next())))
        });
        match found {
            Some((form, side)) => {
                at += form.len();
                found_sides.push((at, *side));
                previous = form.chars().last();
            }
            None => {
                at += c.len_utf8();
                previous = Some(c);
            }
        }
    }
    found_sides
}

/// 直す前と後で左右の語が変わるか。左右の取り違えは見落とすと重大なので、機械には変えさせず、
/// 学習もしない(「右配→左肺」を覚えると次から左右が変わる)。「右配→右肺」のように左右が同じなら変わらない。
pub fn changes_laterality(before: &str, after: &str) -> bool {
    laterality_signature(before) != laterality_signature(after)
}

/// 直したあとで右か左の語が増えるか(右系・左系それぞれの出現数を比べる)。手直しの門に使う: 右→左の反転は
/// 左が増えるので捕まえ、重複の削除や文の並べ替え(並びは変わるが数は増えない)は通す。訂正役は並びまで見る
/// `changes_laterality` を使う(1 文の誤認識の直しで左右の並びが変わる理由は無い)。
pub fn adds_laterality(before: &str, after: &str) -> bool {
    let count = |text: &str, side: char| laterality_signature(text).iter().filter(|c| **c == side).count();
    ['右', '左'].iter().any(|side| count(after, *side) > count(before, *side))
}

/// 右系・左系の語の数(右, 左)。数え方は `adds_laterality` と同じ。補完の選択の左右の門(§16.6 の L5)に使う。
pub fn side_counts(text: &str) -> (usize, usize) {
    let signature = laterality_signature(text);
    let count = |side: char| signature.iter().filter(|c| **c == side).count();
    (count('右'), count('左'))
}

/// 否定を表す語。あるかないかが変わる直しは、誤認識ではなく内容の変更とみなす。
const NEGATIONS: [&str; 5] = ["なし", "無し", "ない", "無い", "ません"];
/// 入れ替わると所見の向きが変わる字の組(上葉と下葉、高信号と低信号、陽性と陰性など)。
const OPPOSITES: [(char, char); 12] = [
    ('上', '下'),
    ('前', '後'),
    ('内', '外'),
    ('高', '低'),
    ('増', '減'),
    ('大', '小'),
    ('良', '悪'),
    ('陽', '陰'),
    ('有', '無'),
    ('近', '遠'),
    ('腹', '背'),
    ('頭', '尾'),
];

/// 所見の意味を変える直しか。左右、否定の有無、向きの逆な語の入れ替えを見る。
/// 手直しは誤認識の修正か内容の変更かを区別できず、覚えると次から自動で当たるので、
/// こうした組は覚えない(「あり→なし」を覚えると、次から「嚢胞あり」が「嚢胞なし」になる)。
pub fn changes_meaning(before: &str, after: &str) -> bool {
    let negated = |text: &str| NEGATIONS.iter().any(|word| text.contains(word));
    let swapped = |(a, b): &(char, char)| {
        let sides = |text: &str| -> Vec<char> {
            text.chars().filter(|c| c == a || c == b).collect()
        };
        let (from, to) = (sides(before), sides(after));
        !from.is_empty() && !to.is_empty() && from != to
    };
    changes_laterality(before, after)
        || negated(before) != negated(after)
        || OPPOSITES.iter().any(swapped)
}

/// 答えの半分以上が元の文の字か。そうなら直した文とみなす
/// (「秀成大血管転位症 → 修正大血管転位症」のような語を説明と取り違えないように)。
fn mostly_original(original: &str, corrected: &str) -> bool {
    let length = corrected.chars().count();
    let changed: usize = changed_ranges(original, corrected)
        .iter()
        .map(|(start, end)| end - start)
        .sum();
    (length - changed.min(length)) * 2 >= length
}

/// 答えが文ではなく、作業についての説明か(「訂正」「原文」などの語や、英語の答え)。
pub fn explains_the_task(original: &str, corrected: &str) -> bool {
    const MARKS: [&str; 8] = [
        "認識文",
        "訂正",
        "修正",
        "校正",
        "原文",
        "元の文",
        "申し訳",
        "すみません",
    ];
    let length = corrected.chars().count();
    let latin = corrected.chars().filter(char::is_ascii_alphabetic).count();
    !mostly_original(original, corrected)
        && (MARKS.iter().any(|mark| corrected.contains(mark) && !original.contains(mark))
            || (latin * 2 > length && !original.is_ascii()))
}

/// 答えが直した文ではなく、説明や断りになっていないか。
fn looks_like_commentary(original: &str, corrected: &str) -> bool {
    const REPLY_ENDINGS: [&str; 5] = ["ません", "ました", "ますか", "ください", "すぎます"];
    let text = corrected.trim().trim_end_matches(['。', '.', '!', '！', '?', '？']);
    explains_the_task(original, corrected)
        || (!mostly_original(original, corrected)
            && REPLY_ENDINGS.iter().any(|ending| text.ends_with(ending)))
}

/// 答えの長さが文として見られるか。空、極端に長い、極端に短いものは使わない。
pub fn within_length(original: &str, corrected: &str) -> bool {
    let before = original.chars().count();
    let after = corrected.trim().chars().count();
    after > 0 && after <= before * 2 + 20 && after >= (before / 4).max(1)
}

/// 訂正役の答えを使ってよいか。空、極端に長い、極端に短いもの、左右の語を変えたもの、説明は使わない。
pub fn accept_correction(original: &str, corrected: &str) -> bool {
    within_length(original, corrected)
        && !changes_laterality(original, corrected)
        && !looks_like_commentary(original, corrected)
}

/// 訂正後の文のうち、元の文に無かった文字の範囲(文字位置)。印をつけるのに使う。
pub fn changed_ranges(original: &str, corrected: &str) -> Vec<(usize, usize)> {
    let a: Vec<char> = original.chars().collect();
    let b: Vec<char> = corrected.chars().collect();
    match common_subsequence(&a, &b) {
        Some((_, kept)) => runs_of(&kept.iter().map(|keep| !keep).collect::<Vec<_>>()),
        None if b.is_empty() => Vec::new(),
        None => vec![(0, b.len())],
    }
}

/// 最長共通部分列に入る字の印(元の文の分、訂正後の文の分)。どちらかが空か大きすぎる文は None。
fn common_subsequence(a: &[char], b: &[char]) -> Option<(Vec<bool>, Vec<bool>)> {
    let (n, m) = (a.len(), b.len());
    if n == 0 || m == 0 || n * m > MAX_DIFF_CELLS {
        return None;
    }
    // table[i][j] = a[i..] と b[j..] の最長共通部分列の長さ
    let mut table = vec![vec![0u32; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            table[i][j] = if a[i] == b[j] {
                table[i + 1][j + 1] + 1
            } else {
                table[i + 1][j].max(table[i][j + 1])
            };
        }
    }
    let mut kept_a = vec![false; n];
    let mut kept_b = vec![false; m];
    let (mut i, mut j) = (0, 0);
    while i < n && j < m {
        if a[i] == b[j] {
            kept_a[i] = true;
            kept_b[j] = true;
            i += 1;
            j += 1;
        } else if table[i + 1][j] >= table[i][j + 1] {
            i += 1;
        } else {
            j += 1;
        }
    }
    Some((kept_a, kept_b))
}

/// 印の付いた字の続きを範囲にする。
fn runs_of(marked: &[bool]) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    let mut start = None;
    for (index, mark) in marked.iter().enumerate() {
        match (mark, start) {
            (true, None) => start = Some(index),
            (false, Some(from)) => {
                ranges.push((from, index));
                start = None;
            }
            _ => {}
        }
    }
    if let Some(from) = start {
        ranges.push((from, marked.len()));
    }
    ranges
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 右系・左系の語が増えたときだけ真。反転は捕まえ、重複の削除と並べ替えは通す(並びを見る `changes_laterality` は捕まえる)。
    #[test]
    fn added_side_words_are_detected_but_removals_and_reorders_are_not() {
        assert!(adds_laterality("右肺に結節。", "左肺に結節。"), "反転は左が増える");
        assert!(adds_laterality("肺に結節。", "右肺に結節。"), "無かった左右を足す");
        assert!(adds_laterality("みぎ肺", "右肺と左肺"));
        let twice = "右肺に結節。右肺に結節。";
        assert!(!adds_laterality(twice, "右肺に結節。"));
        assert!(changes_laterality(twice, "右肺に結節。"), "訂正役の判定は並びの変化で断る");
        assert!(!adds_laterality("右肺に結節。左腎に嚢胞。", "左腎に嚢胞。右肺に結節。"));
        assert!(!adds_laterality("Rt lung", "Right lung"), "同じ右の書き換え");
        assert!(!adds_laterality("右肺", "右肺"));
    }

    #[test]
    fn meaning_changes_are_detected() {
        for (before, after) in [
            ("あり", "なし"),
            ("めます", "めません"),
            ("増大", "縮小"),
            ("は陽性", "は陰性"),
            ("悪性の", "良性の"),
            ("肺上葉", "肺下葉"),
            ("高信", "低信"),
            ("結節影あり", "異常なし。"),
            ("右配", "左肺"),
        ] {
            assert!(changes_meaning(before, after), "{before} → {after}");
        }
        for (before, after) in [
            ("右配上", "右肺上"),
            ("上波", "上葉"),
            ("背上波", "肺上葉"),
            ("開業", "改行"),
            ("腹水", "腹水"),
            ("認めません", "認めません"),
        ] {
            assert!(!changes_meaning(before, after), "{before} → {after}");
        }
    }

    #[test]
    fn changed_ranges_mark_substitutions_and_insertions() {
        assert_eq!(changed_ranges("右肺上波に", "右肺上葉に"), vec![(3, 4)]);
        assert_eq!(
            changed_ranges("結節を認めます", "結節を認めます。"),
            vec![(7, 8)]
        );
        assert_eq!(changed_ranges("5ミリの結節", "5mmの結節"), vec![(1, 3)]);
        assert_eq!(changed_ranges("同じ", "同じ"), Vec::<(usize, usize)>::new());
        assert_eq!(
            changed_ranges("えー右肺", "右肺"),
            Vec::<(usize, usize)>::new()
        );
        assert_eq!(changed_ranges("", "新規"), vec![(0, 2)]);
    }

    #[test]
    fn laterality_changes_are_detected() {
        assert!(changes_laterality("右肺", "左肺"));
        assert!(changes_laterality("右肺上葉", "左肺下葉"));
        // 入れ替えと誤認識が同じ塊になっていても見つける。
        assert!(changes_laterality("右配", "左肺"));
        assert!(changes_laterality("両側の胸水", "右の胸水"));
        assert!(changes_laterality("右の結節、左の結節", "左の結節、右の結節"));
        assert!(!changes_laterality("右配上", "右肺上"));
        assert!(!changes_laterality("右肺", "右肺"));
        assert!(!changes_laterality("背上波", "肺上葉"));
        // かなで書き起こされた左右を漢字に直すのは入れ替えではない。
        assert!(!changes_laterality("みぎ肺に結節", "右肺に結節"));
        assert!(!changes_laterality("ヒダリ腎", "左腎"));
        assert!(!changes_laterality("ﾐｷﾞ肺", "右肺"));
    }

    #[test]
    fn latin_laterality_counts_only_as_a_whole_word() {
        assert!(changes_laterality("Rt. lung", "Lt. lung"));
        assert!(changes_laterality("right lung", "left lung"));
        assert!(!changes_laterality("Rt.肺", "右肺"));
        assert!(!changes_laterality("えーえるてぃーの上昇", "ALT の上昇"));
        assert!(!changes_laterality("アールティー後", "RT後"));
        assert!(!changes_laterality("アールティーピーシーアール", "RT-PCR"));
        assert!(changes_laterality("Ｒｔ肺", "Ｌｔ肺"));
        assert!(!changes_laterality("HEART", "PORT"));
        assert!(!changes_laterality("copyright", "bright"));
    }

}
