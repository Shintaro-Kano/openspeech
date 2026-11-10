//! 単位 S3(docs/simplify-and-assistant-plan.md §16.6・§16.8)の試験。Claude の答えを固定データにして、ネットワーク
//! 無しで「依頼の下ごしらえ → 選択の読み替えと守り → 描画」を通す。23 例 = §14.5・§14.6 の既存 9 例 + 新 14 例
//! (文脈 2・三値 2・程度 2・§16.14 の検査名の無い 8 依頼。束 2 は 8 依頼の中の 2 本: 肝・腎・前立腺・腸管、
//! 脂肪肝と副腎腺腫)。源泉は `templates14.json`(実物と同一)、構造は手書きの正解 `structure14.json`。

use openspeech::core::assistant::{reply_from_value, Assistant, AssistantReply, AssistantRequest, Target};
use openspeech::core::completion::{prepare, resolve, run, Outcome, Plan, Resolution, Route};
use openspeech::core::dictionary::{Dictionary, Template};
use openspeech::core::structure::{StructureFile, TemplateStructure};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;
use std::sync::Mutex;

#[derive(Deserialize)]
struct TemplateFixture {
    templates: Vec<Template>,
}

#[derive(Deserialize)]
struct StructureFixture {
    structures: BTreeMap<String, TemplateStructure>,
}

fn fixture(name: &str) -> String {
    std::fs::read_to_string(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name)).unwrap()
}

fn templates() -> Vec<Template> {
    serde_json::from_str::<TemplateFixture>(&fixture("templates14.json")).unwrap().templates
}

/// 14 件の源泉と手書きの構造(源泉のハッシュで引ける形)。
fn plan() -> Plan {
    let templates = templates();
    let structures = serde_json::from_str::<StructureFixture>(&fixture("structure14.json")).unwrap().structures;
    let mut file = StructureFile::default();
    for template in &templates {
        file.insert(template, structures[&template.keywords[0]].clone());
    }
    Plan::new(Dictionary { templates, ..Dictionary::default() }, file)
}

fn body(name: &str) -> Vec<String> {
    let template = templates().into_iter().find(|t| t.keywords[0] == name).unwrap();
    template.body.split('\n').map(str::to_string).collect()
}

fn module_line(template: &str, module: &str, line: usize) -> String {
    let template = templates().into_iter().find(|t| t.keywords[0] == template).unwrap();
    template.modules.iter().find(|m| m.name == module).unwrap().text.split('\n').nth(line).unwrap().to_string()
}

fn module_text(template: &str, module: &str) -> String {
    let template = templates().into_iter().find(|t| t.keywords[0] == template).unwrap();
    template.modules.iter().find(|m| m.name == module).unwrap().text.clone()
}

/// 決まった答えを順に返す補完役。受けた依頼を覚える。
struct Fixed {
    replies: Mutex<VecDeque<Value>>,
    seen: Mutex<Vec<AssistantRequest>>,
}

impl Fixed {
    fn new(replies: Vec<Value>) -> Self {
        Fixed { replies: Mutex::new(replies.into()), seen: Mutex::new(Vec::new()) }
    }
}

impl Assistant for Fixed {
    fn name(&self) -> String {
        "固定の答え".to_string()
    }

    fn complete(&self, request: &AssistantRequest) -> Result<AssistantReply, String> {
        self.seen.lock().unwrap().push(request.clone());
        let value = self.replies.lock().unwrap().pop_front().expect("答えが足りない");
        reply_from_value(&value)
    }
}

/// Claude の答えの形(構造の経路)。選択は (module, line, grade, organ, text)。
fn answer(template: &str, selections: Vec<Value>) -> Value {
    json!({"kind": "template", "template": template, "text": null, "selections": selections, "reply": null, "replace": false})
}

fn module(name: &str, line: Option<usize>, text: &str) -> Value {
    json!({"module": name, "line": line, "grade": null, "organ": null, "text": text})
}

fn graded(name: &str, line: usize, grade: &str, text: &str) -> Value {
    json!({"module": name, "line": line, "grade": grade, "organ": null, "text": text})
}

fn body_line(line: usize, text: &str) -> Value {
    json!({"module": null, "line": line, "grade": null, "organ": null, "text": text})
}

fn complete(request: &str, target: Option<Target>, replies: Vec<Value>) -> (Outcome, Fixed) {
    let plan = plan();
    let prepared = prepare(AssistantRequest { request: request.to_string(), target, ..AssistantRequest::default() }, None, &plan);
    let fixed = Fixed::new(replies);
    let outcome = run(&fixed, &prepared, &plan).unwrap();
    (outcome, fixed)
}

/// 23 例の共通の確かめ: 構造の経路で描け、落ちた(退けた・全文経路に逃げた)数が 0 で、本文が期待どおり、Claude は 1 回。
fn assert_rendered(case: &str, outcome: &Outcome, expected: &[String]) {
    assert_eq!(outcome.route, Route::Rendered, "{case}: {:?}", outcome.route);
    assert_eq!(outcome.failures(), 0, "{case}: {:?}", outcome.dropped);
    assert_eq!(outcome.calls, 1, "{case}");
    assert_eq!(outcome.reply.text.as_deref(), Some(expected.join("\n").as_str()), "{case}\n{:?}", outcome.notes);
}

fn s(text: &str) -> String {
    text.to_string()
}

// ------------------------------------------------------------------------------------------------
// §14.5・§14.6 の既存 9 例
// ------------------------------------------------------------------------------------------------

#[test]
fn e1_prostate_size_is_filled_and_category_1_is_kept() {
    let filled = "前立腺のサイズは5.5×4.5×6.0cmと計測され、推定約77mL(0.52×5.5×4.5×6.0)で腫大がみられます。";
    let (outcome, fixed) = complete(
        "前立腺MRIのテンプレートを呼び出して、サイズは5.5かける4.5かける6センチメートル、PI-RADSカテゴリーは1",
        None,
        vec![answer("前立腺MRI", vec![body_line(0, filled)])],
    );
    let b = body("前立腺MRI");
    assert_rendered("e1", &outcome, &[s(filled), b[1].clone(), b[2].clone(), b[3].clone()]);
    let sent = &fixed.seen.lock().unwrap()[0];
    assert!(sent.candidates.is_empty() && sent.context.is_empty(), "検査名があれば候補を添えない");
}

#[test]
fn e2_prostate_category_5_replaces_the_cancer_line() {
    let filled = "前立腺のサイズは5.5×4.5×6.0cmと計測され、推定約77mL(0.52×5.5×4.5×6.0)で腫大がみられます。";
    let cancer = "左辺縁域に12mm程度の前立腺癌を疑う異常信号（T2WI低信号・DWI高信号・ADC低下）を認めます。\nカテゴリー5と判定されます（PI-RADS v2.1）。";
    let (outcome, _) = complete(
        "前立腺MRIのテンプレートを呼び出して、サイズは5.5かける4.5かける6センチメートル、PI-RADSカテゴリーは5で前立腺癌疑い、左辺縁域に12ミリ",
        None,
        vec![answer("前立腺MRI", vec![body_line(0, filled), module("PI-RADS カテゴリー4/5", None, cancer)])],
    );
    let b = body("前立腺MRI");
    assert_rendered("e2", &outcome, &[s(filled), s(cancer), b[2].clone(), b[3].clone()]);
}

#[test]
fn e3_emphysema_is_added_before_the_lung_line() {
    let text = "背景肺には上肺優位の気腫性変化がみられます(Goddard 5点)。";
    let (outcome, _) = complete("胸部CTのテンプレートで、肺気腫あり、ゴダード5点", None, vec![answer("胸部CT", vec![module("肺気腫", None, text)])]);
    let mut expected = vec![s(text)];
    expected.extend(body("胸部CT"));
    assert_rendered("e3", &outcome, &expected);
}

