//! エディター: 認識結果の表示と色分け、途中結果の行、キー操作。

use super::theme::{self, Palette};
use crate::app::{MicState, VoiceApp};
use crate::core::editor_model::{Span, SpanKind};
use egui::text::{CCursor, CCursorRange, LayoutJob, TextFormat};
use egui::{Color32, Context, FontId, Key, Modifiers, RichText, Stroke};
use std::sync::atomic::Ordering;
use std::sync::Arc;

const EDITOR_ID: &str = "dictation-editor";
const TOOLBAR_HEIGHT: f32 = 42.0;
const CAPTION_HEIGHT: f32 = 30.0;

impl VoiceApp {
    pub fn draw_editor(&mut self, ctx: &Context) {
        self.handle_editor_keys(ctx);
        let p = theme::palette(ctx);
        egui::TopBottomPanel::top("editor-toolbar")
            .exact_height(TOOLBAR_HEIGHT)
            .frame(
                egui::Frame::new()
                    .fill(p.bg)
                    .inner_margin(egui::Margin::symmetric(12, 0)),
            )
            .show(ctx, |ui| self.draw_toolbar(ui, ctx, p));
        self.draw_caption(ctx, p);
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(p.bg).inner_margin(egui::Margin {
                left: 12,
                right: 12,
                top: 0,
                bottom: 10,
            }))
            .show(ctx, |ui| {
                theme::card(p)
                    .inner_margin(egui::Margin::same(14))
                    .show(ui, |ui| {
                        ui.set_min_size(ui.available_size());
                        self.draw_text_area(ui, p);
                    });
            });
    }

    fn handle_editor_keys(&mut self, ctx: &Context) {
        let pressed = |modifiers, key| ctx.input_mut(|i| i.consume_key(modifiers, key));
        // 補助ウィンドウの入力欄へのキーは横取りしない。
        let free = !self.windows.any_open();
        if pressed(Modifiers::COMMAND, Key::Enter) || pressed(Modifiers::CTRL, Key::Enter) {
            self.transfer();
        }
        if free && pressed(Modifiers::NONE, Key::Escape) {
            self.take_back();
        }
        // Option+Shift+↑↓(段落まで選ぶ)は文字入力のキーなので横取りしない(`consume_key` は Shift を見ない)。
        let shift = ctx.input(|i| i.modifiers.shift);
        if !shift && pressed(Modifiers::ALT, Key::ArrowDown) {
            self.toggle_playback();
        }
        // Option+↑ は以前「戻す」に使っていた。手が覚えていて押しても、カーソルを上へ動かして次の発話を
        // 思わぬ位置に入れないよう、受け取って何もしない。
        if free && !shift {
            pressed(Modifiers::ALT, Key::ArrowUp);
        }
        if free && pressed(Modifiers::COMMAND | Modifiers::SHIFT, Key::D) {
            self.add_word_from_selection();
        }
    }

    /// 上の道具の行: 案内、再生、取り消し、転送、その他のメニュー。
    fn draw_toolbar(&mut self, ui: &mut egui::Ui, _ctx: &Context, p: &Palette) {
        ui.horizontal_centered(|ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            theme::hint(ui, "⌘Enter で転送 ・ Esc で直前の発話を取り消し", p);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                let send = ui
                    .add_enabled(!self.editor.is_empty(), theme::primary("転送", p))
                    .on_hover_text("⌘Enter。転送先のアプリの入力欄へ差し込みます");
                if send.clicked() {
                    self.transfer();
                }
                ui.menu_button("…", |ui| self.more_menu(ui));
                let play = if self.playback.is_some() {
                    "停止"
                } else {
                    "再生"
                };
                if theme::ghost_button(ui, play, p)
                    .on_hover_text("カーソル位置の発話を聞き直す(Option+↓)")
                    .clicked()
                {
                    self.toggle_playback();
                }
                if theme::ghost_button(ui, "取り消し", p)
                    .on_hover_text("直前の発話を消す(Esc)")
                    .clicked()
                {
                    self.take_back();
                }
            });
        });
    }

    fn more_menu(&mut self, ui: &mut egui::Ui) {
        if ui.button("選択した語を単語登録 (⌘⇧D)").clicked() {
            self.add_word_from_selection();
            ui.close_menu();
        }
        if ui.button("すべて削除").clicked() {
            self.request_close_editor();
            ui.close_menu();
        }
        if ui
            .button("直前の文章をもう一度転送")
            .on_hover_text("転送に失敗したときや、別のアプリへ送り直すとき")
            .clicked()
        {
            self.retransfer();
            ui.close_menu();
        }
    }

    /// 話している最中の途中結果の行。確定すると消える。
    fn draw_caption(&mut self, ctx: &Context, p: &Palette) {
        let live = self.session.live.clone();
        let speaking = live.speaking.load(Ordering::Relaxed);
        let pending = live.pending();
        let waiting = self.transfer_waiting();
        let listening = self.mic == MicState::On
            && (speaking || pending > 0 || !self.live_caption.is_empty());
        if !waiting && !listening {
            return;
        }
        let text = if waiting {
            let held = self.held_utterance_count();
            let next = if held > 0 {
                format!(" ・ 話した続き {held} 件はそのあとに入ります")
            } else {
                String::new()
            };
            format!("AI 訂正が戻ったら転送します(Esc で取りやめ){next}")
        } else if !self.live_caption.is_empty() {
            format!("{}…", self.live_caption)
        } else if pending > 0 {
            "文字にしています…".to_string()
        } else {
            "聞き取り中…".to_string()
        };
        egui::TopBottomPanel::bottom("caption")
            .exact_height(CAPTION_HEIGHT)
            .frame(
                egui::Frame::new()
                    .fill(p.accent_soft)
                    .inner_margin(egui::Margin::symmetric(16, 0)),
            )
            .show(ctx, |ui| {
                ui.horizontal_centered(|ui| {
                    ui.label(RichText::new("●").small().color(p.recording));
                    let width = ui.available_width().max(40.0);
                    ui.allocate_ui_with_layout(
                        egui::vec2(width, CAPTION_HEIGHT - 4.0),
                        egui::Layout::left_to_right(egui::Align::Center),
                        |ui| {
                            ui.add(egui::Label::new(RichText::new(text).color(p.text)).truncate());
                        },
                    );
                });
            });
    }

    fn draw_text_area(&mut self, ui: &mut egui::Ui, p: &Palette) {
        let font_size = self.data.settings.editor.font_size;
        let show_return = self.data.settings.editor.show_return;
        let id = egui::Id::new(EDITOR_ID);
        if self.selection_dirty {
            self.selection_dirty = false;
            let mut state = egui::TextEdit::load_state(ui.ctx(), id).unwrap_or_default();
            let (a, b) = self.editor.selection;
            state
                .cursor
                .set_char_range(Some(CCursorRange::two(CCursor::new(a), CCursor::new(b))));
            state.store(ui.ctx(), id);
            ui.ctx().memory_mut(|memory| memory.request_focus(id));
        }
        if self.editor.is_empty() {
            self.draw_empty_hint(ui, p);
        }
        let spans = self.editor.spans.clone();
        let provisional = self.editor.provisional_ranges();
        let palette = *p;
        let mut layouter = move |ui: &egui::Ui, text: &str, wrap_width: f32| {
            let mut job = build_layout(text, &spans, &provisional, &palette, font_size);
            job.wrap.max_width = wrap_width;
            ui.fonts(|fonts| fonts.layout_job(job))
        };
        let mut buffer = self.editor.text.clone();
        let output = egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                egui::TextEdit::multiline(&mut buffer)
                    .id(id)
                    .frame(false)
                    .desired_width(f32::INFINITY)
                    .desired_rows(12)
                    .lock_focus(true)
                    .font(FontId::proportional(font_size))
                    .text_color(p.text)
                    .layouter(&mut layouter)
                    .show(ui)
            })
            .inner;
        if let Some(range) = output.cursor_range {
            let ccursor = range.as_ccursor_range();
            self.sync_editor_from_input(&buffer, (ccursor.secondary.index, ccursor.primary.index));
        } else if buffer != self.editor.text {
            let selection = self.editor.selection;
            self.sync_editor_from_input(&buffer, selection);
        }
        if show_return {
            paint_returns(ui, &output, font_size, p);
        }
    }

    fn draw_empty_hint(&self, ui: &egui::Ui, p: &Palette) {
        let rect = ui.max_rect();
        let key = self.data.settings.record.shortcut.label();
        let lines = match self.mic {
            MicState::On if self.iphone_wait_seconds().is_some() => [
                "iPhone に接続しています。".to_string(),
                "音が届くまでは、話しても文字になりません。".to_string(),
            ],
            MicState::Off => [
                format!("左上のマイクを押すか、{key} で音声入力を始めます。"),
                "話した内容がここに表示され、⌘Enter で元のアプリへ転送します。".to_string(),
            ],
            MicState::On => [
                "話しかけてください。話している間は途中の文字が下に出ます。".to_string(),
                "話し終えて少し待つと、ここに文章が入ります。".to_string(),
            ],
        };
        for (index, line) in lines.iter().enumerate() {
            ui.painter().text(
                rect.center() + egui::vec2(0.0, index as f32 * 26.0 - 20.0),
                egui::Align2::CENTER_CENTER,
                line,
                FontId::proportional(15.0),
                p.dim,
            );
        }
    }

    pub fn recognize_from_file(&mut self) {
        if self.engine_name.is_none() {
            self.notify_error("認識エンジンが見つかりません。設定の「認識」を確かめてください");
            return;
        }
        let Some(path) = rfd::FileDialog::new()
            .add_filter("音声ファイル", &["wav", "mp3", "flac", "ogg"])
            .pick_file()
        else {
            return;
        };
        self.notify(format!("{} を認識しています", path.display()));
        self.session.recognize_file(path);
    }
}

