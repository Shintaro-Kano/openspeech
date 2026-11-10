//! 単位 S1(docs/simplify-and-assistant-plan.md §16)の試験。LLM を使わず、14 件の源泉と手で付けた構造で
//! 法則 L1〜L7、§14.5・§14.6 の補完 9 例の再現、§16.14 の 8 依頼の候補、ハッシュの参照実装との一致、
//! Markdown の往復を確かめる。

use openspeech::core::dictionary::{Dictionary, Template};
use openspeech::core::judge::adds_laterality;
use openspeech::core::structure::{
    candidates, check_structure, match_template, render, source_hash, Context, Mode, RenderError, Selection,
    StructureFile, TemplateStructure, Variant, Vocabulary,
};
use openspeech::core::template_md::{parse, write, Annotations};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::PathBuf;

#[derive(Deserialize)]
struct TemplateFixture {
    templates: Vec<Template>,
}

#[derive(Deserialize)]
struct StructureFixture {
    structures: BTreeMap<String, TemplateStructure>,
}

fn fixture_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name)
}

fn templates() -> Vec<Template> {
    let text = std::fs::read_to_string(fixture_path("templates14.json")).unwrap();
    serde_json::from_str::<TemplateFixture>(&text).unwrap().templates
}

fn structures() -> BTreeMap<String, TemplateStructure> {
    let text = std::fs::read_to_string(fixture_path("structure14.json")).unwrap();
    serde_json::from_str::<StructureFixture>(&text).unwrap().structures
}

/// (源泉, 構造) の 14 組。
fn pairs() -> Vec<(Template, TemplateStructure)> {
    let structures = structures();
    templates()
        .into_iter()
        .map(|t| {
            let s = structures
                .get(&t.keywords[0])
                .unwrap_or_else(|| panic!("structure14.json に {} が無い", t.keywords[0]))
                .clone();
            (t, s)
        })
        .collect()
}

fn pair(name: &str) -> (Template, TemplateStructure) {
    pairs().into_iter().find(|(t, _)| t.keywords[0] == name).unwrap()
}

fn dictionary() -> Dictionary {
    Dictionary {
        templates: templates(),
        ..Default::default()
    }
}

fn vocabulary() -> Vocabulary {
    Vocabulary::builtin()
}

fn module_text<'a>(template: &'a Template, name: &str) -> &'a str {
    &template.modules.iter().find(|m| m.name == name).unwrap().text
}

fn module_line<'a>(template: &'a Template, name: &str, line: usize) -> &'a str {
    module_text(template, name).split('\n').nth(line).unwrap()
}

/// 変種をそのままの文で選ぶ選択(束なら行、束でなければモジュール全体)。
fn source_selection(template: &Template, variant: &Variant) -> Selection {
    let text = match variant.line {
        Some(line) => module_line(template, &variant.module, line).to_string(),
        None => module_text(template, &variant.module).to_string(),
    };
    Selection::variant(&variant.module, variant.line, &text)
}

/// 口述で選べる変種(全体差し替えの印と派生の外すは除く)。
fn selectable(structure: &TemplateStructure) -> Vec<&Variant> {
    structure.variants.iter().filter(|v| !v.is_whole() && !v.derived).collect()
}

fn render_text(template: &Template, structure: &TemplateStructure, context: &Context, selections: &[Selection]) -> String {
    render(template, structure, &vocabulary(), context, selections)
        .unwrap_or_else(|e| panic!("{}: {e}", template.keywords[0]))
        .text
}

fn is_subsequence(needle: &[&str], haystack: &[&str]) -> bool {
    let mut i = 0;
    for line in haystack {
        if i < needle.len() && needle[i] == *line {
            i += 1;
        }
    }
    i == needle.len()
}

fn ctx(sex: Option<&str>, age: Option<&str>, purposes: &[&str]) -> Context {
    Context {
        sex: sex.map(str::to_string),
        age_band: age.map(str::to_string),
        purposes: purposes.iter().map(|p| p.to_string()).collect(),
    }
}

// ------------------------------------------------------------------------------------------------
// 構造の検査とハッシュ
// ------------------------------------------------------------------------------------------------

#[test]
fn the_fourteen_structures_pass_the_checks() {
    let vocabulary = vocabulary();
    for (template, structure) in pairs() {
        let errors = check_structure(&template, &structure, &vocabulary);
        assert!(errors.is_empty(), "{}: {errors:#?}", template.keywords[0]);
    }
}

#[test]
fn the_structure_file_is_keyed_by_the_source_hash_and_prunes() {
    let mut file = StructureFile::default();
    let all = pairs();
    for (template, structure) in &all {
        file.insert(template, structure.clone());
    }
    assert_eq!(file.templates.len(), 14);
    let json = serde_json::to_string_pretty(&file).unwrap();
    let back: StructureFile = serde_json::from_str(&json).unwrap();
    assert_eq!(back, file);
    for (template, structure) in &all {
        assert_eq!(back.structure_for(template), Some(structure));
    }
    let mut edited = all[0].0.clone();
    edited.body.push_str("\n膀胱に明らかな異常を認めません。");
    assert!(file.structure_for(&edited).is_none(), "本文を直せば未注釈");
    let mut renamed = all[0].0.clone();
    renamed.keywords.push("別名".to_string());
    assert!(file.structure_for(&renamed).is_some(), "キーワードを直しても構造は残る");
    let kept: Vec<Template> = all.iter().skip(2).map(|(t, _)| t.clone()).collect();
    assert_eq!(file.prune(&kept), 2);
    assert_eq!(file.templates.len(), 12);
}

#[test]
fn source_hash_matches_the_python_reference() {
    let script = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("scripts/source_hash.py");
    let output = std::process::Command::new("python3")
        .arg("-I")
        .arg(&script)
        .arg(fixture_path("templates14.json"))
        .output();
    let Ok(output) = output else {
        eprintln!("python3 が無いので参照実装との突き合わせは飛ばす");
        return;
    };
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let text = String::from_utf8(output.stdout).unwrap();
    let reference: Vec<(&str, &str)> = text
        .lines()
        .map(|l| l.split_once(' ').unwrap())
        .collect();
    let templates = templates();
    assert_eq!(reference.len(), templates.len());
    for (template, (hash, name)) in templates.iter().zip(reference) {
        assert_eq!(template.keywords[0], name);
        assert_eq!(source_hash(template), hash, "{name}");
    }
}