#[test]
fn e4_head_ct_without_findings_is_the_body() {
    let (outcome, _) = complete("頭部CTのテンプレート、異常なし", None, vec![answer("頭部CT", vec![])]);
    assert_rendered("e4", &outcome, &body("頭部CT"));
}

#[test]
fn e5_lumbar_spondylosis_fills_the_levels_and_keeps_the_rest() {
    let m = "変形性腰椎症";
    let l3 = "これらにより、L3/4-L4/5にかけて脊柱管狭窄および椎間孔の圧排が連続してみられます。";
    let l4 = "脊柱管はL4/5レベルで最も強く狭窄しています。";
    let t = "腰椎MRI";
    let (outcome, fixed) = complete(
        "腰椎MRIテンプレート、変形性腰椎症でL4/5が最も強い狭窄、L3/4も比較的強い",
        None,
        vec![answer(
            t,
            vec![
                module(m, Some(0), &module_line(t, m, 0)),
                module(m, Some(1), &module_line(t, m, 1)),
                module(m, Some(2), &module_line(t, m, 2)),
                module(m, Some(3), l3),
                module(m, Some(4), l4),
                module(m, Some(5), &module_line(t, m, 5)),
            ],
        )],
    );
    assert!(fixed.seen.lock().unwrap()[0].grades.is_empty(), "「最も強い」「比較的強い」は程度ではない");
    assert_rendered(
        "e5",
        &outcome,
        &[module_line(t, m, 0), module_line(t, m, 1), module_line(t, m, 2), s(l3), s(l4), module_line(t, m, 5)],
    );
}

#[test]
fn e6_left_maxillary_sinusitis_and_septal_deviation_to_the_right() {
    let septum = "鼻中隔はやや右側へ偏位しています。";
    let sinus = [
        "左上顎洞に粘膜肥厚〜粘液貯留に相当する軟部濃度域が広がっており、上顎洞炎と思われます。".to_string(),
        module_line("副鼻腔CT", "副鼻腔炎", 2),
        module_line("副鼻腔CT", "副鼻腔炎", 3),
    ]
    .join("\n");
    let (outcome, _) = complete(
        "副鼻腔CTで左上顎洞炎、鼻中隔は右に偏位",
        None,
        vec![answer("副鼻腔CT", vec![module("鼻中隔偏位", None, septum), module("副鼻腔炎", None, &sinus)])],
    );
    let b = body("副鼻腔CT");
    assert_rendered("e6", &outcome, &[s(septum), sinus, b[2].clone(), b[3].clone()]);
}

#[test]
fn e7_ground_glass_nodule_replaces_the_lung_line() {
    let text = "右上葉に8mm程度のすりガラス結節を認めます。\n肺腺癌（Adenocarcinoma in situなど非浸潤癌）あるいは前癌病変（Atypical adenomatous hyperplasia）などの可能性があります。";
    let (outcome, _) = complete(
        "胸部CTのテンプレートで、右上葉に8ミリのすりガラス結節",
        None,
        vec![answer("胸部CT", vec![module("すりガラス結節", None, text)])],
    );
    let b = body("胸部CT");
    assert_rendered("e7", &outcome, &[s(text), b[1].clone(), b[2].clone(), b[3].clone()]);
}

#[test]
fn e8_retention_cyst_alone_is_added_after_the_sinus_line() {
    let text = "右上顎洞に貯留嚢胞を認めます。";
    let (outcome, _) = complete("副鼻腔CTで右上顎洞に貯留嚢胞", None, vec![answer("副鼻腔CT", vec![module("貯留嚢胞", None, text)])]);
    let b = body("副鼻腔CT");
    assert_rendered("e8", &outcome, &[b[0].clone(), b[1].clone(), s(text), b[2].clone(), b[3].clone()]);
}

#[test]
fn e9_atrophy_and_desh_both_replace_the_morphology_line() {
    let atrophy = module_text("頭部CT", "脳萎縮");
    let desh = module_text("頭部CT", "水頭症");
    let (outcome, _) = complete(
        "頭部CTのテンプレートで、脳萎縮とDESHの所見",
        None,
        vec![answer("頭部CT", vec![module("水頭症", None, &desh), module("脳萎縮", None, &atrophy)])],
    );
    let b = body("頭部CT");
    assert_rendered("e9", &outcome, &[b[0].clone(), atrophy, desh, b[2].clone()]);
}

// ------------------------------------------------------------------------------------------------
// 新 14 例: 文脈 2・三値 2・程度 2・§16.14 の検査名の無い 8 依頼
// ------------------------------------------------------------------------------------------------

const T0: &str = "肝、胆、膵、脾、腎、副腎に明らかな異常を認めません。";
const T1: &str = "子宮および両付属器は年齢相応の所見と考えます。";
const T4: &str = "撮像範囲の肺に特記すべき異常みられません。";
const T5: &str = "胸水なし。";

#[test]
fn n1_context_a_young_woman_without_findings_gets_the_short_form() {
    let (outcome, fixed) = complete("若い女性、腹部骨盤CTテンプレート、異常なし", None, vec![answer("腹部骨盤CT", vec![])]);
    assert_eq!(fixed.seen.lock().unwrap()[0].context, "女性・若年");
    let b = body("腹部骨盤CT");
    assert_rendered("n1", &outcome, &[s(T0), s(T1), b[5].clone(), b[6].clone(), s(T4), s(T5)]);
}

#[test]
fn n2_context_a_vertigo_workup_adds_the_inner_ear_lines() {
    let (outcome, fixed) = complete("めまいの精査、頭部MRIテンプレートで異常なし", None, vec![answer("頭部MRI", vec![])]);
    assert_eq!(fixed.seen.lock().unwrap()[0].context, "目的=めまい");
    let b = body("頭部MRI");
    let mut expected: Vec<String> = b[..5].to_vec();
    expected.push(module_text("頭部MRI", "めまいの異常なし"));
    expected.push(b[5].clone());
    assert_rendered("n2", &outcome, &expected);
}

#[test]
fn n3_three_valued_a_young_woman_with_a_little_ascites_reads_physiological() {
    let (outcome, _) = complete(
        "若い女性、腹部骨盤CTテンプレート、少量の腹水",
        None,
        vec![answer("腹部骨盤CT", vec![module("若い女性", Some(3), "生理的な少量の腹水を認めます。")])],
    );
    let b = body("腹部骨盤CT");
    assert_rendered("n3", &outcome, &[s(T0), s(T1), b[5].clone(), s("生理的な少量の腹水を認めます。"), s(T4), s(T5)]);
}

#[test]
fn n4_three_valued_an_adult_man_with_a_little_ascites_reads_abnormal() {
    let (outcome, fixed) = complete(
        "成人男性、腹部骨盤CTテンプレート、少量の腹水",
        None,
        vec![answer("腹部骨盤CT", vec![body_line(6, "少量の腹水を認めます。")])],
    );
    assert_eq!(fixed.seen.lock().unwrap()[0].context, "男性・成人");
    let mut expected = body("腹部骨盤CT");
    expected[6] = s("少量の腹水を認めます。");
    assert_rendered("n4", &outcome, &expected);
    assert_eq!(outcome.kept[0].slot.as_deref(), Some("腹水"), "〔　〕の無い行の直しは臨時の充填");
}

