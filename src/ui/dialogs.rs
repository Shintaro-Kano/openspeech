//! 単語登録と、ヘルプ・マイク・iPhone・転送の許可・バージョン情報・確認の小窓。

use super::theme;
use crate::app::{VoiceApp, APP_VERSION};
use crate::core::dictionary::Dictionary;
use crate::core::voice_command::normalized;
use crate::core::{audio, diag, mic_access, models, transfer};
use egui::{Context, RichText};

#[derive(Default)]
pub struct AddWordForm {
    pub word: String,
    pub reading: String,
    pub keep_open: bool,
    pub message: Option<(String, bool)>,
}

/// 中央に出す補助ウィンドウの共通の形。
fn dialog_window<'a>(title: &str, width: f32, open: &'a mut bool) -> egui::Window<'a> {
    egui::Window::new(title)
        .open(open)
        .collapsible(false)
        .resizable(false)
        .default_width(width)
        .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
}

fn status_line(ui: &mut egui::Ui, text: &str, ok: bool, p: &theme::Palette) {
    let color = if ok { p.success } else { p.danger };
    ui.label(RichText::new(text).strong().color(color));
}

impl VoiceApp {
    pub fn open_add_word(&mut self) {
        self.windows
            .add_word
            .get_or_insert_with(AddWordForm::default);
    }

    /// エディターで選んでいる語を入れた状態で単語登録を開く。
    pub fn add_word_from_selection(&mut self) {
        let word = self.editor.selected_text();
        self.open_add_word();
        if let Some(form) = &mut self.windows.add_word {
            if !word.trim().is_empty() {
                form.word = word.trim().to_string();
            }
        }
    }

