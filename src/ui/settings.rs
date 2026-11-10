//! 設定画面。左に 10 の項目、右に選んだ項目の内容。
//! 編集は手元の写し(`SettingsDialog`)に対して行い、「適用」「OK」で本体へ反映する。

use super::theme::{self, Palette, SliderSpec};
use crate::app::{detector_problem, engine_problem, VoiceApp};
use crate::core::audio;
use crate::core::caps_lock;
use crate::core::assistant;
use crate::core::corrector;
use crate::core::proc;
use crate::core::mic_access;
use crate::core::models::{self, ModelFile};
use crate::core::record_key::{self, KeyCapture, Pressed};
use crate::core::remote_asr;
use crate::core::settings::{
    CorrectorBackend, GlobalSettings, RecognizerKind, RecordShortcut, ThemeChoice, TransferMethod,
    UserSettings, HISTORY_COUNTS, MIC_GAIN_RANGE, POST_SILENCE_RANGE, REQUEST_LISTEN_RANGE, REQUEST_LISTEN_STEP,
};
use crate::core::text_filter::{
    NumeralStyle, DATE_FORMATS, ENGLISH_STYLES, PUNCTUATION_KINDS, TIME_FORMATS, WIDTH_STYLES,
};
use crate::core::dictionary::{split_keywords, template_key};
use crate::core::voice_command::{CommandAction, CommandWords};
use egui::{Context, RichText, Ui};
use std::path::PathBuf;

const TRANSFER_PAGE: &str = "入力と転送";

pub const PAGES: [&str; 9] = [
    "マイク",
    "認識",
    "AI 訂正",
    TRANSFER_PAGE,
    "表現",
    "音声コマンド",
    "学習",
    "外観",
    "履歴とログ",
];

const NAV_WIDTH: f32 = 150.0;
/// 仕切りと下のボタンの分。実際より少し多めに取る(足りないと中身が窓より大きくなり、毎フレーム窓が伸びる)。
const FOOTER_HEIGHT: f32 = 64.0;

const MIC_GAIN: SliderSpec = SliderSpec {
    min: *MIC_GAIN_RANGE.start() as f64,
    max: *MIC_GAIN_RANGE.end() as f64,
    step: 0.1,
    decimals: 1,
    suffix: " 倍",
    low: "",
    high: "",
};
const POST_SILENCE: SliderSpec = SliderSpec {
    min: *POST_SILENCE_RANGE.start() as f64,
    max: *POST_SILENCE_RANGE.end() as f64,
    step: 50.0,
    decimals: 0,
    suffix: " ms",
    low: "速い",
    high: "切れにくい",
};
const REMOTE_TIMEOUT: SliderSpec = SliderSpec {
    min: 3.0,
    max: 60.0,
    step: 1.0,
    decimals: 0,
    suffix: " 秒",
    low: "",
    high: "",
};
const REQUEST_LISTEN: SliderSpec = SliderSpec {
    min: *REQUEST_LISTEN_RANGE.start() as f64,
    max: *REQUEST_LISTEN_RANGE.end() as f64,
    step: REQUEST_LISTEN_STEP as f64,
    decimals: 0,
    suffix: " 秒",
    low: "",
    high: "",
};
const FONT_SIZE: SliderSpec = SliderSpec {
    min: 12.0,
    max: 36.0,
    step: 1.0,
    decimals: 0,
    suffix: "",
    low: "小",
    high: "大",
};
const MIN_COUNT: SliderSpec = SliderSpec {
    min: 1.0,
    max: 5.0,
    step: 1.0,
    decimals: 0,
    suffix: " 回",
    low: "",
    high: "",
};
const SCALE: SliderSpec = SliderSpec {
    min: 0.8,
    max: 1.6,
    step: 0.05,
    decimals: 2,
    suffix: " 倍",
    low: "",
    high: "",
};
const LOG_LIMIT: SliderSpec = SliderSpec {
    min: 5.0,
    max: 500.0,
    step: 5.0,
    decimals: 0,
    suffix: " MB",
    low: "",
    high: "",
};

/// 開いている設定画面。値は本体の写しで、適用するまで本体には届かない。
pub struct SettingsDialog {
    /// PAGES の何番目を見ているか。
    pub page: usize,
    pub user: UserSettings,
    pub global: GlobalSettings,
    pub api_key: String,
    /// 保存済みの API キー(変更の有無の判定用)。
    api_key_saved: String,
    /// 外部の音声認識 API の鍵(provider, 入力中, 保存済み)。provider を切り替えても入力は残す。
    remote_keys: Vec<(String, String, String)>,
    /// 録音デバイスの一覧(開いた時点)。
    pub devices: Vec<String>,
    /// 認識エンジンごとの、使えない理由(開いた時点)。
    engine_problems: Vec<(RecognizerKind, Option<String>)>,
    /// 声の判定(Silero VAD)が使えない理由。使えるなら None。
    detector_problem: Option<String>,
    /// Claude Code の場所(開いた時点)。
    claude_path: Option<PathBuf>,
    /// 補足のファイルを開けなかった理由。
    guidance_error: Option<String>,
    /// 録音の切り替えキーの入力待ち。
    key_capture: Option<KeyCapture>,
    /// 切り替えキーを選んだ結果の知らせ。
    key_note: Option<String>,
    /// 音声コマンドの入力欄の文字(操作ごと)。「、」を打っている途中も崩さないよう、文字のまま持つ。
    command_text: Vec<(CommandAction, String)>,
    /// Claude を呼ぶ言葉の入力欄の文字(「、」区切り)。
    call_text: String,
    /// テンプレートのキーワード(開いた時点、重なりの注意に使う)。
    template_keywords: Vec<String>,
    /// 記号の読み(開いた時点、重なりの注意に使う)。
    symbol_readings: Vec<String>,
}

/// 描画中に集めておき、描き終えてから本体に対して行う操作。
#[derive(Clone, Copy, PartialEq, Eq)]
enum Action {
    IphoneHelp,
    MicHelp,
    AskAccessibility,
    AccessibilityHelp,
    OpenDictMemory,
    RevealData,
    EditGuidance,
    EditRequestGuidance,
    EditDictionary,
    FetchModels,
    CancelModels,
    ClearHistory,
}

/// 下のボタンの結果。
#[derive(Clone, Copy, PartialEq, Eq)]
enum Outcome {
    None,
    Cancel,
    Apply,
    Ok,
}

