//! AI の診断(`--correct-check`・`--complete-check`)。

use super::*;

/// `--correct-check 文 [--context 直前の文] [--backend api|claude-code]`:
/// 訂正役(Claude)に 1 文を送り、結果と所要時間を印字する。
pub(crate) fn run_correct_check(args: &[String]) -> i32 {
    let Some(text) = argument_value(args, "--correct-check").cloned() else {
        println!("使い方: --correct-check 文 [--context 直前の文] [--backend api|claude-code]");
        return 2;
    };
    let store = Store::open();
    let mut global = store.load_global();
    match argument_value(args, "--backend").map(String::as_str) {
        Some("api") => global.corrector.backend = CorrectorBackend::ApiKey,
        Some("claude-code") => global.corrector.backend = CorrectorBackend::ClaudeCode,
        _ => {}
    }
    let data = store.load_user();
    let Some(corrector) = openspeech::app::make_corrector(&store, &global, &data) else {
        println!("訂正役が使えません: 設定画面の「AI 訂正」で有効にし、接続方法を確かめてください");
        return 1;
    };
    let request = CorrectionRequest {
        text: text.clone(),
        unsure: Vec::new(),
        context: argument_value(args, "--context")
            .cloned()
            .unwrap_or_default(),
    };
    println!("訂正役: {}", corrector.name());
    let began = Instant::now();
    match corrector.correct(&request) {
        Ok(corrected) => {
            println!("{:.2}s {corrected}", began.elapsed().as_secs_f32());
            let changed = changed_ranges(&text, &corrected);
            if changed.is_empty() {
                println!("変更なし");
            } else {
                let chars: Vec<char> = corrected.chars().collect();
                let words: Vec<String> = changed
                    .iter()
                    .map(|(s, e)| chars[*s..*e].iter().collect())
                    .collect();
                println!("変更箇所: {}", words.join(" / "));
            }
            0
        }
        Err(message) => {
            println!("{:.2}s エラー: {message}", began.elapsed().as_secs_f32());
            1
        }
    }
}

const ANNOTATE_USAGE: &str =
    "使い方: --annotate-templates [--model モデル] [--only 名前] [--backend api|claude-code]";

/// `--annotate-templates [--model モデル] [--only 名前] [--backend api|claude-code]`:
/// 辞書のテンプレート(`--only` なら 1 件)を注釈役(既定は Sonnet)に注釈させ、行の鍵・変種・警告
/// (`unresolved`)を印字して `structure.json` に書く(合うテンプレートの無い項は消す)。`dictionary.json` は書かない。
pub(crate) fn run_annotate_templates(args: &[String]) -> i32 {
    let value = |name: &str| -> Result<Option<String>, ()> {
        match argument_value(args, name) {
            Some(v) if v.starts_with("--") => Err(()),
            Some(v) => Ok(Some(v.clone())),
            None if args.iter().any(|a| a == name) => Err(()),
            None => Ok(None),
        }
    };
    let (Ok(model), Ok(only), Ok(backend)) = (value("--model"), value("--only"), value("--backend")) else {
        println!("{ANNOTATE_USAGE}");
        return 2;
    };
    let model = model.unwrap_or_else(|| annotate::ANNOTATION_MODEL.to_string());
    let store = Store::open();
    let mut global = store.load_global();
    match backend.as_deref() {
        Some("api") => global.corrector.backend = CorrectorBackend::ApiKey,
        Some("claude-code") => global.corrector.backend = CorrectorBackend::ClaudeCode,
        Some(other) => {
            println!("--backend は api か claude-code です(「{other}」は知りません)");
            return 2;
        }
        None => {}
    }
    let data = store.load_user();
    let templates = &data.dictionary.templates;
    let picked: Vec<&Template> = match &only {
        Some(name) => match structure::match_template(&data.dictionary, name) {
            Some(index) => vec![&templates[index]],
            None => {
                println!("「{name}」に当たるテンプレートがありません");
                return 1;
            }
        },
        None => templates.iter().collect(),
    };
    if picked.is_empty() {
        println!("テンプレートがありません");
        return 1;
    }
    let Some(annotator) = openspeech::app::make_annotator(&store, &global, &model) else {
        println!("注釈役が使えません: 設定画面の「AI 訂正」で Claude に接続してください");
        return 1;
    };
    let mut file = store.load_structure();
    let vocabulary = Vocabulary::builtin().with_extras(&file);
    println!("注釈役: {}(テンプレート {} 件。考える量 {})", annotator.name(), picked.len(), annotate::ANNOTATION_EFFORT);
    let mut failed = 0;
    for template in picked {
        let began = Instant::now();
        let name = template.keywords.first().map_or("", String::as_str);
        match annotate::annotate(annotator.as_ref(), template, &vocabulary) {
            Ok(structure) => {
                println!("\n== {name}  {:.1}s", began.elapsed().as_secs_f32());
                print_structure(template, &structure);
                file.insert(template, structure);
            }
            Err(message) => {
                println!("\n== {name}  {:.1}s エラー: {message}(未注釈のまま)", began.elapsed().as_secs_f32());
                failed += 1;
            }
        }
    }
    let pruned = file.prune(templates);
    match store.save_structure(&file) {
        Ok(()) => println!(
            "\n{} に書きました(構造 {} 件{})",
            store.structure_path().display(),
            file.templates.len(),
            if pruned > 0 { format!("。合うテンプレートの無い {pruned} 件を消した") } else { String::new() }
        ),
        Err(message) => {
            println!("\n構造を書けません: {message}");
            return 1;
        }
    }
    if failed > 0 {
        1
    } else {
        0
    }
}

