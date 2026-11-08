//! 転送: 認識した文章を入力先のアプリへ届ける。
//!
//! 0.1 は AppleScript(System Events)でキー入力を送っていたが、アクセシビリティの許可が無いと黙って失敗し、
//! 許可が無いことも画面に出なかった。この版は Mac の API を直接使う:
//!   1. 入力先を NSWorkspace で追い(自分以外で最後に前面にあったアプリ)、NSRunningApplication で前面に出す
//!   2. アクセシビリティ API で、いま入力フォーカスのある欄に文章を直接差し込む(AXSelectedText)
//!   3. 差し込めない欄なら、クリップボード経由で ⌘V を CGEvent で送る(終わったらクリップボードを戻す)
//!   4. 貼り付けも受け付けないアプリ向けに、1 文字ずつキー入力として送る方法も選べる
//!
//! どの方法もアクセシビリティの許可が要る。無ければ理由を返し、画面が許可の取り方を案内する。

use super::settings::TransferMethod;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

/// 入力先のアプリ。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub pid: i32,
    pub bundle_id: String,
    pub name: String,
}

/// 届け方の結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delivery {
    /// アクセシビリティ API で直接差し込んだ。
    Inserted,
    /// クリップボード経由で貼り付けた。
    Pasted,
    /// キー入力として送った。
    Typed,
}

impl Delivery {
    pub fn label(self) -> &'static str {
        match self {
            Delivery::Inserted => "直接入力",
            Delivery::Pasted => "貼り付け",
            Delivery::Typed => "キー入力",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransferError {
    /// 入力先のアプリがまだ分からない(このアプリしか前面に出ていない)。
    NoTarget,
    /// アクセシビリティの許可が無い。
    NotTrusted,
    /// 入力先を前面に出せなかった。
    Activate(String),
    /// 届けられなかった。
    Deliver(String),
}

impl std::fmt::Display for TransferError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TransferError::NoTarget => {
                write!(f, "転送先が分かりません。先に入力したいアプリをクリックしてから戻ってください")
            }
            TransferError::NotTrusted => write!(
                f,
                "アクセシビリティの許可がありません。システム設定 → プライバシーとセキュリティ → アクセシビリティで OpenSpeech をオンにしてください"
            ),
            TransferError::Activate(name) => write!(f, "{name} を前面に出せませんでした"),
            TransferError::Deliver(message) => write!(f, "転送できませんでした: {message}"),
        }
    }
}

pub struct TransferRequest<'a> {
    pub text: &'a str,
    pub target: Option<&'a Target>,
    pub method: TransferMethod,
    pub restore_clipboard: bool,
    pub enter_after: bool,
}

pub fn own_pid() -> i32 {
    std::process::id() as i32
}

/// 入力先の追跡。自分以外で最後に前面にあったアプリを覚えておく。
#[derive(Default)]
pub struct TargetTracker {
    current: Arc<Mutex<Option<Target>>>,
    running: Arc<AtomicBool>,
}

impl TargetTracker {
    /// 追跡を始める。別スレッドで前面のアプリを見続ける。
    pub fn start() -> Self {
        let tracker = TargetTracker::default();
        tracker.running.store(true, Ordering::Relaxed);
        let current = tracker.current.clone();
        let running = tracker.running.clone();
        thread::spawn(move || {
            let me = own_pid();
            while running.load(Ordering::Relaxed) {
                if let Some(app) = frontmost_app() {
                    if app.pid != me {
                        if let Ok(mut slot) = current.lock() {
                            if slot.as_ref() != Some(&app) {
                                *slot = Some(app);
                            }
                        }
                    }
                }
                thread::sleep(Duration::from_millis(200));
            }
        });
        tracker
    }

    pub fn current(&self) -> Option<Target> {
        self.current.lock().ok().and_then(|slot| slot.clone())
    }
}

impl Drop for TargetTracker {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Relaxed);
    }
}

