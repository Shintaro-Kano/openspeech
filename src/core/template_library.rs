//! テンプレートの共有ライブラリ(GitHub の `Shintaro-Kano/openspeech-templates`)。
//! 取り込み: 押したときだけ目録 `index.json` を取り、選んだテンプレートを辞書に足す(`meta` は `extra` に残る)。
//! 共有: 投稿フォームの URL に項目を埋めて既定のブラウザで開くだけ。アプリ自身は何も送らない。

use super::assistant::MAX_TEMPLATES;
use super::dictionary::{modules_text, normalize_keywords, template_key, Dictionary, Template};
use std::collections::HashSet;
use super::remote_asr::http::percent_encode;
use serde_json::Value;
use std::path::Path;
use std::process::{Command, Stdio};

/// 共有ライブラリの目録(GitHub Pages)。設定では変えない。
pub const TEMPLATE_LIBRARY_INDEX: &str =
    "https://shintaro-kano.github.io/openspeech-templates/index.json";
/// 投稿フォーム(GitHub の Issue Form)。項目は URL で埋める。
pub const TEMPLATE_SHARE_FORM: &str =
    "https://github.com/Shintaro-Kano/openspeech-templates/issues/new?template=share-template.yml";
/// 投稿フォームの URL の長さの上限。超えたら本文とモジュールの文はクリップボードで渡す。
pub const SHARE_URL_LIMIT: usize = 6000;
/// 投稿フォームの欄の名前(フォームの `id`)。GitHub が URL で予約している `title`・`body`(issue の題名と本文)
/// とは別の名前にする。
const FIELD_EXAM: &str = "exam";
const FIELD_KEYWORDS: &str = "keywords";
const FIELD_FINDINGS: &str = "findings";
const FIELD_MODULES: &str = "modules";
/// issue 自身の題名(GitHub の予約語 `title` を意図して使う)の頭。
const ISSUE_TITLE_PREFIX: &str = "[テンプレート] ";
/// クリップボードに入れるとき、本文とモジュールの文の間に置く行。
const MODULES_DIVIDER: &str = "----- ここから下は「所見モジュール」の欄へ -----";
/// 目録を取る時間の上限(秒)。
const FETCH_SECONDS: &str = "15";
/// 目録の大きさの上限(バイト)。全文入りでも数 MB の見込み。
const MAX_INDEX_BYTES: &str = "20000000";
/// 目録から読む件数の上限。
const MAX_INDEX_TEMPLATES: usize = 1000;
/// 1 件の大きさの上限。超える物は読まない(辞書と、「ヘイクロード」で Claude に送る依頼に入るので)。
const MAX_BODY_CHARS: usize = 5000;
const MAX_KEYWORDS: usize = 20;
const MAX_KEYWORD_CHARS: usize = 40;
const MAX_MODULES: usize = 30;
const MAX_MODULE_CHARS: usize = 3000;

/// 分野(`categories.json` の 1 件)。
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Category {
    pub id: String,
    pub label: String,
}

/// 目録の 1 件。辞書に入れる形(`meta` は `extra` に入ったまま)と、一覧に出す `meta` の項目。
#[derive(Debug, Clone, PartialEq)]
pub struct LibraryTemplate {
    pub template: Template,
    pub id: String,
    /// 題名(`meta.title`、無ければ先頭のキーワード)。
    pub title: String,
    pub category: String,
    pub author: String,
    pub updated: String,
    pub note: String,
}

impl LibraryTemplate {
    /// 一覧に出すキーワード(先頭 `count` 個を「、」でつなぐ。残りがあれば「…」)。
    pub fn keywords_head(&self, count: usize) -> String {
        let keywords = &self.template.keywords;
        let mut text = keywords.iter().take(count).cloned().collect::<Vec<_>>().join("、");
        if keywords.len() > count {
            text.push('…');
        }
        text
    }

    /// 本文の字数。
    pub fn chars(&self) -> usize {
        self.template.body.chars().count()
    }

    /// 分野(None はすべて)と検索語(題名・キーワード・本文・投稿者。大文字小文字は区別しない)で絞る。
    pub fn matches(&self, category: Option<&str>, query: &str) -> bool {
        if category.is_some_and(|id| id != self.category) {
            return false;
        }
        let query = query.trim().to_lowercase();
        if query.is_empty() {
            return true;
        }
        let keywords = self.template.keywords.join("、");
        [self.title.as_str(), &keywords, &self.template.body, &self.author]
            .iter()
            .any(|field| field.to_lowercase().contains(&query))
    }
}