/// 構造の印字: 行(番号・鍵・文)、変種(モジュール・行・対象・置き方・極性・役割・ほか)、尺度、警告。
fn print_structure(template: &Template, structure: &TemplateStructure) {
    let body = structure::canonical_lines(&template.body);
    println!("行:");
    for line in &structure.lines {
        let text = body.get(line.i).copied().unwrap_or("");
        let key = match line.kind {
            LineKind::Blank => "(空行)".to_string(),
            LineKind::Heading => "(見出し)".to_string(),
            LineKind::ListNegation => format!("{}(列挙)", line.slot.as_deref().unwrap_or("鍵なし")),
            LineKind::Sentence => line.slot.clone().unwrap_or_else(|| "鍵なし".to_string()),
        };
        println!("  {:>2} {key}  {text}", line.i);
    }
    println!("変種:");
    for variant in &structure.variants {
        let line = variant.line.map_or("-".to_string(), |l| l.to_string());
        if variant.is_whole() {
            let how = if variant.split { "全体差し替え(行に割った)" } else { "全体差し替え(全文経路)" };
            println!("  {} {line}  {how}", variant.module);
            continue;
        }
        let mut row = format!(
            "  {} {line}  {} {} {}",
            variant.module,
            variant.target.join("・"),
            annotate::mode_label(variant.mode),
            annotate::polarity_label(variant.polarity.as_deref())
        );
        if let Some(role) = &variant.role {
            row.push_str(&format!(" 役割={role}"));
        }
        let extras = annotate::variant_extras(variant);
        if !extras.is_empty() {
            row.push_str(&format!("({extras})"));
        }
        if variant.derived {
            row.push_str("(派生)");
        }
        println!("{row}");
    }
    for group in &structure.groups {
        println!("尺度: {}", group.module);
    }
    if structure.unresolved.is_empty() {
        println!("警告: なし");
    } else {
        println!("警告:");
        for item in &structure.unresolved {
            println!("  - {item}");
        }
    }
}

const COMPLETE_USAGE: &str =
    "使い方: --complete-check 依頼 [--previous 直前の補完] [--target 文 | --target-file 場所 [--whole-text]] [--backend api|claude-code] [--compare]";

/// 依頼に添える対象の文章(`--target 文` か `--target-file 場所`)。選択範囲として扱う(考える量は medium)。
/// `--whole-text` を付けると全文として扱う(選択が無いときと同じ、考える量 low)。
/// 長ければアプリと同じく先頭 3000 字。どちらも無ければ None、読めなければ Err。
fn check_target(args: &[String]) -> Result<Option<Target>, String> {
    let text = match (argument_value(args, "--target"), argument_value(args, "--target-file")) {
        (Some(_), Some(_)) => return Err("--target と --target-file はどちらか一方です".to_string()),
        (Some(text), None) => text.clone(),
        (None, Some(path)) => std::fs::read_to_string(path).map_err(|e| format!("{path} を読めません: {e}"))?,
        (None, None) => return Ok(None),
    };
    if text.trim().is_empty() {
        return Err("対象の文章が空です".to_string());
    }
    let truncated = text.chars().count() > MAX_TARGET_CHARS;
    Ok(Some(Target {
        text: text.chars().take(MAX_TARGET_CHARS).collect(),
        selected: !args.iter().any(|a| a == "--whole-text"),
        truncated,
    }))
}

