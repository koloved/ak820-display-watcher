//! Читает текущую конфигурацию клавиатуры и печатает её байты. Ничего не меняет.
//!
//! Запуск: `cargo run --release --example readcfg`

#[path = "../src/protocol.rs"]
#[allow(dead_code)]
mod protocol;

use hidapi::HidApi;
use protocol::{cmd, read_requests, CONFIG_LEN, INTERFACE, PID, USAGE, USAGE_PAGE, VID};

fn main() {
    let api = match HidApi::new() {
        Ok(a) => a,
        Err(e) => {
            println!("HidApi::new: {e}");
            return;
        }
    };
    let info = api
        .device_list()
        .filter(|d| d.vendor_id() == VID && d.product_id() == PID)
        .find(|d| d.usage_page() == USAGE_PAGE && d.usage() == USAGE)
        .or_else(|| {
            api.device_list()
                .filter(|d| d.vendor_id() == VID && d.product_id() == PID)
                .find(|d| d.interface_number() == INTERFACE)
        });
    let Some(info) = info else {
        println!("клавиатура не найдена");
        return;
    };
    let dev = match info.open_device(&api) {
        Ok(d) => d,
        Err(e) => {
            println!("не удалось открыть: {e}");
            return;
        }
    };

    let (req, _len) = read_requests(cmd::READ_CONFIG, 0, CONFIG_LEN)
        .into_iter()
        .next()
        .expect("один запрос на 29 байт");
    if let Err(e) = dev.write(&req) {
        println!("ошибка записи: {e}");
        return;
    }
    let mut resp = [0u8; protocol::PACKET];
    match dev.read_timeout(&mut resp, 1000) {
        Ok(n) if n > 8 => {
            let cfg = &resp[8..8 + CONFIG_LEN];
            println!("конфигурация ({} байт):", CONFIG_LEN);
            println!("{}", cfg.iter().map(|b| format!("{b:02X}")).collect::<Vec<_>>().join(" "));
            println!("яркость (байт 2): {}", cfg[protocol::BRIGHTNESS_INDEX]);
            println!("режим (байт 1): {}", cfg[1]);
        }
        Ok(n) => println!("короткий ответ: {n} байт"),
        Err(e) => println!("ошибка чтения: {e}"),
    }
}