#[test]
fn n5_grade_moderate_lumbar_spondylosis_drops_the_severe_line() {
    let m = "変形性腰椎症";
    let t = "腰椎MRI";
    let l3 = "これらにより、L3/4-L5/S1にかけて脊柱管狭窄および椎間孔の圧排が連続してみられます。";
    let l4 = "脊柱管はL4/5レベルで最も強く狭窄しています。";
    let (outcome, fixed) = complete(
        "腰椎MRIテンプレート、中等度の変形性腰椎症、L3/4からL5/S1にかけて狭窄、L4/5で最も強い",
        None,
        vec![answer(
            t,
            vec![
                graded(m, 0, "中等度", &module_line(t, m, 0)),
                graded(m, 1, "中等度", &module_line(t, m, 1)),
                graded(m, 2, "中等度", &module_line(t, m, 2)),
                graded(m, 3, "中等度", l3),
                graded(m, 4, "中等度", l4),
                graded(m, 5, "中等度", &module_line(t, m, 5)),
            ],
        )],
    );
    assert_eq!(fixed.seen.lock().unwrap()[0].grades, ["中等度"]);
    assert_rendered("n5", &outcome, &[module_line(t, m, 0), module_line(t, m, 1), module_line(t, m, 2), s(l3), s(l4)]);
    assert!(outcome.warnings.iter().any(|n| n.contains("5 行目") && n.contains("中等度")), "L7: 区間の外の行は捨てて知らせる {:?}", outcome.warnings);
}

#[test]
fn n6_grade_mild_cervical_spondylosis_keeps_the_initial_lines_and_supplies_the_negative() {
    let m = "変形性頸椎症";
    let t = "頸椎MRI";
    let (outcome, _) = complete(
        "頸椎MRIテンプレート、軽度の変形性頸椎症",
        None,
        vec![answer(t, vec![graded(m, 0, "軽度", &module_line(t, m, 0)), graded(m, 2, "軽度", &module_line(t, m, 2))])],
    );
    let b = body(t);
    assert_rendered("n6", &outcome, &[module_line(t, m, 0), module_line(t, m, 2), b[2].clone(), module_line(t, m, 6)]);
    assert!(outcome.notes.iter().any(|n| n.contains("6 行目を源泉の文のまま補いました")), "{:?}", outcome.notes);
}

/// §16.14 の 8 依頼で使う腹部骨盤CT の答え(肝嚢胞・両腎嚢胞・前立腺の腫大・多発結腸憩室)。
fn four_findings() -> Value {
    answer(
        "腹部骨盤CT",
        vec![
            module("肝", Some(1), "肝嚢胞を認めます。"),
            module("腎・副腎・前立腺", Some(0), "両腎嚢胞あり。"),
            module("腎・副腎・前立腺", Some(3), "前立腺の腫大がみられます。"),
            module("腸管", Some(0), "結腸憩室が多発しています。"),
        ],
    )
}

fn four_findings_text() -> Vec<String> {
    let b = body("腹部骨盤CT");
    vec![
        s("肝嚢胞を認めます。"),
        b[1].clone(),
        b[2].clone(),
        b[3].clone(),
        s("脾、副腎に特記所見ありません。"),
        s("両腎嚢胞あり。"),
        s("前立腺の腫大がみられます。"),
        s("結腸憩室が多発しています。"),
        b[6].clone(),
        b[7].clone(),
        b[8].clone(),
    ]
}

#[test]
fn n7_no_exam_name_four_findings_only() {
    let (outcome, fixed) = complete("肝嚢胞、腎嚢胞、前立腺の腫大、多発結腸憩室のみ", None, vec![four_findings()]);
    let sent = &fixed.seen.lock().unwrap()[0];
    assert!(sent.candidates.starts_with("(テンプレートの補完なら)腹部骨盤CT"), "{}", sent.candidates);
    assert_rendered("n7", &outcome, &four_findings_text());
    assert!(!outcome.off_candidates);
}

#[test]
fn n8_no_exam_name_four_findings_without_only() {
    let (outcome, _) = complete("肝嚢胞、腎嚢胞、前立腺の腫大、多発結腸憩室", None, vec![four_findings()]);
    assert_rendered("n8", &outcome, &four_findings_text());
}

#[test]
fn n9_no_exam_name_hepatic_and_renal_cysts_only() {
    let (outcome, fixed) = complete(
        "肝嚢胞と腎嚢胞のみ",
        None,
        vec![answer("腹部骨盤CT", vec![module("肝", Some(1), "肝嚢胞を認めます。"), module("腎・副腎・前立腺", Some(0), "両腎嚢胞あり。")])],
    );
    let b = body("腹部骨盤CT");
    let expected = [
        s("肝嚢胞を認めます。"),
        b[1].clone(),
        b[2].clone(),
        b[3].clone(),
        s("脾、副腎に特記所見ありません。"),
        s("両腎嚢胞あり。"),
        b[5].clone(),
        b[6].clone(),
        b[7].clone(),
        b[8].clone(),
    ];
    assert_rendered("n9", &outcome, &expected);
    assert!(fixed.seen.lock().unwrap()[0].candidates.contains("腹部MRI"), "CT と MRI が並ぶ");
    assert!(outcome.other_candidates.contains(&s("腹部MRI")), "知らせに他の候補 {:?}", outcome.other_candidates);
}

#[test]
fn n10_no_exam_name_hepatic_cyst_only() {
    let (outcome, _) = complete("肝嚢胞のみ", None, vec![answer("腹部骨盤CT", vec![module("肝", Some(1), "肝嚢胞を認めます。")])]);
    let mut expected = body("腹部骨盤CT");
    expected[0] = s("肝嚢胞を認めます。");
    assert_rendered("n10", &outcome, &expected);
}

#[test]
fn n11_no_exam_name_ground_glass_nodule_only() {
    let text = "右上葉に8mm程度のすりガラス結節を認めます。\n肺腺癌（Adenocarcinoma in situなど非浸潤癌）あるいは前癌病変（Atypical adenomatous hyperplasia）などの可能性があります。";
    let (outcome, fixed) = complete("右上葉に8ミリのすりガラス結節のみ", None, vec![answer("胸部CT", vec![module("すりガラス結節", None, text)])]);
    assert!(fixed.seen.lock().unwrap()[0].candidates.contains("胸部CT"));
    let b = body("胸部CT");
    assert_rendered("n11", &outcome, &[s(text), b[1].clone(), b[2].clone(), b[3].clone()]);
}

#[test]
fn n12_no_exam_name_myoma_and_a_little_ascites() {
    let myoma = module_text("女性骨盤MRI", "子宮筋腫");
    let (outcome, _) = complete(
        "子宮筋腫と少量腹水のみ",
        None,
        vec![answer("女性骨盤MRI", vec![module("子宮筋腫", None, &myoma), module("腹水", None, "少量の腹水を認めます。")])],
    );
    let b = body("女性骨盤MRI");
    let mut expected = vec![myoma];
    expected.extend(b[1..5].iter().cloned());
    expected.push(s("少量の腹水を認めます。"));
    expected.push(b[6].clone());
    assert_rendered("n12", &outcome, &expected);
}

#[test]
fn n13_no_exam_name_mild_fatty_liver_and_a_left_adrenal_adenoma() {
    let adrenal = "左副腎に12mm程度の低吸収な結節状構造がみられ、皮質腺腫を疑います。";
    let (outcome, fixed) = complete(
        "軽度の脂肪肝と左副腎に12ミリの腺腫疑い",
        None,
        vec![answer(
            "腹部骨盤CT",
            vec![graded("肝", 0, "軽度", "軽度の脂肪肝を認めます。"), module("腎・副腎・前立腺", Some(2), adrenal)],
        )],
    );
    assert_eq!(fixed.seen.lock().unwrap()[0].grades, ["軽度"]);
    let b = body("腹部骨盤CT");
    let expected = [
        s("軽度の脂肪肝を認めます。"),
        b[1].clone(),
        b[2].clone(),
        b[3].clone(),
        s("脾、腎に特記所見ありません。"),
        s(adrenal),
        b[5].clone(),
        b[6].clone(),
        b[7].clone(),
        b[8].clone(),
    ];
    assert_rendered("n13", &outcome, &expected);
    assert!(outcome.notes.iter().any(|n| n.contains("尺度は無い")), "尺度の無い所見の程度は見ない {:?}", outcome.notes);
}