/// 使っている依頼の指示(`request.md` か既定)。
fn request_guidance_label(store: &Store) -> String {
    match store.read_request_guidance() {
        Ok(text) if assistant::is_previous_default(&text) => {
            "request.md は 1.1.0 の既定のまま(新しい既定を送る。アプリを開くとファイルも書き換わる)".to_string()
        }
        Ok(text) => match guidance::sent_chars(&text) {
            0 => "アプリの既定".to_string(),
            n if n > MAX_REQUEST_GUIDANCE_CHARS => {
                format!("request.md {n} 字(先頭の {MAX_REQUEST_GUIDANCE_CHARS} 字だけ使う)")
            }
            n => format!("request.md {n} 字"),
        },
        Err(error) => format!("request.md を読めないのでアプリの既定({error})"),
    }
}

/// 選択 1 つの表示(`{"module":…,"line":…}` の形。無い項目は書かない)。
fn selection_label(selection: &structure::Selection) -> String {
    serde_json::to_string(selection).unwrap_or_default()
}

/// 1 回の補完を印字する。戻り値は (秒, 結果)。
fn print_completion(
    assistant: &dyn Assistant,
    prepared: &completion::Prepared,
    plan: &completion::Plan,
    title: &str,
) -> (f32, Result<completion::Outcome, String>) {
    let began = Instant::now();
    let result = completion::run(assistant, prepared, plan);
    let seconds = began.elapsed().as_secs_f32();
    println!("\n== {title}");
    match &result {
        Ok(outcome) => {
            let reply = &outcome.reply;
            let kind = match reply.kind {
                ReplyKind::Template => "template",
                ReplyKind::Edit => "edit",
                ReplyKind::None => "none",
            };
            println!(
                "{seconds:.2}s 種類: {kind} テンプレート: {} 経路: {} 呼んだ回数: {}",
                outcome.template.as_deref().unwrap_or("なし"),
                outcome.route.label(),
                outcome.calls
            );
            if reply.replace {
                println!("言い直し: 直前の補完を置き換えます");
            }
            if !outcome.claude_selections.is_empty() {
                println!("Claude の選択:");
                for selection in &outcome.claude_selections {
                    println!("  {}", selection_label(selection));
                }
            }
            if outcome.route == completion::Route::Rendered {
                println!("描画した選択 {} 個:", outcome.kept.len());
                for selection in &outcome.kept {
                    println!("  {}", selection_label(selection));
                }
            }
            if outcome.dropped.is_empty() {
                println!("退けた選択: なし");
            } else {
                println!("退けた選択:");
                for reason in &outcome.dropped {
                    println!("  - {reason}");
                }
            }
            for warning in &outcome.warnings {
                println!("知らせ(画面に出す): {warning}");
            }
            for note in &outcome.notes {
                println!("知らせ: {note}");
            }
            if outcome.off_candidates {
                println!("候補: 選んだテンプレートは候補に無い");
            } else if !outcome.other_candidates.is_empty() {
                println!("候補: ほかに {}", outcome.other_candidates.join("・"));
            }
            if reply.kind == ReplyKind::Edit {
                if let (Some(target), Some(answer)) = (&prepared.request.target, &reply.text) {
                    match assistant::check_edit(&prepared.request.request, &target.text, answer) {
                        EditCheck::Apply(applied) => println!(
                            "手直し: 当てる(変更 {} 箇所)",
                            assistant::edit_changes(&target.text, &applied)
                        ),
                        EditCheck::Unchanged => println!("手直し: 直すところなし"),
                        EditCheck::Refused(why) => println!("手直し: 使わない({why})"),
                    }
                } else {
                    println!("手直し: 対象の文章が無いので当てない");
                }
            }
            println!("本文:");
            println!("{}", reply.text.as_deref().unwrap_or("(なし)"));
            if let Some(text) = &reply.reply {
                println!("返事: {text}");
            }
            // 比較表を作るための 1 行(秒・経路・落ちた数・本文)。
            let record = serde_json::json!({
                "title": title,
                "seconds": (seconds * 100.0).round() / 100.0,
                "kind": kind,
                "template": outcome.template,
                "route": outcome.route.label(),
                "rendered": outcome.route == completion::Route::Rendered,
                "calls": outcome.calls,
                "failures": outcome.failures(),
                "dropped": outcome.dropped,
                "warnings": outcome.warnings,
                "claude_selections": outcome.claude_selections.len(),
                "text": reply.text,
            });
            println!("記録: {record}");
        }
        Err(message) => {
            println!("{seconds:.2}s エラー: {message}");
            println!("記録: {}", serde_json::json!({"title": title, "seconds": seconds, "error": message}));
        }
    }
    (seconds, result)
}

