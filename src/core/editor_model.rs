//! エディターの中身(文字・カーソル・色分け範囲・発話の単位)。
//! 画面から切り離してあるので、音声コマンドの動きをそのまま試験できる。
//! 位置はすべて文字単位。

use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpanKind {
    /// 誤認識の可能性が高い語。
    Unsure,
    /// 訂正役(Claude)や学習した置き換えが直した語。
    Corrected,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    pub start: usize,
    pub end: usize,
    pub kind: SpanKind,
}

/// 発話の AI 訂正の状態。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CorrectionState {
    /// 確定した(依頼していないか、答えが戻った)。
    #[default]
    Settled,
    /// 仮表示。答えを待っている。
    Waiting,
    /// 訂正できないまま確定した(認識したままの文。上限を過ぎて遅れて戻れば、まだ当たる)。
    Failed,
}

/// 1 回の発話。Esc で消す単位で、再生にも使う。
#[derive(Debug, Clone)]
pub struct Utterance {
    /// 話した順の通し番号。訂正が戻ってきたときに探すのに使う。
    pub id: u64,
    pub start: usize,
    pub end: usize,
    pub audio: Option<Arc<Vec<f32>>>,
    /// 入れたときに学習で当てた置き換え(from, to)。手直しで元に戻されたら、その組の重みを下げる。
    pub applied: Vec<(String, String)>,
    /// 機械(AI 訂正後、または認識したまま)が最後に置いた文。これと手直し後の文の差が「手直し」。
    pub machine: String,
    /// テンプレートの文。手を入れても内容の変更なので、誤認識の手直しとして学習しない。
    pub fixed: bool,
    pub correction: CorrectionState,
    /// 文末の命令語を働かせた発話か。音にはその語が入っているので、外部 API の文の文末に語が残る。
    pub spoken_command: bool,
}

#[derive(Debug, Clone, Default)]
pub struct EditorModel {
    pub text: String,
    /// 選択範囲。start == end ならカーソル。
    pub selection: (usize, usize),
    pub spans: Vec<Span>,
    pub utterances: Vec<Utterance>,
    next_utterance_id: u64,
}

fn byte_index(text: &str, char_index: usize) -> usize {
    text.char_indices()
        .nth(char_index)
        .map_or(text.len(), |(i, _)| i)
}

impl EditorModel {
    pub fn new() -> Self {
        EditorModel::default()
    }