// ------------------------------------------------------------------------------------------------
// 法則 L1〜L7
// ------------------------------------------------------------------------------------------------

#[test]
fn l1_no_selection_renders_the_body_unchanged() {
    for (template, structure) in pairs() {
        let out = render(&template, &structure, &vocabulary(), &Context::default(), &[]).unwrap();
        assert_eq!(out.text, template.body, "{}", template.keywords[0]);
        assert!(out.notes.is_empty(), "{}: {:?}", template.keywords[0], out.notes);
    }
}

#[test]
fn l2_a_selection_changes_only_its_own_slot() {
    for (template, structure) in pairs() {
        let body: Vec<&str> = template.body.split('\n').collect();
        for variant in selectable(&structure) {
            let selection = source_selection(&template, variant);
            let out = render_text(&template, &structure, &Context::default(), std::slice::from_ref(&selection));
            let out_lines: Vec<&str> = out.split('\n').collect();
            let untouched: Vec<&str> = structure
                .lines
                .iter()
                .filter(|l| l.slot.as_ref().is_none_or(|s| !variant.target.contains(s)))
                .map(|l| body[l.i])
                .collect();
            assert!(
                is_subsequence(&untouched, &out_lines),
                "{} {}:{:?}\n{out}",
                template.keywords[0],
                variant.module,
                variant.line
            );
            for line in selection.text.split('\n') {
                assert!(out_lines.contains(&line), "{} {}: 選んだ文が無い\n{out}", template.keywords[0], variant.module);
            }
            if matches!(variant.mode, Mode::Prepend | Mode::Append) {
                // 足す変種は既定文を残す(接頭語のある変種は続く既定文の文頭に語が付く)。
                let prefixed: Vec<String> = structure
                    .lines
                    .iter()
                    .map(|l| match &variant.prefix {
                        Some(prefix) if l.slot.as_ref().is_some_and(|s| variant.target.contains(s)) => format!("{prefix}{}", body[l.i]),
                        _ => body[l.i].to_string(),
                    })
                    .collect();
                let prefixed: Vec<&str> = prefixed.iter().map(String::as_str).collect();
                assert!(
                    is_subsequence(&prefixed, &out_lines),
                    "{} {}: 足す変種は既定文を残す\n{out}",
                    template.keywords[0],
                    variant.module
                );
            }
        }
    }
}

#[test]
fn l3_the_order_of_selections_does_not_matter() {
    let mut pairs_checked = 0;
    for (template, structure) in pairs() {
        let variants = selectable(&structure);
        for (a, va) in variants.iter().enumerate() {
            for vb in variants.iter().skip(a + 1) {
                let sa = source_selection(&template, va);
                let sb = source_selection(&template, vb);
                let ab = render_text(&template, &structure, &Context::default(), &[sa.clone(), sb.clone()]);
                let ba = render_text(&template, &structure, &Context::default(), &[sb, sa]);
                assert_eq!(ab, ba, "{} {}:{:?} と {}:{:?}", template.keywords[0], va.module, va.line, vb.module, vb.line);
                pairs_checked += 1;
            }
        }
    }
    assert!(pairs_checked >= 250, "{pairs_checked}");
}

#[test]
fn l4_the_same_selection_twice_is_placed_once() {
    for (template, structure) in pairs() {
        for variant in selectable(&structure) {
            let s = source_selection(&template, variant);
            let once = render_text(&template, &structure, &Context::default(), std::slice::from_ref(&s));
            let twice = render_text(&template, &structure, &Context::default(), &[s.clone(), s]);
            assert_eq!(once, twice, "{} {}", template.keywords[0], variant.module);
        }
    }
    // auto の変種が文脈で当たるとき、同じ変種を口述でも選んでいれば一度だけ。
    let (template, structure) = pair("頭部MRI");
    let vertigo = Selection::variant("めまいの異常なし", None, module_text(&template, "めまいの異常なし"));
    let auto_only = render_text(&template, &structure, &ctx(None, None, &["めまい"]), &[]);
    let both = render_text(&template, &structure, &ctx(None, None, &["めまい"]), &[vertigo]);
    assert_eq!(auto_only, both);
    assert_eq!(auto_only.matches("小脳橋角部").count(), 1);
}

#[test]
fn l5_rendering_adds_no_laterality_that_was_not_dictated() {
    for (template, structure) in pairs() {
        for variant in selectable(&structure) {
            let s = source_selection(&template, variant);
            let out = render_text(&template, &structure, &Context::default(), std::slice::from_ref(&s));
            let before = format!("{}\n{}", template.body, s.text);
            assert!(!adds_laterality(&before, &out), "{} {}\n{out}", template.keywords[0], variant.module);
        }
    }
}

#[test]
fn l6_whole_replacement_is_an_error_for_the_full_text_path() {
    let (template, structure) = pair("腰椎MRI");
    let whole = render(
        &template,
        &structure,
        &vocabulary(),
        &Context::default(),
        &[Selection::variant("変形性腰椎症", None, module_text(&template, "変形性腰椎症"))],
    );
    assert_eq!(whole, Err(RenderError::WholeReplacement("変形性腰椎症".to_string())));
    // 行に割っていない全体差し替えだけのモジュール。
    let mut unsplit = structure.clone();
    unsplit.variants.retain(|v| v.module != "変形性腰椎症" || v.is_whole());
    unsplit.groups.clear();
    let whole = render(
        &template,
        &unsplit,
        &vocabulary(),
        &Context::default(),
        &[Selection::variant("変形性腰椎症", Some(0), "x")],
    );
    assert_eq!(whole, Err(RenderError::WholeReplacement("変形性腰椎症".to_string())));
    let unknown = render(
        &template,
        &structure,
        &vocabulary(),
        &Context::default(),
        &[Selection::variant("無いモジュール", None, "x")],
    );
    assert!(matches!(unknown, Err(RenderError::Unplaceable(_))));
}

