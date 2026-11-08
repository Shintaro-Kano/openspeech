//! 音声コマンド。操作の名前(「改行」「転送」)か登録した言葉が発話全体、または発話の末尾に現れたら操作に置き換える。
//!
//! 操作の名前は漢字でもかなでも常に働く(認識エンジンは「改行」と書き、AI は読みのまま残すことがある)。
//! 利用者は別の言い方を足せる(既定は空)。比べ方はテンプレートのキーワードと同じ `template_key` で、
//! 句読点・空白・全角半角・カタカナ/ひらがな・漢数字の差を吸収する(漢字とかなは別の語)。
//! 同じ言葉を続けて言うと(「改行改行改行」)回数を数える。
//! 記号の読み(「まる」→「。」)は辞書ファイルの `symbols` にあり、発話全体が読みと一致したときだけ記号にする。

use super::dictionary::{strip_key_suffix, template_key, Symbol};
use serde::{Deserialize, Serialize};

/// 音声でできる操作。保存データに名前で残るので、値を消すときは読み込みの互換を確かめる
/// (読めない値が 1 つあると設定全体が既定値に戻る。`settings.rs` は知らない操作の行を読み飛ばす)。
/// 2026-11-07 にタブ・最初に戻る・最後に進むを外した。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CommandAction {
    NewLine,
    Transfer,
}

impl CommandAction {
    pub const ALL: [CommandAction; 2] = [Self::NewLine, Self::Transfer];

    pub fn label(self) -> &'static str {
        match self {
            Self::NewLine => "改行",
            Self::Transfer => "転送",
        }
    }

    /// 名前の読み(ひらがな)。認識エンジンの keywords に「表記=読み」で渡す。
    pub fn reading(self) -> &'static str {
        match self {
            Self::NewLine => "かいぎょう",
            Self::Transfer => "てんそう",
        }
    }

    /// 登録しなくても働く言い方: 名前とその読み。
    pub fn names(self) -> [&'static str; 2] {
        [self.label(), self.reading()]
    }
}

/// 操作ごとの、働く言葉すべて(名前・読み・登録した言葉)。
pub fn all_words(commands: &[CommandWords]) -> impl Iterator<Item = (String, CommandAction)> + '_ {
    CommandAction::ALL.iter().flat_map(move |action| {
        let registered = commands
            .iter()
            .filter(move |c| c.action == *action)
            .flat_map(|c| c.words.iter().map(String::as_str));
        action
            .names()
            .into_iter()
            .chain(registered)
            .map(move |word| (word.to_string(), *action))
    })
}

/// 1 つの操作に足した言い方。空でも名前(「改行」「転送」)は働く。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandWords {
    pub action: CommandAction,
    pub words: Vec<String>,
}

/// 既定は全操作とも言葉なし。
pub fn default_command_words() -> Vec<CommandWords> {
    CommandAction::ALL
        .iter()
        .map(|action| CommandWords {
            action: *action,
            words: Vec::new(),
        })
        .collect()
}

/// 操作ごとに 1 行ずつ、決まった順に揃える(手で編集された保存データでも設定画面に全操作の行を出す)。
pub fn normalized(commands: &[CommandWords]) -> Vec<CommandWords> {
    CommandAction::ALL
        .iter()
        .map(|action| CommandWords {
            action: *action,
            words: commands
                .iter()
                .filter(|c| c.action == *action)
                .flat_map(|c| c.words.iter().cloned())
                .collect(),
        })
        .collect()
}

/// 保存データの `command_words` を読む。知らない操作(外したタブ・移動など)の行は読み飛ばし、
/// 残りの設定を失わない。
pub fn deserialize_known_command_words<'de, D>(deserializer: D) -> Result<Vec<CommandWords>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let rows: Vec<serde_json::Value> = Vec::deserialize(deserializer)?;
    Ok(rows
        .into_iter()
        .filter_map(|row| serde_json::from_value(row).ok())
        .collect())
}

/// 発話を調べた結果。
#[derive(Debug, Clone, PartialEq)]
pub enum Match {
    /// 普通の文章。
    Text,
    /// 文章の後ろにコマンドが続いた、または発話全体がコマンド。`count` は続けて言った回数(1 以上)。
    Command {
        prefix: String,
        action: CommandAction,
        count: usize,
    },
    /// 記号の読み上げ。入れる文字。
    Symbol(String),
}

fn strip_punctuation(text: &str) -> &str {
    text.trim_matches(|c: char| c.is_whitespace() || "、。，．,.!！?？".contains(c))
}