/// 本体から読むだけの情報(描画中に `self` を借りないための写し)。
struct Info {
    iphone_found: Option<String>,
    accessibility_ok: bool,
    corrector_name: Option<String>,
    assistant_name: Option<String>,
    guidance_chars: Option<usize>,
    request_guidance_chars: Option<usize>,
    pairs: usize,
    utterances: u64,
    corrected: u64,
    voice_minutes: f64,
    log_path: Option<PathBuf>,
    data_root: PathBuf,
    /// 取得できるモデルと、置いてあるか。
    models: Vec<(ModelFile, bool)>,
    /// 取得中なら(取れた分, 全体)のバイト数。
    download: Option<(u64, u64)>,
    /// 最後の取得で取れなかった理由。
    download_error: Option<String>,
}

/// 名前(古い呼び名も可)から項目の番号を引く。
fn page_index(name: &str) -> usize {
    let name = match name {
        "録音デバイス" => "マイク",
        "エディター" => "入力と転送",
        other => other,
    };
    PAGES.iter().position(|p| *p == name).unwrap_or(0)
}

impl SettingsDialog {
    /// 入力欄の文字を、保存する形に直す。
    fn command_words(&self) -> Vec<CommandWords> {
        self.command_text
            .iter()
            .map(|(action, text)| CommandWords {
                action: *action,
                words: split_keywords(text),
            })
            .collect()
    }
}

impl VoiceApp {
    pub fn open_settings(&mut self, page: Option<&str>) {
        let index = page.map(page_index);
        if let Some(dialog) = &mut self.windows.settings {
            if let Some(index) = index {
                dialog.page = index;
            }
            return;
        }
        let api_key = self.store.load_api_key().unwrap_or_default();
        self.windows.settings = Some(SettingsDialog {
            page: index.unwrap_or(0),
            user: self.data.settings.clone(),
            global: self.global.clone(),
            api_key_saved: api_key.clone(),
            api_key,
            remote_keys: remote_asr::PROVIDERS
                .iter()
                .map(|(id, ..)| {
                    let key = self.store.load_remote_key(id).unwrap_or_default();
                    (id.to_string(), key.clone(), key)
                })
                .collect(),
            devices: audio::input_device_names(),
            engine_problems: RecognizerKind::ALL
                .iter()
                .map(|kind| (*kind, engine_problem(*kind)))
                .collect(),
            detector_problem: detector_problem(),
            claude_path: proc::find_claude(),
            guidance_error: None,
            key_capture: None,
            key_note: None,
            command_text: self
                .data
                .settings
                .command_words
                .iter()
                .map(|c| (c.action, c.words.join("、")))
                .collect(),
            call_text: self.data.settings.call_words.join("、"),
            template_keywords: self
                .data
                .dictionary
                .template_keywords()
                .map(str::to_string)
                .collect(),
            symbol_readings: self
                .data
                .dictionary
                .symbols
                .iter()
                .flat_map(|s| s.readings.iter().cloned())
                .collect(),
        });
    }

    pub fn draw_settings(&mut self, ctx: &Context) {
        let Some(mut dialog) = self.windows.settings.take() else {
            return;
        };
        let p = theme::palette(ctx);
        let info = Info {
            iphone_found: self.iphone_found().map(str::to_string),
            accessibility_ok: self.accessibility_ok,
            corrector_name: self.corrector_name(),
            assistant_name: self.assistant_name(),
            guidance_chars: self.guidance_chars,
            request_guidance_chars: self.request_guidance_chars,
            pairs: self.data.memory.pairs.len(),
            utterances: self.data.memory.utterances,
            corrected: self.data.memory.corrected,
            voice_minutes: self.data.memory.voice_seconds / 60.0,
            log_path: crate::core::diag::path(),
            data_root: self.store.root.clone(),
            models: models::catalog()
                .into_iter()
                .map(|model| {
                    let installed = models::installed(&model.id).is_some();
                    (model, installed)
                })
                .collect(),
            download: self.model_download_progress(),
            download_error: self.download_error.clone(),
        };
        let mut actions: Vec<Action> = Vec::new();
        let mut outcome = Outcome::None;
        let mut open = true;
        // 拡大率が高いと本体の窓より大きくなり、上下が切れるので、本体に収まる大きさに抑える。
        let screen = ctx.screen_rect();
        let size = egui::vec2(
            780.0_f32.min(screen.width() - 24.0),
            560.0_f32.min(screen.height() - 72.0),
        );
        egui::Window::new("設定")
            .collapsible(false)
            .resizable(true)
            .open(&mut open)
            .default_size(size)
            .max_size(size)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                let body_height = (ui.available_height() - FOOTER_HEIGHT)
                    .max(200.0)
                    .min(size.y - FOOTER_HEIGHT);
                draw_body(ui, &mut dialog, p, &info, &mut actions, body_height);
                ui.separator();
                outcome = draw_footer(ui, p);
            });

        for action in actions {
            self.run_settings_action(action, &info, &mut dialog);
        }
        match outcome {
            Outcome::Apply | Outcome::Ok => self.apply_settings(&mut dialog),
            _ => {}
        }
        let closed = !open || matches!(outcome, Outcome::Cancel | Outcome::Ok);
        if !closed {
            self.windows.settings = Some(dialog);
        }
    }

    fn run_settings_action(&mut self, action: Action, info: &Info, dialog: &mut SettingsDialog) {
        match action {
            Action::IphoneHelp => self.windows.iphone_help = true,
            Action::MicHelp => self.windows.mic_help = true,
            Action::AskAccessibility => self.ask_accessibility(),
            Action::AccessibilityHelp => self.windows.accessibility_help = true,
            Action::OpenDictMemory => self.open_dict_tool(Some(crate::ui::DictTab::Memory)),
            Action::RevealData => {
                if let Err(message) = mic_access::reveal_in_finder(&info.data_root) {
                    self.notify_error(message);
                }
            }
            Action::EditGuidance => {
                let opened = self
                    .store
                    .ensure_guidance()
                    .and_then(|path| mic_access::open_in_text_editor(&path));
                dialog.guidance_error = opened.err().map(|e| format!("guidance.md: {e}"));
            }
            Action::EditRequestGuidance => {
                let opened = self
                    .store
                    .ensure_request_guidance()
                    .and_then(|path| mic_access::open_in_text_editor(&path));
                dialog.guidance_error = opened.err().map(|e| format!("request.md: {e}"));
            }
            Action::FetchModels => self.start_model_download(),
            Action::CancelModels => self.cancel_model_download(),
            Action::ClearHistory => self.clear_history(),
            Action::EditDictionary => {
                // 辞書ファイルは起動時に作られるが、消されていれば今の辞書で作り直してから開く。
                let path = self.store.dictionary_path();
                let ensured = if path.exists() {
                    Ok(())
                } else {
                    self.store.save_dictionary(&self.data.dictionary)
                };
                if let Err(message) =
                    ensured.and_then(|()| mic_access::open_in_text_editor(&path))
                {
                    self.notify_error(message);
                }
            }
        }
    }

    /// 手元の写しを本体へ反映し、変わった項目に応じて必要な作り直しをする。
    fn apply_settings(&mut self, dialog: &mut SettingsDialog) {
        let old_global = self.global.clone();
        let old_user = self.data.settings.clone();
        dialog.user.command_words = dialog.command_words();
        dialog.user.call_words = split_keywords(&dialog.call_text);
        self.data.settings = dialog.user.clone();
        self.global = dialog.global.clone();
        // 設定の窓は開いた時点の写しを持つので、その後に立った「初回の案内を出した」を戻さない。
        self.global.first_run_shown |= old_global.first_run_shown;

        let new_prefer_iphone = self.global.prefer_iphone;
        let prefer_iphone_changed = new_prefer_iphone != old_global.prefer_iphone;
        if prefer_iphone_changed {
            // 切り替えの処理(探す・録り直す)は set_prefer_iphone に任せる。
            self.global.prefer_iphone = old_global.prefer_iphone;
        }

        let key_changed = dialog.api_key != dialog.api_key_saved;
        if key_changed {
            match self.store.save_api_key(&dialog.api_key) {
                Ok(()) => dialog.api_key_saved = dialog.api_key.clone(),
                Err(message) => self.notify_error(message),
            }
        }
        let mut remote_key_changed = false;
        for (provider, key, saved) in &mut dialog.remote_keys {
            if key != saved {
                match self.store.save_remote_key(provider, key) {
                    Ok(()) => {
                        *saved = key.clone();
                        remote_key_changed = true;
                    }
                    Err(message) => self.notify_error(message),
                }
            }
        }
        if remote_key_changed
            || self.global.remote != old_global.remote
            || self.global.recognizer != old_global.recognizer
        {
            self.rebuild_remote();
        }
        self.save_data();
        self.save_global();

        // 外部 API は Haas のあとの段なので、Haas と外部 API の切り替えではエンジン(Haas)を読み直さない。
        let haas_based = |kind: RecognizerKind| matches!(kind, RecognizerKind::Haas | RecognizerKind::Remote);
        let engine_changed = self.global.recognizer != old_global.recognizer
            && !(haas_based(self.global.recognizer) && haas_based(old_global.recognizer));
        if engine_changed {
            self.rebuild_engine();
        } else {
            self.refresh_engine_options();
        }

        let g = &self.global;
        let s = &self.data.settings;
        let mic_changed = g.input_device != old_global.input_device
            || g.mic_gain != old_global.mic_gain
            || s.engine.post_silence_ms != old_user.engine.post_silence_ms;
        if prefer_iphone_changed {
            self.set_prefer_iphone(new_prefer_iphone);
        } else if mic_changed && !engine_changed {
            // エンジンを作り直したときは、その中で新しい設定のまま録り直している。
            self.restart_mic_if_on();
        }

        let s = &self.data.settings;
        if s.appearance != old_user.appearance || s.editor.top_most != old_user.editor.top_most {
            self.apply_appearance();
        }
        if old_user.record.shortcut == RecordShortcut::CapsLock
            && self.data.settings.record.shortcut != RecordShortcut::CapsLock
        {
            self.leave_caps_lock();
        }
        self.register_hotkey();
        if self.data.settings.history_count != old_user.history_count {
            let count = self.data.settings.history_count;
            self.history.set_capacity(count);
        }
    }
}