#[test]
fn l7_the_grade_selects_the_lines_whose_interval_contains_it() {
    let vocabulary = vocabulary();
    for name in ["腰椎MRI", "頸椎MRI"] {
        let (template, structure) = pair(name);
        let group = &structure.groups[0];
        let module = &group.module;
        let members: Vec<&Variant> = selectable(&structure).into_iter().filter(|v| &v.module == module).collect();
        let lines: Vec<&str> = module_text(&template, module).split('\n').collect();
        for grade in &vocabulary.default_scale {
            let allowed: Vec<usize> = members
                .iter()
                .filter(|v| openspeech::core::structure::interval_of(v, &vocabulary, &vocabulary.default_scale).contains(grade))
                .map(|v| v.line.unwrap())
                .collect();
            // すべての行を選んでも、程度で出ない行は落ちる。
            let all: Vec<Selection> = members
                .iter()
                .map(|v| Selection::graded(module, v.line.unwrap(), grade, lines[v.line.unwrap()]))
                .collect();
            let out = render(&template, &structure, &vocabulary, &Context::default(), &all).unwrap();
            for (i, line) in lines.iter().enumerate() {
                assert_eq!(out.text.contains(line), allowed.contains(&i), "{name} {grade} 行 {i}\n{}", out.text);
            }
            // 〔　〕の行だけ選んでも、程度で出る〔　〕の無い行は源泉の文のまま補われる。
            let filled: Vec<Selection> = members
                .iter()
                .filter(|v| lines[v.line.unwrap()].contains('〔'))
                .map(|v| Selection::graded(module, v.line.unwrap(), grade, &lines[v.line.unwrap()].replace('〔', "〔L4/5")))
                .collect();
            let out = render(&template, &structure, &vocabulary, &Context::default(), &filled).unwrap();
            for &i in &allowed {
                if !lines[i].contains('〔') {
                    assert!(out.text.contains(lines[i]), "{name} {grade} 行 {i} が補われていない\n{}", out.text);
                }
            }
            for (i, line) in lines.iter().enumerate() {
                if !allowed.contains(&i) {
                    assert!(!out.text.contains(line), "{name} {grade} 行 {i} は出ない筈\n{}", out.text);
                }
            }
        }
        // 既定表: 腰椎症は 軽度 で 0・2、中等度 で 0〜4、高度 で全部。
        if name == "腰椎MRI" {
            let intervals: Vec<Vec<String>> = members
                .iter()
                .map(|v| openspeech::core::structure::interval_of(v, &vocabulary, &vocabulary.default_scale))
                .collect();
            assert_eq!(intervals[0], ["軽度", "中等度", "高度"]);
            assert_eq!(intervals[1], ["中等度", "高度"]);
            assert_eq!(intervals[2], ["軽度", "中等度", "高度"]);
            assert_eq!(intervals[3], ["中等度", "高度"]);
            assert_eq!(intervals[4], ["中等度", "高度"]);
            assert_eq!(intervals[5], ["高度"]);
        }
    }
}

#[test]
fn structure_lints_catch_broken_groups() {
    let vocabulary = vocabulary();
    let (template, structure) = pair("腰椎MRI");
    fn line(s: &mut TemplateStructure, i: usize) -> &mut Variant {
        s.variants.iter_mut().find(|v| v.module == "変形性腰椎症" && v.line == Some(i)).unwrap()
    }
    let mut gap = structure.clone();
    line(&mut gap, 1).grades = Some(vec!["軽度".into(), "高度".into()]);
    assert!(check_structure(&template, &gap, &vocabulary).iter().any(|e| e.contains("連続")));
    let mut uncovered = structure.clone();
    for i in 0..6 {
        line(&mut uncovered, i).role = Some("障害".into());
    }
    assert!(check_structure(&template, &uncovered, &vocabulary).iter().any(|e| e.contains("軽度")));
    let mut conclusions = structure.clone();
    line(&mut conclusions, 3).role = Some("帰結".into());
    line(&mut conclusions, 4).role = Some("帰結".into());
    assert!(check_structure(&template, &conclusions, &vocabulary).iter().any(|e| e.contains("帰結")));
    let mut unknown_slot = structure.clone();
    unknown_slot.lines[0].slot = Some("無い鍵".into());
    assert!(check_structure(&template, &unknown_slot, &vocabulary).iter().any(|e| e.contains("語彙に無い")));
    let mut auto_abnormal = structure.clone();
    line(&mut auto_abnormal, 0).auto = true;
    line(&mut auto_abnormal, 0).condition = Some(openspeech::core::structure::Condition {
        purpose: Some("外傷".into()),
        ..Default::default()
    });
    assert!(check_structure(&template, &auto_abnormal, &vocabulary).iter().any(|e| e.contains("D4")));
}

// ------------------------------------------------------------------------------------------------
// §14.5・§14.6 の補完 9 例(選択を手で書き、描画が当時の結果と同じ文になる)
// ------------------------------------------------------------------------------------------------

#[test]
fn example_1_prostate_size_filled_category_1_kept() {
    let (template, structure) = pair("前立腺MRI");
    let filled = "前立腺のサイズは5.5×4.5×6.0cmと計測され、推定77mLで腫大がみられます。";
    let out = render_text(&template, &structure, &Context::default(), &[Selection::fill(0, filled)]);
    let mut expected: Vec<&str> = template.body.split('\n').collect();
    expected[0] = filled;
    assert_eq!(out, expected.join("\n"));
}