/// 目録(`index.json`)。
#[derive(Debug, Clone, PartialEq, Default)]
pub struct LibraryIndex {
    pub categories: Vec<Category>,
    pub templates: Vec<LibraryTemplate>,
    /// 形の違いで読めなかったテンプレートの説明。
    pub skipped: Vec<String>,
}

impl LibraryIndex {
    /// 分野の表示名。目録に無い id はそのまま出す。
    pub fn category_label<'a>(&'a self, id: &'a str) -> &'a str {
        self.categories
            .iter()
            .find(|c| c.id == id)
            .map_or(id, |c| c.label.as_str())
    }
}

/// 目録を読む。形の違うテンプレートは飛ばして `skipped` に理由を残し、ほかは読む。未知の項目は読み飛ばす
/// (テンプレートの未知の項目は `extra` として保つ)。目録自体が読めなければ Err(知らせる文)。
pub fn parse_index(text: &str) -> Result<LibraryIndex, String> {
    let bad = || "共有ライブラリの目録の形が違います".to_string();
    let root: Value = serde_json::from_str(text).map_err(|_| bad())?;
    let templates = root.get("templates").and_then(Value::as_array).ok_or_else(bad)?;
    let categories = root
        .get("categories")
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(|c| {
                    let id = c.get("id")?.as_str()?.to_string();
                    let label = c.get("label").and_then(Value::as_str).unwrap_or(&id).to_string();
                    Some(Category { id, label })
                })
                .collect()
        })
        .unwrap_or_default();
    let mut index = LibraryIndex {
        categories,
        ..LibraryIndex::default()
    };
    for (number, value) in templates.iter().enumerate().take(MAX_INDEX_TEMPLATES) {
        match read_template(value) {
            Ok(template) => index.templates.push(template),
            Err(why) => index.skipped.push(format!("{} 件目: {why}", number + 1)),
        }
    }
    if templates.len() > MAX_INDEX_TEMPLATES {
        index.skipped.push(format!(
            "以降 {} 件は出していません(先頭 {MAX_INDEX_TEMPLATES} 件まで)",
            templates.len() - MAX_INDEX_TEMPLATES
        ));
    }
    Ok(index)
}

fn read_template(value: &Value) -> Result<LibraryTemplate, String> {
    if !value.is_object() {
        return Err("テンプレートの形ではありません".to_string());
    }
    let mut template: Template =
        serde_json::from_value(value.clone()).map_err(|e| format!("読めません({e})"))?;
    // 目録のファイルの場所は目録だけの項目なので、辞書には入れない。
    template.extra.remove("file");
    template.keywords = normalize_keywords(&template.keywords);
    let Some(first) = template.keywords.first().cloned() else {
        return Err("キーワードがありません".to_string());
    };
    if template.body.trim().is_empty() {
        return Err(format!("「{first}」の文章がありません"));
    }
    check_size(&template).map_err(|why| format!("「{first}」は{why}"))?;
    let meta = template.extra.get("meta").and_then(Value::as_object);
    let field = |name: &str| {
        meta.and_then(|m| m.get(name))
            .and_then(Value::as_str)
            .map(|s| s.trim().to_string())
            .unwrap_or_default()
    };
    let title = Some(field("title")).filter(|t| !t.is_empty()).unwrap_or(first);
    Ok(LibraryTemplate {
        id: field("id"),
        title,
        category: field("category"),
        author: field("author"),
        updated: field("updated"),
        note: field("note"),
        template,
    })
}

/// 1 件の大きさを確かめる。超えていれば理由。
fn check_size(template: &Template) -> Result<(), String> {
    let chars = |text: &str| text.chars().count();
    if chars(&template.body) > MAX_BODY_CHARS {
        return Err(format!("文章が {MAX_BODY_CHARS} 字を超えています"));
    }
    if template.keywords.len() > MAX_KEYWORDS {
        return Err(format!("キーワードが {MAX_KEYWORDS} 個を超えています"));
    }
    if template.keywords.iter().any(|k| chars(k) > MAX_KEYWORD_CHARS) {
        return Err(format!("{MAX_KEYWORD_CHARS} 字を超えるキーワードがあります"));
    }
    if template.modules.len() > MAX_MODULES {
        return Err(format!("所見モジュールが {MAX_MODULES} 個を超えています"));
    }
    if template.modules.iter().any(|m| chars(&m.text) > MAX_MODULE_CHARS) {
        return Err(format!("{MAX_MODULE_CHARS} 字を超える所見モジュールがあります"));
    }
    Ok(())
}