// ---- 枠組み ----

/// 左の一覧・仕切り・右の内容。
fn draw_body(
    ui: &mut Ui,
    d: &mut SettingsDialog,
    p: &Palette,
    info: &Info,
    actions: &mut Vec<Action>,
    height: f32,
) {
    // キーの入力待ちは「入力と転送」の画面でしか受け取らない。ほかの画面へ移ったらやめる(残すと入力欄が打てない)。
    if PAGES.get(d.page) != Some(&TRANSFER_PAGE) {
        d.key_capture = None;
        d.key_note = None;
    }
    // 横並びの中なので、各列は上から下への配置を明示する(省くと項目が横に流れる)。
    // 横並び自体の高さも body の高さに限る(限らないと間の仕切り線が窓の下まで伸び、中身が窓より高くなる)。
    let column = egui::Layout::top_down(egui::Align::Min);
    let row = egui::Layout::left_to_right(egui::Align::Min);
    ui.allocate_ui_with_layout(egui::vec2(ui.available_width(), height), row, |ui| {
        ui.allocate_ui_with_layout(egui::vec2(NAV_WIDTH, height), column, |ui| {
            ui.set_min_size(egui::vec2(NAV_WIDTH, height));
            ui.spacing_mut().item_spacing.y = 2.0;
            for (index, name) in PAGES.iter().enumerate() {
                if nav_row(ui, name, d.page == index, p).clicked() {
                    d.page = index;
                }
            }
        });
        ui.separator();
        ui.allocate_ui_with_layout(egui::vec2(ui.available_width(), height), column, |ui| {
            egui::ScrollArea::vertical()
                .id_salt(("settings_page", d.page))
                .auto_shrink([false, false])
                .max_height(height)
                .show(ui, |ui| {
                    theme::card(p).show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        let page = PAGES[d.page.min(PAGES.len() - 1)];
                        ui.heading(page);
                        ui.add_space(6.0);
                        match page {
                            "マイク" => page_mic(ui, d, p, info, actions),
                            "認識" => page_recognition(ui, d, p, info, actions),
                            "AI 訂正" => page_corrector(ui, d, p, info, actions),
                            TRANSFER_PAGE => page_transfer(ui, d, p, info, actions),
                            "表現" => page_expression(ui, d),
                            "音声コマンド" => page_commands(ui, d, p, info, actions),
                            "学習" => page_learning(ui, d, p, info, actions),
                            "外観" => page_appearance(ui, d, p),
                            _ => page_history(ui, d, p, info, actions),
                        }
                    });
                });
        });
    });
}

/// 左の一覧の 1 行。選んでいる行は薄い青の地と青い文字。
fn nav_row(ui: &mut Ui, label: &str, selected: bool, p: &Palette) -> egui::Response {
    let (rect, response) =
        ui.allocate_exact_size(egui::vec2(NAV_WIDTH, 32.0), egui::Sense::click());
    if ui.is_rect_visible(rect) {
        if selected {
            ui.painter().rect_filled(rect, theme::RADIUS, p.accent_soft);
        } else if response.hovered() {
            ui.painter().rect_filled(rect, theme::RADIUS, p.bg);
        }
        let color = if selected { p.accent } else { p.text };
        let font = egui::TextStyle::Button.resolve(ui.style());
        let galley = ui.painter().layout_no_wrap(label.to_string(), font, color);
        let pos = egui::pos2(rect.left() + 12.0, rect.center().y - galley.size().y / 2.0);
        ui.painter().galley(pos, galley, color);
    }
    response
}