#[test]
fn example_2_prostate_category_5_replaces_the_cancer_line_only() {
    let (template, structure) = pair("前立腺MRI");
    let text = "左辺縁域に12mm程度の前立腺癌を疑う異常信号（T2WI低信号・DWI高信号・ADC低下）を認めます。\nカテゴリー5と判定されます（PI-RADS v2.1）。";
    let out = render_text(
        &template,
        &structure,
        &ctx(Some("男"), None, &[]),
        &[Selection::variant("PI-RADS カテゴリー4/5", None, text)],
    );
    let body: Vec<&str> = template.body.split('\n').collect();
    assert_eq!(out, [body[0], text, body[2], body[3]].join("\n"));
}

#[test]
fn example_3_emphysema_is_added_before_the_lung_line() {
    let (template, structure) = pair("胸部CT");
    let text = "背景肺には上肺優位の気腫性変化がみられます(Goddard 5点)。";
    let out = render_text(&template, &structure, &Context::default(), &[Selection::variant("肺気腫", Some(0), text)]);
    assert_eq!(out, format!("{text}\n{}", template.body));
}

#[test]
fn example_4_no_findings_leaves_head_ct_as_is() {
    let (template, structure) = pair("頭部CT");
    assert_eq!(render_text(&template, &structure, &Context::default(), &[]), template.body);
}

#[test]
fn example_5_lumbar_spondylosis_split_lines_rebuild_the_module_in_order() {
    let (template, structure) = pair("腰椎MRI");
    let m = "変形性腰椎症";
    let l3 = "これらにより、L3/4-L4/5にかけて脊柱管狭窄および椎間孔の圧排が連続してみられます。";
    let l4 = "脊柱管はL4/5レベルで最も強く狭窄しています。";
    let selections = [
        Selection::variant(m, Some(0), module_line(&template, m, 0)),
        Selection::variant(m, Some(1), module_line(&template, m, 1)),
        Selection::variant(m, Some(2), module_line(&template, m, 2)),
        Selection::variant(m, Some(3), l3),
        Selection::variant(m, Some(4), l4),
        Selection::variant(m, Some(5), module_line(&template, m, 5)),
    ];
    let out = render_text(&template, &structure, &Context::default(), &selections);
    let expected = [
        module_line(&template, m, 0),
        module_line(&template, m, 1),
        module_line(&template, m, 2),
        l3,
        l4,
        module_line(&template, m, 5),
    ]
    .join("\n");
    assert_eq!(out, expected, "程度を言わなければ選んだ行がそのまま源泉の順に並ぶ");
    // 程度 中等度 なら 障害 の行(5)は落ち、〔　〕のまま残らない。
    let graded: Vec<Selection> = selections
        .iter()
        .map(|s| Selection::graded(m, s.line.unwrap(), "中等度", &s.text))
        .collect();
    let out = render(&template, &structure, &vocabulary(), &Context::default(), &graded).unwrap();
    assert_eq!(out.text, expected.lines().take(5).collect::<Vec<_>>().join("\n"));
    assert!(out.notes.iter().any(|n| n.contains("5 行目") && n.contains("中等度")), "{:?}", out.notes);
    // 程度 軽度 なら 初期 の 2 行だけが置かれ、脊柱管・椎間孔の項目は触られないので既定文(狭窄なし)が残る。
    let light: Vec<Selection> = selections
        .iter()
        .map(|s| Selection::graded(m, s.line.unwrap(), "軽度", &s.text))
        .collect();
    let out = render_text(&template, &structure, &Context::default(), &light);
    let body: Vec<&str> = template.body.split('\n').collect();
    assert_eq!(out, [module_line(&template, m, 0), module_line(&template, m, 2), body[2]].join("\n"));
}

#[test]
fn example_6_sinusitis_left_and_septal_deviation_right() {
    let (template, structure) = pair("副鼻腔CT");
    let septum = "鼻中隔はやや右側へ偏位しています。";
    let sinus = "左上顎洞に粘膜肥厚〜粘液貯留に相当する軟部濃度域が広がっており、上顎洞炎と思われます。";
    // 副鼻腔炎は束でない 4 行(モジュール単位、§16.11)。炎症波及の 2 行目を省くのは Claude が文の中で行う。
    let sinusitis = [sinus, module_line(&template, "副鼻腔炎", 2), module_line(&template, "副鼻腔炎", 3)].join("\n");
    let out = render_text(
        &template,
        &structure,
        &Context::default(),
        &[Selection::variant("副鼻腔炎", None, &sinusitis), Selection::variant("鼻中隔偏位", Some(0), septum)],
    );
    let body: Vec<&str> = template.body.split('\n').collect();
    let expected = [septum, &sinusitis, body[2], body[3]].join("\n");
    assert_eq!(out, expected, "炎症波及の行(1)は使わず、貯留嚢胞は使わない");
    // 束でないモジュールに行番号を付けて返されても受ける(L6 の寛容さ)。
    let lenient = render_text(
        &template,
        &structure,
        &Context::default(),
        &[Selection::variant("副鼻腔炎", Some(0), &sinusitis), Selection::variant("鼻中隔偏位", Some(0), septum)],
    );
    assert_eq!(lenient, expected);
}

#[test]
fn example_7_ground_glass_nodule_replaces_the_lung_line() {
    let (template, structure) = pair("胸部CT");
    let text = "右上葉に8mm程度のすりガラス結節を認めます。\n肺腺癌（Adenocarcinoma in situなど非浸潤癌）あるいは前癌病変（Atypical adenomatous hyperplasia）などの可能性があります。";
    let out = render_text(&template, &structure, &Context::default(), &[Selection::variant("すりガラス結節", None, text)]);
    let body: Vec<&str> = template.body.split('\n').collect();
    assert_eq!(out, [text, body[1], body[2], body[3]].join("\n"));
}

#[test]
fn example_8_retention_cyst_alone_is_added_after_the_sinus_line() {
    let (template, structure) = pair("副鼻腔CT");
    let text = "右上顎洞に貯留嚢胞を認めます。";
    let out = render_text(&template, &structure, &Context::default(), &[Selection::variant("貯留嚢胞", Some(0), text)]);
    let body: Vec<&str> = template.body.split('\n').collect();
    assert_eq!(out, [body[0], body[1], text, body[2], body[3]].join("\n"));
}

