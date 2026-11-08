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

/// `--complete-check 依頼 [--previous 直前の補完] [--backend api|claude-code]`:
/// 「ヘイ、クロード」の補完役(Claude)に依頼を送り、答えと所要時間を印字する。テンプレートは辞書ファイルのもの。
pub(crate) fn run_complete_check(args: &[String]) -> i32 {
    let Some(text) = argument_value(args, "--complete-check").cloned() else {
        println!("使い方: --complete-check 依頼 [--previous 直前の補完] [--backend api|claude-code]");
        return 2;
    };
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
    };
    println!(
        "補完役: {}(テンプレート {} 件)",
        assistant.name(),
        data.dictionary.templates.len()
    );
    let began = Instant::now();
    match assistant.complete(&request) {
        Ok(reply) => {
            println!(
                "{:.2}s テンプレート: {}",
                began.elapsed().as_secs_f32(),
                reply.template.as_deref().unwrap_or("なし")
            );
            if reply.replace {
                println!("言い直し: 直前の補完を置き換えます");
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

