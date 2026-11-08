//! マイクと iPhone の開け閉め、アプリとしての導入。

use super::*;

/// マイクと iPhone の開け閉めの状態。録音しているか(`VoiceApp::mic`)は画面と試験が読むので外に置く。
#[derive(Default)]
pub(super) struct Mic {
    /// Mac のマイクから音が届いているかを一度だけ確かめる時刻。
    pub(super) check_at: Option<Instant>,
    /// いまの録音の番号。前の録音から遅れて届いた知らせを見分ける。
    pub(super) generation: u64,
    /// いまの録音のデバイスが開いたか。
    pub(super) opened: bool,
    /// iPhone で録っている間の連係の見張り。
    pub(super) link: Option<LinkWatch>,
    /// iPhone を諦めて Mac のマイクにした。連係は切れても一覧に残るので、この録音の間は戻さない。
    pub(super) iphone_gave_up: bool,
    /// 録音中に実際に使っているデバイス。None は既定のマイク。
    pub(super) active_device: Option<String>,
    /// 近くで見つかっている iPhone のマイク(iPhone を使う設定か、案内を開いている間だけ探す)。
    pub(super) iphone_found: Option<String>,
    /// 見えている iPhone のマイクの一覧。
    pub(super) iphone_names: Vec<String>,
    pub(super) iphone_checked_at: Option<Instant>,
}

impl VoiceApp {
    // ---- マイク ----

    pub fn toggle_mic(&mut self) {
        match self.mic {
            MicState::Off => self.start_mic(),
            MicState::On => self.stop_mic(),
        }
    }

    pub fn start_mic(&mut self) {
        if self.mic != MicState::Off {
            return;
        }
        if self.download.is_some() {
            self.notify("モデルを取得している間はマイクを使えません");
            return;
        }
        if self.engine_name.is_none() {
            let message = if crate::core::speechcpp::find_model().is_none() {
                crate::core::speechcpp::MODEL_MISSING
            } else {
                "認識エンジンが見つかりません。設定の「認識」を確かめてください"
            };
            self.notify_error(message);
            return;
        }
        let (device, fallback) = self.choose_input_device();
        self.open_device(device);
        self.mic = MicState::On;
        if self.using_iphone() {
            diag::log(format!(
                "iPhone 接続待ち device={}",
                self.active_device_label().unwrap_or_default()
            ));
            self.microphone.link = Some(LinkWatch::new(Instant::now()));
        }
        if let Some(message) = fallback {
            diag::log(&message);
            self.notify_error(message);
        }
    }

    /// デバイスを裏で開き始める。開いたら `MicOpened`、開けなければ `MicFailed` が届く。
    pub(super) fn open_device(&mut self, device: Option<String>) {
        let gain = self.global.mic_gain;
        let config = SegmenterConfig {
            post_ms: self.data.settings.engine.post_silence_ms as usize,
            ..SegmenterConfig::default()
        };
        self.microphone.generation = self
            .session
            .start(device.clone(), config, DEFAULT_SENSITIVITY, gain);
        self.microphone.opened = false;
        self.microphone.check_at = None;
        self.microphone.active_device = device;
    }

    /// デバイスが開いた。止めたあとや開き直したあとに届いた古い知らせは無視する。
    pub(super) fn mic_ready(&mut self, generation: u64, took: Duration) {
        if generation != self.microphone.generation || self.mic == MicState::Off {
            return;
        }
        self.microphone.opened = true;
        diag::log(format!(
            "マイク開始 device={} post_ms={} live_preview={}",
            self.active_device_label()
                .unwrap_or_else(|| "(既定)".to_string()),
            self.data.settings.engine.post_silence_ms,
            self.session.supports_partial()
        ));
        let now = Instant::now();
        match &mut self.microphone.link {
            Some(link) => link.opened(now, took),
            None => self.microphone.check_at = Some(now + MIC_SILENCE_CHECK),
        }
    }

