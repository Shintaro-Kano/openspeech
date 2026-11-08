//! 録音の切り替えに使えるキー。押されたキーの受け取りと、全体ショートカットへの変換。
//!
//! 画面の部品(egui)ではテンキーと本体のキーを区別できないので、Mac のキーコードで受け取る。

use super::settings::KeyCombo;
use global_hotkey::hotkey::{Code, HotKey, Modifiers};

/// キーコード。
const ESCAPE: u16 = 0x35;
const CAPS_LOCK: u16 = 0x39;

/// 同時に押していた修飾キー。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Held {
    pub command: bool,
    pub control: bool,
    pub option: bool,
    pub shift: bool,
}

/// 選択中に押されたキー。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pressed {
    CapsLock,
    Cancel,
    Key {
        key_code: u16,
        held: Held,
        /// キーボード配列での文字(JIS と US で同じキーの文字が違う)。
        characters: String,
    },
}

/// キーの種類。単独で使えるのはファンクションキーとテンキーだけ。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// 打った文字を名前にする。
    Typing,
    /// 打つキーだが、決まった名前で出す。
    Named(&'static str),
    /// 単独でも使える。
    Alone(&'static str),
}

use Kind::{Alone, Named, Typing};

/// Mac のキーコードと全体ショートカットの対応(global-hotkey が扱えるキー)。
const KEYS: &[(u16, Code, Kind)] = &[
    (0x00, Code::KeyA, Typing),
    (0x01, Code::KeyS, Typing),
    (0x02, Code::KeyD, Typing),
    (0x03, Code::KeyF, Typing),
    (0x04, Code::KeyH, Typing),
    (0x05, Code::KeyG, Typing),
    (0x06, Code::KeyZ, Typing),
    (0x07, Code::KeyX, Typing),
    (0x08, Code::KeyC, Typing),
    (0x09, Code::KeyV, Typing),
    (0x0b, Code::KeyB, Typing),
    (0x0c, Code::KeyQ, Typing),
    (0x0d, Code::KeyW, Typing),
    (0x0e, Code::KeyE, Typing),
    (0x0f, Code::KeyR, Typing),
    (0x10, Code::KeyY, Typing),
    (0x11, Code::KeyT, Typing),
    (0x12, Code::Digit1, Typing),
    (0x13, Code::Digit2, Typing),
    (0x14, Code::Digit3, Typing),
    (0x15, Code::Digit4, Typing),
    (0x16, Code::Digit6, Typing),
    (0x17, Code::Digit5, Typing),
    (0x18, Code::Equal, Typing),
    (0x19, Code::Digit9, Typing),
    (0x1a, Code::Digit7, Typing),
    (0x1b, Code::Minus, Typing),
    (0x1c, Code::Digit8, Typing),
    (0x1d, Code::Digit0, Typing),
    (0x1e, Code::BracketRight, Typing),
    (0x1f, Code::KeyO, Typing),
    (0x20, Code::KeyU, Typing),
    (0x21, Code::BracketLeft, Typing),
    (0x22, Code::KeyI, Typing),
    (0x23, Code::KeyP, Typing),
    (0x24, Code::Enter, Named("Return")),
    (0x25, Code::KeyL, Typing),
    (0x26, Code::KeyJ, Typing),
    (0x27, Code::Quote, Typing),
    (0x28, Code::KeyK, Typing),
    (0x29, Code::Semicolon, Typing),
    (0x2a, Code::Backslash, Typing),
    (0x2b, Code::Comma, Typing),
    (0x2c, Code::Slash, Typing),
    (0x2d, Code::KeyN, Typing),
    (0x2e, Code::KeyM, Typing),
    (0x2f, Code::Period, Typing),
    (0x30, Code::Tab, Named("Tab")),
    (0x31, Code::Space, Named("Space")),
    (0x32, Code::Backquote, Typing),
    (0x33, Code::Backspace, Named("Delete")),
    (0x41, Code::NumpadDecimal, Alone("テンキー .")),
    (0x43, Code::NumpadMultiply, Alone("テンキー *")),
    (0x45, Code::NumpadAdd, Alone("テンキー +")),
    (0x47, Code::NumLock, Alone("テンキー Clear")),
    (0x4b, Code::NumpadDivide, Alone("テンキー /")),
    (0x4c, Code::NumpadEnter, Alone("テンキー Enter")),
    (0x4e, Code::NumpadSubtract, Alone("テンキー -")),
    (0x51, Code::NumpadEqual, Alone("テンキー =")),
    (0x52, Code::Numpad0, Alone("テンキー 0")),
    (0x53, Code::Numpad1, Alone("テンキー 1")),
    (0x54, Code::Numpad2, Alone("テンキー 2")),
    (0x55, Code::Numpad3, Alone("テンキー 3")),
    (0x56, Code::Numpad4, Alone("テンキー 4")),
    (0x57, Code::Numpad5, Alone("テンキー 5")),
    (0x58, Code::Numpad6, Alone("テンキー 6")),
    (0x59, Code::Numpad7, Alone("テンキー 7")),
    (0x5b, Code::Numpad8, Alone("テンキー 8")),
    (0x5c, Code::Numpad9, Alone("テンキー 9")),
    (0x7a, Code::F1, Alone("F1")),
    (0x78, Code::F2, Alone("F2")),
    (0x63, Code::F3, Alone("F3")),
    (0x76, Code::F4, Alone("F4")),
    (0x60, Code::F5, Alone("F5")),
    (0x61, Code::F6, Alone("F6")),
    (0x62, Code::F7, Alone("F7")),
    (0x64, Code::F8, Alone("F8")),
    (0x65, Code::F9, Alone("F9")),
    (0x6d, Code::F10, Alone("F10")),
    (0x67, Code::F11, Alone("F11")),
    (0x6f, Code::F12, Alone("F12")),
    (0x69, Code::F13, Alone("F13")),
    (0x6b, Code::F14, Alone("F14")),
    (0x71, Code::F15, Alone("F15")),
    (0x6a, Code::F16, Alone("F16")),
    (0x40, Code::F17, Alone("F17")),
    (0x4f, Code::F18, Alone("F18")),
    (0x50, Code::F19, Alone("F19")),
    (0x5a, Code::F20, Alone("F20")),
    (0x72, Code::Insert, Named("Help")),
    (0x73, Code::Home, Named("Home")),
    (0x74, Code::PageUp, Named("Page Up")),
    (0x75, Code::Delete, Named("⌦")),
    (0x77, Code::End, Named("End")),
    (0x79, Code::PageDown, Named("Page Down")),
    (0x7b, Code::ArrowLeft, Named("←")),
    (0x7c, Code::ArrowRight, Named("→")),
    (0x7d, Code::ArrowDown, Named("↓")),
    (0x7e, Code::ArrowUp, Named("↑")),
];

fn find_key(key_code: u16) -> Option<&'static (u16, Code, Kind)> {
    KEYS.iter().find(|(code, _, _)| *code == key_code)
}

