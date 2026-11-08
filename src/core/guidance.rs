//! 利用者が書き換える指示のファイル(訂正の `guidance.md`・依頼の `request.md`)を、Claude に送る文にする。
//!
//! `<!-- … -->` で囲んだ部分は利用者向けの注釈で、送らず、字数にも数えない。雛形の注釈は
//! 「この <!-- --> で囲んだ部分は…」と注釈の印そのものを書くので、注釈の中の `<!--` は入れ子として数える
//! (HTML の注釈は入れ子にならないが、ここでは書いた通りに閉じる)。閉じていない `<!--` から後ろは文のまま残す
//! (閉じ忘れで後ろの指示が黙って消えないように)。

const OPEN: &str = "<!--";
const CLOSE: &str = "-->";

/// `<!-- … -->` の注釈を取り除く(入れ子は数える。閉じていない注釈は残す)。
pub fn strip_comments(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find(OPEN) {
        let mut depth = 1;
        let mut at = start + OPEN.len();
        let end = loop {
            let open = rest[at..].find(OPEN).map(|i| at + i);
            let Some(close) = rest[at..].find(CLOSE).map(|i| at + i) else {
                break None;
            };
            match open {
                Some(open) if open < close => {
                    depth += 1;
                    at = open + OPEN.len();
                }
                _ => {
                    depth -= 1;
                    at = close + CLOSE.len();
                    if depth == 0 {
                        break Some(at);
                    }
                }
            }
        };
        let Some(end) = end else {
            break;
        };
        out.push_str(&rest[..start]);
        rest = &rest[end..];
    }
    out.push_str(rest);
    out
}

/// 送る指示の文。注釈を剥がして前後の空白を除き、空なら既定(既定の注釈も剥がす)。`limit` 字を超えた分は切る。
pub fn prepare(text: &str, default: &str, limit: usize) -> String {
    let own = strip_comments(text);
    let own = own.trim();
    let chosen = if own.is_empty() {
        strip_comments(default)
    } else {
        own.to_string()
    };
    chosen.trim().chars().take(limit).collect()
}

/// 送る字数(注釈と前後の空白を除く)。0 ならアプリの既定を使う。
pub fn sent_chars(text: &str) -> usize {
    strip_comments(text).trim().chars().count()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn comments_are_removed_and_nested_marks_are_counted() {
        assert_eq!(strip_comments("前<!-- 注 -->後"), "前後");
        assert_eq!(strip_comments("<!--\n説明\nこの <!-- --> で囲んだ部分は送られません。\n-->\n# 規則\n"), "\n# 規則\n");
        assert_eq!(strip_comments("a<!-- 1 -->b<!-- 2 -->c"), "abc");
        assert_eq!(strip_comments("<!---->x"), "x");
        assert_eq!(strip_comments("注釈なし"), "注釈なし");
    }

    #[test]
    fn an_unclosed_comment_is_kept_as_text() {
        assert_eq!(strip_comments("規則 <!-- 閉じ忘れ\n続きの規則"), "規則 <!-- 閉じ忘れ\n続きの規則");
        assert_eq!(strip_comments("a<!-- 1 -->b <!-- <!-- 2 --> c"), "ab <!-- <!-- 2 --> c");
        assert_eq!(strip_comments("--> だけ"), "--> だけ");
    }

    #[test]
    fn only_comments_fall_back_to_the_default_and_the_limit_applies() {
        let default = "<!-- 既定の説明 -->\n既定の規則\n";
        assert_eq!(prepare("", default, 100), "既定の規則");
        assert_eq!(prepare("<!-- 説明だけ -->\n\n", default, 100), "既定の規則");
        assert_eq!(prepare(" 自分の規則 <!-- メモ -->\n", default, 100), "自分の規則");
        assert_eq!(prepare("あいうえお", default, 3), "あいう");
        assert_eq!(sent_chars("<!-- 数えない -->\n 規則 \n"), 2);
        assert_eq!(sent_chars(default), 5);
    }
}