    /// デバイスを開けなかった。iPhone は待ちの上限まで開き直す。
    pub(super) fn mic_failed(&mut self, generation: u64, message: String) {
        if generation != self.microphone.generation || self.mic == MicState::Off {
            return;
        }
        diag::log(format!("マイク開始エラー: {message}"));
        match &mut self.microphone.link {
            Some(link) => link.open_failed(Instant::now()),
            None => {
                self.stop_mic();
                self.notify_error(message);
            }
        }
    }

    /// 音が届いているかを見張る。時刻を受けるのは、試験で待ちの上限まで進めるため。
    pub fn watch_mic(&mut self, now: Instant) {
        if self.mic == MicState::Off {
            return;
        }
        if self.microphone.check_at.is_some_and(|at| now >= at) {
            self.microphone.check_at = None;
            if !self.session.live.has_signal() {
                diag::log("音が届かないため停止(マイクの許可が無いか)");
                self.stop_mic();
                self.windows.mic_help = true;
                return;
            }
        }
        let opened = self.microphone.opened;
        let heard = opened && self.session.live.has_signal();
        let silence = if opened {
            self.session.live.silence()
        } else {
            Duration::ZERO
        };
        let Some(link) = &mut self.microphone.link else {
            return;
        };
        match link.step(heard, silence, now) {
            LinkStep::Keep => {}
            LinkStep::Arrived(waited) => {
                diag::log(format!(
                    "iPhone 接続待ち解消({:.1} 秒)",
                    waited.as_secs_f32()
                ));
                self.remember_iphone();
            }
            LinkStep::Reopen => self.reopen_iphone(),
            LinkStep::GiveUp => self.give_up_on_iphone(),
        }
    }

    /// 連係は開くことで張り直されるので、デバイスを閉じて開き直す。
    pub(super) fn reopen_iphone(&mut self) {
        diag::log(format!(
            "iPhone 張り直し device={}",
            self.active_device_label().unwrap_or_default()
        ));
        self.notify("iPhone の接続を張り直しています");
        let device = self.microphone.active_device.clone();
        self.session.stop();
        self.open_device(device);
    }

    /// 待ちの上限まで音が届かなかった。案内の窓は開かず、Mac のマイクで録り続ける。
    pub(super) fn give_up_on_iphone(&mut self) {
        let source = self.session.source().clone();
        let device = audio::fallback_device(
            self.global.input_device.as_deref(),
            source.default_device().as_deref(),
            &source.device_names(),
        );
        diag::log(format!(
            "iPhone 接続待ちの上限で Mac のマイクへ切り替え device={}",
            device
                .as_deref()
                .map_or("(既定)".to_string(), audio::display_device_name)
        ));
        self.microphone.link = None;
        self.microphone.iphone_gave_up = true;
        self.session.stop();
        self.open_device(device);
        self.notify_error("iPhone から音が届かないため、Mac のマイクに切り替えました");
    }

    /// 音が届いた iPhone を覚える(2 台見えるときに選ぶ)。
    pub(super) fn remember_iphone(&mut self) {
        let Some(name) = self.microphone.active_device.clone() else {
            return;
        };
        let known = self.global.last_iphone.as_deref();
        if known.is_some_and(|last| audio::same_device(last, &name)) {
            return;
        }
        // 設定画面が開いていれば、その写しにも反映する(閉じるときに巻き戻らないように)。
        if let Some(dialog) = &mut self.windows.settings {
            dialog.global.last_iphone = Some(name.clone());
        }
        self.global.last_iphone = Some(name);
        self.save_global();
    }

    /// 近くで見つかっている iPhone のマイクの名前(画面の案内用)。
    pub fn iphone_found(&self) -> Option<&str> {
        self.microphone.iphone_found.as_deref()
    }

    /// iPhone を諦めて Mac のマイクで録っているか。
    pub fn iphone_gave_up(&self) -> bool {
        self.microphone.iphone_gave_up
    }

