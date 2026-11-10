//! 単位 S2 の画面側の試験: 裏の注釈(読み込み時・保存時)、`structure.json` の読み書きと prune、確認表で直した
//! 構造の保存、付け直し、撮影の `--open structure`。Claude は呼ばない(注釈の口を偽物に差し替え、答えは
//! `tests/fixtures/annotate14_sonnet.json` の録った物を使う)。`dictionary.json` を書き換えないことも確かめる。

use openspeech::app::{AnnotateFn, StructureState, VoiceApp};
use openspeech::core::annotate::structure_from_answer;
use openspeech::core::dictionary::{Dictionary, Template};
use openspeech::core::store::Store;
use openspeech::core::structure::source_hash;
use serde::Deserialize;
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Deserialize)]
struct TemplateFixture {
    templates: Vec<Template>,
}

#[derive(Deserialize)]
struct AnswerFixture {
    answers: BTreeMap<String, Value>,
}

fn fixture(name: &str) -> String {
    std::fs::read_to_string(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name)).unwrap()
}

fn templates(names: &[&str]) -> Vec<Template> {
    let all = serde_json::from_str::<TemplateFixture>(&fixture("templates14.json")).unwrap().templates;
    names
        .iter()
        .map(|name| all.iter().find(|t| t.keywords[0] == *name).unwrap().clone())
        .collect()
}

fn answers() -> BTreeMap<String, Value> {
    serde_json::from_str::<AnswerFixture>(&fixture("annotate14_sonnet.json")).unwrap().answers
}

/// 辞書ファイルに `names` のテンプレートを書いたデータのフォルダで、画面を作る(OS に触らない作り方)。
fn app_with(tag: &str, names: &[&str]) -> (VoiceApp, Store) {
    let home = std::env::temp_dir().join(format!("openspeech-s2-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    let store = Store::at(&home);
    let dictionary = Dictionary {
        templates: templates(names),
        ..Dictionary::default()
    };
    store.save_dictionary(&dictionary).unwrap();
    let app = VoiceApp::with_store(egui::Context::default(), store.clone(), None);
    (app, store)
}

/// 録った答えを返す偽の注釈役。呼ばれた回数を数える。
fn recorded_hook(calls: Arc<AtomicUsize>) -> AnnotateFn {
    let answers = answers();
    Arc::new(move |template, vocabulary| {
        calls.fetch_add(1, Ordering::SeqCst);
        let answer = answers.get(&template.keywords[0]).ok_or("答えが無い")?;
        structure_from_answer(template, vocabulary, answer, "2026-11-10T09:00:00+09:00")
    })
}

/// 裏の注釈が終わるまで答えを受け取る。
fn settle(app: &mut VoiceApp) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !app.annotations.running.is_empty() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
        app.receive_annotations();
    }
    app.receive_annotations();
    assert!(app.annotations.running.is_empty(), "注釈が終わらない");
}

/// 試験と撮影の作り方(`with_store`)では裏の注釈は動かない。動かすと構造の無いテンプレートだけを 1 回ずつ
/// 注釈し、`structure.json` に源泉のハッシュで書く。辞書ファイルは一字も変わらない。
#[test]
fn unannotated_templates_are_annotated_in_the_background_into_structure_json_only() {
    let (mut app, store) = app_with("background", &["腹部骨盤CT", "頭部MRI", "頸部単純CT"]);
    let dictionary_before = std::fs::read(store.dictionary_path()).unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    app.set_annotate_hook(recorded_hook(calls.clone()));
    assert!(!app.annotations.automatic, "試験の作り方では動かない");
    app.queue_annotations();
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert!(!store.structure_path().exists());

    app.annotations.automatic = true;
    app.queue_annotations();
    settle(&mut app);
    assert_eq!(calls.load(Ordering::SeqCst), 3);
    let file = store.load_structure();
    assert_eq!(file.templates.len(), 3);
    for template in &app.data.dictionary.templates {
        let structure = file.structure_for(template).expect("構造が書かれている");
        assert_eq!(structure.name, template.keywords[0]);
        assert!(matches!(app.structure_state(template), StructureState::Annotated(_)));
    }
    let abdomen = file.structure_for(&app.data.dictionary.templates[0]).unwrap();
    assert!(abdomen.unresolved.iter().any(|u| u.contains("「膀胱」が本文に無い")), "{:?}", abdomen.unresolved);
    assert_eq!(std::fs::read(store.dictionary_path()).unwrap(), dictionary_before, "辞書ファイルは変わらない");

    // 構造のあるテンプレートは注釈し直さない。
    app.queue_annotations();
    settle(&mut app);
    assert_eq!(calls.load(Ordering::SeqCst), 3);
    // 作り直した画面は structure.json を読む。
    let reopened = VoiceApp::with_store(egui::Context::default(), store.clone(), None);
    assert_eq!(reopened.annotations.structures, file);
}