/// 下のボタン(右寄せ: OK・適用・キャンセルの順に右から置く)。
fn draw_footer(ui: &mut Ui, p: &Palette) -> Outcome {
    let mut outcome = Outcome::None;
    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
        if theme::primary_button(ui, "OK", p).clicked() {
            outcome = Outcome::Ok;
        }
        if ui.button("適用").clicked() {
            outcome = Outcome::Apply;
        }
        if theme::ghost_button(ui, "キャンセル", p).clicked() {
            outcome = Outcome::Cancel;
        }
    });
    outcome
}

// ---- 部品 ----

/// 選択肢から 1 つ選ぶ箱。
fn combo<T: PartialEq + Clone>(ui: &mut Ui, id: &str, value: &mut T, options: &[(T, String)]) {
    let selected = options
        .iter()
        .find(|(v, _)| v == value)
        .map_or(String::new(), |(_, label)| label.clone());
    egui::ComboBox::from_id_salt(id)
        .selected_text(selected)
        .width(320.0)
        .show_ui(ui, |ui| {
            for (v, label) in options {
                if ui.selectable_label(v == value, label).clicked() {
                    *value = v.clone();
                }
            }
        });
}

/// ラベルと選択箱を 1 行に並べる。
fn combo_row<T: PartialEq + Clone>(
    ui: &mut Ui,
    label: &str,
    id: &str,
    value: &mut T,
    options: &[(T, String)],
) {
    ui.horizontal(|ui| {
        ui.label(label);
        combo(ui, id, value, options);
    });
}

/// 添字で選ぶ箱(日付の書式など)。
fn combo_index_row(ui: &mut Ui, label: &str, id: &str, value: &mut usize, labels: Vec<String>) {
    let options: Vec<(usize, String)> = labels.into_iter().enumerate().collect();
    combo_row(ui, label, id, value, &options);
}

/// フォルダを選ぶ行。None は既定の場所。
fn folder_row(ui: &mut Ui, label: &str, value: &mut Option<PathBuf>, p: &Palette) {
    ui.label(label);
    ui.horizontal(|ui| {
        let text = value
            .as_ref()
            .map_or_else(|| "既定の場所".to_string(), |path| path.display().to_string());
        let color = if value.is_some() { p.text } else { p.dim };
        ui.label(RichText::new(text).small().color(color));
    });
    ui.horizontal(|ui| {
        if ui.button("選ぶ").clicked() {
            if let Some(path) = rfd::FileDialog::new().pick_folder() {
                *value = Some(path);
            }
        }
        if ui
            .add_enabled(value.is_some(), egui::Button::new("既定に戻す"))
            .clicked()
        {
            *value = None;
        }
    });
    ui.add_space(4.0);
}

// ---- 1. マイク ----

fn page_mic(
    ui: &mut Ui,
    d: &mut SettingsDialog,
    p: &Palette,
    info: &Info,
    actions: &mut Vec<Action>,
) {
    let mut options: Vec<(Option<String>, String)> = vec![(None, "既定のマイク".to_string())];
    for name in &d.devices {
        let mut label = audio::display_device_name(name);
        if audio::is_iphone_device(name) {
            label.push_str(" (iPhone 連係)");
        }
        options.push((Some(name.clone()), label));
    }
    // 保存済みのデバイスが今は見えなくても、選択状態は保つ。
    if let Some(current) = &d.global.input_device {
        if !options
            .iter()
            .any(|(v, _)| v.as_deref() == Some(current.as_str()))
        {
            options.push((
                Some(current.clone()),
                format!("{}(未接続)", audio::display_device_name(current)),
            ));
        }
    }
    combo_row(
        ui,
        "録音デバイス",
        "mic_device",
        &mut d.global.input_device,
        &options,
    );

    ui.checkbox(
        &mut d.global.prefer_iphone,
        "近くの iPhone が見つかればそのマイクを使う",
    );
    ui.horizontal(|ui| {
        ui.add_space(20.0);
        match &info.iphone_found {
            Some(name) => {
                ui.label(
                    RichText::new(format!(
                        "見つかっています: {}",
                        audio::display_device_name(name)
                    ))
                    .small()
                    .color(p.success),
                );
            }
            None => {
                ui.label(RichText::new("見つかっていません").small().color(p.dim));
                if ui.small_button("接続方法").clicked() {
                    actions.push(Action::IphoneHelp);
                }
            }
        }
    });

    theme::section(ui, "音量と切り出し", p);
    theme::slider_row(ui, "マイクの増幅", &mut d.global.mic_gain, &MIC_GAIN, p);
    theme::hint(
        ui,
        "声が小さくて拾われないときに上げます。「声が大きすぎます」と出るなら下げます",
        p,
    );
    theme::slider_row(
        ui,
        "話し終わりと判断する無音",
        &mut d.user.engine.post_silence_ms,
        &POST_SILENCE,
        p,
    );
    theme::hint(
        ui,
        "短いほど速く文字が出ますが、文の途中で切れやすくなります",
        p,
    );
    if let Some(problem) = &d.detector_problem {
        ui.label(RichText::new(problem).small().color(p.warning));
    }

    ui.add_space(8.0);
    if ui.button("マイクの許可について").clicked() {
        actions.push(Action::MicHelp);
    }
}

// ---- 2. 認識 ----