    pub fn len(&self) -> usize {
        self.text.chars().count()
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    pub fn cursor(&self) -> usize {
        self.selection.1
    }

    pub fn selected_text(&self) -> String {
        let (start, end) = self.ordered_selection();
        self.slice(start, end)
    }

    pub fn slice(&self, start: usize, end: usize) -> String {
        self.text
            .chars()
            .skip(start)
            .take(end.saturating_sub(start))
            .collect()
    }

    fn ordered_selection(&self) -> (usize, usize) {
        let (a, b) = self.selection;
        (a.min(b), a.max(b))
    }

    /// 文字の差し替えの中心。色分け範囲と発話の位置も一緒にずらす。
    fn replace(&mut self, start: usize, end: usize, insert: &str) {
        let from = byte_index(&self.text, start);
        let to = byte_index(&self.text, end);
        self.text.replace_range(from..to, insert);
        let removed = end - start;
        let added = insert.chars().count();
        // 範囲のすぐ後ろへの入力は、その範囲に含めない。始まりと終わりで扱いを分ける。
        let shift_start = |position: usize| {
            if position >= end {
                position - removed + added
            } else if position > start {
                start + added
            } else {
                position
            }
        };
        let shift_end = |position: usize| {
            if position <= start {
                position
            } else if position >= end {
                position - removed + added
            } else {
                start + added
            }
        };
        // あやしい語と訂正の印は手直しされたら消す。
        self.spans.retain(|s| s.end <= start || s.start >= end);
        for span in &mut self.spans {
            span.start = shift_start(span.start);
            span.end = shift_end(span.end);
        }
        self.spans.retain(|s| s.end > s.start);
        for utterance in &mut self.utterances {
            utterance.start = shift_start(utterance.start);
            utterance.end = shift_end(utterance.end);
        }
        self.utterances.retain(|u| u.end > u.start);
    }

    /// 画面で手入力された結果を取り込む。先頭と末尾の一致から差分を求める。
    pub fn sync_from_input(&mut self, new_text: &str, selection: (usize, usize)) {
        if new_text != self.text {
            let old: Vec<char> = self.text.chars().collect();
            let new: Vec<char> = new_text.chars().collect();
            let prefix = old.iter().zip(&new).take_while(|(a, b)| a == b).count();
            let limit = old.len().min(new.len()) - prefix;
            let suffix = old
                .iter()
                .rev()
                .zip(new.iter().rev())
                .take(limit)
                .take_while(|(a, b)| a == b)
                .count();
            let inserted: String = new[prefix..new.len() - suffix].iter().collect();
            self.replace(prefix, old.len() - suffix, &inserted);
        }
        self.selection = selection;
    }

    /// 音声の結果を入れる。選択中なら上書きする(しゃべって修正)。戻り値は発話の番号。
    pub fn insert_recognized(
        &mut self,
        text: &str,
        unsure: &[(usize, usize)],
        audio: Option<Arc<Vec<f32>>>,
    ) -> u64 {
        let (start, end) = self.ordered_selection();
        self.replace(start, end, text);
        let added = text.chars().count();
        for (from, to) in unsure {
            self.spans.push(Span {
                start: start + from,
                end: start + to,
                kind: SpanKind::Unsure,
            });
        }
        let id = self.next_utterance_id;
        self.next_utterance_id += 1;
        self.utterances.push(Utterance {
            id,
            start,
            end: start + added,
            audio,
            applied: Vec::new(),
            machine: text.to_string(),
            fixed: false,
            correction: CorrectionState::Settled,
            spoken_command: false,
        });
        let position = start + added;
        self.selection = (position, position);
        id
    }

    /// テンプレートの文を入れる。Esc で取り消せるよう発話として数えるが、学習の対象にはしない。
    /// 発話の途中へ入れると、その発話の範囲にテンプレートの文が含まれる。その差を手直しとして
    /// 覚えると「結節影あり → 異常なし」のような組ができるので、重なる発話も学習から外す。
    pub fn insert_template(&mut self, text: &str, audio: Option<Arc<Vec<f32>>>) {
        let id = self.insert_recognized(text, &[], audio);
        let Some((start, end)) = self
            .utterances
            .iter()
            .find(|u| u.id == id)
            .map(|u| (u.start, u.end))
        else {
            return;
        };
        self.exclude_from_learning(start, end);
    }

    /// 範囲に重なる発話を、手直しの学習の対象から外す。
    fn exclude_from_learning(&mut self, start: usize, end: usize) {
        for utterance in &mut self.utterances {
            if utterance.start < end && start < utterance.end {
                utterance.fixed = true;
            }
        }
    }

    /// 仮の文をテンプレートの文に置き換える(AI が直して初めてキーワードと分かったとき)。
    /// 話した順は変えず、`insert_template` と同じく学習の対象から外す。
    pub fn replace_with_template(&mut self, id: u64, body: &str) {
        let Some(start) = self.replace_utterance(id, body) else {
            return;
        };
        self.exclude_from_learning(start, start + body.chars().count());
    }

    /// 発話の AI 訂正の状態を変える。消えた発話なら何もしない。
    pub fn set_correction(&mut self, id: u64, state: CorrectionState) {
        if let Some(utterance) = self.utterances.iter_mut().find(|u| u.id == id) {
            utterance.correction = state;
        }
    }

    /// エディターの文の中に、AI 訂正を待っている発話があるか。
    pub fn awaiting_correction(&self) -> bool {
        self.utterances
            .iter()
            .any(|u| u.correction == CorrectionState::Waiting)
    }

    /// `end` より前に始まる発話の中に、AI 訂正を通っていないものがあるか。
    pub fn has_uncorrected(&self, end: usize) -> bool {
        self.utterances
            .iter()
            .any(|u| u.start < end && u.correction != CorrectionState::Settled)
    }

    /// 仮表示の発話の範囲(色分けに使う)。
    pub fn provisional_ranges(&self) -> Vec<(usize, usize)> {
        self.utterances
            .iter()
            .filter(|u| u.correction == CorrectionState::Waiting)
            .map(|u| (u.start, u.end))
            .collect()
    }

    /// 発話が機械の置いた文のまま残っていれば、その始まりの位置。手直し・削除・転送のあとは None。
    pub fn intact_start(&self, id: u64) -> Option<usize> {
        let utterance = self.utterances.iter().find(|u| u.id == id)?;
        self.is_intact(utterance).then_some(utterance.start)
    }

    fn is_intact(&self, utterance: &Utterance) -> bool {
        self.slice(utterance.start, utterance.end) == utterance.machine
    }

    /// 文末の命令語を働かせた発話として控える。
    pub fn set_spoken_command(&mut self, id: u64) {
        if let Some(utterance) = self.utterances.iter_mut().find(|u| u.id == id) {
            utterance.spoken_command = true;
        }
    }

    /// テンプレートの文を含む発話か。話した語の文ではない。
    pub fn is_fixed(&self, id: u64) -> bool {
        self.utterances.iter().any(|u| u.id == id && u.fixed)
    }

    /// 発話の文を置き換える。カーソルが発話の後ろにあればずらし、中にあれば発話の末尾に置く。
    /// 戻り値は発話の始まりの位置。空の文に置き換えると発話は消える。
    fn replace_utterance(&mut self, id: u64, text: &str) -> Option<usize> {
        let utterance = self.utterances.iter().find(|u| u.id == id)?;
        let (start, end) = (utterance.start, utterance.end);
        let (a, b) = self.selection;
        self.replace(start, end, text);
        let added = text.chars().count();
        if let Some(utterance) = self.utterances.iter_mut().find(|u| u.id == id) {
            utterance.machine = text.to_string();
        }
        let shift = |position: usize| {
            if position >= end {
                position - (end - start) + added
            } else if position > start {
                start + added
            } else {
                position
            }
        };
        self.selection = (shift(a), shift(b));
        Some(start)
    }

    /// 訂正役の答えで発話を置き換える。発話がその後に手直しされていたら何もしない。
    /// `changed` は訂正後の文の中で変わった範囲(文字位置)。
    pub fn apply_correction(
        &mut self,
        id: u64,
        original: &str,
        corrected: &str,
        changed: &[(usize, usize)],
    ) -> bool {
        let Some(utterance) = self.utterances.iter().find(|u| u.id == id) else {
            return false;
        };
        if self.slice(utterance.start, utterance.end) != original {
            return false;
        }
        let Some(start) = self.replace_utterance(id, corrected) else {
            return false;
        };
        let added = corrected.chars().count();
        for (from, to) in changed {
            let (from, to) = ((*from).min(added), (*to).min(added));
            if from < to {
                self.spans.push(Span {
                    start: start + from,
                    end: start + to,
                    kind: SpanKind::Corrected,
                });
            }
        }
        true
    }

    /// 音声コマンドや記号など、発話の単位にしない文字を入れる。
    pub fn insert_plain(&mut self, text: &str) {
        let (start, end) = self.ordered_selection();
        self.replace(start, end, text);
        let position = start + text.chars().count();
        self.selection = (position, position);
    }

    /// カーソルと関係のない位置へ、発話の単位にしない文字を入れる(訂正のあとで分かった改行など)。
    /// その位置より後ろのカーソルと発話は一緒にずれ、その位置で終わる発話は伸びない。
    pub fn insert_at(&mut self, position: usize, text: &str) {
        let (a, b) = self.selection;
        self.replace(position, position, text);
        let added = text.chars().count();
        let shift = |at: usize| if at >= position { at + added } else { at };
        self.selection = (shift(a), shift(b));
    }

    /// 先頭の n 文字を消す。カーソルは同じ文字の前に残す(転送した分を消しても、続きの入力位置を変えない)。
    pub fn remove_head(&mut self, count: usize) {
        let (a, b) = self.selection;
        self.replace(0, count, "");
        self.selection = (a.saturating_sub(count), b.saturating_sub(count));
    }

    /// 時間的に最後の発話を消す(文中の位置ではなく話した順)。
    pub fn delete_last_utterance(&mut self) -> bool {
        let Some(last) = self.utterances.last().cloned() else {
            return false;
        };
        self.replace(last.start, last.end, "");
        self.selection = (last.start, last.start);
        true
    }

    pub fn delete_all(&mut self) {
        self.text.clear();
        self.spans.clear();
        self.utterances.clear();
        self.selection = (0, 0);
    }

    pub fn move_start(&mut self) {
        self.selection = (0, 0);
    }

    pub fn move_end(&mut self) {
        let end = self.len();
        self.selection = (end, end);
    }

    pub fn utterance_at(&self, position: usize) -> Option<&Utterance> {
        self.utterances
            .iter()
            .find(|u| u.start <= position && position <= u.end)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(text: &str) -> EditorModel {
        let mut m = EditorModel::new();
        m.insert_recognized(text, &[], None);
        m
    }

    #[test]
    fn recognized_text_is_inserted_at_the_cursor() {
        let mut m = model("結節");
        m.insert_recognized("あり", &[], None);
        assert_eq!(m.text, "結節あり");
        assert_eq!(m.cursor(), 4);
    }

    #[test]
    fn templates_can_be_taken_back_with_esc() {
        let mut m = model("所見。");
        m.insert_template("異常なし。\n以上。", None);
        assert_eq!(m.text, "所見。異常なし。\n以上。");
        assert!(m.utterances[1].fixed);
        assert!(!m.utterances[0].fixed);
        assert!(m.delete_last_utterance());
        assert_eq!(m.text, "所見。");
    }

    #[test]
    fn a_template_inside_an_utterance_keeps_both_out_of_learning() {
        let mut m = model("右肺に結節影あり");
        let next = m.insert_recognized("次の文。", &[], None);
        m.selection = (3, 8);
        m.insert_template("異常なし。", None);
        assert_eq!(m.text, "右肺に異常なし。次の文。");
        let fixed = |id: u64| m.utterances.iter().find(|u| u.id == id).unwrap().fixed;
        assert!(fixed(0), "テンプレートを含むことになった発話");
        assert!(!fixed(next), "隣の発話は学習の対象のまま");
    }

    #[test]
    fn speech_overwrites_the_selection() {
        let mut m = model("右肺の結節");
        m.selection = (0, 2);
        m.insert_recognized("左肺", &[], None);
        assert_eq!(m.text, "左肺の結節");
    }

    #[test]
    fn esc_removes_only_the_last_utterance() {
        let mut m = model("一文目。");
        m.insert_recognized("二文目。", &[], None);
        assert!(m.delete_last_utterance());
        assert_eq!(m.text, "一文目。");
    }

    #[test]
    fn esc_removes_the_most_recent_utterance_wherever_it_is() {
        let mut m = model("古い文");
        m.selection = (0, 0);
        m.insert_recognized("新しい文", &[], None);
        assert!(m.delete_last_utterance());
        assert_eq!(m.text, "古い文");
    }

    #[test]
    fn text_added_after_an_unsure_word_is_not_marked() {
        let mut m = EditorModel::new();
        m.insert_recognized("肺結節", &[(1, 3)], None);
        m.insert_plain("あり");
        assert_eq!(
            m.spans[0],
            Span {
                start: 1,
                end: 3,
                kind: SpanKind::Unsure
            }
        );
    }

    #[test]
    fn unsure_spans_follow_edits_before_them() {
        let mut m = EditorModel::new();
        m.insert_recognized("肺結節", &[(1, 3)], None);
        m.selection = (0, 0);
        m.insert_plain("右");
        assert_eq!(
            m.spans[0],
            Span {
                start: 2,
                end: 4,
                kind: SpanKind::Unsure
            }
        );
    }

    #[test]
    fn replacing_an_unsure_word_clears_the_mark() {
        let mut m = EditorModel::new();
        m.insert_recognized("開業", &[(0, 2)], None);
        m.selection = (0, 2);
        m.insert_recognized("改行", &[], None);
        assert!(m.spans.is_empty());
    }

    #[test]
    fn correction_state_follows_the_text_in_the_editor() {
        let mut m = EditorModel::new();
        let id = m.insert_recognized("文", &[], None);
        assert!(!m.has_uncorrected(m.len()));
        m.set_correction(id, CorrectionState::Waiting);
        assert!(m.awaiting_correction());
        assert_eq!(m.provisional_ranges(), vec![(0, 1)]);
        m.set_correction(id, CorrectionState::Failed);
        assert!(!m.awaiting_correction());
        assert!(m.provisional_ranges().is_empty(), "上限で確定した文は仮表示でない");
        assert!(m.has_uncorrected(m.len()));
        assert!(!m.has_uncorrected(0), "送る範囲の外の発話は数えない");
        m.set_correction(id, CorrectionState::Waiting);
        assert!(m.delete_last_utterance());
        assert!(!m.awaiting_correction(), "消した発話は待たない");
    }

    #[test]
    fn a_cursor_inside_a_corrected_utterance_goes_to_its_new_end() {
        let mut m = model("所見。");
        let id = m.insert_recognized("左肺", &[], None);
        m.selection = (4, 4);
        assert!(m.apply_correction(id, "左肺", "左肺上葉の結節", &[(2, 7)]));
        assert_eq!(m.selection, (10, 10));
    }

    #[test]
    fn text_after_an_utterance_leaves_it_and_moves_what_follows() {
        let mut m = model("一文目");
        let next = m.insert_recognized("二文目", &[], None);
        m.insert_at(3, "\n");
        assert_eq!(m.text, "一文目\n二文目");
        assert_eq!(m.utterances[0].end, 3, "前の発話は伸びない");
        assert_eq!(m.utterances[1].id, next);
        assert_eq!(m.utterances[1].start, 4);
        assert_eq!(m.cursor(), 7);
        m.selection = (1, 1);
        m.insert_at(3, "\t");
        assert_eq!(m.selection, (1, 1), "手前のカーソルは動かない");
    }

    #[test]
    fn a_line_end_after_the_last_utterance_moves_the_cursor_past_it() {
        let mut m = model("一文目");
        m.insert_at(3, "\n");
        assert_eq!(m.cursor(), 4);
        m.insert_recognized("二文目", &[], None);
        assert_eq!(m.text, "一文目\n二文目");
    }

    #[test]
    fn a_late_template_keeps_the_spoken_order() {
        let mut m = model("所見。");
        let keyword = m.insert_recognized("胸部正常", &[(0, 2)], None);
        let next = m.insert_recognized("続き", &[], None);
        m.replace_with_template(keyword, "異常なし。\n以上。");
        assert_eq!(m.text, "所見。異常なし。\n以上。続き");
        assert!(m.spans.is_empty(), "あやしい印は消える");
        assert!(m.utterances[1].fixed);
        assert!(!m.utterances[0].fixed);
        assert!(!m.utterances[2].fixed);
        assert_eq!(m.utterances[2].id, next);
        assert_eq!(m.cursor(), m.len(), "後ろのカーソルはずれる");
        assert!(m.delete_last_utterance());
        assert_eq!(m.text, "所見。異常なし。\n以上。", "Esc は話した順に消す");
        assert!(m.delete_last_utterance());
        assert_eq!(m.text, "所見。");
    }

    #[test]
    fn an_edited_or_gone_utterance_is_no_longer_intact() {
        let mut m = model("右肺");
        assert_eq!(m.intact_start(0), Some(0));
        m.sync_from_input("右肺に", (3, 3));
        assert_eq!(m.intact_start(0), Some(0), "発話の後ろへの入力は手直しではない");
        m.sync_from_input("右の肺に", (4, 4));
        assert_eq!(m.intact_start(0), None);
        m.delete_all();
        assert_eq!(m.intact_start(0), None);
    }

    #[test]
    fn removing_the_head_keeps_the_cursor_on_the_same_text() {
        let mut m = model("送った文。");
        m.insert_recognized("続き", &[], None);
        m.remove_head(5);
        assert_eq!(m.text, "続き");
        assert_eq!(m.cursor(), 2);
        m.insert_recognized("の文", &[], None);
        assert_eq!(m.text, "続きの文");
    }

    #[test]
    fn typing_keeps_positions_in_step() {
        let mut m = EditorModel::new();
        m.insert_recognized("肺結節", &[(1, 3)], None);
        m.sync_from_input("右肺結節", (1, 1));
        assert_eq!(m.spans[0].start, 2);
        assert_eq!(m.utterances[0].end, 4);
    }

    #[test]
    fn correction_replaces_the_utterance_and_marks_changes() {
        let mut m = EditorModel::new();
        let id = m.insert_recognized("右肺上波に結節", &[(3, 4)], None);
        m.insert_plain("。");
        assert!(m.apply_correction(
            id,
            "右肺上波に結節",
            "右肺上葉に結節あり",
            &[(3, 4), (7, 9)]
        ));
        assert_eq!(m.text, "右肺上葉に結節あり。");
        assert_eq!(m.cursor(), 10, "後ろにあったカーソルはずれる");
        let kinds: Vec<SpanKind> = m.spans.iter().map(|s| s.kind).collect();
        assert_eq!(
            kinds,
            vec![SpanKind::Corrected, SpanKind::Corrected],
            "あやしい印は消え、訂正の印が付く"
        );
        assert_eq!((m.spans[0].start, m.spans[0].end), (3, 4));
        assert_eq!(m.utterances[0].end, 9);
    }

    #[test]
    fn correction_is_dropped_when_the_utterance_was_edited() {
        let mut m = EditorModel::new();
        let id = m.insert_recognized("右肺上波", &[], None);
        m.sync_from_input("右肺上葉", (4, 4));
        assert!(!m.apply_correction(id, "右肺上波", "右肺上葉", &[(3, 4)]));
        assert!(m.delete_last_utterance());
        assert!(
            !m.apply_correction(id, "", "x", &[]),
            "消えた発話には当てない"
        );
    }

    #[test]
    fn correction_keeps_a_cursor_placed_before_the_utterance() {
        let mut m = EditorModel::new();
        m.insert_recognized("所見。", &[], None);
        let id = m.insert_recognized("右肺", &[], None);
        m.selection = (1, 1);
        assert!(m.apply_correction(id, "右肺", "右肺上葉", &[(2, 4)]));
        assert_eq!(m.selection, (1, 1));
        m.selection = (4, 4);
        let id = m.insert_recognized("左肺", &[], None);
        m.selection = (5, 5);
        assert!(m.apply_correction(id, "左肺", "左", &[]));
        assert_eq!(m.selection, (5, 5), "発話の中にあったカーソルは末尾へ");
    }
}