    /// iPhone の音を待っている秒数。待っていなければ None。
    pub fn iphone_wait_seconds(&self) -> Option<u64> {
        if self.mic == MicState::Off {
            return None;
        }
        let waited = self.microphone.link.as_ref()?.waiting_for(Instant::now())?;
        Some(waited.as_secs())
    }

    pub fn stop_mic(&mut self) {
        if self.mic != MicState::Off {
            diag::log("マイク停止");
        }
        self.session.stop();
        self.mic = MicState::Off;
        self.microphone.check_at = None;
        self.microphone.opened = false;
        self.microphone.link = None;
        self.microphone.iphone_gave_up = false;
        self.microphone.active_device = None;
        self.live_caption.clear();
        // 呼びかけの待ちは期限だけで消す。押している間だけ録る設定では、呼びかけのあとにキーを離して
        // 押し直し、依頼を言うので、ここで消すと依頼が本文に入る。
    }

    /// 使う録音デバイスを決める。iPhone を使う設定なら、その時点で見つかる iPhone を選ぶ。
    /// 見つからなければいつものデバイスに戻し、その旨の知らせを添える。
    pub(super) fn choose_input_device(&mut self) -> (Option<String>, Option<String>) {
        if self.global.prefer_iphone {
            self.rescan_iphone();
            if let Some(name) = &self.microphone.iphone_found {
                return (Some(name.clone()), None);
            }
        }
        // 録音デバイスに選んだ iPhone が見えないときは、30 秒待たせずに既定のマイクを使う。
        let usual = self.global.input_device.clone();
        let names = self.session.source().device_names();
        let missing = usual.as_deref().is_some_and(|device| {
            audio::is_iphone_device(device)
                && !names.iter().any(|name| audio::same_device(name, device))
        });
        let device = if missing { None } else { usual };
        let note = (self.global.prefer_iphone || missing).then(|| {
            "iPhone のマイクが見つからないため、いつもの録音デバイスを使います".to_string()
        });
        (device, note)
    }

    /// 録音中のデバイスが iPhone か。
    pub fn using_iphone(&self) -> bool {
        self.mic != MicState::Off
            && self
                .microphone.active_device
                .as_deref()
                .is_some_and(audio::is_iphone_device)
    }

    /// 録音中のデバイスの表示名。既定のマイクなら None。
    pub fn active_device_label(&self) -> Option<String> {
        self.microphone.active_device
            .as_deref()
            .map(audio::display_device_name)
    }

    /// 近くの iPhone をいま探し直す。2 台以上見えたら一覧を記録に残す。
    pub fn rescan_iphone(&mut self) {
        let names = self.session.source().device_names();
        self.microphone.iphone_found = audio::choose_iphone(&names, self.global.last_iphone.as_deref());
        self.microphone.iphone_checked_at = Some(Instant::now());
        let visible: Vec<String> = names
            .into_iter()
            .filter(|name| audio::is_iphone_device(name))
            .collect();
        if visible != self.microphone.iphone_names && visible.len() >= 2 {
            let list: Vec<String> = visible
                .iter()
                .map(|name| audio::display_device_name(name))
                .collect();
            diag::log(format!(
                "iPhone のマイクが {} 台見える: {}(使う: {})",
                list.len(),
                list.join(" / "),
                self.microphone.iphone_found
                    .as_deref()
                    .map_or(String::new(), audio::display_device_name)
            ));
        }
        self.microphone.iphone_names = visible;
    }

