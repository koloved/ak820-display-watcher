//! Диагностика DDC/CI: что мониторы сообщают о питании (VCP 0xD6).
//!
//! Печатает значение для каждого физического монитора раз в секунду.
//! Запуск: `cargo run --release --example ddc -- 90`
//! Во время работы погасите экраны — будет видно, отвечает ли монитор и каким значением.

use std::time::{Duration, Instant};

use windows_sys::Win32::Devices::Display::{
    DestroyPhysicalMonitors, GetNumberOfPhysicalMonitorsFromHMONITOR, GetPhysicalMonitorsFromHMONITOR,
    GetVCPFeatureAndVCPFeatureReply, PHYSICAL_MONITOR,
};
use windows_sys::Win32::Foundation::{BOOL, LPARAM, RECT};
use windows_sys::Win32::Graphics::Gdi::{EnumDisplayMonitors, HDC, HMONITOR};

/// Спрашивает значение VCP-кода. `None` — монитор не ответил.
fn query(handle: windows_sys::Win32::Foundation::HANDLE, code: u8) -> Option<u32> {
    let mut kind = 0i32;
    let mut current = 0u32;
    let mut maximum = 0u32;
    let ok = unsafe { GetVCPFeatureAndVCPFeatureReply(handle, code, &mut kind, &mut current, &mut maximum) };
    if ok != 0 {
        Some(current)
    } else {
        None
    }
}

unsafe extern "system" fn collect(hmon: HMONITOR, _hdc: HDC, _rect: *mut RECT, data: LPARAM) -> BOOL {
    let out = &mut *(data as *mut Vec<HMONITOR>);
    out.push(hmon);
    1
}

fn describe(value: Option<u32>) -> String {
    match value {
        None => "нет ответа".to_string(),
        Some(1) => "1 (включён)".to_string(),
        Some(2) => "2 (standby)".to_string(),
        Some(3) => "3 (suspend)".to_string(),
        Some(4) => "4 (выключен)".to_string(),
        Some(5) => "5 (питание снято)".to_string(),
        Some(v) => format!("{v} (неизвестно)"),
    }
}

fn main() {
    let secs: u64 = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(60);

    unsafe {
        let mut hmons: Vec<HMONITOR> = Vec::new();
        let ok = EnumDisplayMonitors(
            std::ptr::null_mut(),
            std::ptr::null(),
            Some(collect),
            &mut hmons as *mut Vec<HMONITOR> as LPARAM,
        );
        if ok == 0 || hmons.is_empty() {
            println!("Мониторы не найдены");
            return;
        }

        let mut monitors: Vec<PHYSICAL_MONITOR> = Vec::new();
        for hmon in &hmons {
            let mut count = 0u32;
            if GetNumberOfPhysicalMonitorsFromHMONITOR(*hmon, &mut count) == 0 || count == 0 {
                continue;
            }
            let mut array: Vec<PHYSICAL_MONITOR> = (0..count).map(|_| std::mem::zeroed()).collect();
            if GetPhysicalMonitorsFromHMONITOR(*hmon, count, array.as_mut_ptr()) == 0 {
                continue;
            }
            monitors.extend(array);
        }

        println!("Физических мониторов: {}", monitors.len());
        for (i, m) in monitors.iter().enumerate() {
            let description: [u16; 128] = m.szPhysicalMonitorDescription;
            let name: String = description
                .iter()
                .take_while(|&&c| c != 0)
                .map(|&c| char::from_u32(c as u32).unwrap_or('?'))
                .collect();
            println!("  [{i}] {name:?}");
        }
        println!("\nОпрашиваю {secs} с. Погасите экраны — смотрите значения.\n");

        let started = Instant::now();
        while started.elapsed().as_secs() < secs {
            let elapsed = started.elapsed().as_secs();
            let mut line = format!("t+{elapsed:>3}с ");
            for (i, m) in monitors.iter().enumerate() {
                let (power, bright) = (query(m.hPhysicalMonitor, 0xD6), query(m.hPhysicalMonitor, 0x10));
                line.push_str(&format!(" [{}]={}", i, describe(power)));
                if bright.is_some() {
                    line.push_str("(яр.)");
                }
            }
            println!("{line}");
            std::thread::sleep(Duration::from_secs(1));
        }

        DestroyPhysicalMonitors(monitors.len() as u32, monitors.as_ptr());
        println!("\nГотово");
    }
}
