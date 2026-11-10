//! 単位 S2(docs/simplify-and-assistant-plan.md §16.8)の試験。ネットワークを使わない。
//!
//! - 実物の 14 件(`templates14.json`)で、機械の検査が古い注 4 件だけを見つける。
//! - Sonnet の実際の答え(`annotate14_sonnet.json`。下の `record_live_answers` で録った物)を構造に組み、
//!   S1 の手書きの構造(`structure14.json`)との一致を測る(完了条件 ≥ 90%)。
//!
//! 録り直し(ネットワークと Claude Code のログインが要る。利用者のデータは使わない):
//! `ANNOTATE_RECORD_OUT=tests/fixtures/annotate14_sonnet.json cargo test --test annotate -- --ignored --nocapture`

use openspeech::core::annotate::{
    agreement, note_problems, review, structure_from_answer, Agreement, Annotator, ClaudeCodeAnnotator,
    ANNOTATION_MODEL,
};
use openspeech::core::dictionary::Template;
use openspeech::core::proc;
use openspeech::core::structure::{check_structure, render, Context, TemplateStructure, Vocabulary};
use serde::Deserialize;
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Instant;

#[derive(Deserialize)]
struct TemplateFixture {
    templates: Vec<Template>,
}

#[derive(Deserialize)]
struct StructureFixture {
    structures: BTreeMap<String, TemplateStructure>,
}

#[derive(Deserialize)]
struct AnswerFixture {
    #[serde(default)]
    model: String,
    answers: BTreeMap<String, Value>,
}

fn fixture_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name)
}

fn templates() -> Vec<Template> {
    let text = std::fs::read_to_string(fixture_path("templates14.json")).unwrap();
    serde_json::from_str::<TemplateFixture>(&text).unwrap().templates
}

fn gold() -> BTreeMap<String, TemplateStructure> {
    let text = std::fs::read_to_string(fixture_path("structure14.json")).unwrap();
    serde_json::from_str::<StructureFixture>(&text).unwrap().structures
}

fn answers() -> AnswerFixture {
    let text = std::fs::read_to_string(fixture_path("annotate14_sonnet.json")).unwrap();
    serde_json::from_str(&text).unwrap()
}

/// 古い注 4 件(§16.2: 膀胱・門脈周囲浮腫・白質病変の引用・副鼻腔炎の引用)を機械が見つける。ほかの 10 件の
/// 注には何も言わない(誤検出なし)。手書きの構造の鍵で見る。
#[test]
fn the_machine_finds_exactly_the_four_old_notes() {
    let gold = gold();
    let mut found: Vec<String> = Vec::new();
    for template in templates() {
        found.extend(note_problems(&template, &gold[&template.keywords[0]]));
    }
    assert_eq!(
        found,
        vec![
            "腎・副腎・前立腺: 注の「脾・腎・副腎・膀胱の文」の「膀胱」が本文に無い",
            "肝: 注の「門脈周囲浮腫・肝内胆管拡張の否定」の「門脈周囲浮腫」「肝内胆管拡張」が本文に無い",
            "白質病変: 注の引用「頭蓋内に明らかな異常信号を認めず…」が本文に無い",
            "副鼻腔炎: 注の引用「明らかな粘膜肥厚や液体貯留は認められません」が本文に無い",
        ]
    );
}

/// 手書きの構造は S1 の検査を通るので、`review` の警告は古い注だけ。
#[test]
fn the_hand_structures_review_to_the_old_notes_only() {
    let gold = gold();
    let vocabulary = Vocabulary::builtin();
    let mut count = 0;
    for template in templates() {
        let structure = &gold[&template.keywords[0]];
        assert_eq!(check_structure(&template, structure, &vocabulary), Vec::<String>::new());
        let warnings = review(&template, structure, &vocabulary);
        assert!(warnings.iter().all(|w| w.contains("注の")), "{warnings:?}");
        count += warnings.len();
    }
    assert_eq!(count, 4);
}

