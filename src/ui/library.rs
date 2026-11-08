//! 「共有ライブラリから取り込む…」の窓: 分野・検索で絞り、選んで本文を見て、まとめて取り込む。

use super::{rows, theme};
use crate::app::{VoiceApp, IMPORT_WHILE_EDITING};
use crate::core::assistant::MAX_TEMPLATES;
use crate::core::dictionary::modules_text;
use crate::core::template_library::{LibraryIndex, LibraryTemplate, Pick};
use egui::{Context, RichText};
use std::collections::BTreeSet;

/// 目録の取得の状態。
#[derive(Debug, Default)]
pub enum LibraryLoad {
    #[default]
    Loading,
    Failed(String),
    Ready(LibraryIndex),
}

/// 窓の中で選んでいる物(目録の番号)。
#[derive(Debug, Default)]
pub struct LibraryView {
    /// 分野の id。None はすべて。
    pub category: Option<String>,
    pub query: String,
    /// 取り込む物。
    pub checked: BTreeSet<usize>,
    /// キーワードが手元のテンプレートと重なる物のうち、置き換える物(ほかは飛ばす)。
    pub replace: BTreeSet<usize>,
    /// 本文を見ている物。
    pub focus: Option<usize>,
    /// 1 件も入らなかった取り込みの結果(窓を閉じずに出す)。
    pub result: Option<String>,
}

impl LibraryView {
    /// 取り込む物を目録の順に。重なる物は、置き換えを選んでいなければ飛ばす。
    pub fn picks(&self, index: &LibraryIndex) -> Vec<Pick> {
        self.checked
            .iter()
            .filter_map(|&number| {
                index.templates.get(number).map(|item| Pick {
                    template: item.template.clone(),
                    replace: self.replace.contains(&number),
                })
            })
            .collect()
    }
}

#[derive(Debug, Default)]
pub struct LibraryWindow {
    pub load: LibraryLoad,
    pub view: LibraryView,
}

/// 一覧の文を字数で切る上限。はみ出しを防ぐのは列の幅で切る `rows::text` の方で、これは極端に長い文の
/// 描画を軽くするため(列に見えるのはこれより短い)。
const TEXT_CHARS: usize = 80;
/// 一覧の列の幅。窓の最大幅 720 から引いた残りがキーワードの列になる。
const TITLE_WIDTH: f32 = 150.0;
const COUNT_WIDTH: f32 = 100.0;
const AUTHOR_WIDTH: f32 = 96.0;
/// 「重なり 2 件: 置き換える」が切れずに入る幅(左右の余白を詰めて)。
const OVERLAP_WIDTH: f32 = 146.0;
const OVERLAP_PADDING: f32 = 6.0;
/// 列と列の間に足す隙間(部品どうしの間隔に加えて。もとの Grid の 12 に合わせる)。
const GAP: f32 = 4.0;

fn clip(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let mut short: String = text.chars().take(limit).collect();
    short.push('…');
    short
}

