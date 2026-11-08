//! 辞書ツール: 単語・テンプレート・学習(誤認識 → 正しい語)の 3 つの一覧。

use super::{rows, theme};
use crate::app::VoiceApp;
use crate::core::judge::changes_meaning;
use crate::core::dictionary::{modules_text, Template};
use egui::{Context, RichText};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DictTab {
    #[default]
    Words,
    Templates,
    Memory,
}

#[derive(Default)]
pub struct DictTool {
    pub tab: DictTab,
    pub filter: String,
    pub new_word: String,
    pub new_reading: String,
    pub new_keywords: String,
    pub new_body: String,
    /// 所見モジュールの欄(【名前】・文・「使い方:」)。
    pub new_modules: String,
    /// 編集中のテンプレートの番号。None なら新しく追加する。
    pub editing: Option<usize>,
    pub new_from: String,
    pub new_to: String,
    pub confirm_clear: bool,
    pub message: Option<(String, bool)>,
}

impl DictTool {
    /// 一覧の「編集」: 欄にそのテンプレートを入れ、保存で上書きする状態にする。
    pub fn begin_edit(&mut self, index: usize, template: &Template) {
        self.new_keywords = template.keywords.join("、");
        self.new_body = template.body.clone();
        self.new_modules = modules_text(&template.modules);
        self.editing = Some(index);
        self.message = None;
    }
}

/// 窓の中で選ばれた操作。窓を描いた後でまとめて行う。
#[derive(Default)]
struct Pending {
    dict_changed: bool,
    memory_changed: bool,
    /// 一覧から差し込むテンプレートの文章。
    insert: Option<String>,
}

fn matches(filter: &str, fields: &[&str]) -> bool {
    let filter = filter.trim().to_lowercase();
    filter.is_empty() || fields.iter().any(|f| f.to_lowercase().contains(&filter))
}

/// 一覧に出す文章の冒頭。
fn preview(body: &str) -> String {
    const LIMIT: usize = 36;
    let line = body.lines().next().unwrap_or("");
    let mut text: String = line.chars().take(LIMIT).collect();
    if line.chars().count() > LIMIT || body.lines().count() > 1 {
        text.push('…');
    }
    text
}

/// テンプレートの一覧のキーワード列の幅。窓の最大幅 700 から文章とボタンの列を引いて収まる値。
const TEMPLATE_KEYWORD_WIDTH: f32 = 170.0;
/// 一覧の文を字数で切る上限。はみ出しを防ぐのは列の幅で切る `rows::text` の方で、これは極端に長い文の
/// 描画を軽くするため(列に見えるのはこれより短い)。
const KEYWORD_CHARS: usize = 80;

/// 文を `limit` 字までに切り、切ったら「…」を付ける。
fn clip(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let mut short: String = text.chars().take(limit).collect();
    short.push('…');
    short
}

fn pick_csv() -> Option<Vec<u8>> {
    let path = rfd::FileDialog::new()
        .add_filter("CSV", &["csv", "txt"])
        .pick_file()?;
    std::fs::read(path).ok()
}

fn save_csv(name: &str, text: &str) -> Result<bool, String> {
    let Some(path) = rfd::FileDialog::new().set_file_name(name).save_file() else {
        return Ok(false);
    };
    std::fs::write(path, text).map_err(|e| format!("書き出せません: {e}"))?;
    Ok(true)
}

/// 読み込めなかった行の知らせ文(長くなりすぎないように切る)。
/// テンプレートの欄の下に残す高さ(保存の行と、一覧の最低限)。
const FIXED_BELOW_FIELDS: f32 = 150.0;
/// 欄 2 つに充てる高さの最低限(窓を小さくしても欄が潰れないように)。
const MIN_FIELDS_HEIGHT: f32 = 120.0;

/// 高さに上限のある複数行の欄。中身が上限を超えると欄の中で送り、窓を伸ばさない。
fn bounded_multiline(
    ui: &mut egui::Ui,
    id: &str,
    text: &mut String,
    max_height: f32,
    rows: usize,
    hint: &str,
) {
    egui::ScrollArea::vertical()
        .id_salt(id)
        .max_height(max_height)
        .auto_shrink([false, true])
        .show(ui, |ui| {
            ui.add(
                egui::TextEdit::multiline(text)
                    .desired_width(f32::INFINITY)
                    .desired_rows(rows)
                    .hint_text(hint),
            );
        });
}

fn bad_lines_note(bad: &[String]) -> String {
    if bad.is_empty() {
        return String::new();
    }
    let shown: Vec<&str> = bad.iter().take(3).map(String::as_str).collect();
    let more = if bad.len() > 3 { " ほか" } else { "" };
    format!(
        "(読めなかった行 {} 件: {}{})",
        bad.len(),
        shown.join(" / "),
        more
    )
}