#[test]
fn n14_no_exam_name_with_an_mri_in_the_editor_picks_abdominal_mri() {
    let target = Target { text: "腹部単純MRI".to_string(), selected: false, truncated: false };
    let (outcome, fixed) = complete(
        "肝嚢胞と腎嚢胞のみ",
        Some(target),
        vec![answer("腹部MRI", vec![module("肝", None, "肝嚢胞あり。"), module("腎・副腎", Some(0), "両腎嚢胞あり。")])],
    );
    assert_eq!(fixed.seen.lock().unwrap()[0].candidates, "(テンプレートの補完なら)腹部MRI(エディターの文章から)");
    let b = body("腹部MRI");
    let expected = [
        b[0].clone(),
        b[1].clone(),
        s("肝嚢胞あり。"),
        b[3].clone(),
        b[4].clone(),
        s("脾、副腎に明らかな異常を認めません。"),
        s("両腎嚢胞あり。"),
        b[6].clone(),
    ];
    assert_rendered("n14", &outcome, &expected);
}

// ------------------------------------------------------------------------------------------------
// 守り(§16.6)と経路の切り替え
// ------------------------------------------------------------------------------------------------

#[test]
fn a_selection_contradicting_the_context_is_dropped_and_counted() {
    let (outcome, _) = complete(
        "女性、腹部骨盤CTテンプレート、前立腺の腫大",
        None,
        vec![answer("腹部骨盤CT", vec![module("腎・副腎・前立腺", Some(3), "前立腺の腫大がみられます。")])],
    );
    assert_eq!(outcome.route, Route::Rendered);
    assert_eq!(outcome.failures(), 1);
    assert!(outcome.dropped[0].contains("食い違う"), "{:?}", outcome.dropped);
    assert!(!outcome.reply.text.as_deref().unwrap().contains("前立腺"));
}

#[test]
fn a_bundle_line_whose_nouns_were_not_dictated_is_dropped() {
    let (outcome, _) = complete(
        "腹部骨盤CTテンプレート、軽度の脂肪肝",
        None,
        vec![answer("腹部骨盤CT", vec![module("肝", Some(1), "肝嚢胞を認めます。")])],
    );
    assert_eq!(outcome.failures(), 1);
    assert!(outcome.dropped[0].contains("取り違え"), "{:?}", outcome.dropped);
    assert_eq!(outcome.reply.text.as_deref().unwrap(), body("腹部骨盤CT").join("\n"));
}

fn full(template: &str, text: &str) -> Value {
    json!({"kind": "template", "template": template, "text": text, "selections": [], "reply": null, "replace": false})
}

/// 左右の門・長さの門に掛かった選択は、黙って落とさず全文で答え直させる(QA H4: 落とすと陰性の既定文が残り、
/// 本文が口述と逆になる)。
#[test]
fn a_side_that_was_not_dictated_falls_back_to_the_full_text() {
    let (outcome, fixed) = complete(
        "胸部CTのテンプレートで、右上葉に8ミリのすりガラス結節",
        None,
        vec![
            answer("胸部CT", vec![module("すりガラス結節", None, "左上葉に8mm程度のすりガラス結節を認めます。")]),
            full("胸部CT", "全文"),
        ],
    );
    assert!(matches!(&outcome.route, Route::Fallback(why) if why.contains("左右")), "{:?}", outcome.route);
    assert_eq!(outcome.calls, 2);
    assert_eq!(fixed.seen.lock().unwrap()[1].full_text.as_deref(), Some("胸部CT"));
}

#[test]
fn a_too_long_selection_falls_back_to_the_full_text() {
    let long = "肝嚢胞を認めます。".repeat(30);
    let (outcome, _) = complete(
        "腹部骨盤CTテンプレート、肝嚢胞",
        None,
        vec![answer("腹部骨盤CT", vec![module("肝", Some(1), &long)]), full("腹部骨盤CT", "全文")],
    );
    assert!(matches!(&outcome.route, Route::Fallback(why) if why.contains("長すぎる")), "{:?}", outcome.route);
}

/// S1 で決めた意味(§16.3): Claude が返した選択は自動の変種でも口述。源泉の文のまま選んだ自動の行で条件が文脈に
/// 合う物は自動と同じ(落ちたと数えない)。言われていない束の自動の行(虫垂。目的=腹痛でない)は名詞の門で退けて知らせる。
#[test]
fn auto_lines_selected_by_claude_are_dictated_and_a_stray_bundle_line_is_rejected() {
    let (outcome, _) = complete(
        "若い女性、腹部骨盤CTテンプレート、異常なし",
        None,
        vec![answer(
            "腹部骨盤CT",
            vec![module("若い女性", Some(0), T0), module("若い女性", Some(2), "虫垂は正常です。その他、消化管に粗大な異常を認めません。")],
        )],
    );
    assert_eq!(outcome.route, Route::Rendered);
    assert_eq!(outcome.failures(), 1, "{:?}", outcome.dropped);
    assert!(outcome.dropped[0].contains("2 行目") && outcome.dropped[0].contains("取り違え"), "{:?}", outcome.dropped);
    let b = body("腹部骨盤CT");
    assert_eq!(outcome.reply.text.as_deref(), Some([s(T0), s(T1), b[5].clone(), b[6].clone(), s(T4), s(T5)].join("\n").as_str()));
}

/// 目的が分からなくても、Claude が選んだめまいの行は口述として置く(条件と食い違わないので。S1 の意味)。
#[test]
fn a_selected_auto_line_is_placed_when_the_context_is_unknown() {
    let vertigo = module_text("頭部MRI", "めまいの異常なし");
    let (outcome, _) = complete("頭部MRIテンプレート、ふらつき、異常なし", None, vec![answer("頭部MRI", vec![module("めまいの異常なし", None, &vertigo)])]);
    let b = body("頭部MRI");
    let mut expected: Vec<String> = b[..5].to_vec();
    expected.push(vertigo);
    expected.push(b[5].clone());
    assert_rendered("unknown-purpose", &outcome, &expected);
}

#[test]
fn a_whole_replacement_falls_back_to_the_full_text_with_a_second_call() {
    let full = "若い女性の全文。";
    let (outcome, fixed) = complete(
        "若い女性、腹部骨盤CTテンプレート、異常なし",
        None,
        vec![
            answer("腹部骨盤CT", vec![module("若い女性", None, "肝、胆、膵…(全体)")]),
            json!({"kind": "template", "template": "腹部骨盤CT", "text": full, "selections": [], "reply": null, "replace": false}),
        ],
    );
    assert!(matches!(&outcome.route, Route::Fallback(why) if why.contains("全体差し替え")), "{:?}", outcome.route);
    assert_eq!(outcome.calls, 2);
    assert_eq!(outcome.failures(), 1);
    assert_eq!(outcome.reply.text.as_deref(), Some(full));
    let seen = fixed.seen.lock().unwrap();
    assert_eq!(seen[1].full_text.as_deref(), Some("腹部骨盤CT"), "2 回目は全文で答えさせる");
}

#[test]
fn an_unknown_module_falls_back_too() {
    let (outcome, _) = complete(
        "胸部CTのテンプレートで、胸水少量",
        None,
        vec![
            answer("胸部CT", vec![module("胸水", None, "少量の胸水を認めます。")]),
            json!({"kind": "template", "template": "胸部CT", "text": "全文", "selections": [], "reply": null, "replace": false}),
        ],
    );
    assert!(matches!(&outcome.route, Route::Fallback(why) if why.contains("無いモジュール")), "{:?}", outcome.route);
    assert_eq!(outcome.reply.text.as_deref(), Some("全文"));
}

