//! 辞書の窓の「構造」折り畳み(§16.4 の確認表、単位 S2)。テンプレートの編集の下に出す。
//!
//! 行ごとの鍵、変種ごとの対象・置き方・極性・役割と「ほか」(足す鍵・外す臓器・接頭語・列挙・条件・自動)、
//! 警告(`unresolved`)。鍵・対象・置き方・極性・役割は選び直せ、選ぶとすぐ `structure.json` に保存する
//! (警告は直した構造から作り直す)。「付け直す」で Claude に注釈し直させる。

use super::dict_tool::DictTool;
use super::{rows, theme};
use crate::app::{StructureState, VoiceApp};
use crate::core::annotate::{mode_label, polarity_label, variant_extras};
use crate::core::dictionary::modules_text;
use crate::core::structure::{canonical_lines, Condition, LineKind, Mode, TemplateStructure, Variant, Vocabulary};
use egui::collapsing_header::CollapsingState;
use egui::{PopupCloseBehavior, RichText};

const INDEX_WIDTH: f32 = 20.0;
const SLOT_WIDTH: f32 = 160.0;
const MODULE_WIDTH: f32 = 108.0;
const TARGET_WIDTH: f32 = 130.0;
const MODE_WIDTH: f32 = 98.0;
const POLARITY_WIDTH: f32 = 98.0;
/// 「直す」(条件・自動・外す臓器・役割を直す小さな窓)のボタンの列。
const MORE_WIDTH: f32 = 64.0;
/// 確認表の下に残す高さ(一覧を隠した知らせと、窓の知らせの行)。
const TABLE_RESERVE: f32 = 48.0;
/// 確認表の高さの最低限。
const MIN_TABLE_HEIGHT: f32 = 100.0;

const POLARITIES: [(&str, &str); 3] = [("negative", "陰性"), ("benign", "正常範囲"), ("abnormal", "異常")];
const MODES: [Mode; 4] = [Mode::Replace, Mode::Prepend, Mode::Append, Mode::Remove];

/// 見出しの小さな文字(列の名前)。
fn heading(ui: &mut egui::Ui, text: &str, p: &theme::Palette) {
    ui.label(RichText::new(text).small().color(p.dim));
}

/// 鍵を選ぶ欄。語彙の全部を領域ごとに出す。選び直したら Some(新しい鍵)。
fn slot_combo(
    ui: &mut egui::Ui,
    id: impl std::hash::Hash,
    current: &Option<String>,
    list: bool,
    vocabulary: &Vocabulary,
) -> Option<Option<String>> {
    let mut choice = current.clone();
    let shown = match current {
        Some(slot) if list => format!("{slot}(列挙)"),
        Some(slot) => slot.clone(),
        None => "鍵なし".to_string(),
    };
    egui::ComboBox::from_id_salt(id)
        .width(SLOT_WIDTH - 6.0)
        .height(360.0)
        .selected_text(shown)
        .show_ui(ui, |ui| {
            ui.selectable_value(&mut choice, None, "鍵なし");
            for group in &vocabulary.slots {
                ui.label(RichText::new(&group.region).small().weak());
                for key in &group.keys {
                    ui.selectable_value(&mut choice, Some(key.clone()), key.as_str());
                }
            }
        });
    (choice != *current).then_some(choice)
}

