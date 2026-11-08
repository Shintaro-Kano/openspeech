//! 録音の切り替えキー(全体ショートカットと Caps Lock)。

use super::*;

/// 録音の切り替えキー(全体ショートカットと Caps Lock)の状態。`caps_watcher` は Caps Lock に加えて画面のロックと
/// スリープも見張り、どのキーの設定でも録音を止める(ロック中に周りの声を録らないため)。
#[derive(Default)]
pub(super) struct Keys {
    pub(super) hotkeys: Option<GlobalHotKeyManager>,
    pub(super) hotkey: Option<HotKey>,
    /// Caps Lock と画面のロックの見張り。None の間は OS に触らない(試験用)。
    pub(super) caps_watcher: Option<Watcher>,
    pub(super) caps: CapsFollower,
    /// ランプを変えられないことを知らせたか。
    pub(super) lamp_reported: bool,
}

impl VoiceApp {
    /// 選ばれたキーを全体ショートカットにする。Caps Lock は登録できないので状態を見張る。
    pub fn register_hotkey(&mut self) {
        let Some(manager) = &self.keys.hotkeys else {
            return;
        };
        if let Some(previous) = self.keys.hotkey.take() {
            let _ = manager.unregister(previous);
        }
        let RecordShortcut::Key(combo) = &self.data.settings.record.shortcut else {
            return;
        };
        let label = combo.label.clone();
        match record_key::hotkey_of(combo).map(|hotkey| (hotkey, manager.register(hotkey))) {
            Some((hotkey, Ok(()))) => self.keys.hotkey = Some(hotkey),
            _ => self.notify_error(format!(
                "「{label}」を登録できませんでした。ほかのアプリが使っている場合は別のキーを選んでください"
            )),
        }
    }

    pub(super) fn poll_hotkey(&mut self) {
        while let Ok(event) = GlobalHotKeyEvent::receiver().try_recv() {
            if Some(event.id) != self.keys.hotkey.map(|h| h.id()) {
                continue;
            }
            match (self.data.settings.record.hold, event.state) {
                (true, HotKeyState::Pressed) => self.start_mic(),
                (true, HotKeyState::Released) => self.stop_mic(),
                (false, HotKeyState::Pressed) => self.toggle_mic(),
                (false, HotKeyState::Released) => {}
            }
        }
    }

    /// Caps Lock をやめて別のキーにしたときの後始末。録音とランプを止めて、普段の入力に戻す。
    pub fn leave_caps_lock(&mut self) {
        if self.keys.caps_watcher.is_some() {
            self.stop_mic();
            caps_lock::set(false);
            self.keys.caps = CapsFollower::default();
        }
    }

    /// 画面のロックとスリープでは録音を止める(席を離れたまま周囲の声を拾わないように)。
    /// Caps Lock を使う設定なら、録音を Caps Lock に合わせ、録音の実際の状態にランプを合わせる。
    pub(super) fn follow_caps_lock(&mut self) {
        let Some(watcher) = &self.keys.caps_watcher else {
            return;
        };
        let locked = watcher.is_locked();
        let away = watcher.take_away();
        if away && self.mic != MicState::Off {
            diag::log("画面のロックかスリープのためマイク停止");
            self.stop_mic();
            self.notify("画面のロックかスリープのため、マイクを止めました");
        }
        if away {
            self.cancel_voice_transfer("画面のロックかスリープのため、転送を取りやめました");
        }
        if self.data.settings.record.shortcut != RecordShortcut::CapsLock {
            return;
        }
        let caps = caps_lock::is_on();
        if locked && !away {
            // パスワードの入力などで押されても録音は始めない。
            self.keys.caps.accept(caps);
            return;
        }
        if !away {
            match self.keys.caps.observe(caps, Instant::now()) {
                Some(MicRequest::Start) => self.start_mic(),
                Some(MicRequest::Stop) => self.stop_mic(),
                None => {}
            }
        }
        // 離席のときもランプを消す(ロック画面のパスワードが大文字にならないように)。
        let recording = self.mic != MicState::Off;
        if let Some(on) = self.keys.caps.align(caps, recording, Instant::now()) {
            if !caps_lock::set(on) {
                self.keys.caps.lamp_unavailable(caps);
            }
        }
        if self.keys.caps.lamp_failed() && !self.keys.lamp_reported {
            self.keys.lamp_reported = true;
            diag::log("Caps Lock のランプを変えられません");
            self.notify_error("Caps Lock のランプを変えられないため、録音の状態と合わないことがあります");
        }
    }
}
