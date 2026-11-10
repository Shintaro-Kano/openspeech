//! 録音デバイスがいま動いているか(IO が回っているか)を CoreAudio から読む。診断 `--release-check` 用。
//! cpal はこの値を出さないので、CoreAudio の公開 API を直接呼ぶ。

/// デバイスの動き。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeviceRunning {
    /// どこかのプロセスがこのデバイスの IO を回している(`kAudioDevicePropertyDeviceIsRunningSomewhere`)。
    pub somewhere: bool,
    /// このプロセスの中で IO が回っている(`kAudioDevicePropertyDeviceIsRunning`)。
    pub here: bool,
}

#[cfg(target_os = "macos")]
mod mac {
    use super::DeviceRunning;
    use crate::core::audio::same_device;
    use core_foundation::base::TCFType;
    use core_foundation::string::{CFString, CFStringRef};
    use std::ffi::c_void;

    #[repr(C)]
    struct PropertyAddress {
        selector: u32,
        scope: u32,
        element: u32,
    }

    #[link(name = "CoreAudio", kind = "framework")]
    extern "C" {
        fn AudioObjectGetPropertyDataSize(
            object: u32,
            address: *const PropertyAddress,
            qualifier_size: u32,
            qualifier: *const c_void,
            size: *mut u32,
        ) -> i32;
        fn AudioObjectGetPropertyData(
            object: u32,
            address: *const PropertyAddress,
            qualifier_size: u32,
            qualifier: *const c_void,
            size: *mut u32,
            data: *mut c_void,
        ) -> i32;
    }

    const fn code(text: &[u8; 4]) -> u32 {
        u32::from_be_bytes(*text)
    }

    const SYSTEM_OBJECT: u32 = 1;
    const SCOPE_GLOBAL: u32 = code(b"glob");
    const ELEMENT_MAIN: u32 = 0;
    const DEVICES: u32 = code(b"dev#");
    const NAME: u32 = code(b"lnam");
    const IS_RUNNING: u32 = code(b"goin");
    const IS_RUNNING_SOMEWHERE: u32 = code(b"gone");

    fn address(selector: u32) -> PropertyAddress {
        PropertyAddress {
            selector,
            scope: SCOPE_GLOBAL,
            element: ELEMENT_MAIN,
        }
    }

    fn device_ids() -> Result<Vec<u32>, String> {
        let address = address(DEVICES);
        let mut size = 0u32;
        // SAFETY: 公開 API に、正しい大きさの受け皿を渡す。
        let status = unsafe {
            AudioObjectGetPropertyDataSize(SYSTEM_OBJECT, &address, 0, std::ptr::null(), &mut size)
        };
        if status != 0 {
            return Err(format!("デバイスの一覧を読めません({status})"));
        }
        let mut ids = vec![0u32; size as usize / std::mem::size_of::<u32>()];
        let status = unsafe {
            AudioObjectGetPropertyData(
                SYSTEM_OBJECT,
                &address,
                0,
                std::ptr::null(),
                &mut size,
                ids.as_mut_ptr().cast(),
            )
        };
        if status != 0 {
            return Err(format!("デバイスの一覧を読めません({status})"));
        }
        ids.truncate(size as usize / std::mem::size_of::<u32>());
        Ok(ids)
    }

    fn device_name(id: u32) -> Option<String> {
        let address = address(NAME);
        let mut name: CFStringRef = std::ptr::null();
        let mut size = std::mem::size_of::<CFStringRef>() as u32;
        // SAFETY: 名前は作られた CFString で返る(呼んだ側が手放す)。
        let status = unsafe {
            AudioObjectGetPropertyData(
                id,
                &address,
                0,
                std::ptr::null(),
                &mut size,
                (&mut name as *mut CFStringRef).cast(),
            )
        };
        if status != 0 || name.is_null() {
            return None;
        }
        Some(unsafe { CFString::wrap_under_create_rule(name) }.to_string())
    }

    fn flag(id: u32, selector: u32) -> Result<bool, String> {
        let address = address(selector);
        let mut value = 0u32;
        let mut size = std::mem::size_of::<u32>() as u32;
        let status = unsafe {
            AudioObjectGetPropertyData(
                id,
                &address,
                0,
                std::ptr::null(),
                &mut size,
                (&mut value as *mut u32).cast(),
            )
        };
        if status != 0 {
            return Err(format!("デバイスの状態を読めません({status})"));
        }
        Ok(value != 0)
    }

    pub fn device_running(name: &str) -> Result<DeviceRunning, String> {
        let id = device_ids()?
            .into_iter()
            .find(|id| device_name(*id).is_some_and(|n| same_device(&n, name)))
            .ok_or_else(|| format!("デバイスが見つかりません: {name}"))?;
        Ok(DeviceRunning {
            somewhere: flag(id, IS_RUNNING_SOMEWHERE)?,
            here: flag(id, IS_RUNNING)?,
        })
    }
}

/// 名前のデバイスがいま動いているか。
#[cfg(target_os = "macos")]
pub fn device_running(name: &str) -> Result<DeviceRunning, String> {
    mac::device_running(name)
}

#[cfg(not(target_os = "macos"))]
pub fn device_running(_name: &str) -> Result<DeviceRunning, String> {
    Err("この OS では読めません".to_string())
}