/// 文字ごとの色を決めて、表示用の組み立てを作る。後から決めたものが優先。
/// 確定を待っている発話は灰色、あやしい語は黄系の下線、直した語は緑。
pub fn build_layout(
    text: &str,
    spans: &[Span],
    provisional: &[(usize, usize)],
    p: &Palette,
    font_size: f32,
) -> LayoutJob {
    #[derive(Clone, Copy, PartialEq)]
    enum Tone {
        Plain,
        Provisional,
        Unsure,
        Corrected,
    }
    let chars: Vec<char> = text.chars().collect();
    let mut tones = vec![Tone::Plain; chars.len()];
    for (start, end) in provisional {
        let end = (*end).min(chars.len());
        if *start < end {
            tones[*start..end].fill(Tone::Provisional);
        }
    }
    for span in spans {
        let tone = match span.kind {
            SpanKind::Unsure => Tone::Unsure,
            SpanKind::Corrected => Tone::Corrected,
        };
        let end = span.end.min(chars.len());
        if span.start < end {
            tones[span.start..end].fill(tone);
        }
    }
    let font = FontId::proportional(font_size);
    let format_for = |tone: Tone| {
        let (color, underline) = match tone {
            Tone::Plain => (p.text, Stroke::NONE),
            Tone::Provisional => (p.dim, Stroke::NONE),
            Tone::Unsure => (p.unsure, Stroke::new(1.5_f32, p.unsure)),
            Tone::Corrected => (p.corrected, Stroke::NONE),
        };
        TextFormat {
            font_id: font.clone(),
            color,
            underline,
            background: Color32::TRANSPARENT,
            ..TextFormat::default()
        }
    };
    let mut job = LayoutJob::default();
    let mut run = String::new();
    let mut run_style: Option<Tone> = None;
    for (index, c) in chars.iter().enumerate() {
        let style = tones[index];
        if run_style != Some(style) {
            if let Some(previous) = run_style.take() {
                job.append(&std::mem::take(&mut run), 0.0, format_for(previous));
            }
            run_style = Some(style);
        }
        run.push(*c);
    }
    if let Some(style) = run_style {
        job.append(&run, 0.0, format_for(style));
    }
    if job.sections.is_empty() {
        job.append("", 0.0, format_for(Tone::Plain));
    }
    job
}