/// 文章を届ける。入力先を前面に出し、設定の方法で差し込む。
pub fn send(request: TransferRequest<'_>) -> Result<Delivery, TransferError> {
    let target = request.target.ok_or(TransferError::NoTarget)?;
    if !accessibility_trusted() {
        return Err(TransferError::NotTrusted);
    }
    activate(target)?;
    let delivery = match request.method {
        TransferMethod::Auto => match ax_insert(request.text) {
            Ok(()) => Delivery::Inserted,
            Err(reason) => {
                super::diag::log(format!("直接差し込みできず、貼り付けに切り替え: {reason}"));
                paste(request.text, request.restore_clipboard)?;
                Delivery::Pasted
            }
        },
        TransferMethod::Paste => {
            paste(request.text, request.restore_clipboard)?;
            Delivery::Pasted
        }
        TransferMethod::Type => {
            type_text(request.text)?;
            Delivery::Typed
        }
    };
    if request.enter_after {
        thread::sleep(Duration::from_millis(60));
        press_key(KEY_RETURN, false)?;
    }
    Ok(delivery)
}

/// 元のクリップボードの中身(文字か画像)。
enum Previous {
    Text(String),
    Image(arboard::ImageData<'static>),
}

fn snapshot(clipboard: &mut arboard::Clipboard) -> Option<Previous> {
    if let Ok(text) = clipboard.get_text() {
        return Some(Previous::Text(text));
    }
    clipboard.get_image().ok().map(Previous::Image)
}

/// クリップボードに入れて ⌘V を送る。終わったら元の中身(文字か画像)を戻す。
fn paste(text: &str, restore: bool) -> Result<(), TransferError> {
    let mut clipboard = arboard::Clipboard::new()
        .map_err(|e| TransferError::Deliver(format!("クリップボードを開けません: {e}")))?;
    let previous = restore.then(|| snapshot(&mut clipboard)).flatten();
    // クリップボード管理アプリの履歴に残さない印(`org.nspasteboard.ConcealedType`)を付ける。
    // Apple の機器どうしで共有するクリップボード(Handoff)には効かない。
    use arboard::SetExtApple;
    clipboard
        .set()
        .exclude_from_history()
        .text(text.to_string())
        .map_err(|e| TransferError::Deliver(format!("クリップボードに書けません: {e}")))?;
    thread::sleep(Duration::from_millis(40));
    press_key(KEY_V, true)?;
    if let Some(previous) = previous {
        // 入力先がクリップボードを読み終えるのを待ってから戻す。
        thread::sleep(Duration::from_millis(400));
        // 戻す中身にも同じ印を付ける(パスワードのように印付きで置かれていた中身を、印なしで書き戻さない)。
        let _ = match previous {
            Previous::Text(text) => clipboard.set().exclude_from_history().text(text),
            Previous::Image(image) => clipboard.set().exclude_from_history().image(image),
        };
    }
    Ok(())
}

/// 入力先を前面に出し、実際に前面になるまで待つ。
fn activate(target: &Target) -> Result<(), TransferError> {
    if frontmost_app().is_some_and(|app| app.pid == target.pid) {
        return Ok(());
    }
    if !activate_pid(target.pid) {
        return Err(TransferError::Activate(target.name.clone()));
    }
    let deadline = Instant::now() + Duration::from_millis(1500);
    while Instant::now() < deadline {
        if frontmost_app().is_some_and(|app| app.pid == target.pid) {
            // 前面になってすぐはキー入力が落ちることがあるので、わずかに待つ。
            thread::sleep(Duration::from_millis(80));
            return Ok(());
        }
        thread::sleep(Duration::from_millis(20));
    }
    Err(TransferError::Activate(target.name.clone()))
}

pub const KEY_V: u16 = 9;
pub const KEY_RETURN: u16 = 36;

// ---- macOS の実装 ----

#[cfg(target_os = "macos")]
mod native {
    use super::{Target, TransferError, KEY_RETURN};
    use core_foundation::base::{CFType, CFTypeRef, TCFType};
    use core_foundation::boolean::CFBoolean;
    use core_foundation::dictionary::CFDictionary;
    use core_foundation::string::{CFString, CFStringRef};
    use core_graphics::event::{CGEvent, CGEventFlags, CGEventTapLocation};
    use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
    use objc2_app_kit::{NSApplicationActivationOptions, NSRunningApplication, NSWorkspace};
    use std::ffi::c_void;
    use std::thread;
    use std::time::Duration;