/// `--complete-check 依頼 [--previous 直前の補完] [--target 文 | --target-file 場所] [--backend api|claude-code] [--compare]`:
/// 「ヘイ、クロード」の補完役(Claude)に依頼を送り、下ごしらえ(文脈・程度語・候補)、答えの選択、描画(経路・退けた
/// 選択と理由・知らせ)、本文と所要時間を印字する。テンプレートと構造はデータフォルダの `dictionary.json`・
/// `structure.json`、指示は `request.md`(無ければ既定)。対象の文章を添えると手直しの依頼も試せ、アプリが当てる前に
/// 掛ける門(左右・長さ・同じ文)の結果も印字する。`--compare` を付けると、同じ依頼を構造なし(全文経路)でも通して
/// 本文を並べる(S3 の前後の比較、§16.8)。
pub(crate) fn run_complete_check(args: &[String]) -> i32 {
    let Some(text) = argument_value(args, "--complete-check").cloned() else {
        println!("{COMPLETE_USAGE}");
        return 2;
    };
    let target = match check_target(args) {
        Ok(target) => target,
        Err(message) => {
            println!("{message}");
            return 2;
        }
    };
    if target.is_none() && args.iter().any(|a| a == "--whole-text") {
        println!("--whole-text は --target か --target-file と一緒に使います");
        return 2;
    }
    let store = Store::open();
    let mut global = store.load_global();
    match argument_value(args, "--backend").map(String::as_str) {
        Some("api") => global.corrector.backend = CorrectorBackend::ApiKey,
        Some("claude-code") => global.corrector.backend = CorrectorBackend::ClaudeCode,
        Some(other) => {
            println!("--backend は api か claude-code です(「{other}」は知りません)");
            return 2;
        }
        None => {}
    }
    let data = store.load_user();
    let structures = store.load_structure();
    let Some(assistant) = openspeech::app::make_assistant(&store, &global, &data, &structures) else {
        println!("補完役が使えません: 設定画面の「AI 訂正」で Claude に接続してください");
        return 1;
    };
    let annotated = data
        .dictionary
        .templates
        .iter()
        .filter(|t| structures.structure_for(t).is_some())
        .count();
    let plan = completion::Plan::new(data.dictionary.clone(), structures);
    let request = AssistantRequest {
        request: text,
        previous: argument_value(args, "--previous").cloned(),
        target,
        ..AssistantRequest::default()
    };
    let prepared = completion::prepare(request, None, &plan);
    println!(
        "補完役: {}(テンプレート {} 件、構造あり {annotated} 件)",
        assistant.name(),
        data.dictionary.templates.len()
    );
    println!("依頼の指示: {}", request_guidance_label(&store));
    println!("依頼: {}", prepared.request.request);
    match &prepared.request.target {
        Some(target) => println!(
            "対象の文章: {}の {} 字{}(考える量 {})",
            if target.selected { "選択範囲" } else { "全文" },
            target.text.chars().count(),
            if target.truncated { "(先頭だけ)" } else { "" },
            assistant::effort(&prepared.request)
        ),
        None => println!("対象の文章: なし(考える量 {})", assistant::effort(&prepared.request)),
    }
    let context = &prepared.request.context;
    println!("文脈: {}", if context.is_empty() { "なし" } else { context });
    for note in &prepared.extracted.notes {
        println!("文脈の知らせ: {note}");
    }
    println!(
        "程度の語: {}",
        if prepared.extracted.grades.is_empty() { "なし".to_string() } else { prepared.extracted.grades.join("・") }
    );
    match prepared.named {
        Some(index) => println!("検査名: {}", data.dictionary.templates[index].keywords[0]),
        None => println!(
            "検査名: なし。候補: {}",
            if prepared.request.candidates.is_empty() { "なし" } else { &prepared.request.candidates }
        ),
    }
    let (_, result) = print_completion(assistant.as_ref(), &prepared, &plan, "構造の経路(S3)");
    if args.iter().any(|a| a == "--compare") {
        let bare = completion::Plan::new(data.dictionary.clone(), StructureFile::default());
        let Some(full) = openspeech::app::make_assistant(&store, &global, &data, &bare.structures) else {
            return 1;
        };
        let prepared_full = completion::prepare(prepared.request.clone(), None, &bare);
        let (_, full_result) = print_completion(full.as_ref(), &prepared_full, &bare, "全文経路(構造なし)");
        if let (Ok(a), Ok(b)) = (&result, &full_result) {
            let same = a.reply.text == b.reply.text;
            println!("\n比較: 本文は{}", if same { "同じ" } else { "違う" });
        }
    }
    i32::from(result.is_err())
}