/// 改行の印を行末に描く。
fn paint_returns(
    ui: &egui::Ui,
    output: &egui::text_edit::TextEditOutput,
    font_size: f32,
    p: &Palette,
) {
    let painter = ui.painter_at(output.text_clip_rect);
    let galley: &Arc<egui::Galley> = &output.galley;
    let origin = output.galley_pos.to_vec2();
    // フォントに無いことがある記号なので、線で描く。
    let size = font_size * 0.32;
    let stroke = egui::Stroke::new(1.5_f32, p.border);
    for row in galley.rows.iter().filter(|row| row.ends_with_newline) {
        let at = row.rect.right_center() + origin + egui::vec2(6.0, 0.0);
        let corner = at + egui::vec2(size, 0.0);
        let top = corner - egui::vec2(0.0, size);
        let tip = at - egui::vec2(size * 0.1, 0.0);
        painter.line_segment([top, corner], stroke);
        painter.line_segment([corner, tip], stroke);
        painter.line_segment([tip, tip + egui::vec2(size * 0.4, -size * 0.4)], stroke);
        painter.line_segment([tip, tip + egui::vec2(size * 0.4, size * 0.4)], stroke);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::theme::LIGHT;

    fn tone_at(job: &LayoutJob, byte: usize) -> Color32 {
        job.sections
            .iter()
            .find(|s| s.byte_range.contains(&byte))
            .unwrap()
            .format
            .color
    }

    #[test]
    fn corrected_words_are_green() {
        let spans = [Span {
            start: 1,
            end: 3,
            kind: SpanKind::Corrected,
        }];
        let job = build_layout("右肺上葉", &spans, &[], &LIGHT, 20.0);
        assert_eq!(tone_at(&job, 0), LIGHT.text);
        assert_eq!(tone_at(&job, 3), LIGHT.corrected);
    }

    /// 確定を待っている発話は灰色。あやしい語と学習で当てた語の印はその上に出る。
    #[test]
    fn provisional_text_is_dim_under_its_marks() {
        let spans = [Span {
            start: 2,
            end: 3,
            kind: SpanKind::Corrected,
        }];
        let job = build_layout("確定仮の文", &spans, &[(2, 5)], &LIGHT, 20.0);
        assert_eq!(tone_at(&job, 0), LIGHT.text);
        assert_eq!(tone_at(&job, 6), LIGHT.corrected);
        assert_eq!(tone_at(&job, 9), LIGHT.dim);
    }

    #[test]
    fn stale_spans_do_not_panic() {
        let spans = [Span {
            start: 5,
            end: 9,
            kind: SpanKind::Unsure,
        }];
        let job = build_layout("短い", &spans, &[(5, 9), (2, 1), (1, 1)], &LIGHT, 20.0);
        assert_eq!(tone_at(&job, 0), LIGHT.text);
    }

    #[test]
    fn empty_text_still_has_a_section() {
        let job = build_layout("", &[], &[], &LIGHT, 20.0);
        assert_eq!(job.sections.len(), 1);
    }
}
