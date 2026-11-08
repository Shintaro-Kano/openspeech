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

const COMPLETE_USAGE: &str =
    "使い方: --complete-check 依頼 [--previous 直前の補完] [--target 文 | --target-file 場所 [--whole-text]] [--backend api|claude-code]";

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

/// `--complete-check 依頼 [--previous 直前の補完] [--target 文 | --target-file 場所] [--backend api|claude-code]`:
/// 「ヘイ、クロード」の補完役(Claude)に依頼を送り、答えの種類・本文と所要時間を印字する。テンプレートは
/// 辞書ファイルのもの、指示は `request.md`(無ければ既定)。対象の文章を添えると手直しの依頼も試せ、
/// アプリが当てる前に掛ける門(左右・長さ・同じ文)の結果も印字する。
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
    let Some(assistant) = openspeech::app::make_assistant(&store, &global, &data) else {
        println!("補完役が使えません: 設定画面の「AI 訂正」で Claude に接続してください");
        return 1;
    };
    let request = AssistantRequest {
        request: text,
        previous: argument_value(args, "--previous").cloned(),
        target,
    };
    println!(
        "補完役: {}(テンプレート {} 件)",
        assistant.name(),
        data.dictionary.templates.len()
    );
    println!("依頼の指示: {}", request_guidance_label(&store));
    match &request.target {
        Some(target) => println!(
            "対象の文章: {}の {} 字{}(考える量 {})",
            if target.selected { "選択範囲" } else { "全文" },
            target.text.chars().count(),
            if target.truncated { "(先頭だけ)" } else { "" },
            assistant::effort(&request)
        ),
        None => println!("対象の文章: なし(考える量 {})", assistant::effort(&request)),
    }
    let began = Instant::now();
    match assistant.complete(&request) {
        Ok(reply) => {
            let kind = match reply.kind {
                ReplyKind::Template => "template",
                ReplyKind::Edit => "edit",
                ReplyKind::None => "none",
            };
            println!(
                "{:.2}s 種類: {kind} テンプレート: {}",
                began.elapsed().as_secs_f32(),
                reply.template.as_deref().unwrap_or("なし")
            );
            if reply.replace {
                println!("言い直し: 直前の補完を置き換えます");
            }
            if reply.kind == ReplyKind::Edit {
                if let (Some(target), Some(answer)) = (&request.target, &reply.text) {
                    match assistant::check_edit(&request.request, &target.text, answer) {
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
            if let Some(text) = reply.text {
                println!("{text}");
            }
            if let Some(reply) = reply.reply {
                println!("返事: {reply}");
            }
            0
        }
        Err(message) => {
            println!("{:.2}s エラー: {message}", began.elapsed().as_secs_f32());
            1
        }
    }
}