/// 目録を `curl` で取る(押したときだけ。15 秒まで、https だけ)。中身か、知らせる文。
pub fn fetch_index(curl: &Path, url: &str) -> Result<String, String> {
    let output = Command::new(curl)
        .args(["-q", "-sS", "-f", "-L", "--max-time", FETCH_SECONDS])
        .args(["--max-filesize", MAX_INDEX_BYTES])
        .args(["--proto", "=https", "--proto-redir", "=https"])
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| format!("curl を起動できません: {e}"))?;
    if !output.status.success() {
        return Err(match output.status.code() {
            Some(28) => "共有ライブラリから時間内に答えが返りませんでした".to_string(),
            Some(22) => "共有ライブラリの目録が見つかりません".to_string(),
            _ => format!(
                "共有ライブラリに接続できません: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        });
    }
    String::from_utf8(output.stdout).map_err(|_| "共有ライブラリの目録が読めません".to_string())
}

/// 取り込む 1 件と、キーワードが既存のテンプレートと重なったときに置き換えるか(既定は飛ばす)。
#[derive(Debug, Clone, PartialEq)]
pub struct Pick {
    pub template: Template,
    pub replace: bool,
}

/// 取り込みの結果。
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ImportSummary {
    pub added: usize,
    /// 手元の物を置き換えて入れた数。
    pub replaced: usize,
    /// 置き換えで消えた手元のテンプレートの数(置き換え 1 件につき、重なっていた件数)。
    pub removed: usize,
    /// キーワードが重なって飛ばした数。
    pub skipped: usize,
    /// 取り込めなかった物(題名と理由)。
    pub failed: Vec<String>,
}

impl ImportSummary {
    /// 取り込みの知らせ。テンプレートの総数 `total` が `MAX_TEMPLATES` を超えたら、Claude に送る分の注意も添える。
    pub fn message(&self, total: usize) -> String {
        let mut text = format!("共有ライブラリから {} 件を取り込みました", self.added + self.replaced);
        if self.removed > 0 {
            text.push_str(&format!("(手元の {} 件を置き換え)", self.removed));
        }
        if self.skipped > 0 {
            text.push_str(&format!("。キーワードが重なる {} 件は飛ばしました", self.skipped));
        }
        if !self.failed.is_empty() {
            let shown: Vec<&str> = self.failed.iter().take(3).map(String::as_str).collect();
            let more = if self.failed.len() > 3 { " ほか" } else { "" };
            text.push_str(&format!(
                "。取り込めなかった {} 件: {}{more}",
                self.failed.len(),
                shown.join(" / ")
            ));
        }
        if total > MAX_TEMPLATES {
            text.push_str(&format!(
                "。テンプレートが {total} 件になりました。「ヘイクロード」では先頭 {MAX_TEMPLATES} 件だけ Claude に送ります"
            ));
        }
        text
    }
}

/// 選んだテンプレートを 1 件ずつ辞書の末尾に足す(置き換えはその位置に入れる)。キーワードが手元の物と
/// 重なる物は、`replace` なら置き換え、でなければ飛ばす。この取り込みで先に入れた物と重なる物は、`replace` に
/// かかわらず飛ばす(取り込んだばかりの物は消さない)。
pub fn import_templates(dictionary: &mut Dictionary, picks: Vec<Pick>) -> ImportSummary {
    let mut summary = ImportSummary::default();
    let mut imported: HashSet<String> = HashSet::new();
    for mut pick in picks {
        pick.template.keywords = normalize_keywords(&pick.template.keywords);
        let keys: Vec<String> = pick.template.keywords.iter().map(|k| template_key(k)).collect();
        if keys.iter().any(|key| imported.contains(key)) {
            summary.skipped += 1;
            continue;
        }
        let title = pick.template.keywords.first().cloned().unwrap_or_default();
        let overlaps = dictionary.conflicting_templates(&pick.template.keywords, None).len();
        if overlaps > 0 && !pick.replace {
            summary.skipped += 1;
            continue;
        }
        match dictionary.add_template(pick.template, pick.replace) {
            Ok(_) => {
                imported.extend(keys);
                if overlaps > 0 {
                    summary.replaced += 1;
                    summary.removed += overlaps;
                } else {
                    summary.added += 1;
                }
            }
            Err(why) => summary.failed.push(format!("{title}: {why}")),
        }
    }
    summary
}

/// 投稿フォームを開く URL。長すぎて URL に入らなかった本文とモジュールの文は `clipboard` に入る。
#[derive(Debug, Clone, PartialEq)]
pub struct ShareLink {
    pub url: String,
    pub clipboard: Option<String>,
}

/// テンプレート 1 件から投稿フォームの URL を組む(題名・キーワード・文章・所見モジュール。投稿者は空)。
pub fn share_link(template: &Template) -> ShareLink {
    share_link_within(template, SHARE_URL_LIMIT)
}

fn share_link_within(template: &Template, limit: usize) -> ShareLink {
    let title = template
        .extra
        .get("meta")
        .and_then(|meta| meta.get("title"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|title| !title.is_empty())
        .map(str::to_string)
        .or_else(|| template.keywords.first().cloned())
        .unwrap_or_default();
    let modules = modules_text(&template.modules);
    let param = |name: &str, value: &str| format!("&{name}={}", percent_encode(value));
    // 題名とキーワードだけで上限を超えても(キーワードが極端に多い)、そのまま開く。
    let head = format!(
        "{TEMPLATE_SHARE_FORM}{}{}{}",
        param("title", &format!("{ISSUE_TITLE_PREFIX}{title}")),
        param(FIELD_EXAM, &title),
        param(FIELD_KEYWORDS, &template.keywords.join("、"))
    );
    let mut full = format!("{head}{}", param(FIELD_FINDINGS, &template.body));
    if !modules.is_empty() {
        full.push_str(&param(FIELD_MODULES, &modules));
    }
    if full.len() <= limit {
        return ShareLink {
            url: full,
            clipboard: None,
        };
    }
    let mut text = template.body.trim_end().to_string();
    if !modules.is_empty() {
        text.push_str(&format!("\n\n{MODULES_DIVIDER}\n{}", modules.trim_end()));
    }
    ShareLink {
        url: head,
        clipboard: Some(text),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::dictionary::{parse_csv_records, Module};

    const INDEX: &str = r#"{
      "generated": "2026-11-09T02:00:00Z", "count": 3, "future": {"x": 1},
      "categories": [{"id": "chest", "label": "胸部"}, {"id": "abdomen-pelvis", "label": "腹部・骨盤"}],
      "templates": [
        {"keywords": ["胸部CT", "胸部単純CT", "胸部", "chest CT"],
         "body": ["胸部単純CT", "肺に明らかな異常陰影を認めません。"],
         "modules": [{"name": "肺気腫", "text": ["小葉中心性肺気腫を認めます。"], "note": "肺野の文と差し替え"}],
         "meta": {"id": "chest-ct", "title": "胸部単純CT", "category": "chest", "modality": "CT",
                  "author": "Shintaro Kano", "updated": "2026-11-09", "license": "CC0-1.0", "note": "一言"},
         "file": "templates/chest/chest-ct.json", "unknown": 5},
        {"keywords": ["前立腺MRI"], "body": "前立腺のサイズは〔　〕cm。"},
        {"keywords": [], "body": "キーワードが無い"},
        {"keywords": ["空"], "body": "  "},
        "文字列"
      ]}"#;

    #[test]
    fn the_index_is_read_with_missing_and_unknown_fields() {
        let index = parse_index(INDEX).unwrap();
        assert_eq!(index.categories.len(), 2);
        assert_eq!(index.category_label("chest"), "胸部");
        assert_eq!(index.category_label("spine"), "spine", "目録に無い分野は id のまま");
        assert_eq!(index.templates.len(), 2);
        assert_eq!(index.skipped.len(), 3, "{:?}", index.skipped);

        let chest = &index.templates[0];
        assert_eq!(chest.title, "胸部単純CT");
        assert_eq!(chest.category, "chest");
        assert_eq!(chest.author, "Shintaro Kano");
        assert_eq!(chest.updated, "2026-11-09");
        assert_eq!(chest.template.body, "胸部単純CT\n肺に明らかな異常陰影を認めません。", "行の配列");
        assert_eq!(chest.template.modules[0].note, "肺野の文と差し替え");
        assert_eq!(chest.keywords_head(3), "胸部CT、胸部単純CT、胸部…");
        assert_eq!(chest.template.extra["meta"]["license"], "CC0-1.0", "meta は extra に残る");
        assert_eq!(chest.template.extra["unknown"], 5, "未知の項目も残る");
        assert!(!chest.template.extra.contains_key("file"), "目録だけの項目は入れない");

        let prostate = &index.templates[1];
        assert_eq!(prostate.title, "前立腺MRI", "meta が無ければ先頭のキーワード");
        assert_eq!(prostate.template.body, "前立腺のサイズは〔　〕cm。", "文字列の本文");
        assert_eq!(prostate.chars(), 14);
        assert!(prostate.category.is_empty() && prostate.author.is_empty());

        assert!(chest.matches(Some("chest"), "肺に"));
        assert!(chest.matches(None, "kano"));
        assert!(!chest.matches(Some("abdomen-pelvis"), ""));
        assert!(!prostate.matches(None, "肺"));

        assert!(parse_index("{}").is_err());
        assert!(parse_index("<html>").is_err());
    }

    fn library_template(keywords: &[&str], body: &str) -> Template {
        Template {
            keywords: keywords.iter().map(|k| k.to_string()).collect(),
            body: body.to_string(),
            ..Template::default()
        }
    }

    #[test]
    fn importing_adds_skips_or_replaces_and_keeps_meta() {
        let index = parse_index(INDEX).unwrap();
        let mut dictionary = Dictionary::default();
        dictionary.save_template(None, "胸部単純CT、胸部造影CT", "手元の文").unwrap();
        dictionary.save_template(None, "頭部CT", "頭の文").unwrap();

        let picks = vec![
            Pick { template: index.templates[0].template.clone(), replace: false },
            Pick { template: index.templates[1].template.clone(), replace: false },
        ];
        let summary = import_templates(&mut dictionary, picks);
        assert_eq!(summary, ImportSummary { added: 1, skipped: 1, ..ImportSummary::default() });
        assert_eq!(dictionary.templates.len(), 3);
        assert_eq!(dictionary.templates[0].body, "手元の文", "飛ばした物は手元のまま");
        assert_eq!(dictionary.templates[2].keywords, vec!["前立腺MRI"]);

        let picks = vec![Pick { template: index.templates[0].template.clone(), replace: true }];
        let summary = import_templates(&mut dictionary, picks);
        assert_eq!((summary.replaced, summary.removed), (1, 1));
        assert_eq!(dictionary.templates.len(), 3, "置き換えは数を変えない");
        assert_eq!(dictionary.templates[0].keywords[0], "胸部CT", "重なった物の位置に入れる");
        assert_eq!(dictionary.templates[0].extra["meta"]["id"], "chest-ct", "meta を extra に保つ");
        assert_eq!(dictionary.templates[0].modules.len(), 1);

        // 辞書ファイルに書いて読み直しても meta は残る。
        let saved = serde_json::to_string(&dictionary.templates[0]).unwrap();
        let reread: Template = serde_json::from_str(&saved).unwrap();
        assert_eq!(reread.extra["meta"]["title"], "胸部単純CT");

        // 「〜テンプレート」で終わるキーワードは画面の保存と同じに断る。
        let picks = vec![Pick { template: library_template(&["腹部テンプレート"], "文"), replace: false }];
        let summary = import_templates(&mut dictionary, picks);
        assert_eq!(summary.failed.len(), 1, "{summary:?}");
        assert!(summary.message(3).contains("取り込めなかった 1 件"), "{}", summary.message(3));
    }

    #[test]
    fn a_replacement_overlapping_two_templates_takes_the_first_place() {
        let mut dictionary = Dictionary::default();
        dictionary.save_template(None, "腹部CT", "腹").unwrap();
        dictionary.save_template(None, "頭部CT", "頭").unwrap();
        dictionary.save_template(None, "骨盤CT", "骨盤").unwrap();
        let picks = vec![Pick { template: library_template(&["腹部CT", "骨盤CT"], "腹部骨盤"), replace: true }];
        let summary = import_templates(&mut dictionary, picks);
        assert_eq!((summary.replaced, summary.removed), (1, 2));
        assert_eq!(summary.message(2), "共有ライブラリから 1 件を取り込みました(手元の 2 件を置き換え)");
        let bodies: Vec<&str> = dictionary.templates.iter().map(|t| t.body.as_str()).collect();
        assert_eq!(bodies, vec!["腹部骨盤", "頭"]);
    }

    /// この取り込みで先に入れた物と重なる物は、置き換えを選んでいても飛ばす(取り込んだばかりの物を消さない)。
    #[test]
    fn picks_overlapping_each_other_never_remove_what_was_just_imported() {
        let mut dictionary = Dictionary::default();
        dictionary.save_template(None, "胸部単純CT", "手元").unwrap();
        let picks = vec![
            Pick { template: library_template(&["胸部CT"], "A"), replace: false },
            Pick { template: library_template(&["胸部CT", "胸部単純CT"], "B"), replace: true },
        ];
        let summary = import_templates(&mut dictionary, picks);
        assert_eq!(summary, ImportSummary { added: 1, skipped: 1, ..ImportSummary::default() });
        let bodies: Vec<&str> = dictionary.templates.iter().map(|t| t.body.as_str()).collect();
        assert_eq!(bodies, vec!["手元", "A"]);
    }

    /// キーワードは画面の欄と同じに分け直す(「、」入りの 1 語を 2 語にし、重複を除く)。
    #[test]
    fn imported_keywords_are_split_like_the_field() {
        let mut dictionary = Dictionary::default();
        let picks = vec![Pick { template: library_template(&["胸部CT、胸部単純CT", " 胸部CT "], "文"), replace: false }];
        import_templates(&mut dictionary, picks);
        assert_eq!(dictionary.templates[0].keywords, vec!["胸部CT", "胸部単純CT"]);
        let index = parse_index(r#"{"templates": [{"keywords": ["腹部CT,腹部"], "body": "文"}]}"#).unwrap();
        assert_eq!(index.templates[0].template.keywords, vec!["腹部CT", "腹部"]);
    }

    /// 画面で直したら共有ライブラリの meta を外す(共有の題名に古い物を使わない)。ほかの extra は残す。
    #[test]
    fn editing_an_imported_template_drops_its_meta() {
        let index = parse_index(INDEX).unwrap();
        let mut dictionary = Dictionary::default();
        let picks = vec![Pick { template: index.templates[0].template.clone(), replace: false }];
        import_templates(&mut dictionary, picks);
        dictionary.templates[0].extra.insert("memo".to_string(), "自分の覚え書き".into());
        assert!(share_link(&dictionary.templates[0]).url.contains(&format!("&exam={}", percent_encode("胸部単純CT"))));
        dictionary
            .save_template_with_modules(Some(0), "胸部CT", "直した文", &modules_text(&dictionary.templates[0].modules.clone()))
            .unwrap();
        let template = &dictionary.templates[0];
        assert!(!template.extra.contains_key("meta"));
        assert_eq!(template.extra["memo"], "自分の覚え書き");
        assert!(share_link(template).url.contains(&format!("&exam={}", percent_encode("胸部CT"))), "題名は先頭のキーワード");
    }

    /// 大きすぎる物・形の違う物・危ない入力は、panic せず飛ばすか Err。
    #[test]
    fn oversized_or_odd_entries_are_skipped_without_panicking() {
        let big = "あ".repeat(MAX_BODY_CHARS + 1);
        let many: Vec<String> = (0..=MAX_KEYWORDS).map(|n| format!("\"語{n}\"")).collect();
        let long_keyword = "語".repeat(MAX_KEYWORD_CHARS + 1);
        let modules: Vec<String> = (0..=MAX_MODULES).map(|n| format!(r#"{{"name": "m{n}", "text": "文"}}"#)).collect();
        let long_module = "文".repeat(MAX_MODULE_CHARS + 1);
        let text = format!(
            r#"{{"templates": [
              {{"keywords": "胸部CT", "body": "文"}},
              {{"keywords": ["A"], "body": "文", "modules": [null]}},
              {{"keywords": ["B"], "body": "{big}"}},
              {{"keywords": [{}], "body": "文"}},
              {{"keywords": ["{long_keyword}"], "body": "文"}},
              {{"keywords": ["C"], "body": "文", "modules": [{}]}},
              {{"keywords": ["D"], "body": "文", "modules": [{{"name": "m", "text": "{long_module}"}}]}},
              {{"keywords": ["E"], "body": "文", "meta": "文字列の meta"}},
              {{"keywords": ["F"], "body": "文"}}
            ]}}"#,
            many.join(","),
            modules.join(",")
        );
        let index = parse_index(&text).unwrap();
        let read: Vec<&str> = index.templates.iter().map(|t| t.template.keywords[0].as_str()).collect();
        assert!(read.contains(&"F") && read.contains(&"E"), "{read:?} / {:?}", index.skipped);
        for gone in ["B", "C", "D"] {
            assert!(!read.contains(&gone), "{gone}");
        }
        assert!(index.skipped.iter().any(|s| s.contains("文章が 5000 字を超えています")), "{:?}", index.skipped);
        assert!(index.skipped.iter().any(|s| s.contains("キーワードが 20 個を超えています")));
        assert!(index.skipped.iter().any(|s| s.contains("40 字を超えるキーワード")));
        assert!(index.skipped.iter().any(|s| s.contains("所見モジュールが 30 個を超えています")));
        assert!(index.skipped.iter().any(|s| s.contains("3000 字を超える所見モジュール")));
        assert_eq!(index.templates.len() + index.skipped.len(), 9);
        let e = index.templates.iter().find(|t| t.template.keywords[0] == "E").unwrap();
        assert_eq!(e.title, "E", "meta が物でなければ先頭のキーワード");

        let deep = format!("{}{}", "[".repeat(100_000), "]".repeat(100_000));
        assert!(parse_index(&deep).is_err());
        let deep_template = format!(r#"{{"templates": [{{"keywords": ["G"], "body": "文", "x": {}{}}}]}}"#, "[".repeat(10_000), "]".repeat(10_000));
        assert!(parse_index(&deep_template).is_err());
    }

    /// 目録は先頭 1000 件まで読む。
    #[test]
    fn only_the_first_thousand_templates_are_read() {
        let entries: Vec<String> = (0..MAX_INDEX_TEMPLATES + 3)
            .map(|n| format!(r#"{{"keywords": ["検査{n}"], "body": "文"}}"#))
            .collect();
        let index = parse_index(&format!(r#"{{"templates": [{}]}}"#, entries.join(","))).unwrap();
        assert_eq!(index.templates.len(), MAX_INDEX_TEMPLATES);
        assert_eq!(index.skipped, vec!["以降 3 件は出していません(先頭 1000 件まで)".to_string()]);
    }

    #[test]
    fn the_message_warns_past_the_templates_sent_to_claude() {
        let summary = ImportSummary { added: 3, ..ImportSummary::default() };
        assert_eq!(summary.message(MAX_TEMPLATES), "共有ライブラリから 3 件を取り込みました");
        let message = summary.message(MAX_TEMPLATES + 2);
        assert!(message.contains("102 件になりました") && message.contains("先頭 100 件だけ Claude に送ります"), "{message}");
        let summary = ImportSummary { added: 1, replaced: 1, removed: 1, skipped: 2, ..ImportSummary::default() };
        assert_eq!(
            summary.message(5),
            "共有ライブラリから 2 件を取り込みました(手元の 1 件を置き換え)。キーワードが重なる 2 件は飛ばしました"
        );
    }

    #[test]
    fn the_share_link_fills_the_form_fields() {
        let index = parse_index(INDEX).unwrap();
        let link = share_link(&index.templates[0].template);
        assert!(link.clipboard.is_none());
        assert!(link.url.starts_with(TEMPLATE_SHARE_FORM));
        assert!(link.url.contains(&format!("&title={}&", percent_encode("[テンプレート] 胸部単純CT"))), "issue の題名");
        assert!(link.url.contains(&format!("&exam={}&", percent_encode("胸部単純CT"))), "meta の題名");
        assert!(link.url.contains(&format!("&keywords={}", percent_encode("胸部CT、胸部単純CT、胸部、chest CT"))));
        assert!(link.url.contains("&findings=%E8%83%B8"), "{}", link.url);
        assert!(!link.url.contains("&body="), "GitHub の予約語の body は使わない");
        assert!(link.url.contains("%0A"), "改行は %0A");
        assert!(link.url.contains("&modules=%E3%80%90"), "モジュールの欄は【名前】から");
        assert!(!link.url.contains("author"), "投稿者は空のまま");
        assert!(!link.url[TEMPLATE_SHARE_FORM.len()..].contains(['?', ' ', '\n', '#']));

        // meta が無ければ先頭のキーワードが題名。モジュールが無ければその欄は付けない。
        let plain = library_template(&["頭部CT", "頭部単純CT"], "頭蓋内に異常なし。");
        let link = share_link(&plain);
        assert!(link.url.contains(&format!("&exam={}&", percent_encode("頭部CT"))));
        assert!(!link.url.contains("&modules="));
    }

    #[test]
    fn a_long_share_link_leaves_the_text_to_the_clipboard() {
        let mut template = library_template(&["胸部CT"], &"肺に異常なし。\n".repeat(120));
        template.modules = vec![Module {
            name: "肺気腫".to_string(),
            text: "小葉中心性肺気腫。".to_string(),
            ..Module::default()
        }];
        let full = share_link_within(&template, usize::MAX);
        assert!(full.url.len() > SHARE_URL_LIMIT, "{}", full.url.len());
        let link = share_link(&template);
        assert!(link.url.len() <= SHARE_URL_LIMIT);
        assert!(link.url.contains("&title=") && link.url.contains("&exam=") && link.url.contains("&keywords="));
        assert!(!link.url.contains("&findings=") && !link.url.contains("&modules="));
        let text = link.clipboard.unwrap();
        assert!(text.starts_with("肺に異常なし。\n肺に異常なし。"));
        assert!(text.contains(MODULES_DIVIDER) && text.ends_with("小葉中心性肺気腫。"), "{text}");

        // 6000 字ちょうどまでは URL に入れる。
        let limit = full.url.len();
        assert!(share_link_within(&template, limit).clipboard.is_none());
        assert!(share_link_within(&template, limit - 1).clipboard.is_some());
    }

    /// 取り込んだ物を CSV に書き出すと、一覧ページの CSV と同じ 3 列(キーワード「、」区切り, 本文, モジュールの文)
    /// になり、読み直すと同じテンプレートになる。
    #[test]
    fn imported_templates_round_trip_through_csv() {
        let index = parse_index(INDEX).unwrap();
        let mut dictionary = Dictionary::default();
        let picks = index.templates.iter().map(|t| Pick { template: t.template.clone(), replace: false }).collect();
        import_templates(&mut dictionary, picks);
        let csv = dictionary.export_templates_csv();
        let records = parse_csv_records(&csv);
        assert_eq!(records.len(), 2);
        let chest = &index.templates[0].template;
        assert_eq!(records[0], vec![chest.keywords.join("、"), chest.body.clone(), modules_text(&chest.modules)]);
        assert_eq!(records[1][2], "", "モジュールが無ければ 3 列目は空");

        let mut again = Dictionary::default();
        let (count, bad) = again.import_templates_csv(csv.as_bytes());
        assert_eq!((count, bad.len()), (2, 0));
        for (a, b) in again.templates.iter().zip(&dictionary.templates) {
            assert_eq!((&a.keywords, &a.body, &a.modules), (&b.keywords, &b.body, &b.modules));
        }
    }

    /// 起動できない curl の場所では、知らせる文だけを返す。
    #[test]
    fn a_failing_fetch_only_reports() {
        let missing = Path::new("/nonexistent/curl");
        let error = fetch_index(missing, TEMPLATE_LIBRARY_INDEX).unwrap_err();
        assert!(error.contains("curl を起動できません"), "{error}");
    }

    /// curl に渡す引数: https だけ、時間と大きさの上限、HTTP の誤りは失敗、URL。引数を書き出すだけの sh を
    /// curl の代わりに置いて見る(実物の curl とネットワークは使わない)。
    #[test]
    fn the_fetch_passes_the_expected_arguments_to_curl() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("openspeech-ut-curl-args-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("curl");
        std::fs::write(&script, "#!/bin/sh\nfor a in \"$@\"; do printf '%s\\n' \"$a\"; done\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let output = fetch_index(&script, TEMPLATE_LIBRARY_INDEX).unwrap();
        let args: Vec<&str> = output.lines().collect();
        let after = |flag: &str| args.iter().position(|a| *a == flag).map(|i| args[i + 1]);
        assert_eq!(args[0], "-q", "~/.curlrc を読ませない");
        assert!(args.contains(&"-f") && args.contains(&"-L") && args.contains(&"-sS"), "{args:?}");
        assert_eq!(after("--max-time"), Some("15"));
        assert_eq!(after("--max-filesize"), Some(MAX_INDEX_BYTES));
        assert_eq!(after("--proto"), Some("=https"));
        assert_eq!(after("--proto-redir"), Some("=https"));
        assert_eq!(args.last(), Some(&TEMPLATE_LIBRARY_INDEX));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