#[test]
fn a_finding_without_a_module_edits_the_body_line_and_a_list_organ() {
    let (outcome, _) = complete(
        "腹部骨盤CTテンプレート、右腎結石",
        None,
        vec![answer("腹部骨盤CT", vec![json!({"module": null, "line": 4, "grade": null, "organ": "腎", "text": "右腎に結石を認めます。"})])],
    );
    assert_eq!(outcome.failures(), 0);
    let mut expected = body("腹部骨盤CT");
    expected[4] = s("脾、副腎に特記所見ありません。\n右腎に結石を認めます。");
    assert_eq!(outcome.reply.text.as_deref(), Some(expected.join("\n").as_str()));
}

#[test]
fn a_line_edit_inside_a_two_line_slot_keeps_the_other_line() {
    // 腹部MRI の膵は 2 行で 1 項目。1 行目だけ直しても 0 行目は残る。
    let (outcome, _) = complete(
        "腹部MRIテンプレート、膵頭部に充実性病変",
        None,
        vec![answer("腹部MRI", vec![body_line(1, "膵頭部に充実性病変を認めます。")])],
    );
    let mut expected = body("腹部MRI");
    expected[1] = s("膵頭部に充実性病変を認めます。");
    assert_eq!(outcome.reply.text.as_deref(), Some(expected.join("\n").as_str()));
}

#[test]
fn full_text_answers_unstructured_templates_and_revisions_keep_the_full_text_path() {
    let plan = plan();
    // 構造の無いテンプレート(辞書に足した物)。
    let mut dictionary = plan.dictionary.clone();
    dictionary.save_template(None, "膝MRI", "半月板に異常なし。").unwrap();
    let unannotated = Plan::new(dictionary, plan.structures.clone());
    let prepared = prepare(AssistantRequest { request: "膝MRIのテンプレート".to_string(), ..AssistantRequest::default() }, None, &unannotated);
    let reply = reply_from_value(&json!({"kind": "template", "template": "膝MRI", "text": "半月板に異常なし。", "selections": [], "reply": null, "replace": false})).unwrap();
    assert!(matches!(resolve(&prepared, &reply, &unannotated), Resolution::Text { why, .. } if why == "構造の無いテンプレート"));
    // 言い直し(D11)は全文経路。
    let prepared = prepare(AssistantRequest { request: "体積を 70 に".to_string(), previous: Some("前の補完".to_string()), ..AssistantRequest::default() }, None, &plan);
    let reply = reply_from_value(&json!({"kind": "template", "template": "前立腺MRI", "text": "直した全文", "selections": [], "reply": null, "replace": true})).unwrap();
    assert!(matches!(resolve(&prepared, &reply, &plan), Resolution::Text { why, .. } if why == "言い直し"));
}

#[test]
fn edits_and_none_pass_through_untouched() {
    let (outcome, _) = complete(
        "誤字を直して",
        Some(Target { text: "右肺に結節を認めまる。".to_string(), selected: true, truncated: false }),
        vec![json!({"kind": "edit", "template": null, "text": "右肺に結節を認めます。", "selections": [], "reply": null, "replace": false})],
    );
    assert_eq!(outcome.route, Route::NotTemplate);
    assert_eq!(outcome.reply.text.as_deref(), Some("右肺に結節を認めます。"));
}

#[test]
fn a_graded_line_supplied_by_the_interval_does_not_break_locality() {
    // 中等度で 2 行目(椎間板の変性。〔　〕無し)を Claude が返さなくても、L7 で補われ、L2 の確かめで落ちない。
    let m = "変形性腰椎症";
    let t = "腰椎MRI";
    let (outcome, _) = complete(
        "腰椎MRIテンプレート、中等度の変形性腰椎症",
        None,
        vec![answer(t, vec![graded(m, 0, "中等度", &module_line(t, m, 0)), graded(m, 1, "中等度", &module_line(t, m, 1))])],
    );
    assert_eq!(outcome.route, Route::Rendered, "{:?}", outcome.route);
    let text = outcome.reply.text.unwrap();
    assert!(text.contains(&module_line(t, m, 2)), "{text}");
}

// ------------------------------------------------------------------------------------------------
// 下ごしらえ・指示文・答えの読み取り
// ------------------------------------------------------------------------------------------------

#[test]
fn an_exam_name_needs_a_modality_or_the_template_suffix() {
    use openspeech::core::completion::named_template;
    let plan = plan();
    let name = |request: &str| named_template(request, &plan.dictionary, &plan.vocabulary).map(|i| plan.dictionary.templates[i].keywords[0].clone());
    assert_eq!(name("前立腺MRIのテンプレートを呼び出して").as_deref(), Some("前立腺MRI"));
    assert_eq!(name("副鼻腔CTで左上顎洞炎").as_deref(), Some("副鼻腔CT"));
    assert_eq!(name("腰椎テンプレート、圧迫骨折").as_deref(), Some("腰椎MRI"), "別名でも「テンプレート」が続けば検査名");
    assert_eq!(name("肝嚢胞、腎嚢胞、前立腺の腫大"), None, "別名「前立腺」だけでは検査名と見ない");
    assert_eq!(name("胸部骨盤単純CTで異常なし").as_deref(), Some("胸部骨盤単純CT"), "長いキーワードを優先");
}

#[test]
fn the_previous_template_breaks_a_tie_between_candidates() {
    let plan = plan();
    let request = AssistantRequest { request: "肝嚢胞と腎嚢胞のみ".to_string(), ..AssistantRequest::default() };
    let prepared = prepare(request.clone(), Some("腹部MRI"), &plan);
    assert_eq!(prepared.request.candidates, "(テンプレートの補完なら)腹部MRI(直前の補完から)");
    let prepared = prepare(request.clone(), None, &plan);
    assert!(prepared.hint.is_none());
    // 前回・比較の文は手がかりにしない。
    let target = Target { text: "前回の腹部単純MRIと比較しました".to_string(), selected: false, truncated: false };
    let prepared = prepare(AssistantRequest { target: Some(target), ..request }, None, &plan);
    assert!(prepared.hint.is_none(), "{:?}", prepared.request.candidates);
}

#[test]
fn the_system_prompt_lists_structured_templates_with_line_numbers() {
    use openspeech::core::assistant::{build_system_prompt, build_system_prompt_with};
    let plan = plan();
    let system = build_system_prompt_with(&plan.dictionary, &plan.structures, "");
    for part in [
        "abdominopelvic CT\n(構造あり)\n本文:\n0: 肝に明らかな占拠性病変を認めません。\n",
        "4: 脾、腎、副腎に特記所見ありません。(列挙: 脾・腎・副腎)\n",
        "- 若い女性(行ごと。使い方に全体の差し替えとあっても、使う行を選ぶ):\n  0: 肝、胆、膵、脾、腎、副腎に明らかな異常を認めません。(自動: 女・若年)\n",
        "  2: 虫垂は正常です。その他、消化管に粗大な異常を認めません。(自動: 目的=腹痛)\n  3: 生理的な少量の腹水を認めます。(条件: 女・若年)\n",
        "  3: 前立腺の腫大がみられます。(条件: 男)\n",
        "- 肝(行ごと):\n  0: 軽度の脂肪肝を認めます。\n  1: 肝嚢胞を認めます。\n  使い方: 言われた所見の行だけを肝の文と差し替える\n",
        "- 変形性腰椎症(行ごと・程度あり。行末の《》は出る程度。使い方に全体の差し替えとあっても、使う行を選ぶ):\n  0: 腰椎の変形や骨棘形成を認めます。《全程度》\n  1: 黄色靭帯の肥厚や、椎間関節の過形成を認めます。《中等度以上》\n",
        "  5: また〔　〕で強い椎間孔狭窄が生じており、神経根周囲の脂肪織が消失しています。《高度》\n",
        "- 肺気腫: 背景肺には上肺優位の気腫性変化がみられます。\n",
        "- すりガラス結節: 〔　〕に〔　〕mm程度のすりガラス結節を認めます。\n  肺腺癌",
        "2:\n3: 仰臥位・腹臥位の2体位で撮像しました。\n",
        "- めまいの異常なし(自動: 目的=めまい): 両側の小脳橋角部",
    ] {
        assert!(system.contains(part), "{part}\n----\n{system}");
    }
    // 構造の無い辞書では S3 の前と同じ一覧(行番号も「構造あり」も無い)。
    let bare = build_system_prompt(&plan.dictionary, "");
    assert_eq!(bare, build_system_prompt_with(&plan.dictionary, &StructureFile::default(), ""));
    assert!(!bare.contains("\n(構造あり)\n本文:") && bare.contains("abdominopelvic CT\n肝に明らかな"));
    // 同じ辞書と構造なら同じ文(キャッシュが効く)。
    assert_eq!(system, build_system_prompt_with(&plan.dictionary, &plan.structures, ""));
}