    type AXUIElementRef = *const c_void;
    type AXError = i32;
    const AX_SUCCESS: AXError = 0;

    #[link(name = "ApplicationServices", kind = "framework")]
    extern "C" {
        fn AXUIElementCreateSystemWide() -> AXUIElementRef;
        fn AXUIElementCopyAttributeValue(
            element: AXUIElementRef,
            attribute: CFStringRef,
            value: *mut CFTypeRef,
        ) -> AXError;
        fn AXUIElementSetAttributeValue(
            element: AXUIElementRef,
            attribute: CFStringRef,
            value: CFTypeRef,
        ) -> AXError;
        fn AXIsProcessTrusted() -> bool;
        fn AXIsProcessTrustedWithOptions(
            options: core_foundation::dictionary::CFDictionaryRef,
        ) -> bool;
        static kAXTrustedCheckOptionPrompt: CFStringRef;
    }

    pub fn accessibility_trusted() -> bool {
        unsafe { AXIsProcessTrusted() }
    }

    /// 許可が無ければ Mac の確認画面を出す(システム設定へ誘導される)。
    pub fn request_accessibility() -> bool {
        unsafe {
            let key = CFString::wrap_under_get_rule(kAXTrustedCheckOptionPrompt);
            let options = CFDictionary::from_CFType_pairs(&[(
                key.as_CFType(),
                CFBoolean::true_value().as_CFType(),
            )]);
            AXIsProcessTrustedWithOptions(options.as_concrete_TypeRef())
        }
    }

    fn target_of(app: &NSRunningApplication) -> Target {
        Target {
            pid: app.processIdentifier(),
            bundle_id: app
                .bundleIdentifier()
                .map(|s| s.to_string())
                .unwrap_or_default(),
            name: app
                .localizedName()
                .map(|s| s.to_string())
                .unwrap_or_else(|| "アプリ".to_string()),
        }
    }

    pub fn frontmost_app() -> Option<Target> {
        // 追跡スレッドから 200ms ごとに呼ばれるので、自動解放の対象をその場で片付ける。
        objc2::rc::autoreleasepool(|_| {
            let workspace = NSWorkspace::sharedWorkspace();
            let app = workspace.frontmostApplication()?;
            Some(target_of(&app))
        })
    }

    pub fn activate_pid(pid: i32) -> bool {
        let Some(app) = NSRunningApplication::runningApplicationWithProcessIdentifier(pid) else {
            return false;
        };
        if app.isTerminated() {
            return false;
        }
        #[allow(deprecated)]
        app.activateWithOptions(NSApplicationActivationOptions::ActivateIgnoringOtherApps)
    }

    /// 自分が前面のとき、指定のアプリへ前面を譲る宣言(macOS 14 以降の協調的なアクティベーション)。
    /// 主スレッドからだけ呼べる。
    pub fn yield_activation(pid: i32) {
        let Some(mtm) = objc2::MainThreadMarker::new() else {
            return;
        };
        let Some(app) = NSRunningApplication::runningApplicationWithProcessIdentifier(pid) else {
            return;
        };
        let shared = objc2_app_kit::NSApplication::sharedApplication(mtm);
        shared.yieldActivationToApplication(&app);
    }

    fn ax_copy(element: AXUIElementRef, attribute: &str) -> Option<CFType> {
        let name = CFString::new(attribute);
        let mut value: CFTypeRef = std::ptr::null();
        let status = unsafe {
            AXUIElementCopyAttributeValue(element, name.as_concrete_TypeRef(), &mut value)
        };
        if status != AX_SUCCESS || value.is_null() {
            return None;
        }
        Some(unsafe { CFType::wrap_under_create_rule(value) })
    }

    fn ax_string(element: AXUIElementRef, attribute: &str) -> Option<String> {
        let value = ax_copy(element, attribute)?;
        value.downcast::<CFString>().map(|s| s.to_string())
    }