/// 一致の数え方そのもの: 手書きどうしは 100%、鍵を 1 つ変えると 1 つ減り、変種を足すと分母が増える。
#[test]
fn agreement_counts_line_keys_and_variant_targets_and_modes() {
    let gold = gold();
    let mut total = Agreement::default();
    for structure in gold.values() {
        let same = agreement(structure, structure);
        assert_eq!(same.rate(), 1.0);
        total.add(&same);
    }
    assert_eq!(total.lines, 87, "14 件の本文の行");
    assert_eq!(total.variants, 66, "37 モジュールの変種(割った行・外す・印を含む)");
    let structure = &gold["腹部骨盤CT"];
    let mut changed = structure.clone();
    changed.lines[2].slot = Some("胆嚢".to_string());
    changed.variants[1].mode = openspeech::core::structure::Mode::Prepend;
    let mut extra = changed.variants[8].clone();
    extra.line = Some(5);
    changed.variants.push(extra);
    let result = agreement(structure, &changed);
    assert_eq!((result.lines_matched, result.lines), (8, 9));
    assert_eq!((result.variants_matched, result.variants), (19, 21), "置き方の違い 1 つ・余分 1 つ");
    assert_eq!(result.mismatches.len(), 3, "{:?}", result.mismatches);
}

/// Sonnet の実際の答え(録った物)を構造に組み、手書きの構造と比べる。行の鍵と変種の対象・置き方の一致が
/// 合わせて 90% 以上。どの答えも描画できる(L1: 選択なし・文脈なしで本文のまま)。古い注 4 件が `unresolved` に出る。
#[test]
fn recorded_sonnet_answers_agree_with_the_hand_structures() {
    let fixture = answers();
    let gold = gold();
    let vocabulary = Vocabulary::builtin();
    let mut total = Agreement::default();
    let mut unresolved: Vec<String> = Vec::new();
    println!("モデル: {}", fixture.model);
    println!("{:<16} {:>9} {:>9} {:>9}", "テンプレート", "行の鍵", "変種", "合計");
    for template in templates() {
        let name = &template.keywords[0];
        let answer = fixture.answers.get(name).unwrap_or_else(|| panic!("{name} の答えが無い"));
        let structure = structure_from_answer(&template, &vocabulary, answer, "").unwrap();
        let rendered = render(&template, &structure, &vocabulary, &Context::default(), &[])
            .unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(
            rendered.text,
            openspeech::core::structure::canonical_lines(&template.body).join("\n"),
            "{name}: L1"
        );
        let result = agreement(&gold[name], &structure);
        println!(
            "{:<16} {:>3}/{:<3}   {:>3}/{:<3}   {:>5.1}%",
            name,
            result.lines_matched,
            result.lines,
            result.variants_matched,
            result.variants,
            result.rate() * 100.0
        );
        total.add(&result);
        unresolved.extend(structure.unresolved.iter().map(|u| format!("{name}: {u}")));
    }
    println!(
        "合計: 行の鍵 {}/{} ({:.1}%)、変種 {}/{} ({:.1}%)、合わせて {}/{} ({:.1}%)",
        total.lines_matched,
        total.lines,
        total.line_rate() * 100.0,
        total.variants_matched,
        total.variants,
        total.variant_rate() * 100.0,
        total.matched(),
        total.total(),
        total.rate() * 100.0
    );
    println!(
        "参考(完了条件の外): 行の種類 {}/{}、極性 {}/{}、役割 {}/{}、外す臓器 {}/{}",
        total.kinds_matched,
        total.lines,
        total.polarities_matched,
        total.polarities,
        total.roles_matched,
        total.roles,
        total.organs_matched,
        total.organs
    );
    println!("不一致:");
    for mismatch in &total.mismatches {
        println!("  {mismatch}");
    }
    println!("unresolved:");
    for item in &unresolved {
        println!("  {item}");
    }
    assert!(total.rate() >= 0.90, "一致 {:.1}% < 90%", total.rate() * 100.0);
    for old in [
        "腹部骨盤CT: 腎・副腎・前立腺: 注の「脾・腎・副腎・膀胱の文」の「膀胱」が本文に無い",
        "腹部MRI: 肝: 注の「門脈周囲浮腫・肝内胆管拡張の否定」の「門脈周囲浮腫」「肝内胆管拡張」が本文に無い",
        "頭部MRI: 白質病変: 注の引用「頭蓋内に明らかな異常信号を認めず…」が本文に無い",
        "副鼻腔CT: 副鼻腔炎: 注の引用「明らかな粘膜肥厚や液体貯留は認められません」が本文に無い",
    ] {
        assert!(unresolved.iter().any(|u| u == old), "{old} が unresolved に無い");
    }
}

