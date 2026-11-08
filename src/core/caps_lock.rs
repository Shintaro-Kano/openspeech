//! Caps Lock で録音を切り替える。Mac 専用。
//!
//! Caps Lock は全体ショートカットとして登録できない(修飾キーの扱いになる)ので、状態を見張る。
//! オンの間は認識し、オフの間は止める。キーボードのランプが録音中の目印になるよう、
//! 画面のボタンや自動停止で録音が変わったときはランプも合わせる。
//! 状態の読み書きと画面のロックの確認に、Mac の許可は要らない。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

/// Caps Lock を見る間隔。押してから録音が始まるまでの遅れになるので短くする。
const POLL_INTERVAL: Duration = Duration::from_millis(25);
/// 画面のロックは急がないので、この回数に 1 回だけ見る(約 1 秒ごと)。
const LOCK_CHECK_TICKS: u32 = 40;
/// 時計がこれ以上飛んだら、Mac がスリープしていたとみなす。
const SLEEP_GAP: Duration = Duration::from_secs(5);
/// ランプを変えてから状態の読み取りに届くまで少しかかる。届くのを待つ上限。
const LAMP_WAIT: Duration = Duration::from_secs(1);
/// 続けてこの回数ランプが変わらなければ、変えられないものとして合わせるのをやめる。
const LAMP_MISSES: u8 = 3;

/// 利用者の切り替えを受けて録音をどうするか。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MicRequest {
    Start,
    Stop,
}

/// Caps Lock と録音の突き合わせ。OS から切り離してあるので、動きをそのまま試験できる。
#[derive(Debug, Default)]
pub struct CapsFollower {
    /// 最後に確かめた Caps Lock の状態。
    seen: bool,
    /// 自分で変えたランプが読み取りに届くのを待っている(値と期限)。この間の変化は利用者の操作と数えない。
    waiting: Option<(bool, Instant)>,
    /// 続けてランプが変わらなかった回数。
    misses: u8,
    /// ランプを変えられないと分かったら、合わせるのをやめる。
    lamp_failed: bool,
}

impl CapsFollower {
    /// ランプの変化を待っている間は true。
    fn wait_for_lamp(&mut self, caps: bool, now: Instant) -> bool {
        let Some((target, deadline)) = self.waiting else {
            return false;
        };
        if caps == target {
            self.waiting = None;
            self.misses = 0;
            return false;
        }
        if now < deadline {
            return true;
        }
        // 変わらなかった。録音は動かさず、いまの状態を受け入れる。
        self.waiting = None;
        self.seen = caps;
        self.misses += 1;
        self.lamp_failed = self.misses >= LAMP_MISSES;
        false
    }

    /// 利用者が切り替えたかを調べる。
    pub fn observe(&mut self, caps: bool, now: Instant) -> Option<MicRequest> {
        if self.wait_for_lamp(caps, now) || caps == self.seen {
            return None;
        }
        self.seen = caps;
        Some(if caps {
            MicRequest::Start
        } else {
            MicRequest::Stop
        })
    }

    /// 録音の実際の状態にランプを合わせる必要があれば、合わせる値を返す。
    pub fn align(&mut self, caps: bool, recording: bool, now: Instant) -> Option<bool> {
        if self.lamp_failed || self.waiting.is_some() || caps == recording {
            return None;
        }
        self.seen = recording;
        self.waiting = Some((recording, now + LAMP_WAIT));
        Some(recording)
    }

    /// 切り替えを録音に結びつけずに、いまの状態として受け入れる(画面のロック中など)。
    pub fn accept(&mut self, caps: bool) {
        self.seen = caps;
        self.waiting = None;
    }

    /// ランプを変えられなかった。繰り返さず、いまの状態を受け入れる。
    pub fn lamp_unavailable(&mut self, caps: bool) {
        self.lamp_failed = true;
        self.seen = caps;
        self.waiting = None;
    }

    /// ランプが変わらないと分かっているか。
    pub fn lamp_failed(&self) -> bool {
        self.lamp_failed
    }
}

/// Caps Lock と画面のロックを見張る。変わったら `wake` で画面を起こす。
pub struct Watcher {
    away: Arc<AtomicBool>,
    locked: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
}