/// 画面で源泉を直して保存すると、古い構造は消え(prune)、新しい源泉が裏で注釈される。外で辞書ファイルを
/// 書き換えても(見張りが読み直す)同じ。
#[test]
fn editing_the_source_prunes_the_old_structure_and_annotates_the_new_one() {
    let (mut app, store) = app_with("edit", &["胸部CT", "頸部単純CT"]);
    let calls = Arc::new(AtomicUsize::new(0));
    app.set_annotate_hook(recorded_hook(calls.clone()));
    app.annotations.automatic = true;
    app.queue_annotations();
    settle(&mut app);
    let old_hash = source_hash(&app.data.dictionary.templates[1]);

    app.data.dictionary.templates[1].body.push_str("\n甲状腺の左葉に小結節を認めます。");
    app.save_data();
    settle(&mut app);
    assert_eq!(calls.load(Ordering::SeqCst), 3);
    let file = store.load_structure();
    assert_eq!(file.templates.len(), 2, "古い源泉の構造は消える");
    assert!(!file.templates.contains_key(&old_hash));
    let neck = file.structure_for(&app.data.dictionary.templates[1]).unwrap();
    assert_eq!(neck.lines.len(), 5);
    assert!(neck.unresolved.iter().any(|u| u.contains("本文 4 行目の注釈が無い")), "{:?}", neck.unresolved);

    // 外での書き換え: テンプレートを 1 件消して 1 件足す。
    let mut dictionary = store.read_dictionary().unwrap().unwrap();
    dictionary.templates.remove(0);
    dictionary.templates.extend(templates(&["腰椎CT"]));
    std::thread::sleep(Duration::from_millis(20));
    store.save_dictionary(&dictionary).unwrap();
    app.watch_files(Instant::now());
    settle(&mut app);
    assert_eq!(calls.load(Ordering::SeqCst), 4);
    let file = store.load_structure();
    let names: Vec<&str> = file.templates.values().map(|s| s.name.as_str()).collect();
    assert_eq!(file.templates.len(), 2, "{names:?}");
    assert!(names.contains(&"腰椎CT") && names.contains(&"頸部単純CT"));
}

/// 接続の失敗は未注釈のまま(同じ起動では裏で付け直さない)。「付け直す」は押せば行い、失敗を知らせる。
#[test]
fn failures_stay_unannotated_until_annotated_again_by_hand() {
    let (mut app, store) = app_with("failure", &["頭部CT"]);
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    app.set_annotate_hook(Arc::new(move |_, _| {
        counter.fetch_add(1, Ordering::SeqCst);
        Err("Claude Code にログインしていません".to_string())
    }));
    app.annotations.automatic = true;
    app.queue_annotations();
    settle(&mut app);
    let template = app.data.dictionary.templates[0].clone();
    assert!(matches!(app.structure_state(&template), StructureState::Failed(reason) if reason.contains("ログイン")));
    assert!(app.notice.is_none(), "裏の失敗は知らせない");
    assert!(!store.structure_path().exists());
    app.queue_annotations();
    settle(&mut app);
    assert_eq!(calls.load(Ordering::SeqCst), 1, "同じ起動では裏で付け直さない");

    app.reannotate(0);
    settle(&mut app);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    let notice = app.notice.as_ref().expect("付け直しの失敗は知らせる");
    assert!(notice.is_error && notice.text.contains("構造を付け直せませんでした"), "{}", notice.text);

    // 付け直しが通れば構造が入る。
    app.set_annotate_hook(recorded_hook(Arc::new(AtomicUsize::new(0))));
    app.reannotate(0);
    settle(&mut app);
    assert!(matches!(app.structure_state(&template), StructureState::Annotated(_)));
    assert!(store.load_structure().structure_for(&template).is_some());
}

/// 確認表で直した構造はすぐ保存し、警告は直した構造から作り直す(鍵を外すと「鍵が無い」、戻すと消える)。
#[test]
fn table_edits_are_saved_and_the_warnings_are_rebuilt() {
    let (mut app, store) = app_with("table", &["腹部MRI"]);
    app.set_annotate_hook(recorded_hook(Arc::new(AtomicUsize::new(0))));
    app.annotations.automatic = true;
    app.queue_annotations();
    settle(&mut app);
    let template = app.data.dictionary.templates[0].clone();
    let original = store.load_structure().structure_for(&template).unwrap().clone();

    let mut edited = original.clone();
    edited.lines[2].slot = None;
    app.update_structure(0, edited);
    let saved = store.load_structure().structure_for(&template).unwrap().clone();
    assert_eq!(saved.lines[2].slot, None);
    assert!(saved.unresolved.iter().any(|u| u == "2 行目に鍵が無い"), "{:?}", saved.unresolved);
    assert!(saved.unresolved.iter().any(|u| u.contains("門脈周囲浮腫")), "注のずれは残る");

    let mut fixed = saved.clone();
    fixed.lines[2].slot = original.lines[2].slot.clone();
    app.update_structure(0, fixed);
    let saved = store.load_structure().structure_for(&template).unwrap().clone();
    assert!(!saved.unresolved.iter().any(|u| u.contains("鍵が無い")), "{:?}", saved.unresolved);
}

/// 撮影の `--open structure`: 変種の最も多いテンプレートを編集中にして「構造」を開く求めを出す。
#[test]
fn the_structure_view_opens_on_the_busiest_template() {
    let (mut app, _store) = app_with("open", &["胸部CT", "腹部骨盤CT", "頭部CT"]);
    app.set_annotate_hook(recorded_hook(Arc::new(AtomicUsize::new(0))));
    app.annotations.automatic = true;
    app.queue_annotations();
    settle(&mut app);
    app.open_window_by_name("structure");
    let tool = app.windows.dict.as_ref().unwrap();
    assert_eq!(tool.editing, Some(1), "腹部骨盤CT");
    assert!(tool.open_structure);
}
