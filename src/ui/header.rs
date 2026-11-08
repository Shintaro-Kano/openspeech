//! ヘッダー: マイクのボタンと状態、音量、道具への入口。
//! 「小さく表示」ではこの行だけの細長い窓になり、途中結果と転送先を見せる。

use super::theme::{self, Palette};
use crate::app::{MicState, VoiceApp, APP_NAME};
use crate::core::segmenter::{threshold_for_sensitivity, DEFAULT_SENSITIVITY};
use crate::core::settings::REQUEST_LISTEN_RANGE;
use egui::{
    pos2, vec2, Align, Color32, Context, Layout, Painter, Pos2, Rect, RichText, Sense, Shape,
    Stroke, TextStyle,
};
use std::sync::atomic::Ordering;
use std::time::Instant;

const FULL_HEIGHT: f32 = 56.0;
const MIC_DIAMETER: f32 = 36.0;
const METER_WIDTH: f32 = 104.0;

impl VoiceApp {
    pub fn draw_header(&mut self, ctx: &Context) {
        let p = theme::palette(ctx);
        let height = if self.compact {
            super::COMPACT_SIZE.y
        } else {
            FULL_HEIGHT
        };
        egui::TopBottomPanel::top("header")
            .exact_height(height)
            .frame(
                egui::Frame::new()
                    .fill(p.panel)
                    .inner_margin(egui::Margin::symmetric(12, 0)),
            )
            .show(ctx, |ui| {
                let clip = ui.clip_rect();
                ui.painter().hline(
                    clip.x_range(),
                    clip.bottom() - 0.5,
                    Stroke::new(1.0_f32, p.border),
                );
                let rect = ui.max_rect();
                let response =
                    ui.interact(rect, ui.id().with("header-back"), Sense::click_and_drag());
                if self.compact && response.dragged() {
                    ctx.send_viewport_cmd(egui::ViewportCommand::StartDrag);
                }
                response.context_menu(|ui| self.header_menu(ui, ctx));
                ui.horizontal_centered(|ui| {
                    ui.spacing_mut().item_spacing.x = 10.0;
                    self.draw_mic_button(ui, p);
                    self.draw_state(ui, p);
                    if !self.compact {
                        ui.add_space(6.0);
                        ui.label(RichText::new(APP_NAME).strong().size(17.0).color(p.text));
                    }
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if self.compact {
                            self.draw_compact_right(ui, ctx, p);
                        } else {
                            self.draw_tools(ui, ctx, p);
                        }
                    });
                });
            });
    }

    fn draw_mic_button(&mut self, ui: &mut egui::Ui, p: &Palette) {
        let (rect, response) =
            ui.allocate_exact_size(vec2(MIC_DIAMETER, MIC_DIAMETER), Sense::click());
        let source = self.mic_source_note();
        let key = self.data.settings.record.shortcut.label();
        let tip = match self.mic {
            MicState::Off => format!("マイクを入れる({key})"),
            MicState::On => format!("マイクを止める({key}){source}"),
        };
        let response = response.on_hover_text(tip);
        if response.clicked() {
            self.toggle_mic();
        }
        let speaking = self.session.live.speaking.load(Ordering::Relaxed);
        let (fill, glyph, ring) = match self.mic {
            MicState::Off => (p.bg, p.dim, p.border),
            MicState::On => (p.recording, Color32::WHITE, p.recording),
        };
        let painter = ui.painter();
        let center = rect.center();
        if speaking {
            painter.circle_stroke(
                center,
                MIC_DIAMETER / 2.0 + 3.0,
                Stroke::new(2.0_f32, p.recording.gamma_multiply(0.45)),
            );
        }
        let ring = if response.hovered() && self.mic == MicState::Off {
            p.accent
        } else {
            ring
        };
        painter.circle(center, MIC_DIAMETER / 2.0, fill, Stroke::new(1.0_f32, ring));
        paint_mic_glyph(painter, center, MIC_DIAMETER * 0.5, glyph);
    }

    /// 状態の文字と音量メーター。
    fn draw_state(&mut self, ui: &mut egui::Ui, p: &Palette) {
        let live = self.session.live.clone();
        let speaking = live.speaking.load(Ordering::Relaxed);
        let now = Instant::now();
        let (text, color) = if let Some(elapsed) = self.completing_for(now) {
            // 「ヘイ、クロード」の依頼を送っている間(マイクを止めていても答えは受ける)。経過は切り捨て。
            (format!("Claude に依頼中({} 秒)", elapsed.as_secs()), p.accent)
        } else if self.request_listening() {
            // 時間が来て、話している途中の発話を待っている間は秒数を出さない。
            match self.listen_seconds_left(now) {
                Some(left) => (format!("依頼を聞き取り中(あと {left} 秒)"), p.success),
                None => ("依頼を聞き取り中".to_string(), p.success),
            }
        } else if let Some(left) = self.call_wait_seconds_left(now) {
            (format!("依頼をどうぞ(あと {left} 秒)"), p.accent)
        } else {
            let (text, color) = match self.mic {
                MicState::Off => ("待機中", p.dim),
                MicState::On if self.iphone_wait_seconds().is_some() => ("接続待ち", p.warning),
                MicState::On if speaking => ("聞き取り中", p.success),
                MicState::On if live.pending() > 0 => ("文字にしています", p.accent),
                MicState::On if self.pending_corrections() > 0 => ("AI 訂正中", p.accent),
                MicState::On => ("録音中", p.recording),
            };
            (text.to_string(), color)
        };
        // 秒数の付いた文はメーターより長いので、折り返さずに組む。待ち・聞き取り・依頼中の間は、数が変わっても
        // 右側が動かないよう、いちばん長い文(最長の聞き取りの初め)の幅に揃える。
        let galley = small_galley(ui, text, color);
        let width = if self.call_waiting() || self.completing() {
            let widest = format!("依頼を聞き取り中(あと {} 秒)", REQUEST_LISTEN_RANGE.end());
            METER_WIDTH.max(small_galley(ui, widest, color).size().x)
        } else {
            METER_WIDTH
        }
        .max(galley.size().x);
        let level = live.level();
        // 録音中は雑音の床に応じて動く基準を、止まっているときは静かな場所での基準を印にする。
        let live_threshold = live.threshold();
        let threshold = if self.mic != MicState::Off && live_threshold > 0.0 {
            live_threshold
        } else {
            threshold_for_sensitivity(DEFAULT_SENSITIVITY)
        };
        let active = self.mic != MicState::Off;
        ui.allocate_ui_with_layout(
            vec2(width, 34.0),
            Layout::top_down(Align::Min),
            |ui| {
                ui.spacing_mut().item_spacing.y = 5.0;
                ui.label(galley);
                let (rect, _) = ui.allocate_exact_size(vec2(METER_WIDTH, 6.0), Sense::hover());
                paint_meter(ui.painter(), rect, level, threshold, active, p);
            },
        );
    }

    /// 録音中のデバイスを添える文。既定のマイクなら空。
    fn mic_source_note(&self) -> String {
        match self.active_device_label() {
            Some(name) if self.using_iphone() => format!(" ・ iPhone({name})"),
            Some(name) => format!(" ・ {name}"),
            None if self.iphone_gave_up() => " ・ Mac のマイク(iPhone から音が届きません)".to_string(),
            None if self.global.prefer_iphone && self.mic != MicState::Off => {
                " ・ Mac のマイク(iPhone 未検出)".to_string()
            }
            None => String::new(),
        }
    }

    /// 右側の道具(右から順に置く)。
    fn draw_tools(&mut self, ui: &mut egui::Ui, ctx: &Context, p: &Palette) {
        ui.spacing_mut().item_spacing.x = 2.0;
        if theme::ghost_button(ui, "小さく", p)
            .on_hover_text("この行だけの小さな窓にする(常に手前)")
            .clicked()
        {
            self.set_compact(true, ctx);
        }
        if theme::ghost_button(ui, "ヘルプ", p).clicked() {
            self.windows.help = true;
        }
        if theme::ghost_button(ui, "設定", p).clicked() {
            self.open_settings(None);
        }
        if theme::ghost_button(ui, "辞書", p)
            .on_hover_text("単語・テンプレート・学習した置き換え")
            .clicked()
        {
            self.open_dict_tool(None);
        }
        if theme::ghost_button(ui, "単語登録", p)
            .on_hover_text("認識されにくい語を登録する(選択中の語があればそれを入れる)")
            .clicked()
        {
            self.add_word_from_selection();
        }
        ui.add_space(10.0);
        ui.spacing_mut().item_spacing.x = 6.0;
        if self.global.prefer_iphone {
            let (label, tip) = if self.using_iphone() {
                ("iPhone ●", "iPhone のマイクを使っています")
            } else if self.iphone_gave_up() {
                (
                    "iPhone ×",
                    "iPhone から音が届かないため Mac のマイクで録っています(マイクを入れ直すと試し直します)",
                )
            } else if self.iphone_found().is_some() {
                (
                    "iPhone ○",
                    "iPhone のマイクが見つかっています(マイクを入れると使います)",
                )
            } else {
                (
                    "iPhone ×",
                    "iPhone のマイクが見つかりません(クリックで接続方法)",
                )
            };
            if theme::chip(ui, label, p, self.using_iphone())
                .on_hover_text(tip)
                .clicked()
            {
                self.windows.iphone_help = true;
            }
        }
    }

    /// 小さな窓のときの右側: 広げるボタンと、途中結果か転送先。
    fn draw_compact_right(&mut self, ui: &mut egui::Ui, ctx: &Context, p: &Palette) {
        if theme::ghost_button_sized(ui, "広げる", p, TextStyle::Small, vec2(12.0, 8.0))
            .on_hover_text("エディターを表示")
            .clicked()
        {
            self.set_compact(false, ctx);
        }
        let (text, color) = if !self.live_caption.is_empty() {
            (format!("{}…", self.live_caption), p.text)
        } else if self.iphone_wait_seconds().is_some() {
            ("iPhone に接続しています…".to_string(), p.dim)
        } else if self.mic == MicState::On {
            ("話しかけてください".to_string(), p.dim)
        } else if let Some(target) = self.current_target() {
            (format!("転送先: {}", target.name), p.dim)
        } else {
            ("転送先が決まっていません".to_string(), p.dim)
        };
        ui.add(egui::Label::new(RichText::new(text).color(color)).truncate());
    }

    fn header_menu(&mut self, ui: &mut egui::Ui, ctx: &Context) {
        if ui.button("音声ファイルから認識").clicked() {
            self.recognize_from_file();
            ui.close_menu();
        }
        ui.separator();
        let mut prefer_iphone = self.global.prefer_iphone;
        if ui
            .checkbox(&mut prefer_iphone, "iPhone をマイクにする")
            .changed()
        {
            self.set_prefer_iphone(prefer_iphone);
            ui.close_menu();
        }
        if ui.button("iPhone マイクの接続方法").clicked() {
            self.windows.iphone_help = true;
            ui.close_menu();
        }
        ui.separator();
        if cfg!(target_os = "macos") {
            // アプリとして動いているなら、もう置いてある(can_relaunch_as_app が偽)。
            // ここから作るともう 1 つの束ができる。
            if crate::core::mic_access::can_relaunch_as_app()
                && ui.button("アプリケーションフォルダに追加").clicked()
            {
                self.install_to_applications();
                ui.close_menu();
            }
            if ui.button("Dock に追加").clicked() {
                self.add_to_dock();
                ui.close_menu();
            }
            ui.separator();
        }
        if ui.button(format!("{APP_NAME} について")).clicked() {
            self.windows.about = true;
            ui.close_menu();
        }
        if ui.button("終了").clicked() {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            ui.close_menu();
        }
    }
}

