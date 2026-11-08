//! アプリとしての導入と、OS とのやりとりの診断(`--install-app`・`--add-to-dock`・`--caps-check`・`--transfer-check`)。

use super::*;

/// `--install-app <フォルダ>`: 単独で開けるアプリを指定の場所に作る。
pub(crate) fn run_install_mode(folder: &str) -> i32 {
    match mic_access::install_app(std::path::Path::new(folder)) {
        Ok(bundle) => {
            println!("{}", bundle.display());
            eprintln!("{}", mic_access::signing_notice());
            0
        }
        Err(message) => {
            eprintln!("{message}");
            1
        }
    }
}

/// `--add-to-dock`: Dock に登録する。束の中から動いているならその束を、そうでなければ
/// アプリケーションフォルダに置いた束を登録する。
pub(crate) fn run_dock_mode() -> i32 {
    let result = mic_access::bundle_for_dock()
        .and_then(|bundle| mic_access::add_to_dock(&bundle).map(|added| (bundle, added)));
    match result {
        Ok((bundle, added)) => {
            println!("{}", bundle.display());
            println!(
                "{}",
                if added {
                    "Dock に追加しました"
                } else {
                    "すでに Dock にあります"
                }
            );
            0
        }
        Err(message) => {
            eprintln!("{message}");
            1
        }
    }
}

/// `--caps-check 秒`: Caps Lock と画面のロックの変化を印字する(画面は開かない)。
/// `--set-on` / `--set-off` なら Caps Lock をその状態にしてから見る。
pub(crate) fn run_caps_check(args: &[String]) -> i32 {
    if args.iter().any(|a| a == "--set-on" || a == "--set-off") {
        let on = args.iter().any(|a| a == "--set-on");
        println!("set({on}) = {}", caps_lock::set(on));
    }
    let seconds = argument_value(args, "--caps-check")
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(10);
    let start = Instant::now();
    let mut state = None;
    while start.elapsed() < Duration::from_secs(seconds) {
        let now = (caps_lock::is_on(), caps_lock::screen_locked());
        if state != Some(now) {
            state = Some(now);
            println!(
                "{:6.2}s Caps Lock: {} / 画面のロック: {}",
                start.elapsed().as_secs_f32(),
                if now.0 { "オン" } else { "オフ" },
                if now.1 { "あり" } else { "なし" }
            );
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    0
}

/// `--transfer-check 文 [--method auto|paste|type] [--wait 秒]`: 転送の確認。
/// 指定秒(既定 4)の間に入力先のアプリをクリックしてもらい、前面のアプリへ文を届ける。
pub(crate) fn run_transfer_check(args: &[String]) -> i32 {
    let Some(text) = argument_value(args, "--transfer-check").cloned() else {
        println!("使い方: --transfer-check 文 [--method auto|paste|type] [--wait 秒]");
        return 2;
    };
    let method = match argument_value(args, "--method").map(|m| m.as_str()) {
        Some("paste") => TransferMethod::Paste,
        Some("type") => TransferMethod::Type,
        _ => TransferMethod::Auto,
    };
    let wait = argument_value(args, "--wait")
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(4);
    println!("accessibility: {}", transfer::accessibility_trusted());
    if !transfer::accessibility_trusted() {
        println!("アクセシビリティの許可がありません。システム設定 → プライバシーとセキュリティ → アクセシビリティで許可してください");
        transfer::request_accessibility();
        return 3;
    }
    println!("{wait} 秒以内に入力先のアプリの入力欄をクリックしてください…");
    let me = transfer::own_pid();
    let deadline = Instant::now() + Duration::from_secs(wait);
    let mut target = None;
    while Instant::now() < deadline {
        if let Some(app) = transfer::frontmost_app() {
            if app.pid != me {
                target = Some(app);
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let Some(target) = target else {
        println!("入力先のアプリが前面になりませんでした");
        return 4;
    };
    println!(
        "target: {} ({}, pid {})",
        target.name, target.bundle_id, target.pid
    );
    let began = Instant::now();
    let result = transfer::send(TransferRequest {
        text: &text,
        target: Some(&target),
        method,
        restore_clipboard: true,
        enter_after: false,
    });
    match result {
        Ok(delivery) => {
            println!(
                "{:.2}s 転送しました({})",
                began.elapsed().as_secs_f32(),
                delivery.label()
            );
            0
        }
        Err(error) => {
            println!("{:.2}s エラー: {error}", began.elapsed().as_secs_f32());
            1
        }
    }
}