/// アプリか `--annotate-templates` が書いた `structure.json`(`ANNOTATE_MEASURE_FILE`)を、源泉のハッシュで
/// 14 件に引き当てて手書きの構造と比べ、一致を印字する。手で走らせる物(`--ignored`)。
#[test]
#[ignore]
fn measure_a_structure_file() {
    let Some(path) = std::env::var_os("ANNOTATE_MEASURE_FILE").map(PathBuf::from) else {
        println!("ANNOTATE_MEASURE_FILE が無いので測らない");
        return;
    };
    let file: openspeech::core::structure::StructureFile =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    let gold = gold();
    let mut total = Agreement::default();
    for template in templates() {
        let name = &template.keywords[0];
        let Some(structure) = file.structure_for(&template) else {
            println!("{name}: 構造が無い(未注釈)");
            continue;
        };
        let result = agreement(&gold[name], structure);
        println!(
            "{name}: 行の鍵 {}/{}、変種 {}/{}({:.1}%)、警告 {} 件",
            result.lines_matched,
            result.lines,
            result.variants_matched,
            result.variants,
            result.rate() * 100.0,
            structure.unresolved.len()
        );
        total.add(&result);
    }
    println!(
        "合計: 行の鍵 {}/{}、変種 {}/{}、合わせて {}/{} ({:.1}%)。参考: 行の種類 {}/{}、極性 {}/{}、役割 {}/{}、外す臓器 {}/{}",
        total.lines_matched,
        total.lines,
        total.variants_matched,
        total.variants,
        total.matched(),
        total.total(),
        total.rate() * 100.0,
        total.kinds_matched,
        total.lines,
        total.polarities_matched,
        total.polarities,
        total.roles_matched,
        total.roles,
        total.organs_matched,
        total.organs
    );
    for mismatch in &total.mismatches {
        println!("  {mismatch}");
    }
    println!("構造の数: {}(14 件のどれにも合わない項 {} 件)", file.templates.len(), {
        let hashes: Vec<String> = templates().iter().map(openspeech::core::structure::source_hash).collect();
        file.templates.keys().filter(|k| !hashes.contains(k)).count()
    });
}

/// 14 件を Claude Code(既定 Sonnet)で注釈し、答えをそのまま `ANNOTATE_RECORD_OUT` に書く。
/// `ANNOTATE_MODEL` でモデル、`ANNOTATE_ONLY` で 1 件に絞れる。手で走らせる物(`--ignored`)。
#[test]
#[ignore]
fn record_live_answers() {
    let Some(out) = std::env::var_os("ANNOTATE_RECORD_OUT").map(PathBuf::from) else {
        println!("ANNOTATE_RECORD_OUT が無いので録らない");
        return;
    };
    let model = std::env::var("ANNOTATE_MODEL").unwrap_or_else(|_| ANNOTATION_MODEL.to_string());
    let only = std::env::var("ANNOTATE_ONLY").ok();
    let binary = proc::find_claude().expect("claude が見つからない");
    let work = std::env::temp_dir().join(format!("openspeech-annotate-record-{}", std::process::id()));
    std::fs::create_dir_all(&work).unwrap();
    let annotator = Arc::new(ClaudeCodeAnnotator::new(&model, &binary, &work));
    let vocabulary = Arc::new(Vocabulary::builtin());
    let jobs: Vec<Template> = templates()
        .into_iter()
        .filter(|t| only.as_deref().is_none_or(|o| t.keywords[0] == o))
        .collect();
    let results: Arc<Mutex<BTreeMap<String, Value>>> = Arc::default();
    let queue = Arc::new(Mutex::new(jobs));
    let workers: Vec<_> = (0..4)
        .map(|_| {
            let (queue, results, annotator, vocabulary) =
                (queue.clone(), results.clone(), annotator.clone(), vocabulary.clone());
            std::thread::spawn(move || loop {
                let Some(template) = queue.lock().unwrap().pop() else { break };
                let began = Instant::now();
                let answer = annotator.answer(&template, &vocabulary);
                let name = template.keywords[0].clone();
                println!("{name}: {:.1}s {}", began.elapsed().as_secs_f32(), if answer.is_ok() { "ok" } else { "失敗" });
                match answer {
                    Ok(value) => {
                        results.lock().unwrap().insert(name, value);
                    }
                    Err(message) => println!("  {message}"),
                }
            })
        })
        .collect();
    for worker in workers {
        worker.join().unwrap();
    }
    let answers = results.lock().unwrap().clone();
    let file = serde_json::json!({
        "_note": "Claude Code(claude -p)の注釈役の答えをそのまま録った物。tests/annotate.rs の record_live_answers で作る。",
        "model": model,
        "recorded": openspeech::core::annotate::now_stamp(),
        "answers": answers,
    });
    std::fs::write(&out, serde_json::to_string_pretty(&file).unwrap() + "\n").unwrap();
    println!("{} 件を {} に書いた", answers.len(), out.display());
}
