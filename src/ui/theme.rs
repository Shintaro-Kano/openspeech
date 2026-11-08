//! 文字と色。配色は OpenRef/OpenNote と同じ系統(明るい灰の地、白いパネル、青いアクセント)で、
//! ライトとダークの両方を持つ。日本語フォントは OS のものを使う(アプリには同梱しない)。

use crate::core::settings::ThemeChoice;
use egui::{
    Color32, Context, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Response,
    RichText, Sense, Stroke, TextStyle, Theme, ThemePreference, Ui, Visuals,
};
use std::sync::Arc;

const FONT_CANDIDATES: [&str; 3] = [
    "/System/Library/Fonts/ヒラギノ角ゴシック W3.ttc",
    "/System/Library/Fonts/Hiragino Sans GB.ttc",
    "/Library/Fonts/Arial Unicode.ttf",
];

pub fn install_fonts(ctx: &Context) {
    let Some(bytes) = FONT_CANDIDATES
        .iter()
        .find_map(|path| std::fs::read(path).ok())
    else {
        return;
    };
    let mut fonts = FontDefinitions::default();
    // ヒラギノ等は字面が上に寄るので、部品の中央に来るよう少し下げる。
    let tweak = egui::FontTweak {
        y_offset_factor: 0.14,
        ..Default::default()
    };
    fonts.font_data.insert(
        "japanese".to_string(),
        Arc::new(FontData::from_owned(bytes).tweak(tweak)),
    );
    for family in [FontFamily::Proportional, FontFamily::Monospace] {
        fonts
            .families
            .entry(family)
            .or_default()
            .insert(0, "japanese".to_string());
    }
    ctx.set_fonts(fonts);
}

/// 画面の配色。
#[derive(Debug, Clone, Copy)]
pub struct Palette {
    pub bg: Color32,
    pub panel: Color32,
    pub border: Color32,
    pub text: Color32,
    pub dim: Color32,
    pub accent: Color32,
    pub accent_soft: Color32,
    pub on_accent: Color32,
    pub danger: Color32,
    pub success: Color32,
    pub warning: Color32,
    /// あやしい語(AI に「ここを疑え」と伝えた語)。
    pub unsure: Color32,
    /// AI や学習が直した語。
    pub corrected: Color32,
    /// 録音中のマイク。
    pub recording: Color32,
}

pub const LIGHT: Palette = Palette {
    bg: Color32::from_rgb(0xf6, 0xf7, 0xf8),
    panel: Color32::WHITE,
    border: Color32::from_rgb(0xdf, 0xe3, 0xe6),
    text: Color32::from_rgb(0x1c, 0x27, 0x33),
    dim: Color32::from_rgb(0x6b, 0x7a, 0x89),
    accent: Color32::from_rgb(0x1a, 0x73, 0xe8),
    accent_soft: Color32::from_rgb(0xe8, 0xf0, 0xfe),
    on_accent: Color32::WHITE,
    danger: Color32::from_rgb(0xc0, 0x39, 0x2b),
    success: Color32::from_rgb(0x1e, 0x8e, 0x3e),
    warning: Color32::from_rgb(0xb2, 0x6a, 0x00),
    unsure: Color32::from_rgb(0xb2, 0x6a, 0x00),
    corrected: Color32::from_rgb(0x1e, 0x8e, 0x3e),
    recording: Color32::from_rgb(0xd9, 0x30, 0x25),
};

pub const DARK: Palette = Palette {
    bg: Color32::from_rgb(0x1b, 0x1f, 0x24),
    panel: Color32::from_rgb(0x23, 0x28, 0x2e),
    border: Color32::from_rgb(0x36, 0x3d, 0x45),
    text: Color32::from_rgb(0xe4, 0xe9, 0xee),
    dim: Color32::from_rgb(0x8b, 0x98, 0xa5),
    accent: Color32::from_rgb(0x6a, 0xb0, 0xff),
    accent_soft: Color32::from_rgb(0x22, 0x36, 0x50),
    on_accent: Color32::from_rgb(0x0f, 0x17, 0x22),
    danger: Color32::from_rgb(0xff, 0x7b, 0x6b),
    success: Color32::from_rgb(0x5d, 0xd3, 0x7f),
    warning: Color32::from_rgb(0xf0, 0xb2, 0x4a),
    unsure: Color32::from_rgb(0xf0, 0xb2, 0x4a),
    corrected: Color32::from_rgb(0x5d, 0xd3, 0x7f),
    recording: Color32::from_rgb(0xff, 0x6b, 0x5e),
};