#[test]
fn the_user_message_carries_context_grades_candidates_and_the_full_text_order() {
    use openspeech::core::assistant::build_user_message;
    let plan = plan();
    let prepared = prepare(
        AssistantRequest { request: "若い女性、軽度の脂肪肝のみ".to_string(), ..AssistantRequest::default() },
        None,
        &plan,
    );
    let message = build_user_message(&prepared.request);
    assert!(message.starts_with("依頼:\n若い女性、軽度の脂肪肝のみ\n文脈(アプリが依頼の語から取った): 女性・若年\n程度の語: 軽度\n候補: (テンプレートの補完なら)"), "{message}");
    let mut again = prepared.request.clone();
    again.full_text = Some("腹部骨盤CT".to_string());
    assert!(build_user_message(&again).contains("\n全文で答える: テンプレート「腹部骨盤CT」を、selections を空にして text に本文全体で書く。"));
}

#[test]
fn selections_are_read_from_the_answer() {
    let reply = reply_from_value(&json!({
        "kind": "template", "template": "腹部骨盤CT", "text": null, "reply": null, "replace": false,
        "selections": [
            {"module": "肝", "line": 1, "grade": null, "organ": null, "text": " 肝嚢胞を認めます。 "},
            {"module": null, "line": 4, "grade": null, "organ": "腎", "text": "右腎に結石を認めます。"},
            {"module": "肝", "line": 0, "grade": null, "organ": null, "text": ""},
            {"module": null, "line": null, "grade": null, "organ": null, "text": "どこにも置けない"}
        ]
    }))
    .unwrap();
    assert_eq!(reply.selections.len(), 2, "文の無い物と、行もモジュールも無い物は捨てる");
    assert_eq!(reply.selections[0].text, "肝嚢胞を認めます。");
    assert_eq!(reply.selections[1].organ.as_deref(), Some("腎"));
    // 構造のあるテンプレートの「異常なし」: 本文も選択も無くても補完として読む。
    let none = reply_from_value(&json!({"kind": "template", "template": "頭部CT", "text": null, "selections": [], "reply": null, "replace": false})).unwrap();
    assert_eq!(none.kind, openspeech::core::assistant::ReplyKind::Template);
    // テンプレート名も無ければ読めない答え。
    assert!(reply_from_value(&json!({"kind": "template", "template": null, "text": null, "selections": [], "reply": null})).is_err());
}

#[test]
fn notices_list_only_the_top_scoring_other_candidates() {
    let (outcome, _) = complete("肝嚢胞のみ", None, vec![answer("腹部骨盤CT", vec![module("肝", Some(1), "肝嚢胞を認めます。")])]);
    assert_eq!(outcome.other_candidates, ["腹部MRI"], "嚢胞だけ当たる副鼻腔CT・女性骨盤MRI は知らせない");
    let (outcome, _) = complete("肝嚢胞のみ", None, vec![answer("胸部CT", vec![])]);
    assert!(outcome.off_candidates, "候補に無いテンプレートは採るが知らせる");
}

/// 指示文の長さ(報告用。手で走らせる: `cargo test --test completion prompt_sizes -- --ignored --nocapture`)。
#[test]
#[ignore]
fn prompt_sizes() {
    use openspeech::core::assistant::{build_system_prompt, build_system_prompt_with};
    let plan = plan();
    let bare = build_system_prompt(&plan.dictionary, "");
    let structured = build_system_prompt_with(&plan.dictionary, &plan.structures, "");
    println!("構造なし {} 字 / 構造あり {} 字", bare.chars().count(), structured.chars().count());
}

// ------------------------------------------------------------------------------------------------
// 独立 QA の指摘の再現(Claude の答えは QA が実際の Claude で得た物)
// ------------------------------------------------------------------------------------------------

/// Q1: 「左上顎洞炎」で Claude が注どおり「右上顎洞…は認められません」を書いても、打ち消しの文の左右は数えない。
#[test]
fn qa_q1_the_negative_sentence_for_the_other_sinuses_passes_the_side_gate() {
    let sinus = "左上顎洞に粘膜肥厚〜粘液貯留に相当する軟部濃度域が広がっており、上顎洞炎と思われます。\n右上顎洞、両側の篩骨洞、前頭洞および蝶形骨洞には明らかな粘膜肥厚や液体貯留は認められません。\n歯性や真菌性を考慮する所見は指摘できません。";
    let (outcome, _) = complete("副鼻腔CTで左上顎洞炎", None, vec![answer("副鼻腔CT", vec![module("副鼻腔炎", None, sinus)])]);
    let b = body("副鼻腔CT");
    assert_rendered("q1", &outcome, &[b[0].clone(), s(sinus), b[2].clone(), b[3].clone()]);
}

/// Q2: 「胆石」で胆嚢結石の行(名詞の 2-gram は重ならないが、胆・石の 2 字が重なる)。
#[test]
fn qa_q2_gallstone_is_not_mistaken_for_a_line_mix_up() {
    let (outcome, _) = complete("腹部骨盤CTテンプレート、胆石", None, vec![answer("腹部骨盤CT", vec![module("胆", Some(0), "胆嚢結石あり。")])]);
    let mut expected = body("腹部骨盤CT");
    expected[1] = s("胆嚢結石あり。");
    assert_rendered("q2", &outcome, &expected);
}

/// Q3: 外傷の精査で骨髄浮腫が言われたら、自動の陰性文ではなく口述の文を置く。
#[test]
fn qa_q3_a_dictated_positive_finding_on_an_auto_line_is_placed() {
    let text = "C5椎体に骨髄浮腫を認めます。その他、骨折や骨挫傷を疑う骨髄浮腫や、軟部組織損傷を疑う浮腫性変化は認められません。";
    let (outcome, _) = complete("頸椎MRIテンプレート、外傷、C5椎体に骨髄浮腫あり", None, vec![answer("頸椎MRI", vec![module("外傷", None, text)])]);
    let mut expected = vec![s(text)];
    expected.extend(body("頸椎MRI"));
    assert_rendered("q3", &outcome, &expected);
}

