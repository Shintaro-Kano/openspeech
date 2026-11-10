//! 画面。上にヘッダー(マイク・状態・道具)、中にエディター、下に状態行。
//! 「小さく表示」でヘッダーだけの細長い窓にもできる。

mod dialogs;
mod dict_tool;
mod editor;
mod header;
pub mod library;
mod rows;
mod settings;
mod structure_view;
pub mod theme;

use crate::app::{MicState, VoiceApp, SCREENSHOT_FRAMES};
use crate::core::dictionary::modules_text;
use egui::{Context, RichText};
use std::time::{Duration, Instant};

pub use dialogs::AddWordForm;
pub use dict_tool::{DictTab, DictTool};
pub use library::LibraryWindow;
pub use settings::SettingsDialog;

/// 開いている補助ウィンドウ。
#[derive(Default)]
pub struct Windows {
    pub add_word: Option<AddWordForm>,
    pub dict: Option<DictTool>,
    /// 「共有ライブラリから取り込む…」の窓。
    pub library: Option<LibraryWindow>,
    pub settings: Option<SettingsDialog>,
    pub help: bool,
    pub mic_help: bool,
    /// iPhone をマイクにする方法の案内。
    pub iphone_help: bool,
    /// 転送に必要なアクセシビリティの許可の案内。
    pub accessibility_help: bool,
    pub about: bool,
    /// 初めて開いたときの案内(マイク・モデル・転送の許可・AI 訂正の鍵)。
    pub first_run: bool,
    pub confirm_close: bool,
    pub confirm_quit: bool,
    /// 起動時の読み込みで起きた問題の知らせ。
    pub startup_warning: Option<String>,
}

impl Windows {
    /// 補助ウィンドウが開いているか(開いている間はエディターの Esc・⌘Enter などを横取りしない)。
    pub fn any_open(&self) -> bool {
        self.add_word.is_some()
            || self.dict.is_some()
            || self.library.is_some()
            || self.settings.is_some()
            || self.help
            || self.mic_help
            || self.iphone_help
            || self.accessibility_help
            || self.about
            || self.first_run
            || self.confirm_close
            || self.confirm_quit
            || self.startup_warning.is_some()
    }
}

/// ヘッダーだけのときのウィンドウの大きさ。
pub const COMPACT_SIZE: egui::Vec2 = egui::vec2(560.0, 52.0);
pub const FULL_SIZE: egui::Vec2 = egui::vec2(920.0, 640.0);
const STATUS_HEIGHT: f32 = 28.0;

impl eframe::App for VoiceApp {
    fn update(&mut self, ctx: &Context, _frame: &mut eframe::Frame) {
        self.poll_events();
        self.restore_window_size(ctx);
        self.guard_close(ctx);
        self.drive_screenshot(ctx);
        self.draw_header(ctx);
        if !self.compact {
            self.draw_status(ctx);
            self.draw_editor(ctx);
        } else {
            egui::CentralPanel::default()
                .frame(egui::Frame::NONE)
                .show(ctx, |_| {});
        }
        self.draw_windows(ctx);
        // 全体ショートカットはイベントが来ないと気づけないので、定期的に起きる。
        // Caps Lock は見張り役が変化のたびに起こす。
        ctx.request_repaint_after(Duration::from_millis(100));
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.shutdown();
    }
}