impl Watcher {
    pub fn start(wake: impl Fn() + Send + 'static) -> Self {
        let away = Arc::new(AtomicBool::new(false));
        let locked = Arc::new(AtomicBool::new(screen_locked()));
        let stop = Arc::new(AtomicBool::new(false));
        let watcher = Watcher {
            away: away.clone(),
            locked: locked.clone(),
            stop: stop.clone(),
        };
        std::thread::spawn(move || {
            let mut caps = is_on();
            let mut last_tick = SystemTime::now();
            let mut ticks = 0u32;
            while !stop.load(Ordering::Relaxed) {
                std::thread::sleep(POLL_INTERVAL);
                let now = SystemTime::now();
                // 単調な時計はスリープ中に止まるので、スリープの検知には時刻を使う。
                let slept = now
                    .duration_since(last_tick)
                    .is_ok_and(|gap| gap >= SLEEP_GAP);
                last_tick = now;
                let mut changed = false;
                let current = is_on();
                if current != caps {
                    caps = current;
                    changed = true;
                }
                ticks += 1;
                if slept || ticks >= LOCK_CHECK_TICKS {
                    ticks = 0;
                    let is_locked = screen_locked();
                    let was_locked = locked.swap(is_locked, Ordering::Relaxed);
                    if slept || (is_locked && !was_locked) {
                        away.store(true, Ordering::Relaxed);
                        changed = true;
                    }
                }
                if changed {
                    wake();
                }
            }
        });
        watcher
    }

    /// 前回から、画面のロックかスリープがあったか。読むと消える。
    pub fn take_away(&self) -> bool {
        self.away.swap(false, Ordering::Relaxed)
    }