fn page_recognition(
    ui: &mut Ui,
    d: &mut SettingsDialog,
    p: &Palette,
    info: &Info,
    actions: &mut Vec<Action>,
) {
    let kinds: Vec<(RecognizerKind, String)> = RecognizerKind::ALL
        .iter()
        .map(|k| (*k, k.label().to_string()))
        .collect();
    combo_row(
        ui,
        "認識エンジン",
        "recognizer",
        &mut d.global.recognizer,
        &kinds,
    );
    let chosen = d.global.recognizer;
    let problem = d
        .engine_problems
        .iter()
        .find(|(kind, _)| *kind == chosen)
        .and_then(|(_, problem)| problem.as_deref());
    if let Some(problem) = problem {
        ui.label(RichText::new(problem).small().color(p.warning));
    }
    if chosen == RecognizerKind::Remote {
        remote_section(ui, d, p);
    }
    ui.add_space(8.0);
    theme::section(ui, "モデル", p);
    for (model, installed) in &info.models {
        let (mark, color) = if *installed {
            ("取得済み", p.success)
        } else {
            ("まだありません", p.warning)
        };
        ui.horizontal(|ui| {
            ui.label(&model.title);
            ui.label(RichText::new(mark).small().color(color));
        });
        ui.label(
            RichText::new(format!("{}(ライセンス {})", model.source, model.license))
                .small()
                .color(p.dim),
        );
    }
    let missing: u64 = info
        .models
        .iter()
        .filter(|(_, installed)| !installed)
        .map(|(model, _)| model.size)
        .sum();
    match info.download {
        Some((done, total)) => {
            ui.label(format!(
                "取得しています: {} / {} MB(取得中はマイクを使えません)",
                done / 1_000_000,
                total.max(1) / 1_000_000
            ));
            if ui.button("取得をやめる").clicked() {
                actions.push(Action::CancelModels);
            }
        }
        None if missing > 0 => {
            let label = format!("モデルを取得(約 {} MB)", missing.div_ceil(1_000_000));
            if ui.button(label).clicked() {
                actions.push(Action::FetchModels);
            }
            ui.label(
                RichText::new(
                    "Hugging Face(huggingface.co)から取ります。\
                     取ったファイルは大きさと SHA-256 を確かめてから使います。止まっても、次は続きから取ります。",
                )
                .small()
                .color(p.dim),
            );
        }
        None => {}
    }
    if let (None, Some(error)) = (info.download, &info.download_error) {
        ui.label(RichText::new(format!("前回の取得: {error}")).small().color(p.warning));
    }
}

// ---- 3. AI 訂正 ----

/// 指示の字数の表示(`<!-- -->` の注釈は数えない)。`limit` はその指示の上限。
fn guidance_status(chars: usize, limit: usize) -> String {
    match chars {
        0 => "指示: アプリの既定".to_string(),
        n if n > limit => format!("指示: {n} 字(先頭の {limit} 字だけ使います)"),
        n => format!("指示: {n} 字"),
    }
}

/// 指示のファイルを開くボタンと、その右の字数(読めなければその旨)。
fn guidance_row(ui: &mut Ui, p: &Palette, label: &str, chars: Option<usize>, limit: usize) -> bool {
    let mut clicked = false;
    ui.horizontal(|ui| {
        clicked = ui.button(label).clicked();
        match chars {
            Some(chars) => theme::hint(ui, &guidance_status(chars, limit), p),
            None => {
                ui.label(
                    RichText::new("指示を読めません(文字コードを UTF-8 にして保存してください)")
                        .small()
                        .color(p.danger),
                );
            }
        }
    });
    clicked
}

fn page_corrector(
    ui: &mut Ui,
    d: &mut SettingsDialog,
    p: &Palette,
    info: &Info,
    actions: &mut Vec<Action>,
) {
    ui.checkbox(
        &mut d.global.corrector.enabled,
        "認識した文を Claude で直す",
    );
    let backends: Vec<(CorrectorBackend, String)> = CorrectorBackend::ALL
        .iter()
        .map(|b| (*b, b.label().to_string()))
        .collect();
    combo_row(
        ui,
        "接続方法",
        "corrector_backend",
        &mut d.global.corrector.backend,
        &backends,
    );
    let models: Vec<(String, String)> = corrector::MODELS
        .iter()
        .map(|(id, label)| (id.to_string(), label.to_string()))
        .collect();
    combo_row(
        ui,
        "モデル",
        "corrector_model",
        &mut d.global.corrector.model,
        &models,
    );
    ui.add_space(4.0);
    match d.global.corrector.backend {
        CorrectorBackend::ApiKey => corrector_api_key(ui, d, p),
        CorrectorBackend::ClaudeCode => corrector_claude_code(ui, d, p),
    }
    ui.add_space(6.0);
    if guidance_row(ui, p, "訂正の指示を編集…", info.guidance_chars, corrector::MAX_GUIDANCE_CHARS) {
        actions.push(Action::EditGuidance);
    }
    if guidance_row(
        ui,
        p,
        "依頼の指示を編集…",
        info.request_guidance_chars,
        assistant::MAX_REQUEST_GUIDANCE_CHARS,
    ) {
        actions.push(Action::EditRequestGuidance);
    }
    theme::hint(
        ui,
        "直し方の規則は guidance.md、「ヘイクロード」の依頼の規則は request.md にあり、書き換えられます。空ならアプリの既定を使います",
        p,
    );
    if let Some(error) = &d.guidance_error {
        ui.label(RichText::new(error).small().color(p.danger));
    }
    ui.add_space(6.0);
    let current = info.corrector_name.as_deref().unwrap_or("なし");
    ui.label(RichText::new(format!("いまの訂正役: {current}")).color(p.dim));
}

/// 外部の音声認識 API の欄(認識エンジンで「外部の音声認識 API」を選んだとき)。
fn remote_section(ui: &mut Ui, d: &mut SettingsDialog, p: &Palette) {
    ui.add_space(6.0);
    theme::section(ui, "外部の音声認識 API", p);
    let providers: Vec<(String, String)> = remote_asr::PROVIDERS
        .iter()
        .map(|(id, label, ..)| (id.to_string(), label.to_string()))
        .collect();
    if !remote_asr::is_known_provider(&d.global.remote.provider) {
        ui.label(
            RichText::new(format!("知らない API が設定されています({})。選び直してください", d.global.remote.provider))
                .small()
                .color(p.warning),
        );
    }
    let before = d.global.remote.provider.clone();
    combo_row(ui, "API", "remote_provider", &mut d.global.remote.provider, &providers);
    let provider = d.global.remote.provider.clone();
    // モデル名は API ごとに違うので、API を替えたら既定に戻す。
    if provider != before {
        d.global.remote.model.clear();
    }
    if let Some((.., note)) = remote_asr::PROVIDERS.iter().find(|(id, ..)| *id == provider) {
        theme::hint(ui, note, p);
    }
    ui.horizontal(|ui| {
        ui.label("モデル");
        ui.add(
            egui::TextEdit::singleline(&mut d.global.remote.model)
                .desired_width(220.0)
                .hint_text(remote_asr::default_model(&provider)),
        );
    });
    if let Some((_, key, saved)) = d.remote_keys.iter_mut().find(|(id, ..)| *id == provider) {
        ui.horizontal(|ui| {
            ui.label("API キー");
            ui.add(egui::TextEdit::singleline(key).password(true).desired_width(320.0));
        });
        let (text, color) = if key.trim().is_empty() {
            ("キーが未設定(Haas だけで動きます)", p.warning)
        } else if key == saved {
            ("保存済み", p.success)
        } else {
            ("未保存(OK か適用で保存)", p.warning)
        };
        ui.label(RichText::new(text).small().color(color));
    }
    theme::slider_row(ui, "答えを待つ上限", &mut d.global.remote.timeout_secs, &REMOTE_TIMEOUT, p);
    ui.checkbox(
        &mut d.global.remote.send_glossary,
        "辞書の単語を用語のヒントとして送る(読みと句は送りません)",
    );
    theme::hint(
        ui,
        "Haas が先に文字にして表示と命令語の判定に使い、API の文が届いたら差し替えて AI 訂正に回します。音声は、Haas が声と判定した発話だけをこの API に送ります。失敗・時間切れ・長すぎるか短すぎる答えのときは Haas の文で進みます。",
        p,
    );
}