#[test]
fn example_9_atrophy_and_desh_both_replace_the_morphology_line() {
    let (template, structure) = pair("頭部CT");
    let atrophy = module_line(&template, "脳萎縮", 0);
    let desh = module_line(&template, "水頭症", 0);
    let out = render_text(
        &template,
        &structure,
        &Context::default(),
        &[Selection::variant("水頭症", Some(0), desh), Selection::variant("脳萎縮", Some(0), atrophy)],
    );
    let body: Vec<&str> = template.body.split('\n').collect();
    assert_eq!(out, [body[0], atrophy, desh, body[2]].join("\n"), "源泉の順(脳萎縮が先)");
}

// ------------------------------------------------------------------------------------------------
// §16.3・§16.4 の文脈と列挙否定
// ------------------------------------------------------------------------------------------------

#[test]
fn the_abdominal_request_from_16_14_renders_with_organ_removal() {
    let (template, structure) = pair("腹部骨盤CT");
    let out = render_text(
        &template,
        &structure,
        &ctx(Some("男"), None, &[]),
        &[
            Selection::variant("腸管", Some(0), "結腸憩室が多発しています。"),
            Selection::variant("腎・副腎・前立腺", Some(3), "前立腺の腫大がみられます。"),
            Selection::variant("腎・副腎・前立腺", Some(0), "両腎嚢胞あり。"),
            Selection::variant("肝", Some(1), "肝嚢胞を認めます。"),
        ],
    );
    let body: Vec<&str> = template.body.split('\n').collect();
    let expected = [
        "肝嚢胞を認めます。",
        body[1],
        body[2],
        body[3],
        "脾、副腎に特記所見ありません。",
        "両腎嚢胞あり。",
        "前立腺の腫大がみられます。",
        "結腸憩室が多発しています。",
        body[6],
        body[7],
        body[8],
    ]
    .join("\n");
    assert_eq!(out, expected);
}

#[test]
fn two_organs_removed_from_one_list_line_keep_the_organ_order() {
    let (template, structure) = pair("腹部骨盤CT");
    let adrenal = "左副腎に12mm程度の低吸収な結節状構造がみられ、皮質腺腫を疑います。";
    let out = render_text(
        &template,
        &structure,
        &Context::default(),
        &[
            Selection::variant("腎・副腎・前立腺", Some(2), adrenal),
            Selection::variant("腎・副腎・前立腺", Some(1), "軽度の両腎萎縮あり。"),
            Selection::variant("腎・副腎・前立腺", Some(0), "両腎嚢胞あり。"),
        ],
    );
    let body: Vec<&str> = template.body.split('\n').collect();
    let expected = [
        body[0],
        body[1],
        body[2],
        body[3],
        "脾に特記所見ありません。",
        "両腎嚢胞あり。",
        "軽度の両腎萎縮あり。",
        adrenal,
        body[5],
        body[6],
        body[7],
        body[8],
    ]
    .join("\n");
    assert_eq!(out, expected);
}

#[test]
fn a_young_woman_without_findings_gets_the_short_form() {
    let (template, structure) = pair("腹部骨盤CT");
    let out = render_text(&template, &structure, &ctx(Some("女"), Some("若年"), &[]), &[]);
    let body: Vec<&str> = template.body.split('\n').collect();
    let expected = [
        "肝、胆、膵、脾、腎、副腎に明らかな異常を認めません。",
        "子宮および両付属器は年齢相応の所見と考えます。",
        body[5],
        body[6],
        "撮像範囲の肺に特記すべき異常みられません。",
        "胸水なし。",
    ]
    .join("\n");
    assert_eq!(out, expected, "総胆管・リンパ節の行は消え、腹水の行は既定文のまま(D3)");
    // 腹痛の精査なら虫垂の文、腹水が言われればその文。
    let out = render_text(
        &template,
        &structure,
        &ctx(Some("女"), Some("若年"), &["腹痛"]),
        &[Selection::variant("若い女性", Some(3), module_line(&template, "若い女性", 3))],
    );
    let expected = [
        "肝、胆、膵、脾、腎、副腎に明らかな異常を認めません。",
        "子宮および両付属器は年齢相応の所見と考えます。",
        "虫垂は正常です。その他、消化管に粗大な異常を認めません。",
        "生理的な少量の腹水を認めます。",
        "撮像範囲の肺に特記すべき異常みられません。",
        "胸水なし。",
    ]
    .join("\n");
    assert_eq!(out, expected);
    // 女でも若年でなければ T0 は当たらず、子宮の文だけ足される。
    let out = render_text(&template, &structure, &ctx(Some("女"), Some("成人"), &[]), &[]);
    assert_eq!(out, [body[0], body[1], body[2], body[3], body[4], "子宮および両付属器は年齢相応の所見と考えます。", body[5], body[6], body[7], body[8]].join("\n"));
    // 文脈が無ければ何も当たらない(L1)。
    assert_eq!(render_text(&template, &structure, &Context::default(), &[]), template.body);
}

#[test]
fn a_young_woman_with_findings_has_them_removed_from_the_list_negation() {
    let (template, structure) = pair("腹部骨盤CT");
    let out = render_text(
        &template,
        &structure,
        &ctx(Some("女"), Some("若年"), &[]),
        &[
            Selection::variant("腎・副腎・前立腺", Some(0), "両腎嚢胞あり。"),
            Selection::variant("肝", Some(0), "軽度の脂肪肝を認めます。"),
        ],
    );
    let body: Vec<&str> = template.body.split('\n').collect();
    let expected = [
        "胆、膵、脾、副腎に明らかな異常を認めません。",
        "軽度の脂肪肝を認めます。",
        "両腎嚢胞あり。",
        "子宮および両付属器は年齢相応の所見と考えます。",
        body[5],
        body[6],
        "撮像範囲の肺に特記すべき異常みられません。",
        "胸水なし。",
    ]
    .join("\n");
    assert_eq!(out, expected);
    // 胆嚢の所見は臓器「胆」を外す(胆嚢と総胆管の 2 項目が 1 つの臓器)。
    let out = render_text(
        &template,
        &structure,
        &ctx(Some("女"), Some("若年"), &[]),
        &[Selection::variant("胆", Some(0), "胆嚢結石あり。")],
    );
    assert!(out.starts_with("肝、膵、脾、腎、副腎に明らかな異常を認めません。\n胆嚢結石あり。\n"), "{out}");
    // 肺の所見が言われれば、肺の auto 差し替えは退く(§16.3 (i))が、胸水の足す鍵は残る。
    let out = render_text(
        &template,
        &structure,
        &ctx(Some("女"), Some("若年"), &[]),
        &[Selection::adhoc("肺(撮像範囲)", None, "右下葉に小結節を認めます。")],
    );
    assert!(out.ends_with("右下葉に小結節を認めます。\n胸水なし。"), "{out}");
    assert!(!out.contains("撮像範囲の肺"), "{out}");
}