impl VoiceApp {
    pub fn draw_template_library(&mut self, ctx: &Context) {
        let Some(mut window) = self.windows.library.take() else {
            return;
        };
        let p = *theme::palette(ctx);
        let mut open = true;
        let mut retry = false;
        let mut import = false;
        let screen = ctx.screen_rect();
        let size = egui::vec2(
            720.0_f32.min(screen.width() - 24.0),
            600.0_f32.min(screen.height() - 72.0),
        );
        egui::Window::new("共有ライブラリから取り込む")
            .open(&mut open)
            .collapsible(false)
            .resizable(true)
            .default_size(size)
            .max_size(size)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .show(ctx, |ui| match &window.load {
                LibraryLoad::Loading => {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label("共有ライブラリの目録を取得しています…");
                    });
                }
                LibraryLoad::Failed(message) => {
                    ui.label(RichText::new(message).color(p.danger));
                    if ui.button("もう一度").clicked() {
                        retry = true;
                    }
                }
                LibraryLoad::Ready(index) => {
                    import = self.library_list(ui, index, &mut window.view, &p);
                }
            });
        if retry {
            self.windows.library = Some(window);
            self.start_library_fetch();
            return;
        }
        if import {
            if let LibraryLoad::Ready(index) = &window.load {
                let (message, imported) = self.import_library_templates(window.view.picks(index));
                if imported == 0 {
                    // 1 件も入らなければ窓を閉じず、選び直せるようにする。
                    window.view.result = Some(message);
                    self.windows.library = Some(window);
                    return;
                }
                if let Some(dict) = &mut self.windows.dict {
                    dict.message = Some((message, false));
                }
            }
            return;
        }
        if open {
            self.windows.library = Some(window);
        }
    }

    /// 一覧と本文の表示。「取り込む」が押されたら true。
    fn library_list(
        &self,
        ui: &mut egui::Ui,
        index: &LibraryIndex,
        view: &mut LibraryView,
        p: &theme::Palette,
    ) -> bool {
        let total = self.data.dictionary.templates.len();
        theme::hint(
            ui,
            &format!(
                "選んだテンプレートを辞書の末尾に足します。キーワードが手元のテンプレートと重なる物は、行ごとに「飛ばす」か「置き換える」を選べます(既定は飛ばす)。「ヘイクロード」で Claude に送るのは先頭 {MAX_TEMPLATES} 件までです(いま {total} 件)"
            ),
            p,
        );
        ui.horizontal_wrapped(|ui| {
            if theme::chip(ui, "すべて", p, view.category.is_none()).clicked() {
                view.category = None;
            }
            for category in &index.categories {
                let count = index.templates.iter().filter(|t| t.category == category.id).count();
                if count == 0 {
                    continue;
                }
                let selected = view.category.as_deref() == Some(category.id.as_str());
                let label = format!("{} {count}", category.label);
                if theme::chip(ui, &label, p, selected).clicked() {
                    view.category = (!selected).then(|| category.id.clone());
                }
            }
        });
        ui.add(
            egui::TextEdit::singleline(&mut view.query)
                .desired_width(f32::INFINITY)
                .hint_text("検索(題名・キーワード・本文・投稿者)"),
        );
        if !index.skipped.is_empty() {
            ui.label(
                RichText::new(format!("形の違う {} 件は出していません", index.skipped.len()))
                    .small()
                    .color(p.warning),
            );
        }
        let list_height = (ui.available_height() * 0.5).max(140.0);
        // Grid は列の幅が中身で決まり、長い題名や投稿者名で窓が最大幅を超えて画面からはみ出す。
        // 1 行ずつ、チェック・題名(固定幅)・キーワード(残りの幅)・字数・投稿者・重なり(右寄せの固定幅)に分ける。
        let check = ui.spacing().icon_width.max(ui.spacing().interact_size.y);
        egui::ScrollArea::vertical()
            .id_salt("library_list")
            .auto_shrink([false, false])
            .max_height(list_height)
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                let head = |ui: &mut egui::Ui, text: &str| {
                    ui.label(RichText::new(text).small().color(p.dim));
                };
                rows::row(ui, false, |ui| {
                    rows::column(ui, check, |_| {});
                    rows::column(ui, TITLE_WIDTH, |ui| head(ui, "題名"));
                    ui.add_space(GAP);
                    rows::from_right(ui, |ui| {
                        rows::column(ui, OVERLAP_WIDTH, |_| {});
                        ui.add_space(GAP);
                        rows::column(ui, AUTHOR_WIDTH, |ui| head(ui, "投稿者・更新日"));
                        ui.add_space(GAP);
                        rows::column(ui, COUNT_WIDTH, |ui| head(ui, "字数・モジュール"));
                        ui.add_space(GAP);
                        rows::rest(ui, |ui| head(ui, "キーワード"));
                    });
                });
                // 行の中で view を丸ごと借りる(重なりの切り替え)ので、絞り込みの条件は写しておく。
                let category = view.category.clone();
                let query = view.query.clone();
                let mut shown = 0;
                for (number, item) in index.templates.iter().enumerate() {
                    if !item.matches(category.as_deref(), &query) {
                        continue;
                    }
                    // Grid の striped と同じく、見出しの次の行から 1 行おきに地を敷く。
                    rows::row(ui, shown % 2 == 0, |ui| {
                        rows::column(ui, check, |ui| {
                            let mut checked = view.checked.contains(&number);
                            if ui.checkbox(&mut checked, "").changed() {
                                if checked {
                                    view.checked.insert(number);
                                } else {
                                    view.checked.remove(&number);
                                }
                            }
                        });
                        rows::column(ui, TITLE_WIDTH, |ui| {
                            let focused = view.focus == Some(number);
                            if ui
                                .selectable_label(focused, RichText::new(clip(&item.title, TEXT_CHARS)).strong())
                                .on_hover_text(format!("「{}」の本文を見る", item.title))
                                .clicked()
                            {
                                view.focus = Some(number);
                            }
                        });
                        ui.add_space(GAP);
                        rows::from_right(ui, |ui| {
                            rows::column(ui, OVERLAP_WIDTH, |ui| self.overlap_toggle(ui, item, number, view, p));
                            ui.add_space(GAP);
                            rows::column(ui, AUTHOR_WIDTH, |ui| {
                                rows::text(ui, RichText::new(clip(&item.author, TEXT_CHARS)).small().color(p.dim), &item.author);
                                rows::text(ui, RichText::new(clip(&item.updated, TEXT_CHARS)).small().color(p.dim), &item.updated);
                            });
                            ui.add_space(GAP);
                            rows::column(ui, COUNT_WIDTH, |ui| {
                                let counts = format!("{} 字・{} 件", item.chars(), item.template.modules.len());
                                rows::text(ui, RichText::new(&counts).small(), &counts);
                            });
                            ui.add_space(GAP);
                            rows::rest(ui, |ui| {
                                rows::text(
                                    ui,
                                    RichText::new(clip(&item.keywords_head(3), TEXT_CHARS)).color(p.dim),
                                    &item.template.keywords.join("、"),
                                );
                            });
                        });
                    });
                    shown += 1;
                }
            });
        ui.separator();
        if let Some(item) = view.focus.and_then(|number| index.templates.get(number)) {
            let mut head = format!("{}  ・  {}", item.title, index.category_label(&item.category));
            if !item.author.is_empty() {
                head.push_str(&format!("  ・  {}", item.author));
            }
            ui.label(RichText::new(head).strong());
            if !item.note.is_empty() {
                theme::hint(ui, &item.note, p);
            }
            let mut text = item.template.body.clone();
            if !item.template.modules.is_empty() {
                text.push_str(&format!("\n\n所見モジュール:\n{}", modules_text(&item.template.modules)));
            }
            egui::ScrollArea::vertical()
                .id_salt("library_preview")
                .auto_shrink([false, false])
                .max_height((ui.available_height() - 40.0).max(60.0))
                .show(ui, |ui| {
                    ui.label(text);
                });
        } else {
            theme::hint(ui, "題名を押すと本文とモジュールが出ます", p);
        }
        if let Some(result) = &view.result {
            ui.label(RichText::new(result).small().color(p.warning));
        }
        let editing = self.editing_a_template();
        if editing {
            ui.label(RichText::new(IMPORT_WHILE_EDITING).small().color(p.warning));
        }
        let mut import = false;
        ui.horizontal(|ui| {
            let count = view.checked.len();
            let button =
                ui.add_enabled(count > 0 && !editing, theme::primary(&format!("取り込む({count} 件)"), p));
            if button.clicked() {
                view.result = None;
                import = true;
            }
        });
        import
    }

    /// キーワードが手元のテンプレートと重なる物の「飛ばす / 置き換える」の切り替え。重ならなければ何も出さない。
    fn overlap_toggle(
        &self,
        ui: &mut egui::Ui,
        item: &LibraryTemplate,
        number: usize,
        view: &mut LibraryView,
        p: &theme::Palette,
    ) {
        let overlaps = self
            .data
            .dictionary
            .conflicting_templates(&item.template.keywords, None);
        if overlaps.is_empty() {
            return;
        }
        let names: Vec<String> = overlaps
            .iter()
            .filter_map(|&i| self.data.dictionary.templates.get(i))
            .map(|t| t.keywords.first().cloned().unwrap_or_default())
            .collect();
        ui.spacing_mut().button_padding.x = OVERLAP_PADDING;
        let replace = view.replace.contains(&number);
        let choice = if replace { "置き換える" } else { "飛ばす" };
        let label = if overlaps.len() >= 2 {
            format!("重なり {} 件: {choice}", overlaps.len())
        } else {
            format!("重なり: {choice}")
        };
        if ui
            .selectable_label(replace, RichText::new(label).small().color(p.warning))
            .on_hover_text(format!(
                "置き換えると手元の「{}」は消えます。押すと「飛ばす」と「置き換える」が切り替わります",
                names.join("」「")
            ))
            .clicked()
        {
            if replace {
                view.replace.remove(&number);
            } else {
                view.replace.insert(number);
            }
        }
    }
}