    /// 入力フォーカスのある欄に、選択範囲の置き換えとして文章を差し込む。
    /// 置き換えが受け付けられたか(値が読める欄なら中身が変わったか)まで確かめる。
    pub fn ax_insert(text: &str) -> Result<(), String> {
        let system = unsafe { AXUIElementCreateSystemWide() };
        if system.is_null() {
            return Err("アクセシビリティ API を使えません".to_string());
        }
        let _system_guard = unsafe { CFType::wrap_under_create_rule(system as CFTypeRef) };
        let focused =
            ax_copy(system, "AXFocusedUIElement").ok_or("入力欄にフォーカスがありません")?;
        let element = focused.as_CFTypeRef() as AXUIElementRef;
        let role = ax_string(element, "AXRole").unwrap_or_default();
        let before = ax_string(element, "AXValue");
        let value = CFString::new(text);
        let status = unsafe {
            AXUIElementSetAttributeValue(
                element,
                CFString::new("AXSelectedText").as_concrete_TypeRef(),
                value.as_CFTypeRef(),
            )
        };
        if status != AX_SUCCESS {
            return Err(format!(
                "{role} は直接差し込みを受け付けません(AXError {status})"
            ));
        }
        // 値が読める欄では、中身が変わったことをもって成功とする(同じ文が元からあっても、差し込めば変わる)。
        // 反映が遅い欄(Electron 製など)のために 50ms ごとに最長 400ms まで見直す。変わればその場で戻る。
        // 変わらなければ受け付けていないので、呼び出し側が貼り付けに回る。
        // 値が読めない欄は、受け付けた以上は入ったとみなす(ここで貼り付けに回ると二重に入る)。
        let Some(before) = before else {
            return Ok(());
        };
        for _ in 0..8 {
            thread::sleep(Duration::from_millis(50));
            match ax_string(element, "AXValue") {
                Some(after) if after == before => continue,
                _ => return Ok(()),
            }
        }
        Err(format!(
            "{role} は差し込みを受け付けたように見せて変わりませんでした"
        ))
    }

    fn event_source() -> Result<CGEventSource, TransferError> {
        CGEventSource::new(CGEventSourceStateID::HIDSystemState)
            .map_err(|_| TransferError::Deliver("キー入力の口を開けません".to_string()))
    }

    /// キーを 1 回押す(押して離す)。`command` なら ⌘ を添える。
    pub fn press_key(keycode: u16, command: bool) -> Result<(), TransferError> {
        let source = event_source()?;
        let flags = if command {
            CGEventFlags::CGEventFlagCommand
        } else {
            CGEventFlags::CGEventFlagNull
        };
        for down in [true, false] {
            let event = CGEvent::new_keyboard_event(source.clone(), keycode, down)
                .map_err(|_| TransferError::Deliver("キー入力を作れません".to_string()))?;
            event.set_flags(flags);
            event.post(CGEventTapLocation::HID);
            thread::sleep(Duration::from_millis(12));
        }
        Ok(())
    }

    /// 文章を 1 文字ずつキー入力として送る。改行は Return キーにする。
    pub fn type_text(text: &str) -> Result<(), TransferError> {
        let source = event_source()?;
        for line in text.split_inclusive('\n') {
            let (body, newline) = match line.strip_suffix('\n') {
                Some(body) => (body, true),
                None => (line, false),
            };
            // 1 回のイベントに載せられる長さ(UTF-16 で 20 単位)には上限があるので、短く区切る。
            for piece in crate::core::transfer::utf16_chunks(body, 16) {
                for down in [true, false] {
                    let event = CGEvent::new_keyboard_event(source.clone(), 0, down)
                        .map_err(|_| TransferError::Deliver("キー入力を作れません".to_string()))?;
                    event.set_string(&piece);
                    event.post(CGEventTapLocation::HID);
                }
                thread::sleep(Duration::from_millis(8));
            }
            if newline {
                press_key(KEY_RETURN, false)?;
            }
        }
        Ok(())
    }
}

#[cfg(target_os = "macos")]
pub use native::{
    accessibility_trusted, activate_pid, ax_insert, frontmost_app, press_key,
    request_accessibility, type_text, yield_activation,
};

#[cfg(not(target_os = "macos"))]
mod fallback {
    use super::{Target, TransferError};
    pub fn accessibility_trusted() -> bool {
        false
    }
    pub fn request_accessibility() -> bool {
        false
    }
    pub fn frontmost_app() -> Option<Target> {
        None
    }
    pub fn activate_pid(_pid: i32) -> bool {
        false
    }
    pub fn yield_activation(_pid: i32) {}
    pub fn ax_insert(_text: &str) -> Result<(), String> {
        Err("この OS では使えません".to_string())
    }
    pub fn press_key(_keycode: u16, _command: bool) -> Result<(), TransferError> {
        Err(TransferError::Deliver("この OS では使えません".to_string()))
    }
    pub fn type_text(_text: &str) -> Result<(), TransferError> {
        Err(TransferError::Deliver("この OS では使えません".to_string()))
    }
}

#[cfg(not(target_os = "macos"))]
pub use fallback::{
    accessibility_trusted, activate_pid, ax_insert, frontmost_app, press_key,
    request_accessibility, type_text, yield_activation,
};

/// システム設定のアクセシビリティの画面を開く。
pub fn open_accessibility_settings() -> Result<(), String> {
    std::process::Command::new("/usr/bin/open")
        .arg("x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility")
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("システム設定を開けません: {e}"))
}