/// 発話を判定する。コマンドの言葉(発話全体)→ 記号の読み → 文末の言葉の順。
/// コマンドの言葉を記号より先に見るのは、利用者が決めた語が働かないと分かりにくいため。
pub fn match_utterance(text: &str, commands: &[CommandWords], symbols: &[Symbol]) -> Match {
    let utterance = strip_punctuation(text);
    if utterance.is_empty() {
        return Match::Text;
    }
    let key = template_key(utterance);
    if let Some(action) = whole_command(&key, commands) {
        return Match::Command {
            prefix: String::new(),
            action,
            count: 1,
        };
    }
    if let Some(symbol) = find_symbol(utterance, &key, symbols) {
        return Match::Symbol(symbol.to_string());
    }
    match_trailing(utterance, commands).unwrap_or(Match::Text)
}

/// AI が直した文を判定する。文末に続けた言葉と、それだけの発話を見る。
/// 記号の読みは話した時点の判定だけ(遅れて入ると手直しと衝突する)。
pub fn match_after_correction(text: &str, commands: &[CommandWords]) -> Match {
    let utterance = strip_punctuation(text);
    if utterance.is_empty() {
        return Match::Text;
    }
    match whole_command(&template_key(utterance), commands) {
        Some(action) => Match::Command {
            prefix: String::new(),
            action,
            count: 1,
        },
        None => match_trailing(utterance, commands).unwrap_or(Match::Text),
    }
}

/// 発話全体がコマンドの言葉なら、その操作。`key` はならした発話。
fn whole_command(key: &str, commands: &[CommandWords]) -> Option<CommandAction> {
    if key.is_empty() {
        return None;
    }
    all_words(commands)
        .find(|(word, _)| template_key(word) == key)
        .map(|(_, action)| action)
}

/// 発話全体が記号の読みなら、その記号。読みはカタカナでも同じ(Haas は「てん」を「テン。」と書くことがある)。
fn find_symbol<'a>(utterance: &str, key: &str, symbols: &'a [Symbol]) -> Option<&'a str> {
    symbols
        .iter()
        .find(|symbol| {
            !symbol.text.is_empty()
                && symbol
                    .readings
                    .iter()
                    .any(|reading| reading == utterance || (!key.is_empty() && template_key(reading) == key))
        })
        .map(|symbol| symbol.text.as_str())
}