    /// iPhone を使う設定のときと案内を開いている間、近くの iPhone を定期的に探す。
    pub fn scan_for_iphone(&mut self, now: Instant) {
        if !self.global.prefer_iphone && !self.windows.iphone_help {
            self.microphone.iphone_found = None;
            self.microphone.iphone_checked_at = None;
            return;
        }
        let due = self
            .microphone.iphone_checked_at
            .is_none_or(|at| now >= at + IPHONE_SCAN_INTERVAL);
        if due {
            let before = self.microphone.iphone_found.is_some();
            self.rescan_iphone();
            let after = self.microphone.iphone_found.is_some();
            if before != after {
                diag::log(if after {
                    "iPhone のマイクが見つかった"
                } else {
                    "iPhone のマイクが見えなくなった"
                });
            }
            // 一覧から消えて戻ったなら本当に状態が変わったので、諦めたあとでも試し直す。
            if !before && after {
                self.microphone.iphone_gave_up = false;
            }
            // 録音中に iPhone が現れたら切り替え、消えたらいつものデバイスに戻す(黙って Mac のマイクで録り続けない)。
            if self.global.prefer_iphone && self.mic != MicState::Off {
                let active = self.microphone.active_device.as_deref().unwrap_or_default();
                let active_gone = self.using_iphone()
                    && !self
                        .microphone.iphone_names
                        .iter()
                        .any(|name| audio::same_device(name, active));
                if !self.using_iphone() && after && !self.microphone.iphone_gave_up {
                    self.restart_mic_if_on();
                    self.notify("iPhone のマイクに切り替えました");
                } else if active_gone {
                    self.restart_mic_if_on();
                    if self.using_iphone() {
                        self.notify("使っていた iPhone が見えなくなったため、別の iPhone に切り替えました");
                    } else {
                        self.notify_error(
                            "iPhone が見えなくなったため、いつもの録音デバイスに戻しました",
                        );
                    }
                }
            }
        }
    }

    /// iPhone をマイクにするかを切り替える。録音中ならすぐに切り替えて録り直す。
    pub fn set_prefer_iphone(&mut self, on: bool) {
        if self.global.prefer_iphone == on {
            return;
        }
        self.global.prefer_iphone = on;
        // 設定画面が開いていれば、その写しにも反映する(閉じるときに巻き戻らないように)。
        if let Some(dialog) = &mut self.windows.settings {
            dialog.global.prefer_iphone = on;
        }
        self.save_global();
        if on {
            self.rescan_iphone();
            if self.microphone.iphone_found.is_none() {
                self.windows.iphone_help = true;
            }
        }
        self.restart_mic_if_on();
    }

    /// 録音デバイスの設定が変わったとき、録音中なら新しいデバイスで録り直す。
    pub fn restart_mic_if_on(&mut self) {
        if self.mic == MicState::On {
            self.stop_mic();
            self.start_mic();
        }
    }

    /// マイクの宣言を持つアプリの形で開き直す。入力中の文章はクリップボードへ逃がす。
    pub fn relaunch_for_microphone(&mut self) {
        if !self.editor.is_empty() {
            self.ctx.copy_text(self.editor.text.clone());
        }
        self.save_data();
        self.save_global();
        match mic_access::relaunch_as_app() {
            Ok(()) => {
                self.allow_close = true;
                self.ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
            Err(message) => self.notify_error(message),
        }
    }

    /// アプリケーションフォルダへ置いて、場所を Finder で見せる。
    pub fn install_to_applications(&mut self) {
        let installed = mic_access::applications_folder()
            .and_then(|folder| mic_access::install_app(&folder))
            .and_then(|bundle| mic_access::reveal_in_finder(&bundle));
        match installed {
            Ok(()) => self.notify("追加しました。Spotlight で「OpenSpeech」と検索しても開けます"),
            Err(message) => self.notify_error(message),
        }
    }

    /// Dock に登録する。次からは Dock のクリックで開ける。
    pub fn add_to_dock(&mut self) {
        let result = mic_access::bundle_for_dock().and_then(|bundle| mic_access::add_to_dock(&bundle));
        match result {
            Ok(true) => self.notify("Dock に追加しました。次からは Dock のアイコンで開けます"),
            Ok(false) => self.notify("すでに Dock にあります"),
            Err(message) => self.notify_error(message),
        }
    }
}