/// Q4: 「軽度」は圧迫骨折の程度。Claude が腰椎症に付けた「中等度」は依頼の程度語に無いので、程度で絞らない
/// (別の所見の程度語で腰椎症の行を落とさない)。
#[test]
fn qa_q4_a_grade_word_of_another_finding_does_not_filter_the_group() {
    let m = "変形性腰椎症";
    let t = "腰椎MRI";
    let fracture = "L1椎体に骨髄浮腫を伴う軽度の椎体高減少があり、新鮮な圧迫骨折と考えられます。";
    let l3 = module_line(t, m, 3);
    let l4 = "脊柱管はL4/5レベルで最も強く狭窄しています。";
    let (outcome, _) = complete(
        "腰椎MRIテンプレート、L1の軽度の圧迫骨折、変形性腰椎症でL4/5が最も強い狭窄",
        None,
        vec![answer(
            t,
            vec![
                module("圧迫骨折", None, fracture),
                graded(m, 0, "中等度", &module_line(t, m, 0)),
                graded(m, 1, "中等度", &module_line(t, m, 1)),
                graded(m, 2, "中等度", &module_line(t, m, 2)),
                graded(m, 3, "中等度", &l3),
                graded(m, 4, "中等度", l4),
            ],
        )],
    );
    assert_eq!(outcome.route, Route::Rendered);
    let text = outcome.reply.text.clone().unwrap();
    assert!(text.contains(&module_line(t, m, 1)) && text.contains(l4), "中等度の行が残る: {text}");
    assert!(!text.contains("顕著な狭窄所見はありません"), "{text}");
    assert!(outcome.warnings.iter().any(|w| w.contains("程度で行を絞りませんでした")), "{:?}", outcome.warnings);
}

/// Q5: 2 行で 1 項目の膵を両方直しても、元の文と直した文が並ばない。
#[test]
fn qa_q5_two_edits_in_a_two_line_slot_make_one_block() {
    let (outcome, _) = complete(
        "腹部MRIテンプレート、主膵管拡張あり、膵頭部に充実性病変",
        None,
        vec![answer(
            "腹部MRI",
            vec![body_line(0, "主膵管拡張を認めます。膵の病的萎縮、腫大を認めません。"), body_line(1, "膵頭部に充実性病変を認めます。")],
        )],
    );
    let mut expected = body("腹部MRI");
    expected[0] = s("主膵管拡張を認めます。膵の病的萎縮、腫大を認めません。");
    expected[1] = s("膵頭部に充実性病変を認めます。");
    assert_rendered("q5", &outcome, &expected);
}

/// Q6: めまいの精査で聴神経腫瘍疑いが言われたら、口述の文を置く(「左右差なく」の打ち消しの文の左右は数えない)。
#[test]
fn qa_q6_a_dictated_tumor_replaces_the_vertigo_negatives() {
    let vertigo = "右小脳橋角部に占拠性病変を認め、聴神経腫瘍が疑われます。\n内耳の形態に左右差なく、明らかな異常を認めません。";
    let (outcome, _) = complete(
        "めまいの精査、頭部MRIテンプレート、右小脳橋角部に聴神経腫瘍疑い",
        None,
        vec![answer("頭部MRI", vec![module("めまいの異常なし", None, vertigo)])],
    );
    let b = body("頭部MRI");
    let mut expected: Vec<String> = b[..5].to_vec();
    expected.push(s(vertigo));
    expected.push(b[5].clone());
    assert_rendered("q6", &outcome, &expected);
}

/// Q7: 点数の高い候補(腰椎MRI)があれば、エディターの「頸椎MRI」を手がかりに絞らない。
#[test]
fn qa_q7_a_hint_only_breaks_ties_between_the_top_candidates() {
    let plan = plan();
    let target = Target { text: "頸椎MRI".to_string(), selected: false, truncated: false };
    let prepared = prepare(
        AssistantRequest { request: "変形性腰椎症、L4/5で最も強い狭窄".to_string(), target: Some(target), ..AssistantRequest::default() },
        None,
        &plan,
    );
    assert!(prepared.hint.is_none(), "{:?}", prepared.request.candidates);
    assert!(prepared.request.candidates.starts_with("(テンプレートの補完なら)腰椎MRI"), "{}", prepared.request.candidates);
}

/// Q8: 「顕著な狭窄はない」は程度語ではない(打ち消し)。依頼に「程度の語」を添えない。
#[test]
fn qa_q8_a_negated_grade_word_is_not_a_grade() {
    let plan = plan();
    let prepared = prepare(
        AssistantRequest { request: "頸椎MRIテンプレート、変形性頸椎症、顕著な狭窄はない".to_string(), ..AssistantRequest::default() },
        None,
        &plan,
    );
    assert!(prepared.request.grades.is_empty(), "{:?}", prepared.request.grades);
}

/// Q9: 「右下腹部痛」は目的=腹痛。若い女性の虫垂の行が文脈で置かれる。
#[test]
fn qa_q9_lower_abdominal_pain_is_an_abdominal_pain_workup() {
    let (outcome, fixed) = complete("若い女性、右下腹部痛、腹部骨盤CTテンプレート、虫垂は正常", None, vec![answer("腹部骨盤CT", vec![])]);
    assert_eq!(fixed.seen.lock().unwrap()[0].context, "女性・若年・目的=腹痛");
    assert!(outcome.reply.text.unwrap().contains("虫垂は正常です。"));
}

/// QA M5: 列挙の行を臓器の名前の違い(右腎・脾臓)や臓器の指定なしで直しても、ほかの臓器の陰性文は残る。
#[test]
fn qa_m5_a_list_line_edit_keeps_the_other_organs() {
    let mut expected = body("腹部骨盤CT");
    expected[4] = s("脾、副腎に特記所見ありません。\n右腎に結石を認めます。");
    for organ in [json!("右腎"), json!(null)] {
        let (outcome, _) = complete(
            "腹部骨盤CTテンプレート、右腎結石",
            None,
            vec![answer("腹部骨盤CT", vec![json!({"module": null, "line": 4, "grade": null, "organ": organ, "text": "右腎に結石を認めます。"})])],
        );
        assert_rendered("m5", &outcome, &expected);
    }
}

/// QA M6: 中等度の頸椎症で髄内を本文の行で直しても、6 行目(髄内の陰性文)が補われて並ばない。
#[test]
fn qa_m6_a_body_edit_on_a_group_slot_becomes_the_group_line() {
    let m = "変形性頸椎症";
    let t = "頸椎MRI";
    let cord = "C5/6レベルの髄内にT2WI高信号を認めます。";
    let (outcome, _) = complete(
        "頸椎MRIテンプレート、中等度の変形性頸椎症、C5/6の髄内に高信号",
        None,
        vec![answer(t, vec![graded(m, 0, "中等度", &module_line(t, m, 0)), graded(m, 2, "中等度", &module_line(t, m, 2)), body_line(3, cord)])],
    );
    assert_eq!(outcome.route, Route::Rendered, "{:?}", outcome.route);
    let text = outcome.reply.text.unwrap();
    assert!(text.contains(cord) && !text.contains("髄内に明らかな異常信号域はみられません"), "{text}");
}

/// QA 低: テンプレート名に飾りが付いても照合する。
#[test]
fn qa_a_decorated_template_name_still_matches() {
    let (outcome, _) = complete("肝嚢胞のみ", None, vec![answer("腹部骨盤CT(構造あり)", vec![module("肝", Some(1), "肝嚢胞を認めます。")])]);
    assert_eq!(outcome.route, Route::Rendered);
    assert_eq!(outcome.calls, 1);
}