/// いまの明暗に合う配色。
pub fn palette(ctx: &Context) -> &'static Palette {
    if ctx.theme() == Theme::Dark {
        &DARK
    } else {
        &LIGHT
    }
}

pub const RADIUS: f32 = 6.0;

/// 明暗の希望と拡大率を適用し、部品の見た目をそろえる。
pub fn install_style(ctx: &Context, choice: ThemeChoice, scale: f32) {
    ctx.options_mut(|options| {
        options.theme_preference = match choice {
            ThemeChoice::System => ThemePreference::System,
            ThemeChoice::Light => ThemePreference::Light,
            ThemeChoice::Dark => ThemePreference::Dark,
        };
    });
    ctx.set_visuals_of(Theme::Light, visuals(&LIGHT, false));
    ctx.set_visuals_of(Theme::Dark, visuals(&DARK, true));
    ctx.all_styles_mut(|style| {
        style.spacing.item_spacing = egui::vec2(8.0, 6.0);
        style.spacing.button_padding = egui::vec2(12.0, 5.0);
        style.spacing.window_margin = egui::Margin::same(16);
        style.spacing.menu_margin = egui::Margin::same(6);
        style
            .text_styles
            .insert(TextStyle::Body, FontId::proportional(14.0));
        style
            .text_styles
            .insert(TextStyle::Button, FontId::proportional(14.0));
        style
            .text_styles
            .insert(TextStyle::Small, FontId::proportional(11.5));
        style
            .text_styles
            .insert(TextStyle::Heading, FontId::proportional(18.0));
        style
            .text_styles
            .insert(TextStyle::Monospace, FontId::monospace(13.0));
        style.visuals.indent_has_left_vline = false;
    });
    ctx.set_zoom_factor(scale.clamp(0.8, 1.6));
}

fn visuals(p: &Palette, dark: bool) -> Visuals {
    let mut v = if dark {
        Visuals::dark()
    } else {
        Visuals::light()
    };
    let radius = CornerRadius::same(RADIUS as u8);
    v.panel_fill = p.bg;
    v.window_fill = p.panel;
    v.extreme_bg_color = p.panel;
    v.faint_bg_color = p.bg;
    v.code_bg_color = p.bg;
    v.override_text_color = Some(p.text);
    v.hyperlink_color = p.accent;
    v.warn_fg_color = p.warning;
    v.error_fg_color = p.danger;
    v.window_corner_radius = CornerRadius::same(10);
    v.window_stroke = Stroke::new(1.0_f32, p.border);
    v.menu_corner_radius = radius;
    v.selection.bg_fill = if dark {
        Color32::from_rgba_unmultiplied(0x6a, 0xb0, 0xff, 90)
    } else {
        Color32::from_rgba_unmultiplied(0x1a, 0x73, 0xe8, 60)
    };
    v.selection.stroke = Stroke::new(1.0_f32, p.accent);
    v.text_cursor.stroke = Stroke::new(2.0_f32, p.accent);
    v.widgets.noninteractive.bg_fill = p.panel;
    v.widgets.noninteractive.weak_bg_fill = p.panel;
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0_f32, p.border);
    v.widgets.noninteractive.fg_stroke = Stroke::new(1.0_f32, p.text);
    v.widgets.noninteractive.corner_radius = radius;
    v.widgets.inactive.bg_fill = p.panel;
    v.widgets.inactive.weak_bg_fill = p.panel;
    v.widgets.inactive.bg_stroke = Stroke::new(1.0_f32, p.border);
    v.widgets.inactive.fg_stroke = Stroke::new(1.0_f32, p.text);
    v.widgets.inactive.corner_radius = radius;
    v.widgets.hovered.bg_fill = p.accent_soft;
    v.widgets.hovered.weak_bg_fill = p.accent_soft;
    v.widgets.hovered.bg_stroke = Stroke::new(1.0_f32, p.accent);
    v.widgets.hovered.fg_stroke = Stroke::new(1.5_f32, p.text);
    v.widgets.hovered.corner_radius = radius;
    v.widgets.active.bg_fill = p.accent_soft;
    v.widgets.active.weak_bg_fill = p.accent_soft;
    v.widgets.active.bg_stroke = Stroke::new(1.0_f32, p.accent);
    v.widgets.active.fg_stroke = Stroke::new(1.5_f32, p.accent);
    v.widgets.active.corner_radius = radius;
    v.widgets.open.bg_fill = p.accent_soft;
    v.widgets.open.weak_bg_fill = p.accent_soft;
    v.widgets.open.bg_stroke = Stroke::new(1.0_f32, p.accent);
    v.widgets.open.fg_stroke = Stroke::new(1.0_f32, p.text);
    v.widgets.open.corner_radius = radius;
    v
}