fn corrector_api_key(ui: &mut Ui, d: &mut SettingsDialog, p: &Palette) {
    ui.horizontal(|ui| {
        ui.label("API キー");
        ui.add(
            egui::TextEdit::singleline(&mut d.api_key)
                .password(true)
                .desired_width(320.0)
                .hint_text("sk-ant-..."),
        );
    });
    let key = d.api_key.trim();
    if key.is_empty() {
        ui.label(
            RichText::new("キーが未設定(AI 訂正は働きません)")
                .small()
                .color(p.dim),
        );
    } else if d.api_key == d.api_key_saved {
        ui.label(RichText::new("保存済み").small().color(p.success));
    } else {
        ui.label(
            RichText::new("未保存(OK か適用で保存)")
                .small()
                .color(p.warning),
        );
    }
}

fn corrector_claude_code(ui: &mut Ui, d: &SettingsDialog, p: &Palette) {
    match &d.claude_path {
        Some(path) => {
            ui.label(
                RichText::new(format!("Claude Code: {}", path.display()))
                    .small()
                    .color(p.success),
            );
        }
        None => {
            ui.label(
                RichText::new("Claude Code が見つかりません(AI 訂正は働きません)")
                    .small()
                    .color(p.warning),
            );
        }
    }
    ui.label(
        RichText::new(
            "文章は個人向けの条件で送られます。Claude のプライバシー設定で「モデルの改善に協力」がオンだと、学習に使われ、長く保持されます",
        )
        .small()
        .color(p.danger),
    );
}

// ---- 4. 入力と転送 ----

fn page_transfer(
    ui: &mut Ui,
    d: &mut SettingsDialog,
    p: &Palette,
    info: &Info,
    actions: &mut Vec<Action>,
) {
    let methods: Vec<(TransferMethod, String)> = TransferMethod::ALL
        .iter()
        .map(|m| (*m, m.label().to_string()))
        .collect();
    combo_row(
        ui,
        "転送の方法",
        "transfer_method",
        &mut d.user.transfer.method,
        &methods,
    );
    ui.checkbox(
        &mut d.user.transfer.restore_clipboard,
        "貼り付け後にクリップボードを元に戻す",
    );
    ui.checkbox(&mut d.user.transfer.enter_after, "転送後に Enter を押す");
    ui.horizontal(|ui| {
        if info.accessibility_ok {
            ui.label(RichText::new("転送の許可: あり").color(p.success));
        } else {
            ui.label(RichText::new("転送の許可: なし").color(p.danger));
            if ui.button("許可を求める").clicked() {
                actions.push(Action::AskAccessibility);
            }
            if ui.button("手順を見る").clicked() {
                actions.push(Action::AccessibilityHelp);
            }
        }
    });

    theme::section(ui, "ショートカット", p);
    record_key_row(ui, d, p);

    theme::section(ui, "エディター", p);
    theme::slider_row(ui, "文字の大きさ", &mut d.user.editor.font_size, &FONT_SIZE, p);
    ui.checkbox(
        &mut d.user.editor.confirm_close,
        "文章が残っているとき、閉じる前に確認する",
    );
    ui.checkbox(&mut d.user.editor.show_return, "改行の印を出す");
    ui.checkbox(&mut d.user.editor.top_most, "常に手前に表示");
    ui.checkbox(
        &mut d.user.editor.volume_warning,
        "声が大きすぎるときに知らせる",
    );
}

/// 録音の切り替えキーを選ぶ。「変更」のあとに押したキーをそのまま使う。
fn record_key_row(ui: &mut Ui, d: &mut SettingsDialog, p: &Palette) {
    if let Some(pressed) = d.key_capture.as_ref().and_then(KeyCapture::poll) {
        d.key_note = None;
        match pressed {
            Pressed::Cancel => d.key_capture = None,
            Pressed::CapsLock => {
                // 押したことで本物の Caps Lock も変わっている。選んだだけで録音が始まらないよう戻す。
                if d.user.record.shortcut != RecordShortcut::CapsLock {
                    caps_lock::set(false);
                }
                d.user.record.shortcut = RecordShortcut::CapsLock;
                d.key_capture = None;
            }
            Pressed::Key {
                key_code,
                held,
                characters,
            } => match record_key::accept_key(key_code, held, &characters) {
                Ok(combo) => {
                    d.user.record.shortcut = RecordShortcut::Key(combo);
                    d.key_capture = None;
                }
                // 入力待ちのまま、別のキーを押してもらう。
                Err(message) => d.key_note = Some(message),
            },
        }
    }
    ui.horizontal(|ui| {
        ui.label("録音の切り替えキー");
        if d.key_capture.is_some() {
            ui.label(
                RichText::new("使うキーを押してください(Esc でやめる)")
                    .strong()
                    .color(p.accent),
            );
            if ui.button("やめる").clicked() {
                d.key_capture = None;
                d.key_note = None;
            }
            return;
        }
        ui.label(RichText::new(d.user.record.shortcut.label()).strong());
        if ui.button("変更").clicked() {
            d.key_capture = KeyCapture::start();
            d.key_note = d
                .key_capture
                .is_none()
                .then(|| "キーの入力を受け取れませんでした".to_string());
        }
        if d.user.record.shortcut != RecordShortcut::CapsLock
            && ui.button("Caps Lock に戻す").clicked()
        {
            d.user.record.shortcut = RecordShortcut::CapsLock;
        }
    });
    if let Some(note) = &d.key_note {
        ui.label(RichText::new(note).small().color(p.danger));
    }
    if d.user.record.shortcut == RecordShortcut::CapsLock {
        theme::hint(
            ui,
            "Caps Lock のランプが点いている間は認識し、消えている間は止めます。認識中はキーボードの文字が英大文字になります",
            p,
        );
    } else {
        ui.checkbox(&mut d.user.record.hold, "押している間だけ録音");
    }
}

// ---- 5. 表現 ----