/// このアプリの束の識別子(`mic_access` の Info.plist と同じ)。
pub const BUNDLE_ID: &str = "dev.openspeech.app";

/// アクセシビリティの一覧からこのアプリの登録を外す(`tccutil`。管理者の認証は要らない)。
/// ad-hoc 署名で作り直したあとは、一覧でオンに見えても古い署名への許可なので効かず、
/// Mac の確認画面も(登録済みと見なされて)二度と出ない。外せば「許可を求める」で登録し直せる。
pub fn reset_accessibility_entry() -> Result<(), String> {
    let output = std::process::Command::new("/usr/bin/tccutil")
        .args(["reset", "Accessibility", BUNDLE_ID])
        .output()
        .map_err(|e| format!("一覧から外せません: {e}"))?;
    if output.status.success() {
        return Ok(());
    }
    let reason = String::from_utf8_lossy(&output.stderr).trim().to_string();
    Err(if reason.is_empty() {
        "一覧から外せませんでした".to_string()
    } else {
        format!("一覧から外せませんでした: {reason}")
    })
}

/// UTF-16 で `limit` 単位以内になるように文字列を区切る(キー入力イベント 1 回分)。
pub fn utf16_chunks(text: &str, limit: usize) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut current = String::new();
    let mut units = 0;
    for c in text.chars() {
        let n = c.len_utf16();
        if units + n > limit && !current.is_empty() {
            chunks.push(std::mem::take(&mut current));
            units = 0;
        }
        current.push(c);
        units += n;
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

#[cfg(test)]
mod chunk_tests {
    use super::utf16_chunks;

    #[test]
    fn chunks_respect_utf16_units() {
        assert_eq!(utf16_chunks("", 16), Vec::<String>::new());
        let plain = "あ".repeat(40);
        let chunks = utf16_chunks(&plain, 16);
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks.concat(), plain);
        // 絵文字は 2 単位なので、16 文字では収まらない。
        let emoji = "😀".repeat(20);
        let chunks = utf16_chunks(&emoji, 16);
        assert!(chunks.iter().all(|c| c.encode_utf16().count() <= 16));
        assert_eq!(chunks.concat(), emoji);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errors_have_japanese_messages() {
        assert!(TransferError::NoTarget.to_string().contains("転送先"));
        assert!(TransferError::NotTrusted
            .to_string()
            .contains("アクセシビリティ"));
        assert!(TransferError::Activate("Safari".to_string())
            .to_string()
            .contains("Safari"));
    }

    #[test]
    fn sending_without_a_target_fails_before_touching_the_system() {
        let result = send(TransferRequest {
            text: "x",
            target: None,
            method: TransferMethod::Paste,
            restore_clipboard: false,
            enter_after: false,
        });
        assert_eq!(result, Err(TransferError::NoTarget));
    }

    #[test]
    fn tracker_without_a_thread_has_no_target() {
        assert_eq!(TargetTracker::default().current(), None);
    }
}