impl VoiceApp {
    /// テンプレートの編集の下の「構造」。`index` は編集中のテンプレート(保存済みの源泉の構造を見せる)。
    pub(super) fn structure_fold(
        &mut self,
        ui: &mut egui::Ui,
        tool: &mut DictTool,
        index: usize,
        p: &theme::Palette,
    ) {
        let Some(template) = self.data.dictionary.templates.get(index).cloned() else {
            return;
        };
        let (status, structure, failed, running) = match self.structure_state(&template) {
            StructureState::Annotated(structure) => {
                let warnings = structure.unresolved.len();
                let status = if warnings == 0 {
                    "注釈済み".to_string()
                } else {
                    format!("注釈済み・警告 {warnings} 件")
                };
                (status, Some(structure.clone()), None, false)
            }
            StructureState::Running => ("注釈中…".to_string(), None, None, true),
            StructureState::Failed(reason) => {
                ("注釈できませんでした".to_string(), None, Some(reason.to_string()), false)
            }
            StructureState::Unannotated => ("未注釈".to_string(), None, None, false),
        };
        // 欄で文章を直している途中なら、保存すると源泉のハッシュが変わって付け直しになる。
        let dirty = tool.new_body.trim_end() != template.body.trim_end()
            || tool.new_modules.trim_end() != modules_text(&template.modules).trim_end();
        let vocabulary = self.structure_vocabulary();
        let body: Vec<String> = canonical_lines(&template.body).into_iter().map(str::to_string).collect();

        let id = ui.make_persistent_id("dict_structure");
        let mut state = CollapsingState::load_with_default_open(ui.ctx(), id, false);
        if std::mem::take(&mut tool.open_structure) {
            state.set_open(true);
        }
        tool.structure_open = state.is_open();
        let mut reannotate = false;
        let mut edited: Option<TemplateStructure> = None;
        state
            .show_header(ui, |ui| {
                ui.label(RichText::new("構造").strong());
                let color = if status.contains("警告") || failed.is_some() { p.danger } else { p.dim };
                ui.label(RichText::new(&status).small().color(color));
                if ui
                    .add_enabled(!running, egui::Button::new("付け直す").small())
                    .on_hover_text("Claude(Sonnet)に注釈し直させます。表で直した所は消えます")
                    .clicked()
                {
                    reannotate = true;
                }
            })
            .body(|ui| {
                if dirty {
                    theme::hint(ui, "文章を直したので、保存すると構造は付け直します", p);
                } else if structure.is_none() && !running {
                    theme::hint(ui, "構造が無い間は、今までどおり Claude が全文を書きます", p);
                }
                if let Some(reason) = &failed {
                    ui.label(RichText::new(reason).small().color(p.danger));
                }
                let Some(structure) = &structure else {
                    return;
                };
                for warning in &structure.unresolved {
                    ui.label(RichText::new(format!("警告: {warning}")).small().color(p.danger));
                }
                // 表の下に一覧の代わりの 1 行と、窓の知らせの 1 行を残す。
                let table_height = (ui.available_height() - TABLE_RESERVE).max(MIN_TABLE_HEIGHT);
                egui::ScrollArea::vertical()
                    .id_salt("dict_structure_table")
                    .max_height(table_height)
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                        ui.spacing_mut().item_spacing.y = 0.0;
                        self::lines_table(ui, index, structure, &body, &vocabulary, &mut edited, p);
                        ui.add_space(6.0);
                        self::variants_table(ui, index, structure, &vocabulary, &mut edited, p);
                    });
            });
        if reannotate {
            self.reannotate(index);
        }
        if let Some(structure) = edited {
            self.update_structure(index, structure);
        }
    }
}

/// 行の表: 番号・鍵(選べる)・本文の行。
fn lines_table(
    ui: &mut egui::Ui,
    index: usize,
    structure: &TemplateStructure,
    body: &[String],
    vocabulary: &Vocabulary,
    edited: &mut Option<TemplateStructure>,
    p: &theme::Palette,
) {
    rows::row(ui, false, |ui| {
        rows::column(ui, INDEX_WIDTH, |ui| heading(ui, "行", p));
        ui.add_space(rows::GAP);
        rows::column(ui, SLOT_WIDTH, |ui| heading(ui, "鍵", p));
        ui.add_space(rows::GAP);
        heading(ui, "本文", p);
    });
    for (i, info) in structure.lines.iter().enumerate() {
        let text = body.get(i).map_or("", String::as_str);
        rows::row(ui, i % 2 == 0, |ui| {
            rows::column(ui, INDEX_WIDTH, |ui| {
                ui.label(RichText::new(i.to_string()).small().color(p.dim));
            });
            ui.add_space(rows::GAP);
            rows::column(ui, SLOT_WIDTH, |ui| match info.kind {
                LineKind::Blank => heading(ui, "空行", p),
                LineKind::Heading => heading(ui, "見出し", p),
                LineKind::Sentence | LineKind::ListNegation => {
                    let list = info.kind == LineKind::ListNegation;
                    if let Some(choice) = slot_combo(ui, ("structure_slot", index, i), &info.slot, list, vocabulary) {
                        edited.get_or_insert_with(|| structure.clone()).lines[i].slot = choice;
                    }
                }
            });
            ui.add_space(rows::GAP);
            rows::rest(ui, |ui| {
                rows::text(ui, text, text);
            });
        });
    }
}