impl VoiceApp {
    pub fn open_dict_tool(&mut self, tab: Option<DictTab>) {
        let tool = self.windows.dict.get_or_insert_with(DictTool::default);
        if let Some(tab) = tab {
            tool.tab = tab;
        }
    }

    pub fn draw_dict_tool(&mut self, ctx: &Context) {
        let Some(mut tool) = self.windows.dict.take() else {
            return;
        };
        let p = *theme::palette(ctx);
        let mut open = true;
        let mut pending = Pending::default();
        let screen = ctx.screen_rect();
        // テンプレートの文章は数十行になるので、画面に入る範囲で高めに取る。
        let size = egui::vec2(
            700.0_f32.min(screen.width() - 24.0),
            640.0_f32.min(screen.height() - 72.0),
        );
        egui::Window::new("辞書")
            .open(&mut open)
            .collapsible(false)
            .resizable(true)
            .default_size(size)
            .max_size(size)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    for (tab, label) in [
                        (DictTab::Words, "単語"),
                        (DictTab::Templates, "テンプレート"),
                        (DictTab::Memory, "学習"),
                    ] {
                        if theme::chip(ui, label, &p, tool.tab == tab).clicked() && tool.tab != tab
                        {
                            tool.tab = tab;
                            tool.message = None;
                            tool.confirm_clear = false;
                        }
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.menu_button("…", |ui| {
                            if tool.tab != DictTab::Memory && ui.button("CSV を読み込む").clicked()
                            {
                                self.import_csv(&mut tool, &mut pending);
                                ui.close_menu();
                            }
                            if ui.button("CSV に書き出す").clicked() {
                                self.export_csv(&mut tool);
                                ui.close_menu();
                            }
                            if tool.tab == DictTab::Templates {
                                ui.separator();
                                if ui
                                    .button("共有ライブラリから取り込む…")
                                    .on_hover_text("押したときだけ、GitHub にある共有ライブラリの目録を取得します")
                                    .clicked()
                                {
                                    self.open_template_library();
                                    ui.close_menu();
                                }
                            }
                        });
                        ui.add(
                            egui::TextEdit::singleline(&mut tool.filter)
                                .desired_width(160.0)
                                .hint_text("絞り込み"),
                        );
                    });
                });
                ui.separator();
                match tool.tab {
                    DictTab::Words => self.words_tab(ui, &mut tool, &mut pending, &p),
                    DictTab::Templates => self.templates_tab(ui, &mut tool, &mut pending, &p),
                    DictTab::Memory => self.memory_tab(ui, &mut tool, &mut pending, &p),
                }
                if let Some((text, is_error)) = &tool.message {
                    let color = if *is_error { p.danger } else { p.success };
                    ui.label(RichText::new(text).small().color(color));
                }
            });
        if pending.dict_changed {
            self.save_data();
            self.refresh_engine_options();
        }
        if pending.memory_changed {
            self.after_memory_change();
        }
        if let Some(body) = pending.insert {
            // 入った文が見えるよう、窓を閉じる。
            self.insert_template(&body, None);
            return;
        }
        if open {
            self.windows.dict = Some(tool);
        }
    }

    fn import_csv(&mut self, tool: &mut DictTool, pending: &mut Pending) {
        let Some(bytes) = pick_csv() else {
            return;
        };
        match tool.tab {
            DictTab::Words => {
                let (count, bad) = self.data.dictionary.import_words_csv(&bytes);
                tool.message = Some((
                    format!("{count} 語を読み込みました{}", bad_lines_note(&bad)),
                    count == 0 && !bad.is_empty(),
                ));
                pending.dict_changed |= count > 0;
            }
            DictTab::Templates => {
                let (count, bad) = self.data.dictionary.import_templates_csv(&bytes);
                tool.message = Some((
                    format!("{count} 件を読み込みました{}", bad_lines_note(&bad)),
                    count == 0 && !bad.is_empty(),
                ));
                pending.dict_changed |= count > 0;
            }
            DictTab::Memory => {}
        }
    }

    fn export_csv(&mut self, tool: &mut DictTool) {
        let (name, text) = match tool.tab {
            DictTab::Words => ("単語.csv", self.data.dictionary.export_words_csv()),
            DictTab::Templates => (
                "テンプレート.csv",
                self.data.dictionary.export_templates_csv(),
            ),
            DictTab::Memory => {
                let mut text = String::new();
                for index in self.data.memory.sorted_indices() {
                    let pair = &self.data.memory.pairs[index];
                    text.push_str(&format!(
                        "{},{},{},{}\n",
                        pair.from, pair.to, pair.count, pair.manual
                    ));
                }
                ("学習.csv", text)
            }
        };
        tool.message = match save_csv(name, &text) {
            Ok(true) => Some(("書き出しました".to_string(), false)),
            Ok(false) => None,
            Err(error) => Some((error, true)),
        };
    }

    fn words_tab(
        &mut self,
        ui: &mut egui::Ui,
        tool: &mut DictTool,
        pending: &mut Pending,
        p: &theme::Palette,
    ) {
        theme::hint(
            ui,
            "登録した語は認識エンジンの手がかりと AI 訂正の用語集に使われます",
            p,
        );
        let mut submit = false;
        ui.horizontal(|ui| {
            let a = ui.add(
                egui::TextEdit::singleline(&mut tool.new_word)
                    .desired_width(150.0)
                    .hint_text("単語"),
            );
            let b = ui.add(
                egui::TextEdit::singleline(&mut tool.new_reading)
                    .desired_width(150.0)
                    .hint_text("読み(ひらがな)"),
            );
            let enter = ui.input(|i| i.key_pressed(egui::Key::Enter));
            if (a.lost_focus() || b.lost_focus()) && enter {
                submit = true;
            }
            if theme::primary_button(ui, "追加", p).clicked() {
                submit = true;
            }
        });
        if submit {
            match self.data.dictionary.add_word(&tool.new_word, &tool.new_reading) {
                Ok(()) => {
                    tool.message =
                        Some((format!("「{}」を登録しました", tool.new_word.trim()), false));
                    tool.new_word.clear();
                    tool.new_reading.clear();
                    pending.dict_changed = true;
                }
                Err(error) => tool.message = Some((error, true)),
            }
        }
        ui.add_space(4.0);
        let mut remove: Option<usize> = None;
        let mut restore: Option<usize> = None;
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .max_height(ui.available_height() - 24.0)
            .show(ui, |ui| {
                egui::Grid::new("dict_words")
                    .striped(true)
                    .num_columns(3)
                    .spacing([16.0, 6.0])
                    .show(ui, |ui| {
                        for h in ["単語", "読み", ""] {
                            ui.label(RichText::new(h).small().color(p.dim));
                        }
                        ui.end_row();
                        for (index, word) in self.data.dictionary.words.iter().enumerate() {
                            if !matches(&tool.filter, &[&word.word, &word.reading]) {
                                continue;
                            }
                            ui.label(&word.word);
                            ui.label(RichText::new(&word.reading).color(p.dim));
                            if ui.small_button("×").on_hover_text("この語を消す").clicked() {
                                remove = Some(index);
                            }
                            ui.end_row();
                        }
                    });
                if !self.data.dictionary.disabled.is_empty() {
                    theme::section(ui, "認識から外した語", p);
                    for (index, word) in self.data.dictionary.disabled.iter().enumerate() {
                        if !matches(&tool.filter, &[word]) {
                            continue;
                        }
                        ui.horizontal(|ui| {
                            ui.label(word);
                            if ui.small_button("戻す").clicked() {
                                restore = Some(index);
                            }
                        });
                    }
                }
            });
        if let Some(index) = remove {
            self.data.dictionary.remove_word(index);
            pending.dict_changed = true;
        }
        if let Some(index) = restore {
            self.data.dictionary.disabled.remove(index);
            pending.dict_changed = true;
        }
    }

    fn templates_tab(
        &mut self,
        ui: &mut egui::Ui,
        tool: &mut DictTool,
        pending: &mut Pending,
        p: &theme::Palette,
    ) {
        theme::hint(
            ui,
            "キーワードに続けて「テンプレート」と話すと(「前立腺MRIテンプレート」)、登録した文章が入ります。キーワードだけなら普通の語です。所見モジュールは話しても入らず、「ヘイクロード」の依頼で Claude が正常所見と差し替える材料です。AI 訂正と学習の対象にはなりません。辞書ファイル(dictionary.json)を直接編集しても反映されます",
            p,
        );
        ui.add_space(2.0);
        ui.add(
            egui::TextEdit::singleline(&mut tool.new_keywords)
                .desired_width(f32::INFINITY)
                .hint_text("キーワード(「、」で区切ると複数登録できます)"),
        );
        // 文章と所見モジュールは数十行になる。欄の高さに上限を置いて中で送り、
        // 保存ボタンと一覧を窓の中に残す(上限が無いと欄が伸びて窓が画面からはみ出し、保存できない)。
        let fields = (ui.available_height() - FIXED_BELOW_FIELDS).max(MIN_FIELDS_HEIGHT);
        bounded_multiline(
            ui,
            "dict_template_body",
            &mut tool.new_body,
            fields * 0.4,
            4,
            "差し込む文章(正常所見。改行できます)",
        );
        bounded_multiline(
            ui,
            "dict_template_modules",
            &mut tool.new_modules,
            fields * 0.6,
            3,
            "所見モジュール(省略可)。【名前】の行で始め、次の行に差し替える文。「使い方: …」の行で差し替え先を書けます",
        );
        let mut submit = false;
        ui.horizontal(|ui| {
            let label = if tool.editing.is_some() {
                "保存"
            } else {
                "追加"
            };
            if theme::primary_button(ui, label, p).clicked() {
                submit = true;
            }
            if tool.editing.is_some() && ui.button("やめる").clicked() {
                tool.editing = None;
                tool.new_keywords.clear();
                tool.new_body.clear();
                tool.new_modules.clear();
                tool.message = None;
            }
        });
        if submit {
            self.submit_template(tool, pending);
        }
        ui.add_space(4.0);
        let mut remove: Option<usize> = None;
        let mut edit: Option<usize> = None;
        let mut share: Option<usize> = None;
        // Grid は列の幅が中身で決まり、長いキーワードやモジュール名で窓が最大幅を超えて画面からはみ出す。
        // 1 行ずつ、キーワード(固定幅)・文章(残りの幅)・ボタン(右寄せ)に分け、文は幅で切る。
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .max_height(ui.available_height() - 24.0)
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                rows::row(ui, false, |ui| {
                    rows::column(ui, TEMPLATE_KEYWORD_WIDTH, |ui| {
                        ui.label(RichText::new("キーワード").small().color(p.dim));
                    });
                    ui.add_space(rows::GAP);
                    ui.label(RichText::new("文章").small().color(p.dim));
                });
                let mut shown = 0;
                for (index, template) in self.data.dictionary.templates.iter().enumerate() {
                    let keywords = template.keywords.join("、");
                    let names: Vec<&str> = template.modules.iter().map(|m| m.name.as_str()).collect();
                    let names = names.join("、");
                    if !matches(&tool.filter, &[&keywords, &template.body, &names]) {
                        continue;
                    }
                    // Grid の striped と同じく、見出しの次の行から 1 行おきに地を敷く。
                    rows::row(ui, shown % 2 == 0, |ui| {
                        rows::column(ui, TEMPLATE_KEYWORD_WIDTH, |ui| {
                            rows::text(ui, RichText::new(clip(&keywords, KEYWORD_CHARS)).strong(), &keywords);
                        });
                        ui.add_space(rows::GAP);
                        rows::from_right(ui, |ui| {
                            // 右から置くので、見た目は左から 挿入・編集・共有…・×。
                            if ui
                                .small_button("×")
                                .on_hover_text("このテンプレートを消す")
                                .clicked()
                            {
                                remove = Some(index);
                            }
                            if ui
                                .small_button("共有…")
                                .on_hover_text("GitHub の投稿フォームに項目を入れてブラウザで開きます(アプリは送信しません)")
                                .clicked()
                            {
                                share = Some(index);
                            }
                            if ui.small_button("編集").clicked() {
                                edit = Some(index);
                            }
                            if ui
                                .small_button("挿入")
                                .on_hover_text("カーソルの位置へ差し込む")
                                .clicked()
                            {
                                pending.insert = Some(template.body.clone());
                            }
                            ui.add_space(rows::GAP);
                            rows::rest(ui, |ui| {
                                let first_line = template.body.lines().next().unwrap_or("");
                                rows::text(ui, RichText::new(preview(&template.body)).color(p.dim), first_line);
                                if !names.is_empty() {
                                    let line = format!("モジュール: {names}");
                                    rows::text(ui, RichText::new(clip(&line, KEYWORD_CHARS)).color(p.dim), &line);
                                }
                            });
                        });
                    });
                    shown += 1;
                }
            });
        if let Some(index) = edit {
            tool.begin_edit(index, &self.data.dictionary.templates[index]);
        }
        if let Some(index) = share {
            tool.message = Some(match self.share_template(index) {
                Ok(message) => (message, false),
                Err(message) => (message, true),
            });
        }
        if let Some(index) = remove {
            self.data.dictionary.templates.remove(index);
            // 番号がずれるので、編集は取りやめる。
            tool.editing = None;
            pending.dict_changed = true;
        }
    }

    fn submit_template(&mut self, tool: &mut DictTool, pending: &mut Pending) {
        // 「〜テンプレート」と続けて言うので、命令語や記号の読みと同じキーワードでも構わない。
        let saved = self.data.dictionary.save_template_with_modules(
            tool.editing,
            &tool.new_keywords,
            &tool.new_body,
            &tool.new_modules,
        );
        match saved {
            Ok(_) => {
                tool.message = Some(("テンプレートを保存しました".to_string(), false));
                tool.new_keywords.clear();
                tool.new_body.clear();
                tool.new_modules.clear();
                tool.editing = None;
                pending.dict_changed = true;
            }
            Err(error) => tool.message = Some((error, true)),
        }
    }

    fn memory_tab(
        &mut self,
        ui: &mut egui::Ui,
        tool: &mut DictTool,
        pending: &mut Pending,
        p: &theme::Palette,
    ) {
        theme::hint(
            ui,
            "AI 訂正と手直しから覚えた「誤認識 → 正しい語」。回数が足りたものは認識直後に自動で当て、AI にも例として渡します。間違って覚えたものは × で消してください",
            p,
        );
        let memory = &self.data.memory;
        ui.label(
            RichText::new(format!(
                "覚えた組 {} / 認識 {} 回 / 訂正 {} 回",
                memory.pairs.len(),
                memory.utterances,
                memory.corrected
            ))
            .small()
            .color(p.dim),
        );
        let mut submit = false;
        ui.horizontal(|ui| {
            let a = ui.add(
                egui::TextEdit::singleline(&mut tool.new_from)
                    .desired_width(150.0)
                    .hint_text("誤認識"),
            );
            ui.label("→");
            let b = ui.add(
                egui::TextEdit::singleline(&mut tool.new_to)
                    .desired_width(150.0)
                    .hint_text("正しい語"),
            );
            if (a.lost_focus() || b.lost_focus()) && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                submit = true;
            }
            if theme::primary_button(ui, "覚える", p).clicked() {
                submit = true;
            }
        });
        if submit && changes_meaning(&tool.new_from, &tool.new_to) {
            tool.message = Some((
                "左右・否定・上下などが変わる組は、内容の変更になるので覚えられません".to_string(),
                true,
            ));
        } else if submit {
            let learned = self
                .data
                .memory
                .learn_pair(&tool.new_from, &tool.new_to, true);
            if !learned {
                tool.message = Some((
                    "覚えられない組です(2〜12 文字。同じ語や、句読点・数字だけの語は不可)"
                        .to_string(),
                    true,
                ));
            } else {
                tool.message = Some((
                    format!(
                        "「{}」→「{}」を覚えました",
                        tool.new_from.trim(),
                        tool.new_to.trim()
                    ),
                    false,
                ));
                tool.new_from.clear();
                tool.new_to.clear();
                pending.memory_changed = true;
            }
        }
        ui.add_space(4.0);
        let mut forget: Option<usize> = None;
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .max_height(ui.available_height() - 40.0)
            .show(ui, |ui| {
                egui::Grid::new("dict_memory")
                    .striped(true)
                    .num_columns(5)
                    .spacing([16.0, 6.0])
                    .show(ui, |ui| {
                        for h in ["誤認識", "正しい語", "回数", "最終", ""] {
                            ui.label(RichText::new(h).small().color(p.dim));
                        }
                        ui.end_row();
                        for index in self.data.memory.sorted_indices() {
                            let pair = &self.data.memory.pairs[index];
                            if !matches(&tool.filter, &[&pair.from, &pair.to]) {
                                continue;
                            }
                            ui.label(&pair.from);
                            ui.label(&pair.to);
                            ui.label(
                                RichText::new(format!(
                                    "AI {} / 手直し {}",
                                    pair.count, pair.manual
                                ))
                                .color(p.dim),
                            );
                            ui.label(RichText::new(&pair.last).color(p.dim));
                            if ui
                                .small_button("×")
                                .on_hover_text("この組を忘れる")
                                .clicked()
                            {
                                forget = Some(index);
                            }
                            ui.end_row();
                        }
                    });
            });
        if let Some(index) = forget {
            self.data.memory.forget(index);
            pending.memory_changed = true;
        }
        ui.horizontal(|ui| {
            let label = if tool.confirm_clear {
                "本当に消す"
            } else {
                "すべて消す"
            };
            if ui.button(label).clicked() {
                if tool.confirm_clear {
                    self.data.memory.clear();
                    tool.confirm_clear = false;
                    pending.memory_changed = true;
                } else {
                    tool.confirm_clear = true;
                }
            }
            if tool.confirm_clear && ui.button("やめる").clicked() {
                tool.confirm_clear = false;
            }
        });
    }
}