#[test]
fn dictated_selections_are_dropped_only_when_the_context_contradicts_them() {
    let (template, structure) = pair("腹部骨盤CT");
    let prostate = Selection::variant("腎・副腎・前立腺", Some(3), "前立腺の腫大がみられます。");
    let unknown = render(&template, &structure, &vocabulary(), &Context::default(), std::slice::from_ref(&prostate)).unwrap();
    assert!(unknown.text.contains("前立腺の腫大"), "性別が不明なら置く");
    let woman = render(&template, &structure, &vocabulary(), &ctx(Some("女"), None, &[]), &[prostate]).unwrap();
    assert!(!woman.text.contains("前立腺の腫大"), "女なら退ける");
    assert!(woman.notes.iter().any(|n| n.contains("食い違う")), "{:?}", woman.notes);
}

#[test]
fn purpose_driven_negatives_are_added_where_their_notes_say() {
    let (template, structure) = pair("頭部MRI");
    let out = render_text(&template, &structure, &ctx(None, None, &["めまい"]), &[]);
    let body: Vec<&str> = template.body.split('\n').collect();
    let vertigo = module_text(&template, "めまいの異常なし");
    assert_eq!(out, [body[0], body[1], body[2], body[3], body[4], vertigo, body[5]].join("\n"));

    let (template, structure) = pair("頸椎MRI");
    let out = render_text(&template, &structure, &ctx(None, None, &["外傷"]), &[]);
    assert_eq!(out, format!("{}\n{}", module_text(&template, "外傷"), template.body));

    let (template, structure) = pair("腰椎MRI");
    let out = render_text(&template, &structure, &ctx(None, None, &["分離症"]), &[]);
    assert_eq!(out, format!("{}\n{}", module_text(&template, "腰椎分離症"), template.body));
    // 他の目的では当たらない。
    assert_eq!(render_text(&template, &structure, &ctx(None, None, &["外傷"]), &[]), template.body);
}

#[test]
fn a_prefix_variant_prepends_and_rewrites_the_head_of_the_default_line() {
    let (template, structure) = pair("頭部CT");
    let wm = module_line(&template, "白質病変", 0);
    let out = render_text(&template, &structure, &Context::default(), &[Selection::variant("白質病変", Some(0), wm)]);
    let body: Vec<&str> = template.body.split('\n').collect();
    assert_eq!(out, [wm, &format!("その他、{}", body[0]), body[1], body[2]].join("\n"));
}

#[test]
fn ctc_headings_and_blank_lines_survive_a_replacement() {
    let (template, structure) = pair("CTコロノグラフィ");
    let text = "下行〜S状結腸に憩室の散在を認めます。";
    let out = render_text(
        &template,
        &structure,
        &Context::default(),
        &[Selection::variant("憩室", Some(0), text), Selection::fill(1, "便潜血陽性の精査目的です。")],
    );
    let mut expected: Vec<&str> = template.body.split('\n').collect();
    expected[1] = "便潜血陽性の精査目的です。";
    expected[10] = text;
    assert_eq!(out, expected.join("\n"));
}

#[test]
fn adhoc_selections_need_a_known_slot_that_is_in_the_body() {
    let (template, structure) = pair("胸部CT");
    let out = render(
        &template,
        &structure,
        &vocabulary(),
        &Context::default(),
        &[
            Selection::adhoc("胸水", None, "右胸水を少量認めます。"),
            Selection::adhoc("肝", None, "肝嚢胞あり。"),
            Selection::adhoc("無い鍵", None, "x"),
        ],
    )
    .unwrap();
    let body: Vec<&str> = template.body.split('\n').collect();
    assert_eq!(out.text, [body[0], "右胸水を少量認めます。", body[2], body[3]].join("\n"));
    assert_eq!(out.notes.len(), 2, "{:?}", out.notes);
}

// ------------------------------------------------------------------------------------------------
// 照合と候補(§16.6・§16.14)
// ------------------------------------------------------------------------------------------------

#[test]
fn every_template_name_matches_with_and_without_the_suffix() {
    let dictionary = dictionary();
    for (index, template) in dictionary.templates.iter().enumerate() {
        for keyword in &template.keywords {
            assert_eq!(match_template(&dictionary, keyword), Some(index), "{keyword}");
            assert_eq!(match_template(&dictionary, &format!("{keyword}テンプレート")), Some(index), "{keyword}");
            assert_eq!(match_template(&dictionary, &format!("{keyword}のテンプレート")), Some(index), "{keyword}");
        }
    }
    assert_eq!(match_template(&dictionary, "膝MRI"), None);
}

