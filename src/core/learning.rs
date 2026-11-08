//! 学習 = 訂正の記憶。
//!
//! 考え方: 音声認識エンジンは 100% にならない。AI 訂正を前提にするなら、認識エンジンに求めるのは
//! 「AI が本当の発言を推定できる程度の精度」でよい。そこで学習は、認識モデルを変えるのではなく、
//! 「認識したままの文 → 最終的に送った文」の差分を覚え、
//!   1. 同じ誤認識が次に来たとき、AI の答えを待たずに即座に置き換える(決定論・0 秒)
//!   2. AI 訂正の手がかり(「よくある誤認識の例」)として渡す
//!   3. 正しい語を認識エンジンの hotwords に足し、そもそもの誤認識を減らす
//!
//! の 3 つに使う。覚えるのは短い断片(両側 12 文字以内)だけで、文章そのものは残さない。

use serde::{Deserialize, Serialize};

/// 覚える断片の長さの上限(文字)。
pub const MAX_FRAGMENT: usize = 12;
/// これより短い断片は覚えない(1 文字の組はどこにでも当たる)。
pub const MIN_FRAGMENT: usize = 2;
/// 覚える組の上限。超えたら重みの小さいものから忘れる。
pub const MAX_PAIRS: usize = 2000;
/// 差分計算の上限(文字数の積)。
const MAX_DIFF_CELLS: usize = 4_000_000;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Pair {
    /// 認識したままの断片。
    pub from: String,
    /// 正しい断片。
    pub to: String,
    /// AI 訂正で観察した回数。
    pub count: u32,
    /// 手直しで観察した回数(重みは 3 倍)。
    pub manual: u32,
    /// 最後に観察した日(YYYY-MM-DD)。
    pub last: String,
}