/// 修飾キーの記号を Mac の順に並べる。
fn prefix(held: Held) -> String {
    let mut text = String::new();
    for (on, symbol) in [
        (held.control, '⌃'),
        (held.option, '⌥'),
        (held.shift, '⇧'),
        (held.command, '⌘'),
    ] {
        if on {
            text.push(symbol);
        }
    }
    text
}

/// 押されたキーを録音の切り替えに使えるか確かめる。普段の入力やアプリの操作を奪うものは断る。
pub fn accept_key(key_code: u16, held: Held, characters: &str) -> Result<KeyCombo, String> {
    let Some((_, _, kind)) = find_key(key_code) else {
        return Err("このキーは録音の切り替えに使えません".to_string());
    };
    let name = match kind {
        Typing => characters
            .chars()
            .next()
            .filter(|c| !c.is_control())
            .map_or_else(
                || format!("キー {key_code}"),
                |c| c.to_uppercase().to_string(),
            ),
        Named(name) | Alone(name) => name.to_string(),
    };
    let safe = matches!(kind, Alone(_))
        || held.control
        || held.option
        || (held.command && held.shift);
    if !safe {
        return Err(format!(
            "「{}{name}」は普段の入力やアプリの操作と重なります。⌃ か ⌥ と一緒に押してください",
            prefix(held)
        ));
    }
    Ok(KeyCombo {
        key_code,
        command: held.command,
        control: held.control,
        option: held.option,
        shift: held.shift,
        label: format!("{}{name}", prefix(held)),
    })
}

/// 全体ショートカットに変換する。知らないキーなら None。
pub fn hotkey_of(combo: &KeyCombo) -> Option<HotKey> {
    let (_, code, _) = find_key(combo.key_code)?;
    let mut modifiers = Modifiers::empty();
    for (on, modifier) in [
        (combo.command, Modifiers::SUPER),
        (combo.control, Modifiers::CONTROL),
        (combo.option, Modifiers::ALT),
        (combo.shift, Modifiers::SHIFT),
    ] {
        if on {
            modifiers |= modifier;
        }
    }
    Some(HotKey::new(Some(modifiers), *code))
}

#[cfg(target_os = "macos")]
mod mac {
    use super::{Held, Pressed, CAPS_LOCK, ESCAPE};
    use block2::RcBlock;
    use objc2::rc::Retained;
    use objc2::runtime::AnyObject;
    use objc2_app_kit::{NSEvent, NSEventMask, NSEventModifierFlags, NSEventType};
    use std::ptr::NonNull;
    use std::sync::{Arc, Mutex};

    /// 次に押されたキーを受け取る。持っている間だけ働き、受け取ったキーは他へ渡さない。
    pub struct KeyCapture {
        monitor: Retained<AnyObject>,
        pressed: Arc<Mutex<Option<Pressed>>>,
    }

