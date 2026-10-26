//! Проверка, отвечает ли монитор по DDC/CI.
//!
//! Windows не замечает, когда монитор выключают кнопкой: линия остаётся «подключённой»,
//! никаких событий не приходит. Единственный признак — монитор перестаёт отвечать на
//! DDC-запрос. Читаем VCP-код `0xD6` (состояние питания): пока монитор включён, он
//! возвращает `1`, после выключения кнопкой запрос не проходит.

use std::ptr;

use windows_sys::Win32::Devices::Display::{
    DestroyPhysicalMonitors, GetNumberOfPhysicalMonitorsFromHMONITOR, GetPhysicalMonitorsFromHMONITOR,
    GetVCPFeatureAndVCPFeatureReply, PHYSICAL_MONITOR,
};
use windows_sys::Win32::Foundation::{BOOL, HANDLE, LPARAM, RECT};
use windows_sys::Win32::Graphics::Gdi::{EnumDisplayMonitors, HDC, HMONITOR};

/// VCP-код состояния питания монитора.
const VCP_POWER: u8 = 0xD6;

/// Открытые handles физических мониторов.
pub struct Monitors {
    physical: Vec<PHYSICAL_MONITOR>,
}

impl Monitors {
    /// Перечисляет физические мониторы. `None`, если их не видно.
    pub fn acquire() -> Option<Monitors> {
        unsafe {
            let mut list: Vec<HMONITOR> = Vec::new();
            let ok = EnumDisplayMonitors(
                ptr::null_mut(),
                ptr::null(),
                Some(collect),
                &mut list as *mut Vec<HMONITOR> as LPARAM,
            );
            if ok == 0 || list.is_empty() {
                return None;
            }

            let mut physical = Vec::new();
            for hmon in list {
                let mut count = 0u32;
                if GetNumberOfPhysicalMonitorsFromHMONITOR(hmon, &mut count) == 0 || count == 0 {
                    continue;
                }
                let mut array: Vec<PHYSICAL_MONITOR> = (0..count).map(|_| std::mem::zeroed()).collect();
                if GetPhysicalMonitorsFromHMONITOR(hmon, count, array.as_mut_ptr()) == 0 {
                    continue;
                }
                physical.extend(array);
            }

            if physical.is_empty() {
                None
            } else {
                Some(Monitors { physical })
            }
        }
    }

    /// `true`, если хотя бы один монитор ответил на DDC-запрос.
    pub fn responsive(&self) -> bool {
        self.physical
            .iter()
            .any(|m| query_power(m.hPhysicalMonitor).is_some())
    }
}

impl Drop for Monitors {
    fn drop(&mut self) {
        if !self.physical.is_empty() {
            unsafe {
                DestroyPhysicalMonitors(self.physical.len() as u32, self.physical.as_ptr());
            }
        }
    }
}

unsafe extern "system" fn collect(hmon: HMONITOR, _hdc: HDC, _rect: *mut RECT, data: LPARAM) -> BOOL {
    let out = &mut *(data as *mut Vec<HMONITOR>);
    out.push(hmon);
    1
}

/// Спрашивает у монитора состояние питания. `None` — монитор не ответил.
fn query_power(handle: HANDLE) -> Option<u32> {
    let mut kind = 0i32;
    let mut current = 0u32;
    let mut maximum = 0u32;
    let ok = unsafe {
        GetVCPFeatureAndVCPFeatureReply(handle, VCP_POWER, &mut kind, &mut current, &mut maximum)
    };
    if ok != 0 {
        Some(current)
    } else {
        None
    }
}