impl VoiceApp {
    /// 前回を小さな表示のまま終えると、記憶された窓の大きさが細長いまま通常の表示で開く。初回の描画で直す。
    fn restore_window_size(&mut self, ctx: &Context) {
        if self.size_checked {
            return;
        }
        self.size_checked = true;
        let rect = ctx.input(|input| input.viewport().inner_rect);
        if !self.compact && rect.is_some_and(|r| r.height() < COMPACT_SIZE.y * 3.0) {
            ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(FULL_SIZE));
        }
    }

    /// 文章が残ったまま終了しようとしたら止めて確認する。
    fn guard_close(&mut self, ctx: &Context) {
        let requested = ctx.input(|input| input.viewport().close_requested());
        let unsaved = !self.editor.is_empty() && self.data.settings.editor.confirm_close;
        if requested && unsaved && !self.allow_close && self.screenshot.is_none() {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.windows.confirm_quit = true;
        }
    }

    /// `--screenshot` の処理。規定フレーム描いたら保存して終了する。
    fn drive_screenshot(&mut self, ctx: &Context) {
        let Some(job) = &mut self.screenshot else {
            return;
        };
        ctx.request_repaint();
        job.frames += 1;
        if job.frames >= SCREENSHOT_FRAMES && !job.requested {
            job.requested = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(egui::UserData::default()));
        }
        let image = ctx.input(|input| {
            input.events.iter().find_map(|event| match event {
                egui::Event::Screenshot { image, .. } => Some(image.clone()),
                _ => None,
            })
        });
        if let Some(image) = image {
            let path = job.path.clone();
            if let Some(folder) = path.parent() {
                let _ = std::fs::create_dir_all(folder);
            }
            let [width, height] = image.size;
            let bytes: Vec<u8> = image.pixels.iter().flat_map(|p| p.to_array()).collect();
            let saved = image::save_buffer(
                &path,
                &bytes,
                width as u32,
                height as u32,
                image::ColorType::Rgba8,
            );
            if let Err(error) = saved {
                eprintln!("画像を保存できません: {error}");
                crate::core::speechcpp::exit(1);
            }
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }

    /// 下の状態行: エンジン・AI 訂正・マイク・転送先・許可・知らせ。
    fn draw_status(&mut self, ctx: &Context) {
        if self
            .notice
            .as_ref()
            .is_some_and(|n| Instant::now() >= n.until)
        {
            self.notice = None;
        }
        let p = theme::palette(ctx);
        let live = self.session.live.clone();
        let too_loud = live.too_loud.load(std::sync::atomic::Ordering::Relaxed);
        let target = self.current_target();
        let mut open_accessibility = false;
        egui::TopBottomPanel::bottom("status")
            .exact_height(STATUS_HEIGHT)
            .frame(
                egui::Frame::new()
                    .fill(p.panel)
                    .stroke(egui::Stroke::new(1.0_f32, p.border))
                    .inner_margin(egui::Margin::symmetric(12, 0)),
            )
            .show(ctx, |ui| {
                ui.horizontal_centered(|ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    let dot = |ui: &mut egui::Ui| {
                        ui.label(RichText::new("·").small().color(p.dim));
                    };
                    match &self.engine_name {
                        Some(name) => {
                            ui.label(RichText::new(name).small().color(p.dim));
                        }
                        None => {
                            ui.label(RichText::new("認識エンジンなし(キーボード入力のみ)").small().color(p.warning));
                        }
                    }
                    dot(ui);
                    match self.corrector_name() {
                        Some(name) => {
                            ui.label(RichText::new(format!("AI 訂正: {name}")).small().color(p.dim));
                        }
                        None => {
                            ui.label(RichText::new("AI 訂正: なし").small().color(p.dim))
                                .on_hover_text("設定の「AI 訂正」で接続すると、認識の誤りを AI が直します");
                        }
                    }
                    dot(ui);
                    let mic_text = match (self.mic, self.active_device_label()) {
                        (MicState::Off, _) => "マイク: 停止".to_string(),
                        _ if self.iphone_wait_seconds().is_some() => {
                            let seconds = self.iphone_wait_seconds().unwrap_or_default();
                            format!("マイク: iPhone(接続待ち {seconds} 秒)")
                        }
                        (_, Some(name)) if self.using_iphone() => format!("マイク: iPhone({name})"),
                        (_, Some(name)) => format!("マイク: {name}"),
                        (_, None) if self.global.prefer_iphone => "マイク: Mac(iPhone 未接続)".to_string(),
                        (_, None) => "マイク: Mac".to_string(),
                    };
                    let mic_color = if self.mic != MicState::Off && self.global.prefer_iphone && !self.using_iphone() {
                        p.warning
                    } else {
                        p.dim
                    };
                    ui.label(RichText::new(mic_text).small().color(mic_color));
                    dot(ui);
                    let target_text = target
                        .as_ref()
                        .map_or("転送先: (他のアプリをクリックすると決まります)".to_string(), |t| {
                            format!("転送先: {}", t.name)
                        });
                    ui.label(RichText::new(target_text).small().color(p.dim));
                    if !self.accessibility_ok {
                        dot(ui);
                        let warn = egui::Button::new(RichText::new("転送の許可がありません").small().color(p.on_accent))
                            .fill(p.danger)
                            .stroke(egui::Stroke::NONE)
                            .corner_radius(4.0);
                        if ui.add(warn).on_hover_text("クリックで許可の手順を表示").clicked() {
                            open_accessibility = true;
                        }
                    }
                    if too_loud && self.data.settings.editor.volume_warning {
                        dot(ui);
                        ui.label(RichText::new("声が大きすぎます").small().color(p.danger));
                    }
                    if let Some(notice) = &self.notice {
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            let color = if notice.is_error { p.danger } else { p.success };
                            ui.label(RichText::new(&notice.text).small().color(color));
                        });
                    }
                });
            });
        if open_accessibility {
            self.windows.accessibility_help = true;
        }
    }

    fn draw_windows(&mut self, ctx: &Context) {
        self.draw_add_word(ctx);
        self.draw_dict_tool(ctx);
        self.draw_template_library(ctx);
        self.draw_settings(ctx);
        self.draw_dialogs(ctx);
    }

    /// 画面確認用に、名前で補助ウィンドウを開く。
    pub fn open_window_by_name(&mut self, name: &str) {
        match name {
            "dict" => self.open_dict_tool(Some(DictTab::Words)),
            "templates" => self.open_dict_tool(Some(DictTab::Templates)),
            "learning" => self.open_dict_tool(Some(DictTab::Memory)),
            // 撮影の確認用: 文章と所見モジュールが最も長いテンプレートを編集中にして開く。
            "edit-template" => {
                self.open_dict_tool(Some(DictTab::Templates));
                let longest = self
                    .data
                    .dictionary
                    .templates
                    .iter()
                    .enumerate()
                    .max_by_key(|(_, t)| {
                        t.body.chars().count() + modules_text(&t.modules).chars().count()
                    })
                    .map(|(index, _)| index);
                if let (Some(tool), Some(index)) = (self.windows.dict.as_mut(), longest) {
                    tool.begin_edit(index, &self.data.dictionary.templates[index]);
                }
            }
            // 撮影の確認用: 構造の変種が最も多いテンプレートを編集中にして「構造」を開く。
            "structure" => {
                self.open_dict_tool(Some(DictTab::Templates));
                let busiest = self
                    .data
                    .dictionary
                    .templates
                    .iter()
                    .enumerate()
                    .max_by_key(|(_, t)| {
                        self.annotations.structures.structure_for(t).map_or(0, |s| s.variants.len())
                    })
                    .map(|(index, _)| index);
                if let (Some(tool), Some(index)) = (self.windows.dict.as_mut(), busiest) {
                    tool.begin_edit(index, &self.data.dictionary.templates[index]);
                    tool.open_structure = true;
                }
            }
            "help" => self.windows.help = true,
            "mic-help" => self.windows.mic_help = true,
            "iphone-help" => self.windows.iphone_help = true,
            "accessibility" => self.windows.accessibility_help = true,
            "about" => self.windows.about = true,
            "first-run" => self.windows.first_run = true,
            "add-word" => self.open_add_word(),
            "compact" => {
                let ctx = self.ctx.clone();
                self.set_compact(true, &ctx);
            }
            page => self.open_settings(Some(page)),
        }
    }

    /// ヘッダーだけの表示と通常の表示を切り替える。
    pub fn set_compact(&mut self, compact: bool, ctx: &Context) {
        self.compact = compact;
        let size = if compact { COMPACT_SIZE } else { FULL_SIZE };
        ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(size));
        let level = self.window_level();
        ctx.send_viewport_cmd(egui::ViewportCommand::WindowLevel(level));
    }
}