    pub fn draw_add_word(&mut self, ctx: &Context) {
        let Some(mut form) = self.windows.add_word.take() else {
            return;
        };
        let p = *theme::palette(ctx);
        let init_id = egui::Id::new("add_word_initialized");
        let first = ctx.data(|d| d.get_temp::<bool>(init_id)).is_none();
        ctx.data_mut(|d| d.insert_temp(init_id, true));
        let mut open = true;
        let mut submit = false;
        let mut close = false;
        dialog_window("単語登録", 440.0, &mut open).show(ctx, |ui| {
            ui.set_width(408.0);
            let enter = |r: &egui::Response, ui: &egui::Ui| {
                r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter))
            };
            ui.label(RichText::new("単語").strong());
            let word = ui.add(
                egui::TextEdit::singleline(&mut form.word)
                    .desired_width(f32::INFINITY)
                    .hint_text("登録する単語"),
            );
            if first {
                word.request_focus();
            }
            if enter(&word, ui) {
                submit = true;
            }
            ui.add_space(4.0);
            ui.label(RichText::new("読み(省略できます)").strong());
            let reading = ui.add(
                egui::TextEdit::singleline(&mut form.reading)
                    .desired_width(f32::INFINITY)
                    .hint_text("ひらがな"),
            );
            if enter(&reading, ui) {
                submit = true;
            }
            if let Some(warning) = Dictionary::reading_warning(&form.reading) {
                ui.label(RichText::new(warning).small().color(p.warning));
            }
            ui.add_space(4.0);
            ui.checkbox(&mut form.keep_open, "続けて登録する");
            if let Some((text, is_error)) = &form.message {
                let color = if *is_error { p.danger } else { p.success };
                ui.label(RichText::new(text).color(color));
            }
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if theme::primary_button(ui, "登録", &p).clicked() {
                    submit = true;
                }
                if ui.button("閉じる").clicked() {
                    close = true;
                }
            });
        });
        if submit {
            match self.data.dictionary.add_word(&form.word, &form.reading) {
                Ok(()) => {
                    self.save_data();
                    self.refresh_engine_options();
                    form.message = Some((format!("「{}」を登録しました", form.word.trim()), false));
                    form.word.clear();
                    form.reading.clear();
                    if !form.keep_open {
                        close = true;
                    }
                }
                Err(error) => form.message = Some((error, true)),
            }
        }
        if close || !open {
            ctx.data_mut(|d| d.remove::<bool>(init_id));
        } else {
            self.windows.add_word = Some(form);
        }
    }

    pub fn draw_dialogs(&mut self, ctx: &Context) {
        // 案内の窓は Esc で閉じる(開いている間はエディターが Esc を横取りしないので、ここで受ける)。
        let w = &mut self.windows;
        let guide_open =
            w.help || w.mic_help || w.iphone_help || w.accessibility_help || w.about || w.first_run;
        if guide_open && ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape))
        {
            w.help = false;
            w.mic_help = false;
            w.iphone_help = false;
            w.accessibility_help = false;
            w.about = false;
            if w.first_run {
                w.first_run = false;
                self.finish_first_run();
            }
        }
        self.draw_help(ctx);
        self.draw_mic_help(ctx);
        self.draw_iphone_help(ctx);
        self.draw_accessibility_help(ctx);
        self.draw_about(ctx);
        self.draw_first_run(ctx);
        self.draw_confirm_close(ctx);
        self.draw_confirm_quit(ctx);
        self.draw_startup_warning(ctx);
    }

    /// 起動時の読み込みで起きた問題を知らせる。状態行は小さな表示で見えず、すぐ消えるので窓に出す。
    fn draw_startup_warning(&mut self, ctx: &Context) {
        let Some(message) = self.windows.startup_warning.clone() else {
            return;
        };
        let p = *theme::palette(ctx);
        let mut open = true;
        let mut close = false;
        dialog_window("設定の読み込み", 520.0, &mut open).show(ctx, |ui| {
            ui.set_width(488.0);
            ui.label(RichText::new(message).color(p.danger));
            ui.add_space(6.0);
            if theme::primary_button(ui, "閉じる", &p).clicked() {
                close = true;
            }
        });
        if close || !open {
            self.windows.startup_warning = None;
        }
    }

    fn draw_help(&mut self, ctx: &Context) {
        if !self.windows.help {
            return;
        }
        let p = *theme::palette(ctx);
        let mut open = true;
        let mut to_accessibility = false;
        let mut to_iphone = false;
        let commands: Vec<String> = normalized(&self.data.settings.command_words)
            .iter()
            .map(|c| {
                let words: Vec<&str> = c
                    .action
                    .names()
                    .into_iter()
                    .chain(c.words.iter().map(String::as_str))
                    .collect();
                format!("「{}」: {}", words.join("」「"), c.action.label())
            })
            .collect();
        let symbols: Vec<String> = self
            .data
            .dictionary
            .symbols
            .iter()
            .filter(|s| !s.readings.is_empty() && !s.text.is_empty())
            .map(|s| format!("「{}」: {}", s.readings.join("」「"), s.text))
            .collect();
        let key = self.data.settings.record.shortcut.label().to_string();
        // 自動の大きさの窓の中では ScrollArea が前の描画の高さに縛られるので、高さを明示する。
        let scroll_height = (ctx.screen_rect().height() - 150.0).clamp(160.0, 460.0);
        dialog_window("ヘルプ", 560.0, &mut open).show(ctx, |ui| {
            ui.set_width(528.0);
            egui::ScrollArea::vertical().max_height(scroll_height).min_scrolled_height(scroll_height).show(ui, |ui| {
                theme::section(ui, "使い方", &p);
                ui.label(format!("マイクのボタンか {key} で録音を始め、話します。認識した文章がエディターに入ります。"));
                if key == "Caps Lock" {
                    ui.label("Caps Lock のランプが点いている間は認識します。その間はキーボードの文字が英大文字になるので、手で打つときはオフにしてください。");
                }
                ui.label("画面をロックするかスリープすると、マイクは止まります。");
                ui.label("⌘Enter で、直前に使っていたアプリへ文章を送ります。Esc で直前の発話を取り消します。");
                ui.add_space(6.0);
                theme::section(ui, "転送先", &p);
                ui.label("転送先は、OpenSpeech に戻る前に最後にクリックしたアプリです。画面の下に表示します。");
                ui.label("転送にはアクセシビリティの許可が必要です。");
                if ui.button("転送の許可について").clicked() {
                    to_accessibility = true;
                }
                ui.add_space(6.0);
                theme::section(ui, "音声コマンド", &p);
                theme::hint(ui, "発話の全体が言葉と一致すると働きます。文の最後に続けても働きます。続けて言うと回数分(「改行改行」で 2 行)。設定「音声コマンド」で別の言い方を足せます。", &p);
                for line in &commands {
                    ui.label(line);
                }
                ui.add_space(6.0);
                theme::section(ui, "記号", &p);
                if symbols.is_empty() {
                    ui.label("辞書ファイル(dictionary.json)の symbols に読みと記号を書くと使えます。");
                } else {
                    theme::hint(ui, "読みだけを話すと記号が入ります。辞書ファイル(dictionary.json)の symbols で増やせます。", &p);
                    for line in &symbols {
                        ui.label(line);
                    }
                }
                ui.add_space(6.0);
                theme::section(ui, "テンプレート", &p);
                ui.label("「辞書」の「テンプレート」か、辞書ファイル(dictionary.json)の templates に、検査名(キーワード)と正常所見(文章)を登録します。キーワードに続けて「テンプレート」と話すと(「前立腺MRIテンプレート」)、登録した文章が入ります。キーワードだけなら普通の語です。");
                ui.label("文の途中では働きません。エディターでは Esc で取り消せます。AI 訂正と学習の対象にはなりません。");
                ui.label("「ヘイクロード、前立腺MRIのテンプレート。サイズは5.5かける4.5かける5.0」と話すと、Claude がテンプレートを選び、言われた値を入れた文章を入れます。所見モジュール(「PI-RADS カテゴリー4/5」など)を登録しておくと、依頼でその所見が言われたとき Claude が正常所見の文と差し替えます。呼びかけだけなら次の発話が依頼です(設定「音声コマンド」で言葉を変えられます)。");
                ui.label("文章を選んで(選ばなければ全文)「ヘイクロード、意味が一貫するように整えて」のように頼むと、直した文で置き換えます(直後の Esc で戻せます)。依頼の文、エディターの文章(選んだ範囲か全文、先頭 3000 字)、テンプレートの一覧が Claude に送られます。");
                ui.add_space(6.0);
                theme::section(ui, "AI 訂正", &p);
                ui.label("設定の「AI 訂正」で接続すると、認識の誤りを AI が直します。Claude には文章だけを送ります(音声は送りません)。");
                ui.label("AI が左右の語を変えた答えは使わず、認識したままの文を残します。");
                ui.add_space(6.0);
                theme::section(ui, "外部の音声認識 API", &p);
                ui.label("設定の「認識」で外部の音声認識 API を選び、その API の鍵を入れると、Haas の文を API の文に差し替えてから AI 訂正に回します。このときは、声と判定した発話の音声をその API に送ります。");
                ui.add_space(6.0);
                theme::section(ui, "学習", &p);
                ui.label("AI の訂正と、あなたの手直しから「誤認識 → 正しい語」を覚えます。内容は「辞書」の「学習」で見られます。");
                ui.add_space(6.0);
                theme::section(ui, "iPhone", &p);
                ui.label("近くの iPhone をマイクにできます。");
                if ui.button("iPhone の接続方法").clicked() {
                    to_iphone = true;
                }
            });
        });
        self.windows.help = open;
        if to_accessibility {
            self.windows.accessibility_help = true;
        }
        if to_iphone {
            self.windows.iphone_help = true;
        }
    }

    fn draw_mic_help(&mut self, ctx: &Context) {
        if !self.windows.mic_help {
            return;
        }
        let p = *theme::palette(ctx);
        let mut open = true;
        let mut open_settings = false;
        let mut relaunch = false;
        let mut install = false;
        let can_relaunch = mic_access::can_relaunch_as_app();
        dialog_window("マイクの許可", 520.0, &mut open).show(ctx, |ui| {
            ui.set_width(488.0);
            ui.label("マイクから音が届いていません。多くは、マイクの許可がないことが原因です。");
            ui.add_space(4.0);
            theme::hint(
                ui,
                "システム設定 → プライバシーとセキュリティ → マイク → OpenSpeech をオン",
                &p,
            );
            ui.add_space(6.0);
            ui.horizontal_wrapped(|ui| {
                if theme::primary_button(ui, "システム設定のマイクを開く", &p).clicked() {
                    open_settings = true;
                }
                if can_relaunch {
                    if ui.button("アプリとして開き直す").clicked() {
                        relaunch = true;
                    }
                    if ui.button("アプリケーションフォルダに追加").clicked() {
                        install = true;
                    }
                }
            });
            if can_relaunch {
                theme::hint(
                    ui,
                    "いまはアプリではなく端末から動いているため、許可を求められないことがあります。アプリとして開き直してください。",
                    &p,
                );
            }
        });
        self.windows.mic_help = open;
        if open_settings {
            if let Err(error) = mic_access::open_privacy_settings() {
                self.notify_error(error);
            }
        }
        if relaunch {
            self.relaunch_for_microphone();
        }
        if install {
            self.install_to_applications();
        }
    }

    fn draw_iphone_help(&mut self, ctx: &Context) {
        if !self.windows.iphone_help {
            return;
        }
        let p = *theme::palette(ctx);
        let mut open = true;
        let mut prefer = self.global.prefer_iphone;
        let mut rescan = false;
        let mut open_sound = false;
        let status: (String, bool) = if self.using_iphone() {
            (
                format!("使用中: {}", self.active_device_label().unwrap_or_default()),
                true,
            )
        } else if let Some(name) = self.iphone_found() {
            (
                format!(
                    "見つかっています: {}(マイクを入れると使います)",
                    audio::display_device_name(name)
                ),
                true,
            )
        } else {
            ("iPhone のマイクが見つかりません".to_string(), false)
        };
        dialog_window("iPhone をマイクにする", 560.0, &mut open).show(ctx, |ui| {
            ui.set_width(528.0);
            ui.checkbox(&mut prefer, "近くの iPhone をマイクとして使う");
            status_line(ui, &status.0, status.1, &p);
            ui.add_space(6.0);
            ui.label("Mac は iPhone を Bluetooth のマイク(ヘッドセット)としては受け付けません。");
            ui.label("代わりに Apple の「連係」が、Bluetooth で近くの iPhone を見つけ、Wi-Fi で音を受け取ります。");
            ui.label("次の条件がそろうと、録音デバイスに「〇〇のiPhoneのマイク」が現れます。");
            ui.add_space(2.0);
            ui.label(RichText::new("1. iPhone と Mac を同じ Apple アカウントで iCloud にサインインする").strong());
            ui.label(RichText::new("2. iPhone と Mac の両方で Bluetooth と Wi-Fi をオンにする").strong());
            ui.label(
                RichText::new("3. iPhone の「設定」→「一般」→「AirPlay と連係」で「連係カメラ」をオンにする").strong(),
            );
            ui.label(RichText::new("4. iPhone を Mac の近く(10 m 以内)に横向きに置いて動かさず、画面をオフにする").strong());
            ui.label(
                RichText::new("   手に持って画面を見ている間は一時停止します(Apple の案内)。置いたまま、Mac に向かって話してください。")
                    .small(),
            );
            ui.add_space(4.0);
            ui.label(
                RichText::new("マイクを入れてから音が届くまで数秒かかることがあります。つながると iPhone の画面に「Mac に接続中」と出ます。最長 30 秒待ち、届かなければ Mac のマイクに切り替えます。")
                    .small(),
            );
            ui.label(
                RichText::new("録音中につながりが切れたら(iPhone を手に取ったときなど)、自動で張り直します。置き直すと音が戻ります。")
                    .small(),
            );
            ui.label(
                RichText::new("見つからないときは、いつもの録音デバイス(Mac のマイク)で録音し、状態行に「マイク: Mac(iPhone 未接続)」と出ます。録音中に iPhone が現れれば自動で切り替えます。")
                    .small(),
            );
            ui.label(
                RichText::new("Mac の「システム設定」→「サウンド」→「入力」に iPhone が出ていなければ、連係側の条件が満たされていません。")
                    .small(),
            );
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if ui.button("再検索").clicked() {
                    rescan = true;
                }
                if ui.button("システム設定のサウンドを開く").clicked() {
                    open_sound = true;
                }
            });
        });
        self.windows.iphone_help = open;
        if prefer != self.global.prefer_iphone {
            self.set_prefer_iphone(prefer);
        }
        if rescan {
            self.rescan_iphone();
            if self.iphone_found().is_none() {
                self.notify_error("iPhone のマイクはまだ見つかりません");
            }
        }
        if open_sound {
            if let Err(error) = mic_access::open_sound_settings() {
                self.notify_error(error);
            }
        }
    }

    fn draw_accessibility_help(&mut self, ctx: &Context) {
        if !self.windows.accessibility_help {
            return;
        }
        let p = *theme::palette(ctx);
        let mut open = true;
        let ok = self.accessibility_ok;
        let mut ask = false;
        let mut recheck = false;
        let mut open_settings = false;
        let mut reset = false;
        let adhoc = mic_access::running_signature_is_adhoc() == Some(true);
        dialog_window("転送の許可", 520.0, &mut open).show(ctx, |ui| {
            ui.set_width(488.0);
            status_line(ui, if ok { "許可されています" } else { "許可がありません" }, ok, &p);
            ui.add_space(4.0);
            ui.label("文章を他のアプリの入力欄へ差し込むには、macOS のアクセシビリティの許可が必要です。");
            ui.add_space(4.0);
            ui.label("1. 「許可を求める」を押す(Mac の確認画面と、システム設定のアクセシビリティが開きます)");
            ui.label("2. 一覧の OpenSpeech をオンにする(無ければ「+」でアプリケーションフォルダの OpenSpeech.app を追加)");
            ui.label("3. この画面の「確認し直す」を押す");
            ui.add_space(6.0);
            ui.label(RichText::new("一覧でオンになっているのに許可されないとき").strong());
            ui.label(
                "アプリを作り直すと署名が変わり、一覧の OpenSpeech は古い署名への許可のまま残ります\
                 (オンに見えても効かず、Mac の確認画面も出ません)。「登録し直す」で一覧から外してから、1 からやり直してください。",
            );
            if adhoc {
                theme::hint(
                    ui,
                    "この OpenSpeech.app は ad-hoc 署名なので、作り直すたびにこの状態になります。キーチェーンアクセスで\
                     「OpenSpeech」という名前のコード署名証明書を一度作ると、次の作り直しから許可が保たれます。",
                    &p,
                );
            }
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if theme::primary_button(ui, "許可を求める", &p).clicked() {
                    ask = true;
                }
                if ui.button("確認し直す").clicked() {
                    recheck = true;
                }
                if ui.button("システム設定を開く").clicked() {
                    open_settings = true;
                }
                if ui.button("登録し直す").clicked() {
                    reset = true;
                }
            });
        });
        self.windows.accessibility_help = open;
        if ask {
            self.ask_accessibility();
        }
        if recheck {
            self.check_accessibility(true);
        }
        if open_settings {
            if let Err(error) = transfer::open_accessibility_settings() {
                self.notify_error(error);
            }
        }
        if reset {
            match transfer::reset_accessibility_entry() {
                Ok(()) => {
                    self.notify("一覧から外しました。Mac の確認画面で許可してください");
                    self.ask_accessibility();
                }
                Err(error) => self.notify_error(error),
            }
        }
    }

    /// 初めて開いたときの案内。マイクの許可 → モデルの取得 → 転送の許可 → AI 訂正の鍵の順に、いまの状態と次の一歩を出す。
    fn draw_first_run(&mut self, ctx: &Context) {
        if !self.windows.first_run {
            return;
        }
        let p = *theme::palette(ctx);
        let mut open = true;
        let missing = models::missing();
        let progress = self.model_download_progress();
        let download_error = self.download_error.clone();
        let mut fetch = false;
        let mut cancel = false;
        let mut access = false;
        let mut corrector = false;
        let mut close = false;
        let step = |ui: &mut egui::Ui, number: &str, title: &str| {
            ui.add_space(6.0);
            ui.label(RichText::new(format!("{number}. {title}")).strong());
        };
        dialog_window("はじめに", 520.0, &mut open).show(ctx, |ui| {
            ui.set_width(488.0);
            step(ui, "1", "マイクの許可");
            ui.label("マイクを入れると、macOS が許可を尋ねます。「許可」を押してください。");
            step(ui, "2", "認識のモデル");
            match progress {
                Some((done, total)) => {
                    ui.label(format!(
                        "取得しています: {} / {} MB",
                        done / 1_000_000,
                        total.max(1) / 1_000_000
                    ));
                    if ui.button("取得をやめる").clicked() {
                        cancel = true;
                    }
                }
                None if missing.is_empty() => {
                    ui.label(RichText::new("取得済みです。").color(p.success));
                }
                None => {
                    let size: u64 = missing.iter().map(|model| model.size).sum();
                    ui.label("文字にするためのモデルを、この Mac に取ります(1 回だけ)。");
                    if ui
                        .button(format!("モデルを取得(約 {} MB)", size.div_ceil(1_000_000)))
                        .clicked()
                    {
                        fetch = true;
                    }
                }
            }
            if let (None, Some(error)) = (progress, &download_error) {
                ui.label(RichText::new(format!("前回の取得: {error}")).small().color(p.warning));
            }
            for model in models::catalog() {
                ui.label(RichText::new(&model.source).small().color(p.dim));
            }
            step(ui, "3", "転送の許可");
            if self.accessibility_ok {
                ui.label(RichText::new("許可されています。").color(p.success));
            } else {
                ui.label("文章をほかのアプリへ入れるには、「アクセシビリティ」の許可が要ります。");
                if ui.button("許可の案内を開く").clicked() {
                    access = true;
                }
            }
            step(ui, "4", "AI 訂正(使うときだけ)");
            ui.label(
                "認識の誤りを Claude で直すときと「ヘイクロード」を使うときは、設定の「AI 訂正」に API キーを入れます。\
                 使わなくても音声入力はできます。Claude には文字だけを送り、音声は送りません。\
                 外部の音声認識 API を使うときは、設定の「認識」でその API の鍵を入れます(声と判定した発話の音声をその API に送ります)。",
            );
            if ui.button("AI 訂正の設定を開く").clicked() {
                corrector = true;
            }
            ui.add_space(8.0);
            theme::hint(
                ui,
                "OpenSpeech は医療機器ではありません。認識と AI 訂正の結果は、必ず目で確かめてから使ってください。",
                &p,
            );
            theme::hint(
                ui,
                "録音の切り替えは Caps Lock です(設定の「入力と転送」で変えられます)。Caps Lock を入力の切り替えに使っている場合は、別のキーにしてください。",
                &p,
            );
            ui.add_space(6.0);
            if ui.button("はじめる").clicked() {
                close = true;
            }
        });
        if fetch {
            self.start_model_download();
        }
        if cancel {
            self.cancel_model_download();
        }
        if access {
            self.windows.accessibility_help = true;
        }
        if corrector {
            self.open_settings(Some("AI 訂正"));
        }
        if close || !open {
            self.windows.first_run = false;
            self.finish_first_run();
        }
    }

    /// 案内を閉じた。次からは出さない。
    fn finish_first_run(&mut self) {
        if !self.global.first_run_shown && self.screenshot.is_none() {
            self.global.first_run_shown = true;
            self.save_global();
        }
    }

    fn draw_about(&mut self, ctx: &Context) {
        if !self.windows.about {
            return;
        }
        let p = *theme::palette(ctx);
        let mut open = true;
        let engine = self
            .engine_name
            .clone()
            .unwrap_or_else(|| "なし".to_string());
        let corrector = self.corrector_name().unwrap_or_else(|| "なし".to_string());
        let data = self.store.root.display().to_string();
        let log =
            diag::path().map_or_else(|| "なし".to_string(), |path| path.display().to_string());
        dialog_window("OpenSpeech について", 460.0, &mut open).show(ctx, |ui| {
            ui.set_width(428.0);
            ui.label(RichText::new(format!("OpenSpeech {APP_VERSION}")).heading());
            ui.add_space(6.0);
            egui::Grid::new("about_grid")
                .num_columns(2)
                .spacing([12.0, 4.0])
                .show(ui, |ui| {
                    ui.label(RichText::new("認識エンジン").color(p.dim));
                    ui.label(engine);
                    ui.end_row();
                    ui.label(RichText::new("AI 訂正").color(p.dim));
                    ui.label(corrector);
                    ui.end_row();
                    ui.label(RichText::new("データ").color(p.dim));
                    ui.add(egui::Label::new(data).wrap());
                    ui.end_row();
                    ui.label(RichText::new("記録").color(p.dim));
                    ui.add(egui::Label::new(log).wrap());
                    ui.end_row();
                });
            ui.add_space(8.0);
            let privacy = match &self.remote {
                None => "音声は Mac の外に出ません。AI 訂正には認識した文章だけを送ります。".to_string(),
                Some(remote) => format!(
                    "声と判定した発話の音声を、外部の音声認識 API({})に送ります。AI 訂正には文章だけを送ります。",
                    remote.label()
                ),
            };
            theme::hint(ui, &privacy, &p);
        });
        self.windows.about = open;
    }

    fn draw_confirm_close(&mut self, ctx: &Context) {
        if !self.windows.confirm_close {
            return;
        }
        let p = *theme::palette(ctx);
        let mut open = true;
        let mut yes = false;
        let mut no = false;
        dialog_window("確認", 320.0, &mut open).show(ctx, |ui| {
            ui.label("エディターの文章を消しますか?");
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if theme::primary_button(ui, "消す", &p).clicked() {
                    yes = true;
                }
                if ui.button("やめる").clicked() {
                    no = true;
                }
            });
        });
        if yes {
            self.editor.delete_all();
            self.selection_dirty = true;
        }
        self.windows.confirm_close = open && !yes && !no;
    }

    fn draw_confirm_quit(&mut self, ctx: &Context) {
        if !self.windows.confirm_quit {
            return;
        }
        let p = *theme::palette(ctx);
        let mut open = true;
        let mut yes = false;
        let mut no = false;
        dialog_window("確認", 320.0, &mut open).show(ctx, |ui| {
            ui.label("文章が残っています。終了しますか?");
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if theme::primary_button(ui, "終了", &p).clicked() {
                    yes = true;
                }
                if ui.button("戻る").clicked() {
                    no = true;
                }
            });
        });
        if yes {
            self.allow_close = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        self.windows.confirm_quit = open && !yes && !no;
    }
}