/// 白いパネルの枠(設定の各区画や一覧に使う)。
pub fn card(p: &Palette) -> egui::Frame {
    egui::Frame::new()
        .fill(p.panel)
        .stroke(Stroke::new(1.0_f32, p.border))
        .corner_radius(CornerRadius::same(8))
        .inner_margin(egui::Margin::same(12))
}

/// 塗りのある主役のボタン(転送など)の部品。`ui.add_enabled` と組み合わせられる。
pub fn primary(label: &str, p: &Palette) -> egui::Button<'static> {
    egui::Button::new(RichText::new(label).color(p.on_accent).strong())
        .fill(p.accent)
        .stroke(Stroke::NONE)
        .corner_radius(CornerRadius::same(RADIUS as u8))
}

/// 塗りのある主役のボタン(転送など)。
pub fn primary_button(ui: &mut Ui, label: &str, p: &Palette) -> Response {
    ui.add(primary(label, p))
}

/// 枠も塗りも無い文字だけのボタン。乗せると薄い青になる。
pub fn ghost_button(ui: &mut Ui, label: &str, p: &Palette) -> Response {
    ghost_button_sized(ui, label, p, TextStyle::Button, egui::vec2(14.0, 10.0))
}

pub fn ghost_button_sized(
    ui: &mut Ui,
    label: &str,
    p: &Palette,
    style: TextStyle,
    padding: egui::Vec2,
) -> Response {
    let font = style.resolve(ui.style());
    let galley = ui.painter().layout_no_wrap(label.to_string(), font, p.text);
    let size = galley.size() + padding;
    let (rect, response) = ui.allocate_exact_size(size, Sense::click());
    if ui.is_rect_visible(rect) {
        if response.hovered() || response.is_pointer_button_down_on() {
            ui.painter().rect_filled(rect, RADIUS, p.accent_soft);
        }
        let color = if response.hovered() { p.accent } else { p.text };
        let pos = rect.center() - galley.size() / 2.0;
        ui.painter().galley(pos, galley, color);
    }
    response
}

/// 小さな丸い札(入力の仕方や iPhone の状態の表示に使う)。クリックできる。
pub fn chip(ui: &mut Ui, label: &str, p: &Palette, selected: bool) -> Response {
    let font = TextStyle::Small.resolve(ui.style());
    let galley = ui.painter().layout_no_wrap(label.to_string(), font, p.text);
    let size = galley.size() + egui::vec2(16.0, 8.0);
    let (rect, response) = ui.allocate_exact_size(size, Sense::click());
    if ui.is_rect_visible(rect) {
        let (fill, stroke, color) = if selected || response.hovered() {
            (p.accent_soft, p.accent, p.accent)
        } else {
            (p.bg, p.border, p.dim)
        };
        ui.painter().rect(
            rect,
            rect.height() / 2.0,
            fill,
            Stroke::new(1.0_f32, stroke),
            egui::StrokeKind::Inside,
        );
        let pos = rect.center() - galley.size() / 2.0;
        ui.painter().galley(pos, galley, color);
    }
    response
}

/// 区画の見出し。
pub fn section(ui: &mut Ui, title: &str, p: &Palette) {
    ui.add_space(4.0);
    ui.label(RichText::new(title).strong().color(p.text));
    ui.add_space(2.0);
}

/// 補足の文(薄い色)。
pub fn hint(ui: &mut Ui, text: &str, p: &Palette) {
    ui.label(RichText::new(text).small().color(p.dim));
}

/// 目盛のある横のつまみ(スライダー)の仕様。両端の意味を言葉で添えられる。
pub struct SliderSpec {
    pub min: f64,
    pub max: f64,
    /// 1 段の大きさ。値はこの段に丸める。
    pub step: f64,
    /// 値の表示の小数の桁。
    pub decimals: usize,
    /// 値の後ろに付ける単位。
    pub suffix: &'static str,
    /// 左端・右端の意味。空なら最小・最大の値を出す。
    pub low: &'static str,
    pub high: &'static str,
}