fn page_expression(ui: &mut Ui, d: &mut SettingsDialog) {
    let f = &mut d.user.filters;
    let numerals: Vec<(NumeralStyle, String)> = NumeralStyle::ALL
        .iter()
        .map(|n| (*n, n.label().to_string()))
        .collect();
    combo_row(ui, "数字の書き方", "numeral", &mut f.numeral, &numerals);
    let dates = DATE_FORMATS
        .iter()
        .map(|(_, example)| example.to_string())
        .collect();
    combo_index_row(ui, "日付", "date_format", &mut f.date_format, dates);
    let times = TIME_FORMATS
        .iter()
        .map(|(_, example)| example.to_string())
        .collect();
    combo_index_row(ui, "時刻", "time_format", &mut f.time_format, times);
    ui.checkbox(&mut f.remove_interjections, "えー・あのー等を消す");
    ui.checkbox(&mut f.auto_punctuation, "句読点を自動で付ける");
    let kinds = PUNCTUATION_KINDS
        .iter()
        .map(|(a, b)| format!("{a} {b}"))
        .collect();
    combo_index_row(
        ui,
        "句読点の種類",
        "punctuation",
        &mut f.punctuation_kind,
        kinds,
    );
    let english = ENGLISH_STYLES.iter().map(|s| s.to_string()).collect();
    combo_index_row(ui, "英単語", "english_style", &mut f.english_style, english);
    let widths = WIDTH_STYLES.iter().map(|s| s.to_string()).collect();
    combo_index_row(ui, "全角・半角", "width_style", &mut f.width_style, widths);
}

// ---- 6. 音声コマンド ----

fn page_commands(
    ui: &mut Ui,
    d: &mut SettingsDialog,
    p: &Palette,
    info: &Info,
    actions: &mut Vec<Action>,
) {
    theme::hint(
        ui,
        "別の言い方を足せます。認識結果に現れる表記で登録します(「、」で区切ると複数)。文の中に出てくる語(開業 など)は避けてください",
        p,
    );
    ui.add_space(4.0);
    egui::Grid::new("voice_commands")
        .num_columns(2)
        .spacing([16.0, 6.0])
        .show(ui, |ui| {
            for (action, text) in &mut d.command_text {
                ui.label(RichText::new(action.label()).strong());
                ui.add(
                    egui::TextEdit::singleline(text)
                        .desired_width(320.0)
                        .hint_text(format!("「{}」のほかの言い方", action.label())),
                );
                ui.end_row();
            }
        });
    for note in command_notes(&d.command_words(), &d.symbol_readings) {
        ui.label(RichText::new(note).small().color(p.warning));
    }
    ui.add_space(14.0);
    ui.label(RichText::new("Claude を呼ぶ言葉").strong());
    theme::hint(
        ui,
        &format!(
            "「ヘイクロード、前立腺MRIのテンプレート。サイズは5.5かける4.5かける5.0」のように話すと、Claude が辞書ファイルのテンプレートから合うものを選び、言われた値を入れた文章をエディターに入れます。文章を選んで(選ばなければ全文)「意味が一貫するように整えて」のように頼むと、直した文で置き換えます(直後の Esc で戻せます)。呼びかけだけなら、15 秒以内に話し始めてください。話し始めてから {} 秒の間に言ったことをまとめて 1 つの依頼にします(途中で言葉が切れても続きを待ちます。「以上」と言えばすぐ送ります)",
            d.user.request_listen().as_secs()
        ),
        p,
    );
    theme::hint(
        ui,
        "依頼の文、エディターの文章(選んだ範囲か全文、先頭 3000 字)、テンプレートの一覧が Claude に送られます(音声は送りません)。接続は「AI 訂正」の設定を使います。句読点や、かな・カタカナの違いは同じ語と見なします(「ヘイ、クロード」でも働きます)",
        p,
    );
    ui.add(
        egui::TextEdit::singleline(&mut d.call_text)
            .desired_width(320.0)
            .hint_text("ヘイクロード、へいくろーど"),
    );
    theme::slider_row(ui, "依頼を聞き取る時間", &mut d.user.request_listen_secs, &REQUEST_LISTEN, p);
    for note in call_notes(
        &split_keywords(&d.call_text),
        info.assistant_name.as_deref(),
        &d.command_words(),
        &d.template_keywords,
    ) {
        ui.label(RichText::new(note).small().color(p.warning));
    }
    ui.add_space(14.0);
    theme::hint(
        ui,
        "「まる」→「。」のような記号と、テンプレートは辞書ファイル(dictionary.json)で決めます。保存すると次の発話から反映されます",
        p,
    );
    if ui.button("辞書ファイルを開く…").clicked() {
        actions.push(Action::EditDictionary);
    }
}

/// Claude を呼ぶ言葉の注意: 接続が無い、語が無い、短すぎて普通の文に当たる、空白で区切っている、
/// 命令語やテンプレートのキーワードと重なる(呼びかけが先に働く)。Claude Code の経路なら条件の違い。
fn call_notes(
    call_words: &[String],
    assistant_name: Option<&str>,
    commands: &[CommandWords],
    template_keywords: &[String],
) -> Vec<String> {
    let mut notes = Vec::new();
    match assistant_name {
        None => notes.push("設定「AI 訂正」で Claude に接続すると働きます".to_string()),
        Some(name) if name.contains("Claude Code") => notes.push(
            "Claude Code の経路では、依頼とテンプレートが個人向けの条件で扱われます。読影の文章には API 鍵の経路(商用条件)を勧めます"
                .to_string(),
        ),
        Some(_) => {}
    }
    if call_words.is_empty() {
        notes.push("言葉が無いと、呼びかけで依頼できません".to_string());
    }
    for word in call_words {
        let key = template_key(word);
        if key.chars().count() < 3 {
            notes.push(format!("「{word}」は短いので、普通の文の先頭にも当たります"));
        }
        if word.chars().any(char::is_whitespace) {
            notes.push(format!("「{word}」に空白があります。言葉は「、」で区切ります"));
        }
        let named = CommandAction::ALL
            .iter()
            .find(|action| action.names().iter().any(|n| template_key(n) == key));
        let registered = commands
            .iter()
            .find(|c| c.words.iter().any(|w| template_key(w) == key))
            .map(|c| &c.action);
        if let Some(action) = named.or(registered) {
            notes.push(format!(
                "「{word}」は命令語「{}」と同じです(呼びかけが先に働き、命令は働きません)",
                action.label()
            ));
        }
        if template_keywords.iter().any(|k| template_key(k) == key) {
            notes.push(format!(
                "「{word}」はテンプレートのキーワードと同じです(呼びかけが先に働き、「{word}テンプレート」で本文を呼べません)"
            ));
        }
    }
    notes
}