fn paint_mic_glyph(painter: &Painter, center: Pos2, size: f32, color: Color32) {
    let capsule = Rect::from_center_size(
        center - vec2(0.0, size * 0.14),
        vec2(size * 0.42, size * 0.72),
    );
    painter.rect_filled(capsule, size * 0.21, color);
    let stroke = Stroke::new((size * 0.09).max(1.0), color);
    let bottom = center + vec2(0.0, size * 0.42);
    painter.line_segment([center + vec2(0.0, size * 0.28), bottom], stroke);
    painter.line_segment(
        [
            bottom - vec2(size * 0.2, 0.0),
            bottom + vec2(size * 0.2, 0.0),
        ],
        stroke,
    );
    let cup: Vec<Pos2> = (0..=12)
        .map(|i| {
            let angle = std::f32::consts::PI * i as f32 / 12.0;
            center - vec2(0.0, size * 0.04)
                + vec2(angle.cos() * size * 0.36, angle.sin() * size * 0.3)
        })
        .collect();
    painter.add(Shape::line(cup, stroke));
}

/// 音量メーター。丸い細い棒で、声とみなす基準の位置に印を付ける。
fn paint_meter(
    painter: &Painter,
    rect: Rect,
    level: f32,
    threshold: f32,
    active: bool,
    p: &Palette,
) {
    let radius = rect.height() / 2.0;
    painter.rect_filled(rect, radius, p.border);
    if !active {
        return;
    }
    let position = |value: f32| ((value.max(1e-4).log10() + 3.0) / 3.0).clamp(0.0, 1.0);
    let width = rect.width() * position(level);
    if width > 0.5 {
        let color = if level >= threshold {
            p.success
        } else {
            p.accent
        };
        let bar = Rect::from_min_size(rect.min, vec2(width.max(rect.height()), rect.height()));
        painter.rect_filled(bar, radius, color);
    }
    let x = rect.left() + rect.width() * position(threshold);
    painter.line_segment(
        [pos2(x, rect.top() - 2.0), pos2(x, rect.bottom() + 2.0)],
        Stroke::new(1.5_f32, p.dim),
    );
}

/// 状態の文字の小さい字を、折り返さずに組む。
fn small_galley(ui: &egui::Ui, text: String, color: Color32) -> std::sync::Arc<egui::Galley> {
    egui::WidgetText::from(RichText::new(text).small().color(color)).into_galley(
        ui,
        Some(egui::TextWrapMode::Extend),
        f32::INFINITY,
        TextStyle::Small,
    )
}
