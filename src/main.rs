//! OpenSpeech の起動口。引数が無ければ画面を開き、診断の引数があればそれだけを行って終わる(`cli`)。

mod cli;

use cli::*;
use openspeech::app::{VoiceApp, APP_NAME};
use openspeech::core::{audio, mic_access, speechcpp};
use openspeech::ui::{theme, FULL_SIZE};
use std::path::PathBuf;

fn main() {
    speechcpp::keep_exit_code_on_main_panic();
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("{APP_NAME} {}\n{}", env!("CARGO_PKG_VERSION"), cli::usage());
        speechcpp::exit(0);
    }
    if args.iter().any(|a| a == "--version") {
        println!("{APP_NAME} {}", env!("CARGO_PKG_VERSION"));
        speechcpp::exit(0);
    }
    // 知らない `--` 引数は、画面を開く前に断る(打ち間違いの診断で画面を開いてしまわない)。
    if let Some(unknown) = args
        .iter()
        .find(|a| a.starts_with("--") && !cli::is_known_flag(a))
    {
        eprintln!("知らない引数です: {unknown}\n{}", cli::usage());
        speechcpp::exit(2);
    }
    if args.iter().any(|a| a == "--install-app") {
        // 場所を省くと利用者のアプリケーションフォルダ。
        let folder = argument_value(&args, "--install-app")
            .filter(|f| !f.starts_with("--"))
            .cloned()
            .or_else(|| {
                mic_access::applications_folder()
                    .ok()
                    .map(|p| p.display().to_string())
            })
            .unwrap_or_else(|| "Applications".to_string());
        speechcpp::exit(run_install_mode(&folder));
    }
    if args.iter().any(|a| a == "--list-devices") {
        for name in audio::input_device_names() {
            println!("{name}");
        }
        speechcpp::exit(0);
    }
    if args.iter().any(|a| a == "--add-to-dock") {
        speechcpp::exit(run_dock_mode());
    }
    if args.iter().any(|a| a == "--mic-check") {
        speechcpp::exit(run_mic_check(&args));
    }
    if args.iter().any(|a| a == "--segment-check") {
        speechcpp::exit(run_segment_check(&args));
    }
    if args.iter().any(|a| a == "--recognize-check") {
        speechcpp::exit(run_recognize_check(&args));
    }
    if args.iter().any(|a| a == "--latency-check") {
        speechcpp::exit(run_latency_check(&args));
    }
    if args.iter().any(|a| a == "--correct-check") {
        speechcpp::exit(run_correct_check(&args));
    }
    if args.iter().any(|a| a == "--complete-check") {
        speechcpp::exit(run_complete_check(&args));
    }
    if args.iter().any(|a| a == "--caps-check") {
        speechcpp::exit(run_caps_check(&args));
    }
    if args.iter().any(|a| a == "--transfer-check") {
        speechcpp::exit(run_transfer_check(&args));
    }
    // `--screenshot` に画像の場所が無ければ、画面を開かずに断る(撮るつもりで普通の画面を開かない)。
    if args.iter().any(|a| a == "--screenshot")
        && argument_value(&args, "--screenshot").is_none_or(|value| value.starts_with("--"))
    {
        eprintln!("--screenshot の後ろに画像の場所を書いてください\n{}", cli::usage());
        speechcpp::exit(2);
    }
    let screenshot = argument_value(&args, "--screenshot").map(PathBuf::from);
    let viewport = egui::ViewportBuilder::default()
        .with_title(APP_NAME)
        .with_inner_size(FULL_SIZE)
        .with_min_inner_size([420.0, 52.0])
        .with_icon(window_icon());
    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };
    let result = eframe::run_native(
        APP_NAME,
        options,
        Box::new(move |creation| {
            theme::install_fonts(&creation.egui_ctx);
            let mut app = VoiceApp::new(creation.egui_ctx.clone(), screenshot);
            apply_preview_arguments(&mut app, &args);
            if app.data.settings.appearance.compact_on_start && app.screenshot.is_none() {
                let ctx = creation.egui_ctx.clone();
                app.set_compact(true, &ctx);
            }
            Ok(Box::new(app))
        }),
    );
    // 画面を開けなかったときも 1 で終わる(`atexit` の受け皿が 0 にすり替えないよう、exit を通す)。
    match result {
        Ok(()) => speechcpp::exit(0),
        Err(error) => {
            eprintln!("画面を開けません: {error}");
            speechcpp::exit(1)
        }
    }
}