#[test]
fn candidates_for_the_eight_requests_of_16_14() {
    let dictionary = dictionary();
    let vocabulary = vocabulary();
    let names = |request: &str| -> Vec<String> {
        candidates(request, &dictionary, &vocabulary)
            .into_iter()
            .map(|(i, _)| dictionary.templates[i].keywords[0].clone())
            .collect()
    };
    let cases: [(&str, &str); 7] = [
        ("肝嚢胞、腎嚢胞、前立腺の腫大、多発結腸憩室のみ", "腹部骨盤CT"),
        ("肝嚢胞、腎嚢胞、前立腺の腫大、多発結腸憩室", "腹部骨盤CT"),
        ("肝嚢胞と腎嚢胞のみ", "腹部骨盤CT"),
        ("肝嚢胞のみ", "腹部骨盤CT"),
        ("右上葉に8ミリのすりガラス結節のみ", "胸部CT"),
        ("子宮筋腫と少量腹水のみ", "女性骨盤MRI"),
        ("軽度の脂肪肝と左副腎に12ミリの腺腫疑い", "腹部骨盤CT"),
    ];
    for (request, chosen) in cases {
        let found = names(request);
        assert!(found.iter().any(|n| n == chosen), "{request}: {found:?}");
        // 肝嚢胞(と腎嚢胞)は CT と MRI が同点で並ぶので、先頭はどちらか(辞書の順)。ほかは選んだ物が先頭。
        if request.starts_with("肝嚢胞") && !request.contains("前立腺") {
            assert!(found[0] == "腹部骨盤CT" || found[0] == "腹部MRI", "{request}: {found:?}");
        } else {
            assert_eq!(found[0], chosen, "{request}: 点数順の先頭 {found:?}");
        }
    }
    for request in ["肝嚢胞と腎嚢胞のみ", "肝嚢胞のみ"] {
        let found = names(request);
        assert!(found.contains(&"腹部MRI".to_string()), "{request}: CT と MRI が並ぶ {found:?}");
        let scores: Vec<usize> = candidates(request, &dictionary, &vocabulary).iter().map(|(_, s)| *s).collect();
        assert_eq!(scores[0], scores[1], "{request}: CT と MRI は同点 {scores:?}");
    }
    assert!(names("異常なし").is_empty(), "一般語だけなら候補 0");
    assert!(names("軽度の変化").is_empty(), "程度語と一般語だけなら候補 0");
}

// ------------------------------------------------------------------------------------------------
// Markdown の往復(§16.15)
// ------------------------------------------------------------------------------------------------

#[test]
fn markdown_round_trip_keeps_source_hash_and_annotations_for_all_fourteen() {
    let vocabulary = vocabulary();
    for (template, structure) in pairs() {
        let annotations = Annotations::from_structure(&template, &structure, &vocabulary);
        let markdown = write(&template, &annotations, &vocabulary);
        let parsed = parse(&markdown, &vocabulary).unwrap_or_else(|e| panic!("{}: {e}\n{markdown}", template.keywords[0]));
        assert_eq!(parsed.template, template, "{}\n{markdown}", template.keywords[0]);
        assert_eq!(source_hash(&parsed.template), source_hash(&template));
        assert_eq!(parsed.annotations, annotations, "{}\n{markdown}", template.keywords[0]);
        // 注釈を構造に戻しても同じ構造(役割・区間・尺度・条件が保たれる)。
        let mut applied = structure.clone();
        parsed.annotations.apply(&mut applied, &vocabulary);
        assert_eq!(applied, structure, "{}", template.keywords[0]);
    }
}

#[test]
fn markdown_for_the_lumbar_template_carries_roles_and_a_scale() {
    let vocabulary = vocabulary();
    let (template, structure) = pair("腰椎MRI");
    let markdown = write(&template, &Annotations::from_structure(&template, &structure, &vocabulary), &vocabulary);
    assert!(markdown.contains("## 変形性腰椎症\n> 尺度: 軽度・中等度・高度\n腰椎の変形や骨棘形成を認めます。《初期》\n黄色靭帯の肥厚や、椎間関節の過形成を認めます。《進行》\n"), "{markdown}");
    assert!(markdown.contains("神経根周囲の脂肪織が消失しています。《障害》\n> 使い方: 正常所見全体を"), "{markdown}");
    assert!(markdown.contains("modules_extra:\n  変形性腰椎症:\n    diagnosis: 変形性腰椎症。腰椎椎間板変性症。腰部脊柱管狭窄症。\n"), "{markdown}");
    assert!(markdown.contains("## 腰椎分離症\n> 条件: 目的=分離症\n"), "{markdown}");
    assert!(markdown.starts_with("---\nkeywords: [腰椎MRI, 腰椎, lumbar MRI, L-spine MRI, LS MRI]\nmeta:\n  author: Shintaro Kano\n"), "{markdown}");
}

#[test]
fn explicit_intervals_override_roles_through_markdown() {
    let vocabulary = vocabulary();
    let (template, mut structure) = pair("腰椎MRI");
    let v = structure.variants.iter_mut().find(|v| v.module == "変形性腰椎症" && v.line == Some(3)).unwrap();
    v.grades = Some(vec!["高度".to_string()]);
    let annotations = Annotations::from_structure(&template, &structure, &vocabulary);
    let markdown = write(&template, &annotations, &vocabulary);
    assert!(markdown.contains("圧排が連続してみられます。《波及・高度》\n"), "{markdown}");
    let parsed = parse(&markdown, &vocabulary).unwrap();
    let mut applied = pair("腰椎MRI").1;
    parsed.annotations.apply(&mut applied, &vocabulary);
    assert_eq!(applied, structure);
    let out = render_text(
        &template,
        &structure,
        &Context::default(),
        &[Selection::graded("変形性腰椎症", 3, "中等度", module_line(&template, "変形性腰椎症", 3))],
    );
    assert!(!out.contains("圧排が連続"), "明示の区間 高度 が既定の 中等度以上 に勝つ\n{out}");
}

// ------------------------------------------------------------------------------------------------
// 四回目の独立 QA(§16.18)で見つかった穴の再現
// ------------------------------------------------------------------------------------------------