/// 働かなくなる語の注意: 名前と同じ言葉、ほかの操作と同じ言葉、記号の読みと同じ言葉。
/// (テンプレートは「〜テンプレート」と続けて言うので、キーワードと同じ言葉でも両方働く。)
fn command_notes(commands: &[CommandWords], symbol_readings: &[String]) -> Vec<String> {
    let mut notes = Vec::new();
    for (index, command) in commands.iter().enumerate() {
        for word in &command.words {
            let key = template_key(word);
            let is_name_of = |action: CommandAction| action.names().iter().any(|n| template_key(n) == key);
            // 自分の操作の名前・読み(登録しなくても働く)は知らせない。そうでなく、別の操作の名前と同じなら知らせる。
            if let Some(other) = CommandAction::ALL
                .iter()
                .find(|action| **action != command.action && is_name_of(**action))
                .filter(|_| !is_name_of(command.action))
            {
                notes.push(format!(
                    "「{word}」は「{}」の名前か読みです(そちらが先に働きます)",
                    other.label()
                ));
            }
            let earlier = commands[..index]
                .iter()
                .find(|c| c.words.iter().any(|w| template_key(w) == key));
            if let Some(earlier) = earlier {
                notes.push(format!(
                    "「{word}」は「{}」にもあります(上の行が先に働きます)",
                    earlier.action.label()
                ));
            }
            if symbol_readings.iter().any(|r| template_key(r) == key) {
                notes.push(format!(
                    "「{word}」は辞書ファイルの記号の読みと同じです(コマンドが先に働き、記号は入りません)"
                ));
            }
        }
    }
    notes
}

// ---- 7. 学習 ----

fn page_learning(
    ui: &mut Ui,
    d: &mut SettingsDialog,
    p: &Palette,
    info: &Info,
    actions: &mut Vec<Action>,
) {
    theme::hint(
        ui,
        "認識エンジンは 100% にはなりません。AI 訂正があるので、認識は AI が本来の発言を推測できる程度で十分です。\
         OpenSpeech は AI 訂正と手直しから「誤認識 → 正しい語」の組を覚え、次からは認識直後に当て、\
         AI にも例として渡し、認識エンジンの手がかり語にもします。手直しは AI 訂正の 3 回分として数えます。",
        p,
    );
    ui.add_space(6.0);
    let l = &mut d.user.learning;
    ui.checkbox(&mut l.enabled, "訂正から覚える");
    ui.checkbox(&mut l.auto_apply, "覚えた置き換えを認識直後に当てる");
    theme::slider_row(ui, "自動で当てるのに必要な回数", &mut l.min_count, &MIN_COUNT, p);

    theme::section(ui, "これまでの記録", p);
    ui.label(format!("覚えた組: {}", info.pairs));
    ui.label(format!("発話: {}", info.utterances));
    ui.label(format!("AI が直した発話: {}", info.corrected));
    ui.label(format!("話した時間: {:.0} 分", info.voice_minutes));
    ui.add_space(6.0);
    if ui.button("覚えた語を見る").clicked() {
        actions.push(Action::OpenDictMemory);
    }
}

// ---- 8. 外観 ----

fn page_appearance(ui: &mut Ui, d: &mut SettingsDialog, p: &Palette) {
    let themes: Vec<(ThemeChoice, String)> = ThemeChoice::ALL
        .iter()
        .map(|t| (*t, t.label().to_string()))
        .collect();
    combo_row(ui, "明るさ", "theme", &mut d.user.appearance.theme, &themes);
    theme::slider_row(ui, "拡大率", &mut d.user.appearance.scale, &SCALE, p);
    ui.checkbox(
        &mut d.user.appearance.compact_on_start,
        "起動時に小さな表示にする",
    );
}

// ---- 9. 履歴とログ ----

fn page_history(
    ui: &mut Ui,
    d: &mut SettingsDialog,
    p: &Palette,
    info: &Info,
    actions: &mut Vec<Action>,
) {
    let counts: Vec<(usize, String)> = HISTORY_COUNTS
        .iter()
        .map(|n| (*n, format!("{n} 件")))
        .collect();
    combo_row(
        ui,
        "転送履歴の保存数",
        "history_count",
        &mut d.user.history_count,
        &counts,
    );
    ui.checkbox(&mut d.user.save_history, "転送履歴をファイルに保存する");
    ui.label(
        RichText::new(format!(
            "転送した文章を日ごとのファイルに残します。{} 日より前の分と、合計 {} MB を超える分は古い日から消します。",
            crate::core::history::KEEP_DAYS,
            crate::core::history::KEEP_BYTES / (1024 * 1024)
        ))
        .small()
        .color(p.dim),
    );
    if ui.button("転送履歴をすべて消す").clicked() {
        actions.push(Action::ClearHistory);
    }
    ui.label(RichText::new("消した履歴は元に戻せません。").small().color(p.dim));
    ui.checkbox(
        &mut d.user.auto_backup,
        "設定・辞書・学習を自動でバックアップする",
    );

    theme::section(ui, "音声の保存", p);
    ui.checkbox(&mut d.global.voice_log.enabled, "認識した音声を保存する");
    ui.label(
        RichText::new("医療の音声がこの Mac に残ります。確認が必要なときだけ有効にしてください")
            .small()
            .color(p.danger),
    );
    ui.add_space(4.0);
    folder_row(ui, "保存先のフォルダ", &mut d.global.voice_log.folder, p);
    theme::slider_row(ui, "保存量の上限", &mut d.global.voice_log.limit_mb, &LOG_LIMIT, p);

    theme::section(ui, "記録", p);
    let log = info
        .log_path
        .as_ref()
        .map_or("(なし)".to_string(), |path| path.display().to_string());
    ui.label(RichText::new(format!("ログ: {log}")).small().color(p.dim));
    ui.label(
        RichText::new(format!("データの置き場所: {}", info.data_root.display()))
            .small()
            .color(p.dim),
    );
    if ui.button("Finder で見る").clicked() {
        actions.push(Action::RevealData);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_guidance_length_is_shown_with_the_limit_when_over_it() {
        let limit = corrector::MAX_GUIDANCE_CHARS;
        assert_eq!(guidance_status(0, limit), "指示: アプリの既定");
        assert_eq!(guidance_status(120, limit), "指示: 120 字");
        assert_eq!(guidance_status(limit, limit), "指示: 4000 字");
        assert_eq!(guidance_status(4001, limit), "指示: 4001 字(先頭の 4000 字だけ使います)");
        assert_eq!(assistant::MAX_REQUEST_GUIDANCE_CHARS, limit, "依頼の指示も同じ上限");
    }
}
