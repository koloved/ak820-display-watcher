//! Диагностика: читает конфигурацию AK820, гасит подсветку и возвращает обратно.
//!
//! Запуск: `cargo run --release --example probe`

#[path = "../src/protocol.rs"]
mod protocol;

use std::thread::sleep;
use std::time::Duration;

use hidapi::{HidApi, HidDevice};
use protocol::{cmd, packet, read_requests, write_packets, CONFIG_LEN, INTERFACE, PID, USAGE, USAGE_PAGE, VID};

/// Отправляет пакет и ждёт ответ.
fn send(dev: &HidDevice, pkt: &[u8; protocol::PACKET]) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    dev.write(pkt)?;
    let mut resp = [0u8; protocol::PACKET];
    let n = dev.read_timeout(&mut resp, 1000)?;
    if n == 0 {
        return Err("нет ответа от клавиатуры".into());
    }
    if resp[7] == 0xFF || resp[7] == 0xFE {
        return Err(format!("клавиатура отклонила команду (status 0x{:02X})", resp[7]).into());
    }
    Ok(resp[..n].to_vec())
}

fn read_config(dev: &HidDevice) -> Result<[u8; CONFIG_LEN], Box<dyn std::error::Error>> {
    let (req, _len) = read_requests(cmd::READ_CONFIG, 0, CONFIG_LEN)
        .into_iter()
        .next()
        .expect("один запрос на 29 байт");
    let resp = send(dev, &req)?;
    let mut cfg = [0u8; CONFIG_LEN];
    cfg.copy_from_slice(&resp[8..8 + CONFIG_LEN]);
    Ok(cfg)
}

fn write_config(dev: &HidDevice, cfg: &[u8; CONFIG_LEN]) -> Result<(), Box<dyn std::error::Error>> {
    send(dev, &packet(cmd::BEGIN, 0, 0, &[]))?;
    for p in write_packets(cmd::WRITE_CONFIG, 0, cfg) {
        send(dev, &p)?;
    }
    sleep(Duration::from_millis(10));
    send(dev, &packet(cmd::END, 0, 0, &[]))?;
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("1. HidApi::new…");
    let api = HidApi::new()?;
    println!("2. ok");

    println!("3. интерфейсы {VID:04X}:{PID:04X}:");
    for d in api.device_list().filter(|d| d.vendor_id() == VID && d.product_id() == PID) {
        println!(
            "   if={} usage_page=0x{:04X} usage=0x{:04X}",
            d.interface_number(),
            d.usage_page(),
            d.usage()
        );
    }

    let info = api
        .device_list()
        .filter(|d| d.vendor_id() == VID && d.product_id() == PID)
        .find(|d| d.usage_page() == USAGE_PAGE && d.usage() == USAGE)
        .or_else(|| {
            api.device_list()
                .filter(|d| d.vendor_id() == VID && d.product_id() == PID)
                .find(|d| d.interface_number() == INTERFACE)
        })
        .ok_or("вендорский интерфейс не найден")?;
    println!("4. выбран if={} usage_page=0x{:04X}", info.interface_number(), info.usage_page());

    let dev = info.open_device(&api)?;
    println!("5. устройство открыто");

    let original = read_config(&dev)?;
    println!("6. конфиг прочитан, яркость = {}", original[protocol::BRIGHTNESS_INDEX]);
    println!("   байты: {:02X?}", original);

    let mut off = original;
    off[protocol::BRIGHTNESS_INDEX] = 0;
    write_config(&dev, &off)?;
    println!("7. яркость 0 отправлена — подсветка должна погаснуть (2 с)");
    sleep(Duration::from_secs(2));

    write_config(&dev, &original)?;
    println!("8. исходный конфиг возвращён — подсветка должна вернуться");
    Ok(())
}