impl Pair {
    pub fn weight(&self) -> u32 {
        self.count + 3 * self.manual
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CorrectionMemory {
    pub pairs: Vec<Pair>,
    /// これまでに話した秒数。
    pub voice_seconds: f64,
    /// 発話の数。
    pub utterances: u64,
    /// AI 訂正で何かが直った発話の数。
    pub corrected: u64,
}

fn today() -> String {
    chrono::Local::now().format("%Y-%m-%d").to_string()
}

/// 句読点・空白・数字以外の文字があるか。
fn has_letters(fragment: &str) -> bool {
    fragment.chars().any(|c| {
        !(c.is_whitespace() || c.is_ascii_digit() || "、。，．,.!！?？:：;；/／()（）".contains(c))
    })
}

/// 語として覚える価値があるか。1 文字の断片と、句読点・空白・数字だけの断片は覚えない(当てると壊す)。
fn learnable(fragment: &str) -> bool {
    let count = fragment.chars().count();
    (MIN_FRAGMENT..=MAX_FRAGMENT).contains(&count)
        && !fragment.contains('\n')
        && has_letters(fragment)
}

/// 最長共通部分列で 2 つの文字列を揃え、一致する位置の組を返す。
fn alignment(a: &[char], b: &[char]) -> Vec<(usize, usize)> {
    let (n, m) = (a.len(), b.len());
    if n == 0 || m == 0 || n * m > MAX_DIFF_CELLS {
        return Vec::new();
    }
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
    let mut matches = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < n && j < m {
        if a[i] == b[j] {
            matches.push((i, j));
            i += 1;
            j += 1;
        } else if table[i + 1][j] >= table[i][j + 1] {
            i += 1;
        } else {
            j += 1;
        }
    }
    matches
}

/// 「認識したままの文」と「最終的な文」の差分から、置き換えの組を取り出す。
/// 追加だけ・削除だけの差分(句読点の補い、不要語の除去)と、句読点や数字だけの違いは覚えない。
/// 間に一致が 1 文字しか無い近いずれは 1 つにまとめる(「背上波 → 肺上葉」のように飛び飛びの誤りを 1 語で覚える)。
/// 1 文字だけの置き換えは前後 1 文字を添えて、別の語の一部を誤って当てないようにする。
pub fn substitution_pairs(raw: &str, fixed: &str) -> Vec<(String, String)> {
    let a: Vec<char> = raw.chars().collect();
    let b: Vec<char> = fixed.chars().collect();
    if a == b {
        return Vec::new();
    }
    let matches = alignment(&a, &b);
    if matches.is_empty() {
        return Vec::new();
    }
    // 一致の間にある「ずれ」のブロック(a の範囲, b の範囲)。
    let mut blocks: Vec<(usize, usize, usize, usize)> = Vec::new();
    let mut next = (0usize, 0usize);
    for &(i, j) in matches.iter().chain(std::iter::once(&(a.len(), b.len()))) {
        if next.0 < i || next.1 < j {
            blocks.push((next.0, i, next.1, j));
        }
        next = (i + 1, j + 1);
    }
    // 近いブロックはまとめる。ブロックの間は一致した文字だけなので、a と b で同じ長さ。
    let mut merged: Vec<(usize, usize, usize, usize)> = Vec::new();
    for block in blocks {
        if let Some(last) = merged.last_mut() {
            if block.0 - last.1 <= 1 {
                last.1 = block.1;
                last.3 = block.3;
                continue;
            }
        }
        merged.push(block);
    }
    let mut pairs = Vec::new();
    for (a_start, a_end, b_start, b_end) in merged {
        if a_start >= a_end || b_start >= b_end {
            continue; // 追加だけ、削除だけ
        }
        let core_from: String = a[a_start..a_end].iter().collect();
        let core_to: String = b[b_start..b_end].iter().collect();
        if !(has_letters(&core_from) || has_letters(&core_to)) {
            continue; // 句読点や数字だけの違い
        }
        let (mut a_start, mut a_end, mut b_start, mut b_end) = (a_start, a_end, b_start, b_end);
        if a_end - a_start == 1 || b_end - b_start == 1 {
            // 1 文字だけの置き換えは前後 1 文字ずつを添える。文頭・文末で片側しか取れないときは、
            // その側を 2 文字取る(「配が→肺が」のままだと「分配が」にも当たる)。それも取れなければ覚えない。
            let left = a_start > 0 && b_start > 0;
            let right = a_end < a.len() && b_end < b.len();
            match (left, right) {
                (true, true) => {
                    a_start -= 1;
                    b_start -= 1;
                    a_end += 1;
                    b_end += 1;
                }
                (true, false) if a_start >= 2 && b_start >= 2 => {
                    a_start -= 2;
                    b_start -= 2;
                }
                (false, true) if a_end + 2 <= a.len() && b_end + 2 <= b.len() => {
                    a_end += 2;
                    b_end += 2;
                }
                _ => continue,
            }
        }
        let from: String = a[a_start..a_end].iter().collect();
        let to: String = b[b_start..b_end].iter().collect();
        if from != to && learnable(&from) && learnable(&to) {
            pairs.push((from, to));
        }
    }
    pairs
}

/// `apply_with_pairs` の結果。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Applied {
    pub text: String,
    /// 置き換えた範囲(結果の文の中の文字位置)。
    pub changed: Vec<(usize, usize)>,
    /// 当てた組(from, to)。同じ組は 1 回だけ。
    pub pairs: Vec<(String, String)>,
}

impl CorrectionMemory {
    /// 1 組をそのまま覚える(文脈は足さない)。覚えられない組なら false。
    pub fn learn_pair(&mut self, from: &str, to: &str, manual: bool) -> bool {
        let (from, to) = (from.trim(), to.trim());
        if from == to || !learnable(from) || !learnable(to) {
            return false;
        }
        let date = today();
        if let Some(pair) = self.pairs.iter_mut().find(|p| p.from == from && p.to == to) {
            if manual {
                pair.manual += 1;
            } else {
                pair.count += 1;
            }
            pair.last = date;
            return true;
        }
        // いっぱいなら軽くて古い組から追い出す(新しい組が入れないままにしない)。
        while self.pairs.len() >= MAX_PAIRS {
            let Some(index) = (0..self.pairs.len())
                .min_by_key(|&i| (self.pairs[i].weight(), self.pairs[i].last.clone()))
            else {
                break;
            };
            self.pairs.remove(index);
        }
        self.pairs.push(Pair {
            from: from.to_string(),
            to: to.to_string(),
            count: u32::from(!manual),
            manual: u32::from(manual),
            last: date,
        });
        true
    }