/// 文の末尾に続けて言った言葉を取り出す(「…認めます 改行」)。長い言葉から調べる。
/// 同じ操作の言葉が続いていれば(「改行改行改行」)まとめて外し、回数を数える。
fn match_trailing(utterance: &str, commands: &[CommandWords]) -> Option<Match> {
    let mut candidates: Vec<(String, CommandAction)> = all_words(commands)
        .map(|(word, action)| (template_key(&word), action))
        .filter(|(key, _)| !key.is_empty())
        .collect();
    candidates.sort_by_key(|(key, _)| std::cmp::Reverse(key.chars().count()));
    // 句読点を出すエンジンは言葉の前に読点を置くことがある(「所見なし、改行。」)。文末に残さない。
    let trim = |text: &str| -> String {
        text.trim_end_matches(|c: char| c.is_whitespace() || "、，,".contains(c))
            .to_string()
    };
    let (mut prefix, action) = candidates
        .iter()
        .find_map(|(key, action)| strip_key_suffix(utterance, key).map(|head| (trim(head), *action)))?;
    let mut count = 1;
    while let Some(head) = candidates
        .iter()
        .filter(|(_, a)| *a == action)
        .find_map(|(key, _)| strip_key_suffix(&prefix, key))
    {
        prefix = trim(head);
        count += 1;
    }
    Some(Match::Command {
        prefix,
        action,
        count,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::dictionary::default_symbols;

    fn commands() -> Vec<CommandWords> {
        let mut commands = default_command_words();
        let words = [
            (CommandAction::NewLine, &["改行", "ここで改行"][..]),
            (CommandAction::Transfer, &["転送"]),
        ];
        for (action, list) in words {
            let row = commands.iter_mut().find(|c| c.action == action).unwrap();
            row.words = list.iter().map(|w| w.to_string()).collect();
        }
        commands
    }

    fn symbols() -> Vec<Symbol> {
        default_symbols()
    }

    fn judge(text: &str) -> Match {
        match_utterance(text, &commands(), &symbols())
    }

    fn action_of(result: Match) -> CommandAction {
        match result {
            Match::Command { action, .. } => action,
            other => panic!("not a command: {other:?}"),
        }
    }

    /// 名前(漢字)と読み(かな)は登録しなくても働く。外した操作の言葉は文章のまま。
    #[test]
    fn the_command_names_work_without_registration() {
        let empty = default_command_words();
        assert_eq!(empty.len(), 2);
        assert!(empty.iter().all(|c| c.words.is_empty()));
        for (spoken, action) in [
            ("改行", CommandAction::NewLine),
            ("かいぎょう", CommandAction::NewLine),
            ("カイギョウ。", CommandAction::NewLine),
            ("転送", CommandAction::Transfer),
            ("てんそう", CommandAction::Transfer),
        ] {
            assert_eq!(
                match_utterance(spoken, &empty, &symbols()),
                Match::Command { prefix: String::new(), action, count: 1 },
                "{spoken}"
            );
        }
        for (spoken, prefix, action) in [
            ("結節を認めます 改行", "結節を認めます", CommandAction::NewLine),
            ("前立腺MRI かいぎょう", "前立腺MRI", CommandAction::NewLine),
            ("以上です。転送", "以上です。", CommandAction::Transfer),
        ] {
            assert_eq!(
                match_utterance(spoken, &empty, &symbols()),
                Match::Command { prefix: prefix.to_string(), action, count: 1 },
                "{spoken}"
            );
            assert_eq!(
                match_after_correction(spoken, &empty),
                Match::Command { prefix: prefix.to_string(), action, count: 1 },
                "{spoken}"
            );
        }
        for spoken in ["すべて削除", "音声認識停止", "タブ", "開業"] {
            assert_eq!(match_utterance(spoken, &empty, &symbols()), Match::Text, "{spoken}");
        }
    }

    /// 続けて言った同じ言葉は回数になる。違う操作の言葉は数えない。
    #[test]
    fn repeated_words_are_counted() {
        let empty = default_command_words();
        for (spoken, prefix, count) in [
            ("改行改行改行", "", 3),
            ("胸部単純CT 改行改行改行", "胸部単純CT", 3),
            ("かいぎょうかいぎょう", "", 2),
            ("単純CT 改行、改行。", "単純CT", 2),
            ("改行 かいぎょう", "", 2),
        ] {
            assert_eq!(
                match_utterance(spoken, &empty, &symbols()),
                Match::Command { prefix: prefix.to_string(), action: CommandAction::NewLine, count },
                "{spoken}"
            );
        }
        assert_eq!(
            match_after_correction("所見なし 改行改行", &empty),
            Match::Command { prefix: "所見なし".to_string(), action: CommandAction::NewLine, count: 2 }
        );
        // 転送は何度言っても 1 回。改行のあとの転送は転送だけ外す(改行は文に残る)。
        assert_eq!(
            match_utterance("転送転送", &empty, &symbols()),
            Match::Command { prefix: String::new(), action: CommandAction::Transfer, count: 2 }
        );
        assert_eq!(
            match_utterance("所見なし 改行 転送", &empty, &symbols()),
            Match::Command { prefix: "所見なし 改行".to_string(), action: CommandAction::Transfer, count: 1 }
        );
    }

    #[test]
    fn registered_words_ignore_how_the_engine_writes_them() {
        assert_eq!(action_of(judge("改行。")), CommandAction::NewLine);
        assert_eq!(action_of(judge("転 送")), CommandAction::Transfer);
        assert_eq!(action_of(judge("ここで、改行")), CommandAction::NewLine);
    }

    #[test]
    fn both_words_can_follow_a_sentence() {
        for (spoken, expected, action) in [
            ("結節を認めます 改行", "結節を認めます", CommandAction::NewLine),
            ("結節を認めます、改行", "結節を認めます", CommandAction::NewLine),
            ("結節を認めます。転送", "結節を認めます。", CommandAction::Transfer),
        ] {
            match judge(spoken) {
                Match::Command { prefix, action: a, .. } => {
                    assert_eq!(prefix, expected);
                    assert_eq!(a, action);
                }
                other => panic!("{other:?}"),
            }
        }
    }

    /// 記号の読みはカタカナでも、句読点付きでも同じ。文の途中や文末に続けても記号にはならない。
    #[test]
    fn symbol_readings_match_the_whole_utterance_only() {
        for (spoken, text) in [("まる", "。"), ("テン。", "、"), ("点。", "、"), ("矢印", "→"), ("括弧閉じる", "）")] {
            assert_eq!(judge(spoken), Match::Symbol(text.to_string()), "{spoken}");
        }
        assert_eq!(judge("結節を認めます まる"), Match::Text);
        // 登録した言葉は記号より先に働く。
        let mut with_maru = commands();
        with_maru[0].words.push("まる".to_string());
        assert_eq!(
            match_utterance("まる", &with_maru, &symbols()),
            Match::Command { prefix: String::new(), action: CommandAction::NewLine, count: 1 }
        );
        // 記号は訂正後の判定では働かない。
        assert_eq!(match_after_correction("まる", &commands()), Match::Text);
    }

    #[test]
    fn the_sentence_before_a_trailing_word_stays_as_spoken() {
        match judge("結節を認めます。ここで、改行。") {
            Match::Command { prefix, action, .. } => {
                assert_eq!(prefix, "結節を認めます。");
                assert_eq!(action, CommandAction::NewLine);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn words_inside_a_sentence_stay_text() {
        assert_eq!(judge("改行です"), Match::Text);
        assert_eq!(judge("転送した画像"), Match::Text);
    }

    #[test]
    fn blank_words_never_match() {
        let mut commands = default_command_words();
        commands[0].words = vec!["。".to_string(), " ".to_string()];
        assert_eq!(match_utterance("所見", &commands, &symbols()), Match::Text);
        assert_eq!(match_utterance("。", &commands, &symbols()), Match::Text);
    }

    #[test]
    fn a_registered_word_wins_over_a_symbol_reading() {
        let mut commands = default_command_words();
        commands[0].words = vec!["まる".to_string()];
        assert_eq!(
            action_of(match_utterance("まる", &commands, &symbols())),
            CommandAction::NewLine
        );
    }

    #[test]
    fn symbols_only_match_the_whole_utterance() {
        assert_eq!(judge("まる"), Match::Symbol("。".to_string()));
        assert_eq!(judge("マル。"), Match::Symbol("。".to_string()));
        assert_eq!(judge("テン。"), Match::Symbol("、".to_string()));
        assert_eq!(judge("イコール"), Match::Symbol("＝".to_string()));
        assert_eq!(judge("括弧閉じる"), Match::Symbol("）".to_string()));
        assert_eq!(judge("まるい結節"), Match::Text);
        assert_eq!(judge("はてな"), Match::Text, "既定の表に無い読みは文章");
    }

    #[test]
    fn symbols_come_from_the_dictionary() {
        let mine = vec![
            Symbol {
                readings: vec!["なかぐろ".to_string(), "中黒".to_string()],
                text: "・".to_string(),
                ..Symbol::default()
            },
            Symbol {
                readings: vec!["からっぽ".to_string()],
                text: String::new(),
                ..Symbol::default()
            },
        ];
        assert_eq!(
            match_utterance("中黒", &commands(), &mine),
            Match::Symbol("・".to_string())
        );
        assert_eq!(match_utterance("まる", &commands(), &mine), Match::Text);
        assert_eq!(match_utterance("からっぽ", &commands(), &mine), Match::Text, "文字の無い記号は働かない");
    }

    #[test]
    fn after_a_correction_only_command_words_count() {
        match match_after_correction("所見なし、改行。", &commands()) {
            Match::Command { prefix, action, count } => {
                assert_eq!(prefix, "所見なし");
                assert_eq!(action, CommandAction::NewLine);
                assert_eq!(count, 1);
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(
            match_after_correction("転送", &commands()),
            Match::Command {
                prefix: String::new(),
                action: CommandAction::Transfer,
                count: 1,
            }
        );
        assert_eq!(match_after_correction("まる", &commands()), Match::Text, "記号は訂正後には見ない");
        assert_eq!(match_after_correction("所見なし", &commands()), Match::Text);
    }

    #[test]
    fn rows_with_removed_actions_are_skipped_on_load() {
        #[derive(Deserialize)]
        struct Holder {
            #[serde(deserialize_with = "deserialize_known_command_words")]
            command_words: Vec<CommandWords>,
        }
        let json = r#"{"command_words":[{"action":"NewLine","words":["かいぎょう"]},
            {"action":"Transfer","words":["てんそう"]},{"action":"Tab","words":["たぶ"]},
            {"action":"MoveStart","words":["さいしょにもどる"]},{"action":"MoveEnd","words":["さいごにすすむ"]}]}"#;
        let holder: Holder = serde_json::from_str(json).unwrap();
        let rows = normalized(&holder.command_words);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].words, vec!["かいぎょう"]);
        assert_eq!(rows[1].words, vec!["てんそう"]);
    }
}