/// 実際の Claude(3 回目の計測の N1): 構造のあるテンプレートに、選択ではなく本文で答え、しかも行を落とした
/// (消化管と腹水の行が無い)。本文の行がどれも源泉か自動の行なら、本文は使わず構造から描く。
#[test]
fn a_full_text_answer_without_findings_is_redrawn_from_the_structure() {
    let dropped_lines = [T0, T1, T4, T5].join("\n");
    let (outcome, _) = complete("若い女性、腹部骨盤CTテンプレート、異常なし", None, vec![full("腹部骨盤CT", &dropped_lines)]);
    let b = body("腹部骨盤CT");
    assert!(matches!(&outcome.route, Route::Retried(why) if why.contains("読み替えた")), "{:?}", outcome.route);
    assert_eq!(outcome.failures(), 1, "指示に従わない答えを直したことは落ちたに数える");
    assert_eq!(outcome.reply.text.as_deref(), Some([s(T0), s(T1), b[5].clone(), b[6].clone(), s(T4), s(T5)].join("\n").as_str()));
    // 腹痛精査でないのに虫垂の行、言われていない生理的な腹水の行を本文に書いても、読み替えて名詞の門で退ける。
    let with_extras = [T0, T1, "虫垂は正常です。その他、消化管に粗大な異常を認めません。", "生理的な少量の腹水を認めます。", T4, T5].join("\n");
    let (outcome, _) = complete("若い女性、腹部骨盤CTテンプレート、異常なし", None, vec![full("腹部骨盤CT", &with_extras)]);
    assert!(matches!(&outcome.route, Route::Retried(_)), "{:?}", outcome.route);
    assert_eq!(outcome.dropped.len(), 2, "{:?}", outcome.dropped);
    assert_eq!(outcome.reply.text.as_deref(), Some([s(T0), s(T1), b[5].clone(), b[6].clone(), s(T4), s(T5)].join("\n").as_str()));
    // 源泉に無い文があれば、選択で答え直させる。答え直しでも本文なら、その本文を使う(全文経路に落ちた)。
    let (outcome, _) = complete(
        "腹部骨盤CTテンプレート、肝嚢胞",
        None,
        vec![full("腹部骨盤CT", "肝嚢胞を認めます。\n胆嚢は正常。"), full("腹部骨盤CT", "肝嚢胞を認めます。\n胆嚢は正常。")],
    );
    assert!(matches!(&outcome.route, Route::Fallback(why) if why.contains("答え直させても")), "{:?}", outcome.route);
    assert_eq!(outcome.calls, 2);
}

// ------------------------------------------------------------------------------------------------
// QA の再確認の指摘の再現
// ------------------------------------------------------------------------------------------------

/// 高 1(実際の Claude、R2-2): 若い女性の肺の自動の行を書き換えて肺の結節を書いたら、口述として置く(胸水なしは残る)。
#[test]
fn qa2_h1_a_rewritten_auto_bundle_line_with_the_dictated_finding_is_placed() {
    let nodule = "撮像範囲の右肺下葉に5mmの結節を認めます。";
    let (outcome, _) = complete(
        "若い女性、腹部骨盤CTテンプレート、右肺下葉に5ミリの結節",
        None,
        vec![answer("腹部骨盤CT", vec![module("若い女性", Some(4), nodule)])],
    );
    let b = body("腹部骨盤CT");
    assert_rendered("qa2-h1", &outcome, &[s(T0), s(T1), b[5].clone(), b[6].clone(), s(nodule), s(T5)]);
}

/// 中 1(probe P3): 腰椎症の椎間板の行を選び、本文の椎間板の行にヘルニアを書いても、両方の文が残る(程度の語なし)。
#[test]
fn qa2_m1_a_body_edit_next_to_a_selected_group_line_keeps_both() {
    let m = "変形性腰椎症";
    let t = "腰椎MRI";
    let hernia = "L4/5に椎間板ヘルニアを認めます。";
    let (outcome, _) = complete(
        "腰椎MRIテンプレート、変形性腰椎症、L4/5に椎間板ヘルニア",
        None,
        vec![answer(t, vec![module(m, Some(0), &module_line(t, m, 0)), module(m, Some(2), &module_line(t, m, 2)), body_line(1, hernia)])],
    );
    assert_eq!(outcome.route, Route::Rendered, "{:?}", outcome.route);
    let text = outcome.reply.text.unwrap();
    assert!(text.contains(&module_line(t, m, 2)) && text.contains(hernia), "{text}");
}

/// 中 2(実際の Claude、R2-1): 構造のあるテンプレートに、源泉に無い文を含む本文で答え(しかも行を落とした)たら、
/// 選択で答え直させて描く。答え直させたことは「落ちた」に数える。
#[test]
fn qa2_m2_a_full_text_with_new_lines_is_asked_again_for_selections() {
    let m = "変形性腰椎症";
    let t = "腰椎MRI";
    let hernia = "L4/5に椎間板ヘルニアを認めます。";
    let (outcome, fixed) = complete(
        "腰椎MRIテンプレート、変形性腰椎症、L4/5に椎間板ヘルニア",
        None,
        vec![
            full(t, "腰椎の変形や骨棘形成を認めます。\n椎間板の変性、膨隆を認めます。L4/5に椎間板ヘルニアを認めます。"),
            answer(t, vec![module(m, Some(0), &module_line(t, m, 0)), module(m, Some(2), &format!("{}{hernia}", module_line(t, m, 2)))]),
        ],
    );
    assert!(matches!(&outcome.route, Route::Retried(_)), "{:?}", outcome.route);
    assert_eq!(outcome.calls, 2);
    assert_eq!(outcome.failures(), 1);
    assert!(fixed.seen.lock().unwrap()[1].selections_only.is_some());
    let b = body(t);
    assert_eq!(
        outcome.reply.text.as_deref(),
        Some([module_line(t, m, 0), format!("{}{hernia}", module_line(t, m, 2)), b[2].clone()].join("\n").as_str()),
        "脊柱管の既定文が残る"
    );
}

/// 実際の Claude(最終の計測の R2-4): T0 を臓器を外しただけの文に書き換えたら、元の文として扱い、ほかの選択から
/// コードが外す(全文に落とさない)。
#[test]
fn qa2_a_list_line_with_organs_removed_by_claude_is_kept() {
    let (outcome, _) = complete(
        "若い女性、腹部骨盤CTテンプレート、脂肪肝と腎嚢胞",
        None,
        vec![answer(
            "腹部骨盤CT",
            vec![
                module("若い女性", Some(0), "胆、膵、脾、副腎に明らかな異常を認めません。"),
                module("肝", Some(0), "軽度の脂肪肝を認めます。"),
                module("腎・副腎・前立腺", Some(0), "両腎嚢胞あり。"),
            ],
        )],
    );
    let b = body("腹部骨盤CT");
    assert_rendered(
        "r2-4",
        &outcome,
        &[
            s("胆、膵、脾、副腎に明らかな異常を認めません。"),
            s("軽度の脂肪肝を認めます。"),
            s("両腎嚢胞あり。"),
            s(T1),
            b[5].clone(),
            b[6].clone(),
            s(T4),
            s(T5),
        ],
    );
}

/// 中 3(probe P16): 列挙否定の自動の行(T0)を書き換えて所見を書いたら、全文で答え直させる(繋ぎ直しで所見が消えるため)。
#[test]
fn qa2_m3_a_rewritten_list_negation_line_falls_back() {
    let (outcome, _) = complete(
        "若い女性、腹部骨盤CTテンプレート、脂肪肝と腎嚢胞",
        None,
        vec![
            answer("腹部骨盤CT", vec![module("若い女性", Some(0), "脂肪肝と腎嚢胞を認めます。胆、膵、脾、副腎に明らかな異常を認めません。")]),
            full("腹部骨盤CT", "全文"),
        ],
    );
    assert!(matches!(&outcome.route, Route::Fallback(why) if why.contains("列挙否定")), "{:?}", outcome.route);
}

/// 低(probe P5): 検査名の字(腹部骨盤CT の「腹」)で、言われていない生理的な腹水の行が名詞の門を通らない。
#[test]
fn qa2_low_template_name_characters_do_not_pass_the_noun_gate() {
    let (outcome, _) = complete(
        "若い女性、腹部骨盤CTテンプレート、右水腎症",
        None,
        vec![answer("腹部骨盤CT", vec![module("若い女性", Some(3), "生理的な少量の腹水を認めます。")])],
    );
    assert_eq!(outcome.dropped.len(), 1, "{:?}", outcome.dropped);
    assert!(!outcome.reply.text.unwrap().contains("生理的"));
}