    fn read_event(event: &NSEvent) -> Option<Pressed> {
        let key_code = event.keyCode();
        if event.r#type() == NSEventType::FlagsChanged {
            return (key_code == CAPS_LOCK).then_some(Pressed::CapsLock);
        }
        if event.isARepeat() {
            return None;
        }
        let flags = event.modifierFlags();
        let held = Held {
            command: flags.contains(NSEventModifierFlags::Command),
            control: flags.contains(NSEventModifierFlags::Control),
            option: flags.contains(NSEventModifierFlags::Option),
            shift: flags.contains(NSEventModifierFlags::Shift),
        };
        if key_code == ESCAPE {
            return Some(Pressed::Cancel);
        }
        let characters = event
            .charactersIgnoringModifiers()
            .map_or_else(String::new, |text| text.to_string());
        Some(Pressed::Key {
            key_code,
            held,
            characters,
        })
    }

    impl KeyCapture {
        pub fn start() -> Option<Self> {
            let pressed = Arc::new(Mutex::new(None));
            let slot = pressed.clone();
            let handler = RcBlock::new(move |event: NonNull<NSEvent>| -> *mut NSEvent {
                // SAFETY: AppKit が渡す、呼び出しの間有効なイベント。
                let reference = unsafe { event.as_ref() };
                let is_key = reference.r#type() == NSEventType::KeyDown;
                if let Some(found) = read_event(reference) {
                    if let Ok(mut slot) = slot.lock() {
                        *slot = Some(found);
                    }
                }
                if is_key {
                    // 文字として画面へ流さない。
                    std::ptr::null_mut()
                } else {
                    event.as_ptr()
                }
            });
            // SAFETY: 戻り値が有効なイベントか null の block を渡している。
            let monitor = unsafe {
                NSEvent::addLocalMonitorForEventsMatchingMask_handler(
                    NSEventMask::KeyDown | NSEventMask::FlagsChanged,
                    &handler,
                )
            }?;
            Some(KeyCapture { monitor, pressed })
        }

        /// 押されたキーがあれば取り出す。
        pub fn poll(&self) -> Option<Pressed> {
            self.pressed.lock().ok()?.take()
        }
    }

    impl Drop for KeyCapture {
        fn drop(&mut self) {
            // SAFETY: addLocalMonitor… が返したものをそのまま返している。
            unsafe { NSEvent::removeMonitor(&self.monitor) };
        }
    }
}

#[cfg(not(target_os = "macos"))]
mod mac {
    use super::Pressed;

    pub struct KeyCapture;

    impl KeyCapture {
        pub fn start() -> Option<Self> {
            None
        }
        pub fn poll(&self) -> Option<Pressed> {
            None
        }
    }
}

pub use mac::KeyCapture;

#[cfg(test)]
mod tests {
    use super::*;

    fn plain() -> Held {
        Held::default()
    }

    #[test]
    fn function_and_numpad_keys_work_alone() {
        let f9 = accept_key(0x65, plain(), "").unwrap();
        assert_eq!(f9.label, "F9");
        let slash = accept_key(0x4b, plain(), "/").unwrap();
        assert_eq!(slash.label, "テンキー /");
        assert_eq!(hotkey_of(&slash).unwrap().key, Code::NumpadDivide);
    }

    #[test]
    fn typing_keys_need_a_safe_modifier() {
        assert!(accept_key(0x00, plain(), "a").is_err());
        assert!(accept_key(0x31, plain(), " ").is_err());
        let shift = Held {
            shift: true,
            ..plain()
        };
        assert!(accept_key(0x00, shift, "A").is_err());
        let command = Held {
            command: true,
            ..plain()
        };
        let error = accept_key(0x09, command, "v").unwrap_err();
        assert!(error.contains("⌘V"), "{error}");
        let option = Held {
            option: true,
            ..plain()
        };
        assert_eq!(accept_key(0x31, option, " ").unwrap().label, "⌥Space");
        let both = Held {
            command: true,
            shift: true,
            ..plain()
        };
        assert_eq!(accept_key(0x0f, both, "r").unwrap().label, "⇧⌘R");
    }

    #[test]
    fn the_label_uses_the_character_of_the_keyboard_layout() {
        // JIS 配列では 0x21 に「@」がある。
        let control = Held {
            control: true,
            ..plain()
        };
        assert_eq!(accept_key(0x21, control, "@").unwrap().label, "⌃@");
    }

    #[test]
    fn unknown_keys_are_refused() {
        // JIS の英数キー。全体ショートカットにできない。
        assert!(accept_key(0x66, plain(), "").is_err());
    }

    #[test]
    fn modifiers_reach_the_hotkey() {
        let held = Held {
            control: true,
            option: true,
            ..plain()
        };
        let combo = accept_key(0x65, held, "").unwrap();
        let hotkey = hotkey_of(&combo).unwrap();
        assert_eq!(hotkey.mods, Modifiers::CONTROL | Modifiers::ALT);
        assert_eq!(hotkey.key, Code::F9);
    }

    #[test]
    fn every_key_code_appears_once() {
        for (index, (code, _, _)) in KEYS.iter().enumerate() {
            assert!(
                !KEYS[index + 1..].iter().any(|(other, _, _)| other == code),
                "{code:#x}"
            );
        }
    }
}