impl SliderSpec {
    /// 値を段に丸め、範囲に収める。小数の桁でも丸め、0.30000001 のような表示を防ぐ。
    pub fn snap(&self, value: f64) -> f64 {
        let steps = ((value - self.min) / self.step).round();
        let snapped = (self.min + steps * self.step).clamp(self.min, self.max);
        let scale = 10f64.powi(self.decimals as i32);
        (snapped * scale).round() / scale
    }

    pub fn format(&self, value: f64) -> String {
        format!("{:.*}{}", self.decimals, value, self.suffix)
    }

    fn end_label(&self, label: &str, value: f64) -> String {
        if label.is_empty() {
            self.format(value)
        } else {
            label.to_string()
        }
    }
}

/// つまみで動かせる数(f32・u32・u64・usize)。
pub trait SliderValue {
    fn to_f64(&self) -> f64;
    fn set_f64(&mut self, value: f64);
}

impl SliderValue for f32 {
    fn to_f64(&self) -> f64 {
        f64::from(*self)
    }
    fn set_f64(&mut self, value: f64) {
        *self = value as f32;
    }
}

impl SliderValue for u16 {
    fn to_f64(&self) -> f64 {
        f64::from(*self)
    }
    fn set_f64(&mut self, value: f64) {
        *self = value.round().clamp(0.0, f64::from(u16::MAX)) as u16;
    }
}

impl SliderValue for u32 {
    fn to_f64(&self) -> f64 {
        f64::from(*self)
    }
    fn set_f64(&mut self, value: f64) {
        *self = value.round().max(0.0) as u32;
    }
}

impl SliderValue for u64 {
    fn to_f64(&self) -> f64 {
        *self as f64
    }
    fn set_f64(&mut self, value: f64) {
        *self = value.round().max(0.0) as u64;
    }
}

impl SliderValue for usize {
    fn to_f64(&self) -> f64 {
        *self as f64
    }
    fn set_f64(&mut self, value: f64) {
        *self = value.round().max(0.0) as usize;
    }
}

const SLIDER_WIDTH: f32 = 200.0;
const SLIDER_HEIGHT: f32 = 24.0;
const RAIL_HEIGHT: f32 = 4.0;
const HANDLE_RADIUS: f32 = 7.0;
/// 段の数がこれ以下なら、段ごとに目盛の線を引く。
const MAX_TICKS: f64 = 12.0;

/// 目盛のある横のつまみ。灰色の溝の左端から値までを青く塗り、丸いつまみを置く。
/// 押した位置へ動き、ドラッグで追い、選んでいる間は ← → で 1 段ずつ動く(矢印でほかの部品へ移らないよう留める)。
/// `label` は読み上げ用の項目名。変わったら true。
pub fn slider<T: SliderValue>(
    ui: &mut Ui,
    label: &str,
    value: &mut T,
    spec: &SliderSpec,
    p: &Palette,
) -> bool {
    let (rect, mut response) = ui.allocate_exact_size(
        egui::vec2(SLIDER_WIDTH, SLIDER_HEIGHT),
        Sense::click_and_drag(),
    );
    let rail = egui::Rect::from_x_y_ranges(
        rect.left() + HANDLE_RADIUS..=rect.right() - HANDLE_RADIUS,
        rect.center().y - RAIL_HEIGHT / 2.0..=rect.center().y + RAIL_HEIGHT / 2.0,
    );
    let before = spec.snap(value.to_f64());
    let mut next = before;
    if response.is_pointer_button_down_on() || response.dragged() {
        if let Some(pos) = response.interact_pointer_pos() {
            let t = f64::from(((pos.x - rail.left()) / rail.width()).clamp(0.0, 1.0));
            next = spec.min + t * (spec.max - spec.min);
        }
        response.request_focus();
    }
    if response.has_focus() {
        ui.memory_mut(|memory| {
            memory.set_focus_lock_filter(
                response.id,
                egui::EventFilter {
                    horizontal_arrows: true,
                    ..Default::default()
                },
            );
        });
        let (left, right) = ui.input_mut(|i| {
            (
                i.count_and_consume_key(egui::Modifiers::NONE, egui::Key::ArrowLeft),
                i.count_and_consume_key(egui::Modifiers::NONE, egui::Key::ArrowRight),
            )
        });
        next += spec.step * (right as f64 - left as f64);
    }
    let next = spec.snap(next);
    let changed = next != before;
    if changed {
        value.set_f64(next);
        response.mark_changed();
    }
    let shown = spec.snap(value.to_f64());
    response.widget_info(|| {
        egui::WidgetInfo::slider(true, shown, format!("{label} {}", spec.format(shown)))
    });
    if ui.is_rect_visible(rect) {
        let painter = ui.painter();
        let span = (spec.max - spec.min).max(f64::EPSILON);
        let t = ((shown - spec.min) / span).clamp(0.0, 1.0) as f32;
        let x = rail.left() + rail.width() * t;
        painter.rect_filled(rail, RAIL_HEIGHT / 2.0, p.border);
        painter.rect_filled(
            egui::Rect::from_min_max(rail.min, egui::pos2(x, rail.max.y)),
            RAIL_HEIGHT / 2.0,
            p.accent,
        );
        let steps = (span / spec.step).round();
        if (2.0..=MAX_TICKS).contains(&steps) {
            for k in 0..=steps as usize {
                let tick_x = rail.left() + rail.width() * k as f32 / steps as f32;
                painter.line_segment(
                    [
                        egui::pos2(tick_x, rail.bottom() + 3.0),
                        egui::pos2(tick_x, rail.bottom() + 6.0),
                    ],
                    Stroke::new(1.0_f32, p.border),
                );
            }
        }
        let active = response.hovered() || response.dragged() || response.has_focus();
        let fill = if active { p.accent_soft } else { p.panel };
        painter.circle(
            egui::pos2(x, rail.center().y),
            HANDLE_RADIUS,
            fill,
            Stroke::new(2.0_f32, p.accent),
        );
    }
    changed
}

