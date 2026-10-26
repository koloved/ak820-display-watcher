//! Диагностика DDC/CI: спрашивает у монитора режим питания (VCP 0xD6).
//!
//! Если монитор выключен кнопкой, он обычно перестаёт отвечать по DDC — это и есть
//! признак, по которому Windows не видит, а мы можем увидеть.
//!
//! Запуск: `cargo run --release --example ddc -- 60`
//! Второй аргумент — сколько секунд опрашивать. Нажмите кнопку монитора во время работы.

use std::time::{Duration, Instant};

use windows_sys::Win32::Devices::Display::{
    DestroyPhysicalMonitors, GetNumberOfPhysicalMonitorsFromHMONITOR, GetPhysicalMonitorsFromHMONITOR,
    GetVCPFeatureAndVCPFeatureReply, PHYSICAL_MONITOR,
};
use windows_sys::Win32::Foundation::{HANDLE, POINT};
use windows_sys::Win32::Graphics::Gdi::{MonitorFromPoint, MONITOR_DEFAULTTOPRIMARY};

/// Спрашивает значение VCP-кода. Возвращает (значение, миллисекунды).
fn query(handle: HANDLE, code: u8) -> (Option<u32>, u128) {
    let started = Instant::now();
    let mut kind = 0i32;
    let mut current = 0u32;
    let mut maximum = 0u32;
    let ok = unsafe { GetVCPFeatureAndVCPFeatureReply(handle, code, &mut kind, &mut current, &mut maximum) };
    let ms = started.elapsed().as_millis();
    (if ok != 0 { Some(current) } else { None }, ms)
}

fn describe_power(value: Option<u32>) -> String {
    match value {
        None => "нет ответа".to_string(),
        Some(1) => "включён".to_string(),
        Some(2) => "standby".to_string(),
        Some(3) => "suspend".to_string(),
        Some(4) => "выключен (DPMS)".to_string(),
        Some(5) => "питание отключено".to_string(),
        Some(v) => format!("неизвестно ({v})"),
    }
}

fn main() {
    let secs: u64 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(60);

    unsafe {
        let hmon = MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY);
        if hmon.is_null() {
            println!("Не удалось получить монитор");
            return;
        }

        let mut count = 0u32;
        if GetNumberOfPhysicalMonitorsFromHMONITOR(hmon, &mut count) == 0 || count == 0 {
            println!("Физические мониторы не найдены (DDC/CI недоступен)");
            return;
        }
        let mut monitors: Vec<PHYSICAL_MONITOR> = (0..count).map(|_| std::mem::zeroed()).collect();
        if GetPhysicalMonitorsFromHMONITOR(hmon, count, monitors.as_mut_ptr()) == 0 {
            println!("Не удалось получить физические мониторы");
            return;
        }

        for (i, m) in monitors.iter().enumerate() {
            // Структура упакована, поэтому поле копируется в локальную переменную.
            let description: [u16; 128] = m.szPhysicalMonitorDescription;
            let name: String = description
                .iter()
                .take_while(|&&c| c != 0)
                .map(|&c| char::from_u32(c as u32).unwrap_or('?'))
                .collect();
            println!("Монитор {i}: {name:?}");
        }

        println!("Опрашиваю {secs} с. Нажмите кнопку монитора во время работы.\n");
        let started = Instant::now();
        while started.elapsed().as_secs() < secs {
            let elapsed = started.elapsed().as_secs();
            for m in &monitors {
                let (power, ms) = query(m.hPhysicalMonitor, 0xD6);
                let (bright, _) = query(m.hPhysicalMonitor, 0x10);
                println!(
                    "t+{elapsed:>3}с  питание={:<18} ({:>5} мс)  яркость={:?}",
                    describe_power(power),
                    ms,
                    bright
                );
            }
            std::thread::sleep(Duration::from_secs(1));
        }

        DestroyPhysicalMonitors(count, monitors.as_ptr());
        println!("\nГотово");
    }
}