#[test]
fn qa4_h1_organ_removal_survives_a_whole_line_replacement_on_the_same_list_line() {
    let (template, structure) = pair("腹部骨盤CT");
    let out = render_text(
        &template,
        &structure,
        &Context::default(),
        &[
            Selection::adhoc("脾腎副腎", None, "脾腫あり。"),
            Selection::variant("腎・副腎・前立腺", Some(0), "両腎嚢胞あり。"),
        ],
    );
    let body: Vec<&str> = template.body.split('\n').collect();
    assert_eq!(out, [body[0], body[1], body[2], body[3], "脾腫あり。", "両腎嚢胞あり。", body[5], body[6], body[7], body[8]].join("\n"));
    // 同じ行への充填と差し替えも併置して知らせる(L4)。
    let (template, structure) = pair("CTコロノグラフィ");
    let out = render(
        &template,
        &structure,
        &vocabulary(),
        &Context::default(),
        &[Selection::fill(1, "便潜血陽性の精査目的です。"), Selection::adhoc("目的", None, "検診目的です。")],
    )
    .unwrap();
    assert!(out.text.contains("検診目的です。\n便潜血陽性の精査目的です。"), "{}", out.text);
    assert!(out.notes.iter().any(|n| n.contains("併置")), "{:?}", out.notes);
}

#[test]
fn qa4_m2_a_replacement_on_an_absorbed_line_moves_to_the_block() {
    let (template, structure) = pair("頸椎MRI");
    let straight = module_text(&template, "直線化・軽度変性");
    let hernia = "C5/6に椎間板ヘルニアを認めます。";
    let out = render_text(
        &template,
        &structure,
        &Context::default(),
        &[Selection::variant("直線化・軽度変性", None, straight), Selection::adhoc("椎間板", None, hernia)],
    );
    let body: Vec<&str> = template.body.split('\n').collect();
    assert_eq!(out, [straight, hernia, body[2], body[3]].join("\n"));
    let reversed = render_text(
        &template,
        &structure,
        &Context::default(),
        &[Selection::adhoc("椎間板", None, hernia), Selection::variant("直線化・軽度変性", None, straight)],
    );
    assert_eq!(out, reversed, "L3");
}

#[test]
fn qa4_m4_adhoc_selections_on_absorbed_lines_follow_the_source_order() {
    let (template, structure) = pair("腹部骨盤CT");
    let young = ctx(Some("女"), Some("若年"), &[]);
    let a = Selection::adhoc("総胆管", None, "総胆管が軽度拡張しています。");
    let b = Selection::adhoc("胆嚢", None, "胆嚢結石あり。");
    let ab = render_text(&template, &structure, &young, &[a.clone(), b.clone()]);
    let ba = render_text(&template, &structure, &young, &[b, a]);
    assert_eq!(ab, ba);
    assert!(ab.starts_with("肝、膵、脾、腎、副腎に明らかな異常を認めません。\n胆嚢結石あり。\n総胆管が軽度拡張しています。\n"), "{ab}");
    // 臓器だけ違う臨時は両方置かれる(L5)。
    let out = render_text(
        &template,
        &structure,
        &Context::default(),
        &[Selection::adhoc("脾腎副腎", Some("腎"), "両腎嚢胞あり。"), Selection::adhoc("脾腎副腎", Some("副腎"), "両腎嚢胞あり。")],
    );
    assert_eq!(out.matches("両腎嚢胞あり。").count(), 2, "{out}");
    assert!(out.contains("脾に特記所見ありません。"), "{out}");
}

#[test]
fn qa4_m5_a_structure_with_wrong_line_numbers_is_an_error_not_a_panic() {
    let (template, mut structure) = pair("腹部骨盤CT");
    structure.lines[0].i = 50;
    let out = render(
        &template,
        &structure,
        &vocabulary(),
        &Context::default(),
        &[Selection::variant("肝", Some(0), "x")],
    );
    assert!(matches!(out, Err(RenderError::Unplaceable(_))), "{out:?}");
    assert!(!check_structure(&template, &structure, &vocabulary()).is_empty());
}

#[test]
fn qa4_l7_trailing_blank_lines_and_crlf_in_the_body_render_like_the_hash() {
    let (template, structure) = pair("胸部CT");
    let mut variant = template.clone();
    variant.body = format!("{}\r\n\r\n", template.body.replace('\n', "\r\n"));
    assert_eq!(source_hash(&variant), source_hash(&template));
    let out = render_text(&variant, &structure, &Context::default(), &[Selection::variant("肺気腫", Some(0), "x")]);
    assert_eq!(out, format!("x\n{}", template.body));
}

#[test]
fn qa4_l1_more_structure_lints() {
    let vocabulary = vocabulary();
    let (template, structure) = pair("腹部骨盤CT");
    let errors = |s: &TemplateStructure| check_structure(&template, s, &vocabulary);
    let mut s = structure.clone();
    s.variants[0].organ_slots.remove("膵");
    assert!(errors(&s).iter().any(|e| e.contains("覆っていない")), "{:?}", errors(&s));
    let mut s = structure.clone();
    s.variants[8].polarity = Some("mixed".into());
    assert!(errors(&s).iter().any(|e| e.contains("mixed")));
    let mut s = structure.clone();
    s.variants[1].organ = Some("脾".into());
    assert!(errors(&s).iter().any(|e| e.contains("差し替えだけ")));
    let mut s = structure.clone();
    s.variants[6].line = Some(0);
    assert!(errors(&s).iter().any(|e| e.contains("外すに源泉の行")));
    let mut s = structure.clone();
    s.variants[1].slot = Some("腹水".into());
    assert!(errors(&s).iter().any(|e| e.contains("本文にある項目")));
    let mut s = structure.clone();
    s.variants.push(s.variants[8].clone());
    assert!(errors(&s).iter().any(|e| e.contains("変種が二つ")));
    let (template, structure) = pair("腰椎MRI");
    let mut s = structure.clone();
    s.groups[0].scale = Some(openspeech::core::structure::Scale::Named { name: "X".into(), levels: vec!["1".into(), "2".into(), "3".into()] });
    let errors = check_structure(&template, &s, &vocabulary);
    assert!(errors.iter().any(|e| e.contains("区間を明示")), "{errors:?}");
    let mut s = structure.clone();
    s.variants.retain(|v| !v.is_whole());
    assert!(check_structure(&template, &s, &vocabulary).iter().any(|e| e.contains("split")));
}