    /// 認識したままの文と最終的な文の差分を覚える。`keep` が false を返す組は覚えない。戻り値は覚えた組の数。
    pub fn learn_with(
        &mut self,
        raw: &str,
        fixed: &str,
        manual: bool,
        keep: &dyn Fn(&str, &str) -> bool,
    ) -> usize {
        let mut learned = 0;
        for (from, to) in substitution_pairs(raw, fixed) {
            if keep(&from, &to) && self.learn_pair(&from, &to, manual) {
                learned += 1;
            }
        }
        learned
    }

    /// 認識したままの文と最終的な文の差分を覚える。戻り値は覚えた組の数。
    pub fn learn(&mut self, raw: &str, fixed: &str, manual: bool) -> usize {
        self.learn_with(raw, fixed, manual, &|_, _| true)
    }

    /// 当てた置き換えが元に戻されたとき(AI か手直しが元の語に直した)、その組を弱める。
    /// 手直しで戻されたなら自動で数えた回数は消す。重みが 0 になった組は消す。組が無ければ false。
    pub fn unlearn(&mut self, from: &str, to: &str, manual: bool) -> bool {
        let Some(index) = self.pairs.iter().position(|p| p.from == from && p.to == to) else {
            return false;
        };
        let pair = &mut self.pairs[index];
        if manual {
            pair.count = 0;
            pair.manual = pair.manual.saturating_sub(1);
        } else {
            pair.count = pair.count.saturating_sub(1);
        }
        if pair.weight() == 0 {
            self.pairs.remove(index);
        }
        true
    }

    /// 重みが `min_weight` 以上の置き換えを文に当てる。戻り値は当てた後の文と、変わった範囲(文字位置)。
    /// 同じ断片に複数の答えがあれば重い方を使う。長い断片から先に当てる。
    pub fn apply(&self, text: &str, min_weight: u32) -> (String, Vec<(usize, usize)>) {
        let applied = self.apply_with_pairs(text, min_weight);
        (applied.text, applied.changed)
    }

    /// `apply` に加えて、当てた組そのものも返す(元に戻されたときに弱めるため)。
    pub fn apply_with_pairs(&self, text: &str, min_weight: u32) -> Applied {
        let mut rules: Vec<(&Pair, Vec<char>)> = Vec::new();
        for pair in self
            .pairs
            .iter()
            .filter(|p| p.weight() >= min_weight.max(1))
        {
            match rules.iter_mut().find(|(r, _)| r.from == pair.from) {
                Some(existing) => {
                    if pair.weight() > existing.0.weight() {
                        existing.0 = pair;
                    }
                }
                None => rules.push((pair, pair.from.chars().collect())),
            }
        }
        if rules.is_empty() || text.is_empty() {
            return Applied {
                text: text.to_string(),
                ..Applied::default()
            };
        }
        rules.sort_by_key(|rule| std::cmp::Reverse(rule.1.len()));
        let chars: Vec<char> = text.chars().collect();
        let mut out = String::with_capacity(text.len());
        let mut out_len = 0usize;
        let mut changed = Vec::new();
        let mut pairs: Vec<(String, String)> = Vec::new();
        let mut index = 0;
        while index < chars.len() {
            let hit = rules
                .iter()
                .find(|(_, from)| chars[index..].starts_with(from));
            match hit {
                Some((pair, from)) => {
                    let added = pair.to.chars().count();
                    out.push_str(&pair.to);
                    changed.push((out_len, out_len + added));
                    if !pairs.iter().any(|(f, _)| f == &pair.from) {
                        pairs.push((pair.from.clone(), pair.to.clone()));
                    }
                    out_len += added;
                    index += from.len();
                }
                None => {
                    out.push(chars[index]);
                    out_len += 1;
                    index += 1;
                }
            }
        }
        Applied {
            text: out,
            changed,
            pairs,
        }
    }

    /// AI 訂正に渡す「よくある誤認識の例」。重い順。
    pub fn examples(&self, limit: usize) -> Vec<(&str, &str)> {
        let mut pairs: Vec<&Pair> = self.pairs.iter().collect();
        pairs.sort_by(|x, y| y.weight().cmp(&x.weight()).then(x.from.cmp(&y.from)));
        pairs
            .into_iter()
            .take(limit)
            .map(|p| (p.from.as_str(), p.to.as_str()))
            .collect()
    }

