//! Признаки «экраны погашены», не зависящие от ответов мониторов.
//!
//! Windows гасит экраны сама, ровно по истечении таймаута бездействия из плана питания.
//! Отсюда надёжный вывод: если бездействие достигло таймаута, изображения на экранах нет.
//! Это нужно потому, что монитор может продолжать отвечать по DDC/CI, уже погаснув, и
//! тогда единственный признак «молчания» не срабатывает. События питания тоже приходят
//! не всегда.

use std::ptr;

use windows_sys::core::GUID;
use windows_sys::Win32::Foundation::LocalFree;
use windows_sys::Win32::System::Power::{
    GetSystemPowerStatus, PowerGetActiveScheme, PowerReadACValueIndex, PowerReadDCValueIndex,
    SYSTEM_POWER_STATUS,
};
use windows_sys::Win32::System::SystemInformation::GetTickCount64;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};
use windows_sys::Win32::UI::Shell::{
    SHQueryUserNotificationState, QUNS_PRESENTATION_MODE, QUNS_RUNNING_D3D_FULL_SCREEN,
    QUERY_USER_NOTIFICATION_STATE,
};

/// Подгруппа «Экран» в схеме электропитания.
const VIDEO_SUBGROUP: GUID = GUID {
    data1: 0x7516b95f,
    data2: 0xf776,
    data3: 0x4464,
    data4: [0x8c, 0x53, 0x06, 0x16, 0x7f, 0x40, 0xcc, 0x99],
};
/// «Отключать экран через», секунды.
const VIDEO_POWERDOWN_TIMEOUT: GUID = GUID {
    data1: 0x3c0bc021,
    data2: 0xc8a8,
    data3: 0x4e07,
    data4: [0xa9, 0x73, 0x6b, 0x14, 0xcb, 0xcb, 0x2b, 0x7e],
};
/// «Отключать экран при блокировке через», секунды.
const CONSOLE_LOCK_DISPLAY_OFF: GUID = GUID {
    data1: 0x8ec4b3a5,
    data2: 0x6868,
    data3: 0x48c2,
    data4: [0xbe, 0x75, 0x4f, 0x30, 0x44, 0xbe, 0x88, 0xa7],
};

/// На линии питания (иначе — от батареи).
fn on_ac() -> bool {
    let mut status: SYSTEM_POWER_STATUS = unsafe { std::mem::zeroed() };
    if unsafe { GetSystemPowerStatus(&mut status) } == 0 {
        return true;
    }
    // 1 — от сети, 0 — от батареи, 255 — неизвестно.
    status.ACLineStatus != 0
}

/// Через сколько секунд бездействия Windows гасит экраны.
///
/// `0` означает «никогда» — тогда вывод по бездействию не применяется.
pub fn display_timeout_seconds(locked: bool) -> Option<u32> {
    unsafe {
        let mut scheme: *mut GUID = ptr::null_mut();
        if PowerGetActiveScheme(ptr::null_mut(), &mut scheme) != 0 || scheme.is_null() {
            return None;
        }
        let setting = if locked {
            &CONSOLE_LOCK_DISPLAY_OFF
        } else {
            &VIDEO_POWERDOWN_TIMEOUT
        };
        let mut index = 0u32;
        let rc = if on_ac() {
            PowerReadACValueIndex(ptr::null_mut(), scheme, &VIDEO_SUBGROUP, setting, &mut index)
        } else {
            PowerReadDCValueIndex(ptr::null_mut(), scheme, &VIDEO_SUBGROUP, setting, &mut index)
        };
        LocalFree(scheme as _);
        if rc != 0 {
            None
        } else {
            Some(index)
        }
    }
}

/// Сколько секунд прошло с последнего ввода пользователя.
pub fn idle_seconds() -> u64 {
    let mut info = LASTINPUTINFO {
        cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32,
        dwTime: 0,
    };
    if unsafe { GetLastInputInfo(&mut info) } == 0 {
        return 0;
    }
    // `dwTime` — младшие 32 бита счётчика тиков, поэтому сравниваем с младшими же.
    let now = unsafe { GetTickCount64() } as u32;
    (now.wrapping_sub(info.dwTime) / 1000) as u64
}

/// Полноэкранное приложение держит дисплей включённым.
///
/// Без этой проверки правило «бездействие превысило таймаут» гасило бы подсветку во время
/// фильма или игры: ввода нет, а изображение есть.
///
/// `QUNS_BUSY` сюда намеренно не входит: Windows отдаёт его при любом развёрнутом окне,
/// и тогда правило блокировалось бы почти всегда. Учитываются только исключительный
/// полноэкранный Direct3D-режим и режим презентации.
pub fn screen_held_awake() -> bool {
    let mut state: QUERY_USER_NOTIFICATION_STATE = 0;
    if unsafe { SHQueryUserNotificationState(&mut state) } != 0 {
        return false;
    }
    matches!(state, QUNS_RUNNING_D3D_FULL_SCREEN | QUNS_PRESENTATION_MODE)
}