/// ラベル、左端の意味、つまみ、右端の意味、今の値を 1 行に並べる。
pub fn slider_row<T: SliderValue>(
    ui: &mut Ui,
    label: &str,
    value: &mut T,
    spec: &SliderSpec,
    p: &Palette,
) -> bool {
    let mut changed = false;
    ui.horizontal(|ui| {
        ui.label(label);
        ui.add_space(4.0);
        ui.label(
            RichText::new(spec.end_label(spec.low, spec.min))
                .small()
                .color(p.dim),
        );
        changed = slider(ui, label, value, spec, p);
        ui.label(
            RichText::new(spec.end_label(spec.high, spec.max))
                .small()
                .color(p.dim),
        );
        ui.add_space(6.0);
        // 段の上に無い古い値(手で書いた設定など)も、つまみと同じ丸めた値で出す。
        ui.label(RichText::new(spec.format(spec.snap(value.to_f64()))).strong());
    });
    changed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slider_values_snap_to_the_step_inside_the_range() {
        let gain = SliderSpec {
            min: 0.5,
            max: 8.0,
            step: 0.1,
            decimals: 1,
            suffix: " 倍",
            low: "",
            high: "",
        };
        assert_eq!(gain.snap(0.34), 0.5, "範囲の下に丸まれば最小");
        assert_eq!(gain.snap(2.26), 2.3);
        assert_eq!(gain.snap(9.0), 8.0);
        assert_eq!(gain.snap(0.1), 0.5);
        assert_eq!(gain.format(2.3), "2.3 倍");
        assert_eq!(gain.end_label("", 8.0), "8.0 倍");
        assert_eq!(gain.end_label("大きく", 8.0), "大きく");
        let silence = SliderSpec {
            min: 300.0,
            max: 1200.0,
            step: 50.0,
            decimals: 0,
            suffix: " ms",
            low: "速い",
            high: "切れにくい",
        };
        assert_eq!(silence.snap(624.0), 600.0);
        assert_eq!(silence.snap(626.0), 650.0);
        let mut value: u32 = 600;
        value.set_f64(silence.snap(1300.0));
        assert_eq!(value, 1200);
        let mut scale: f32 = 1.0;
        scale.set_f64(0.85);
        assert_eq!(scale, 0.85);
    }

    #[test]
    fn light_and_dark_text_contrast_with_their_backgrounds() {
        let luminance = |c: Color32| {
            let [r, g, b, _] = c.to_array();
            0.2126 * r as f32 + 0.7152 * g as f32 + 0.0722 * b as f32
        };
        assert!(luminance(LIGHT.bg) - luminance(LIGHT.text) > 150.0);
        assert!(luminance(DARK.text) - luminance(DARK.bg) > 150.0);
    }
}