    /// いま画面がロックされているか。
    pub fn is_locked(&self) -> bool {
        self.locked.load(Ordering::Relaxed)
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

#[cfg(target_os = "macos")]
mod mac {
    use core_foundation::base::{CFType, TCFType};
    use core_foundation::boolean::CFBoolean;
    use core_foundation::dictionary::{CFDictionary, CFDictionaryRef};
    use core_foundation::string::CFString;
    use objc2_foundation::{NSActivityOptions, NSProcessInfo, NSString};
    use std::ffi::{c_char, c_int, c_void};

    /// kCGEventSourceStateCombinedSessionState。
    const COMBINED_SESSION_STATE: i32 = 0;
    /// kCGEventFlagMaskAlphaShift。
    const ALPHA_SHIFT: u64 = 0x0001_0000;
    /// kIOHIDParamConnectType。
    const PARAM_CONNECT_TYPE: u32 = 1;
    /// kIOHIDCapsLockState。
    const CAPS_LOCK_STATE: c_int = 1;

    #[link(name = "CoreGraphics", kind = "framework")]
    extern "C" {
        fn CGEventSourceFlagsState(state: i32) -> u64;
        fn CGSessionCopyCurrentDictionary() -> CFDictionaryRef;
    }

    #[link(name = "IOKit", kind = "framework")]
    extern "C" {
        fn IOServiceMatching(name: *const c_char) -> *mut c_void;
        fn IOServiceGetMatchingService(main_port: u32, matching: *mut c_void) -> u32;
        fn IOServiceOpen(service: u32, owning_task: u32, kind: u32, connect: *mut u32) -> i32;
        fn IOServiceClose(connect: u32) -> i32;
        fn IOObjectRelease(object: u32) -> i32;
        fn IOHIDSetModifierLockState(connect: u32, selector: c_int, state: bool) -> i32;
    }

    extern "C" {
        static mach_task_self_: u32;
    }

    pub fn is_on() -> bool {
        // SAFETY: 引数は定数で、戻り値はただの数値。
        unsafe { CGEventSourceFlagsState(COMBINED_SESSION_STATE) & ALPHA_SHIFT != 0 }
    }

    pub fn set(on: bool) -> bool {
        // SAFETY: IOKit の決まった手順。開いた接続とサービスは下で閉じる。
        unsafe {
            let matching = IOServiceMatching(c"IOHIDSystem".as_ptr());
            if matching.is_null() {
                return false;
            }
            // matching の所有は渡る。
            let service = IOServiceGetMatchingService(0, matching);
            if service == 0 {
                return false;
            }
            let mut connect = 0u32;
            let opened = IOServiceOpen(service, mach_task_self_, PARAM_CONNECT_TYPE, &mut connect);
            IOObjectRelease(service);
            if opened != 0 {
                return false;
            }
            let result = IOHIDSetModifierLockState(connect, CAPS_LOCK_STATE, on);
            IOServiceClose(connect);
            result == 0
        }
    }

    pub fn screen_locked() -> bool {
        // SAFETY: Copy の名の通り所有が渡るので、wrap_under_create_rule で受ける。
        unsafe {
            let dictionary = CGSessionCopyCurrentDictionary();
            if dictionary.is_null() {
                return false;
            }
            let dictionary: CFDictionary<CFString, CFType> =
                CFDictionary::wrap_under_create_rule(dictionary);
            let key = CFString::from_static_string("CGSSessionScreenIsLocked");
            dictionary
                .find(key)
                .and_then(|value| value.downcast::<CFBoolean>())
                .is_some_and(bool::from)
        }
    }

    /// App Nap で眠ると Caps Lock の見張りが何秒も遅れるので、起きたままにする(スリープは妨げない)。
    pub fn prevent_app_nap() {
        let token = NSProcessInfo::processInfo().beginActivityWithOptions_reason(
            NSActivityOptions::UserInitiatedAllowingIdleSystemSleep,
            &NSString::from_str("音声入力の待ち受け"),
        );
        // アプリが動いている間ずっと有効にする。
        std::mem::forget(token);
    }
}

#[cfg(not(target_os = "macos"))]
mod mac {
    pub fn is_on() -> bool {
        false
    }
    pub fn set(_on: bool) -> bool {
        false
    }
    pub fn screen_locked() -> bool {
        false
    }
    pub fn prevent_app_nap() {}
}

pub use mac::{is_on, prevent_app_nap, screen_locked, set};

#[cfg(test)]
mod tests {
    use super::*;

    fn at(start: Instant, ms: u64) -> Instant {
        start + Duration::from_millis(ms)
    }

    #[test]
    fn turning_caps_lock_on_and_off_follows_the_key() {
        let start = Instant::now();
        let mut follower = CapsFollower::default();
        assert_eq!(follower.observe(false, start), None);
        assert_eq!(follower.observe(true, start), Some(MicRequest::Start));
        assert_eq!(follower.observe(true, start), None, "同じ状態では何もしない");
        assert_eq!(follower.align(true, true, start), None);
        assert_eq!(follower.observe(false, start), Some(MicRequest::Stop));
        assert_eq!(follower.align(false, false, start), None);
    }

    #[test]
    fn caps_lock_already_on_at_launch_starts_recording() {
        let mut follower = CapsFollower::default();
        assert_eq!(
            follower.observe(true, Instant::now()),
            Some(MicRequest::Start)
        );
    }

    #[test]
    fn a_failed_start_turns_the_lamp_back_off() {
        let start = Instant::now();
        let mut follower = CapsFollower::default();
        assert_eq!(follower.observe(true, start), Some(MicRequest::Start));
        assert_eq!(follower.align(true, false, start), Some(false));
        // ランプが消えるのを待つ間、まだオンに見えても録音は始めない。
        assert_eq!(follower.observe(true, at(start, 50)), None);
        assert_eq!(follower.align(true, false, at(start, 50)), None);
        assert_eq!(follower.observe(true, at(start, 400)), None, "遅くても待つ");
        assert_eq!(follower.observe(false, at(start, 500)), None);
        assert_eq!(follower.align(false, false, at(start, 500)), None);
    }

    /// 変えたはずのランプが変わらなくても、録音を勝手に動かさず、ずっと繰り返しもしない。
    #[test]
    fn a_lamp_that_never_changes_does_not_loop() {
        let start = Instant::now();
        let mut follower = CapsFollower::default();
        assert_eq!(follower.observe(true, start), Some(MicRequest::Start));
        let mut tries = 0;
        for tick in 0..100u64 {
            let now = at(start, tick * 100);
            assert_eq!(follower.observe(true, now), None);
            if follower.align(true, false, now).is_some() {
                tries += 1;
            }
        }
        assert_eq!(tries, LAMP_MISSES);
        assert!(follower.lamp_failed());
    }

    #[test]
    fn the_mic_button_moves_the_lamp_without_a_feedback_loop() {
        let start = Instant::now();
        let mut follower = CapsFollower::default();
        // 画面のボタンで録音を始めた。
        assert_eq!(follower.observe(false, start), None);
        assert_eq!(follower.align(false, true, start), Some(true));
        // ランプが変わった状態を読んでも、利用者の操作とは数えない。
        assert_eq!(follower.observe(false, at(start, 10)), None);
        assert_eq!(follower.observe(false, at(start, 500)), None, "遅れても止めない");
        assert_eq!(follower.observe(true, at(start, 600)), None);
        assert_eq!(follower.align(true, true, at(start, 600)), None);
        // そのあと利用者がキーで止められる。
        assert_eq!(follower.observe(false, at(start, 700)), Some(MicRequest::Stop));
    }

    #[test]
    fn an_automatic_stop_turns_the_lamp_off() {
        let start = Instant::now();
        let mut follower = CapsFollower::default();
        follower.observe(true, start);
        assert_eq!(follower.align(true, false, at(start, 500)), Some(false));
        assert_eq!(follower.observe(false, at(start, 700)), None);
        assert!(!follower.lamp_failed());
    }

    #[test]
    fn a_lamp_that_cannot_be_set_is_left_alone() {
        let start = Instant::now();
        let mut follower = CapsFollower::default();
        assert_eq!(follower.align(false, true, start), Some(true));
        follower.lamp_unavailable(false);
        assert_eq!(follower.observe(false, start), None, "勝手に止めない");
        assert_eq!(follower.align(false, true, at(start, 500)), None);
        assert_eq!(follower.observe(true, at(start, 600)), Some(MicRequest::Start));
    }

    #[test]
    fn accepted_changes_do_not_start_recording() {
        let start = Instant::now();
        let mut follower = CapsFollower::default();
        follower.accept(true);
        assert_eq!(follower.observe(true, start), None);
    }
}