/// 一つの値を選ぶ横並び(「なし」と選択肢)。選び直したら true。
fn choice_row(ui: &mut egui::Ui, label: &str, value: &mut Option<String>, options: &[String]) -> bool {
    let before = value.clone();
    ui.horizontal_wrapped(|ui| {
        ui.label(RichText::new(label).small());
        ui.selectable_value(value, None, "なし");
        for option in options {
            ui.selectable_value(value, Some(option.clone()), option.as_str());
        }
    });
    *value != before
}

/// 「直す」の小さな窓: 条件(性別・年齢帯・目的)、自動、列挙から外す臓器(対象が列挙否定の行のとき)、
/// 役割(尺度のある所見群のとき)。直したら直した変種を返す。
fn more_menu(
    ui: &mut egui::Ui,
    variant: &Variant,
    list_organs: &[String],
    grouped: bool,
    vocabulary: &Vocabulary,
) -> Option<Variant> {
    let mut changed: Option<Variant> = None;
    ui.menu_button("直す", |ui| {
        ui.set_min_width(300.0);
        let mut next = variant.clone();
        let mut condition: Condition = next.condition.clone().unwrap_or_default();
        let mut dirty = false;
        let sexes = vec!["男".to_string(), "女".to_string()];
        let bands: Vec<String> = vocabulary.age_bands.keys().cloned().collect();
        dirty |= choice_row(ui, "性別", &mut condition.sex, &sexes);
        dirty |= choice_row(ui, "年齢帯", &mut condition.age_band, &bands);
        dirty |= choice_row(ui, "目的", &mut condition.purpose, &vocabulary.purposes);
        let can_auto = (next.polarity.as_deref() == Some("negative") && !condition.is_empty()) || next.mode == Mode::Remove;
        let mut auto = next.auto;
        if ui
            .add_enabled(can_auto, egui::Checkbox::new(&mut auto, "自動(文脈が合えば口述なしで置く)"))
            .on_disabled_hover_text("陰性の文で、条件があるときだけ")
            .changed()
        {
            next.auto = auto;
            dirty = true;
        }
        if !list_organs.is_empty() && next.mode == Mode::Replace {
            dirty |= choice_row(ui, "列挙から外す臓器", &mut next.organ, list_organs);
        }
        if grouped {
            let roles: Vec<String> = vocabulary.roles.iter().map(|r| r.name.clone()).collect();
            dirty |= choice_row(ui, "役割", &mut next.role, &roles);
        }
        if dirty {
            if !can_auto && next.mode != Mode::Remove {
                next.auto = false;
            }
            next.condition = (!condition.is_empty()).then_some(condition);
            changed = Some(next);
        }
    });
    changed
}