    /// 認識エンジンの hotwords に足す正しい語。2 回以上観察したものだけ。
    pub fn hotwords(&self, limit: usize) -> Vec<String> {
        let mut pairs: Vec<&Pair> = self.pairs.iter().filter(|p| p.weight() >= 2).collect();
        pairs.sort_by_key(|pair| std::cmp::Reverse(pair.weight()));
        let mut words: Vec<String> = Vec::new();
        for pair in pairs {
            let word = pair.to.trim();
            if word.chars().count() >= 2 && !words.iter().any(|w| w == word) {
                words.push(word.to_string());
            }
            if words.len() >= limit {
                break;
            }
        }
        words
    }

    pub fn forget(&mut self, index: usize) {
        if index < self.pairs.len() {
            self.pairs.remove(index);
        }
    }

    pub fn clear(&mut self) {
        self.pairs.clear();
    }

    /// 画面に出す順(重い順)の添字。
    pub fn sorted_indices(&self) -> Vec<usize> {
        let mut indices: Vec<usize> = (0..self.pairs.len()).collect();
        indices.sort_by(|x, y| {
            self.pairs[*y]
                .weight()
                .cmp(&self.pairs[*x].weight())
                .then(self.pairs[*x].from.cmp(&self.pairs[*y].from))
        });
        indices
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn substitutions_are_extracted_with_context_for_single_characters() {
        assert_eq!(
            substitution_pairs("右配上葉に結節", "右肺上葉に結節"),
            vec![("右配上".to_string(), "右肺上".to_string())]
        );
        assert_eq!(
            substitution_pairs("5ミリの結節", "5mmの結節"),
            vec![("ミリ".to_string(), "mm".to_string())]
        );
        assert_eq!(
            substitution_pairs("右背上波に結節", "右肺上葉に結節"),
            vec![("背上波".to_string(), "肺上葉".to_string())]
        );
    }

    #[test]
    fn insertions_deletions_and_punctuation_are_not_learned() {
        assert!(substitution_pairs("結節を認めます", "結節を認めます。").is_empty());
        assert!(substitution_pairs("えー右肺", "右肺").is_empty());
        assert!(substitution_pairs("右肺、上葉", "右肺。上葉").is_empty());
        assert!(substitution_pairs("同じ", "同じ").is_empty());
        assert!(substitution_pairs("", "新規").is_empty());
    }

    #[test]
    fn long_fragments_are_skipped_for_privacy() {
        let raw = "あ".repeat(20) + "終";
        let fixed = "い".repeat(20) + "終";
        assert!(substitution_pairs(&raw, &fixed).is_empty());
    }

    #[test]
    fn memory_applies_learned_pairs_after_enough_observations() {
        let mut memory = CorrectionMemory::default();
        memory.learn("右配上葉", "右肺上葉", false);
        assert_eq!(memory.apply("右配上葉に", 2).0, "右配上葉に");
        memory.learn("右配上葉に腫瘤", "右肺上葉に腫瘤", false);
        let (text, changed) = memory.apply("左の右配上葉", 2);
        assert_eq!(text, "左の右肺上葉");
        assert_eq!(changed, vec![(2, 5)]);
        // 手直しは 1 回で重み 3。
        memory.learn("健側に腫瘤", "患側に腫瘤", true);
        assert_eq!(memory.apply("健側に腫瘤", 2).0, "患側に腫瘤");
        assert_eq!(
            memory.apply("健側で", 2).0,
            "健側で",
            "覚えた組は「健側に」なので当たらない"
        );
    }

    #[test]
    fn heavier_answer_wins_and_longer_fragments_apply_first() {
        let mut memory = CorrectionMemory::default();
        for _ in 0..3 {
            memory.learn("開業です", "改行です", false);
        }
        for _ in 0..5 {
            memory.learn("開業です", "階層です", false);
        }
        assert_eq!(memory.apply("開業です", 2).0, "階層です");
        assert_eq!(memory.examples(1)[0], ("開業", "階層"));
        assert_eq!(
            memory.hotwords(5),
            vec!["階層".to_string(), "改行".to_string()]
        );
    }

    #[test]
    fn memory_is_capped() {
        let mut memory = CorrectionMemory::default();
        for i in 0..(MAX_PAIRS + 50) {
            memory.pairs.push(Pair {
                from: format!("あ{i}"),
                to: format!("い{i}"),
                count: 1,
                ..Pair::default()
            });
        }
        memory.learn("右配上葉", "右肺上葉", false);
        assert_eq!(memory.pairs.len(), MAX_PAIRS);
        assert!(
            memory
                .pairs
                .iter()
                .any(|p| p.from == "右配上" && p.to == "右肺上"),
            "いっぱいでも新しい組は入る"
        );
    }

    #[test]
    fn manual_pairs_are_stored_verbatim() {
        let mut memory = CorrectionMemory::default();
        assert!(
            memory.learn_pair("背上波", "肺上葉", true),
            "共通の字が無くても覚える"
        );
        assert!(memory.learn_pair(" CT ", "シーティー", true));
        assert!(!memory.learn_pair("配", "肺", true), "1 文字は覚えない");
        assert!(!memory.learn_pair("肺", "肺", true));
        assert!(!memory.learn_pair("、", "。", true));
        assert_eq!(memory.pairs.len(), 2);
        assert_eq!(
            (memory.pairs[0].from.as_str(), memory.pairs[0].manual),
            ("背上波", 1)
        );
        assert_eq!(memory.pairs[1].from, "CT");
        assert!(memory.learn_pair("背上波", "肺上葉", false));
        assert_eq!((memory.pairs[0].count, memory.pairs[0].manual), (1, 1));
    }

    #[test]
    fn one_character_fragments_are_not_learned() {
        assert!(substitution_pairs("配", "肺").is_empty());
        // 文頭・文末の 1 文字置換は片側しか文脈が取れないので覚えない(「配が→肺が」は「分配が」にも当たる)。
        assert!(substitution_pairs("配が", "肺が").is_empty());
        assert!(substitution_pairs("分配", "分肺").is_empty());
        // 片側に 2 文字あれば、その 2 文字を添えて覚える。
        assert_eq!(
            substitution_pairs("健側に腫瘤", "患側に腫瘤"),
            vec![("健側に".to_string(), "患側に".to_string())]
        );
        assert_eq!(
            substitution_pairs("上葉の結節", "上葉の結接"),
            vec![("の結節".to_string(), "の結接".to_string())]
        );
        assert_eq!(
            substitution_pairs("分配が", "分肺が"),
            vec![("分配が".to_string(), "分肺が".to_string())]
        );
        let mut memory = CorrectionMemory::default();
        assert_eq!(memory.learn("配", "肺", true), 0);
        let (text, changed) = memory.apply("配置", 1);
        assert_eq!((text.as_str(), changed.len()), ("配置", 0));
    }

    #[test]
    fn reverted_replacements_lose_weight() {
        let mut memory = CorrectionMemory::default();
        memory.learn("右配上葉", "右肺上葉", false);
        memory.learn("右配上葉", "右肺上葉", false);
        let applied = memory.apply_with_pairs("右配上葉に腫瘤", 2);
        assert_eq!(applied.text, "右肺上葉に腫瘤");
        assert_eq!(
            applied.pairs,
            vec![("右配上".to_string(), "右肺上".to_string())]
        );
        // AI が戻したら 1 つ減る。
        assert!(memory.unlearn("右配上", "右肺上", false));
        assert_eq!(memory.pairs[0].count, 1);
        // 手直しで戻したら消える。
        assert!(memory.unlearn("右配上", "右肺上", true));
        assert!(memory.pairs.is_empty());
        assert!(!memory.unlearn("右配上", "右肺上", true));
    }

    #[test]
    fn learn_with_can_skip_pairs() {
        let mut memory = CorrectionMemory::default();
        let learned = memory.learn_with("右肺上葉", "左肺上葉", true, &|from, _| {
            !from.contains('右')
        });
        assert_eq!(learned, 0);
        assert!(memory.pairs.is_empty());
    }
}
