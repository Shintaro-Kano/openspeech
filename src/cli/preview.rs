//! 画面の撮影(`--screenshot`)で使う `--open`・`--say` と、窓のアイコン。

use super::*;

/// 画面確認用の `--open 画面名` と `--say 文章`(複数可)を適用する。
pub(crate) fn apply_preview_arguments(app: &mut VoiceApp, args: &[String]) {
    for pair in args.windows(2) {
        if pair[0] == "--say" {
            let recognition = Recognition {
                text: pair[1].clone(),
                unsure: Vec::new(),
            };
            app.handle_utterance(recognition, None);
        }
    }
    if let Some(window) = argument_value(args, "--open") {
        app.open_window_by_name(window);
    }
}

/// ウィンドウのアイコン。
pub(crate) fn window_icon() -> egui::IconData {
    let (width, height, rgba) = mic_access::window_icon();
    egui::IconData {
        rgba,
        width,
        height,
    }
}