/// 変種の表: モジュール(行)・対象・置き方・極性・「直す」(条件・自動・外す臓器・役割)・ほかの表示。
fn variants_table(
    ui: &mut egui::Ui,
    index: usize,
    structure: &TemplateStructure,
    vocabulary: &Vocabulary,
    edited: &mut Option<TemplateStructure>,
    p: &theme::Palette,
) {
    let mut body_slots: Vec<String> = Vec::new();
    for slot in structure.lines.iter().filter_map(|l| l.slot.clone()) {
        if !body_slots.contains(&slot) {
            body_slots.push(slot);
        }
    }
    let grouped: Vec<&str> = structure.groups.iter().map(|g| g.module.as_str()).collect();
    rows::row(ui, false, |ui| {
        rows::column(ui, MODULE_WIDTH, |ui| heading(ui, "モジュール", p));
        ui.add_space(rows::GAP);
        rows::column(ui, TARGET_WIDTH, |ui| heading(ui, "対象", p));
        ui.add_space(rows::GAP);
        rows::column(ui, MODE_WIDTH, |ui| heading(ui, "置き方", p));
        ui.add_space(rows::GAP);
        rows::column(ui, POLARITY_WIDTH, |ui| heading(ui, "極性", p));
        ui.add_space(rows::GAP);
        rows::column(ui, MORE_WIDTH, |ui| heading(ui, "条件など", p));
        ui.add_space(rows::GAP);
        heading(ui, "ほか", p);
    });
    for (vi, variant) in structure.variants.iter().enumerate() {
        rows::row(ui, vi % 2 == 0, |ui| {
            let name = match variant.line {
                Some(line) => format!("{} {line}", variant.module),
                None => variant.module.clone(),
            };
            rows::column(ui, MODULE_WIDTH, |ui| {
                rows::text(ui, name.as_str(), &name);
            });
            ui.add_space(rows::GAP);
            if variant.is_whole() {
                let how = if variant.split {
                    "全体差し替え(行に割った。上の行ごとの変種を使う)"
                } else {
                    "全体差し替え(行に割れないので、今までどおり Claude が全文を書く)"
                };
                rows::rest(ui, |ui| {
                    rows::text(ui, RichText::new(how).small().color(p.dim), how);
                });
                return;
            }
            rows::column(ui, TARGET_WIDTH, |ui| {
                let mut target = variant.target.clone();
                let full = target.join("・");
                egui::ComboBox::from_id_salt(("structure_target", index, vi))
                    .width(TARGET_WIDTH - 6.0)
                    .selected_text(target.join("・"))
                    .close_behavior(PopupCloseBehavior::CloseOnClickOutside)
                    .show_ui(ui, |ui| {
                        for slot in &body_slots {
                            let on = target.contains(slot);
                            if ui.selectable_label(on, slot.as_str()).clicked() {
                                if on && target.len() > 1 {
                                    target.retain(|t| t != slot);
                                } else if !on {
                                    target.push(slot.clone());
                                    target.sort_by_key(|t| body_slots.iter().position(|s| s == t));
                                }
                            }
                        }
                    })
                    .response
                    .on_hover_text(full);
                if target != variant.target {
                    edited.get_or_insert_with(|| structure.clone()).variants[vi].target = target;
                }
            });
            ui.add_space(rows::GAP);
            rows::column(ui, MODE_WIDTH, |ui| {
                let mut mode = variant.mode;
                egui::ComboBox::from_id_salt(("structure_mode", index, vi))
                    .width(MODE_WIDTH - 6.0)
                    .selected_text(mode_label(mode))
                    .show_ui(ui, |ui| {
                        for option in MODES {
                            ui.selectable_value(&mut mode, option, mode_label(option));
                        }
                    });
                if mode != variant.mode {
                    edited.get_or_insert_with(|| structure.clone()).variants[vi].mode = mode;
                }
            });
            ui.add_space(rows::GAP);
            rows::column(ui, POLARITY_WIDTH, |ui| {
                if variant.mode == Mode::Remove {
                    heading(ui, polarity_label(None), p);
                    return;
                }
                let mut polarity = variant.polarity.clone();
                egui::ComboBox::from_id_salt(("structure_polarity", index, vi))
                    .width(POLARITY_WIDTH - 6.0)
                    .selected_text(polarity_label(polarity.as_deref()))
                    .show_ui(ui, |ui| {
                        for (value, label) in POLARITIES {
                            ui.selectable_value(&mut polarity, Some(value.to_string()), label);
                        }
                    });
                if polarity != variant.polarity {
                    edited.get_or_insert_with(|| structure.clone()).variants[vi].polarity = polarity;
                }
            });
            ui.add_space(rows::GAP);
            rows::column(ui, MORE_WIDTH, |ui| {
                let list_organs: Vec<String> = structure
                    .lines
                    .iter()
                    .find(|l| {
                        l.kind == LineKind::ListNegation
                            && variant.target.len() == 1
                            && l.slot.as_deref() == Some(variant.target[0].as_str())
                    })
                    .map(|l| l.organs.clone())
                    .unwrap_or_default();
                let grouped_here = grouped.contains(&variant.module.as_str());
                if let Some(changed) = more_menu(ui, variant, &list_organs, grouped_here, vocabulary) {
                    edited.get_or_insert_with(|| structure.clone()).variants[vi] = changed;
                }
            });
            ui.add_space(rows::GAP);
            let extras = variant_extras(variant);
            let extras = if variant.derived { format!("派生、{extras}") } else { extras };
            rows::rest(ui, |ui| {
                rows::text(ui, RichText::new(extras.as_str()).small().color(p.dim), &extras);
            });
        });
    }
}
