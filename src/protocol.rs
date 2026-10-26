//! EVision HID-протокол клавиатуры AK820 (VID 0x320F, PID 0x505B).
//!
//! Формат пакета — 64 байта, первый байт report ID `0x04`:
//! ```text
//! [0]     0x04           report ID
//! [1..3]  checksum LE    16-битная сумма байтов 3..63
//! [3]     command        код команды
//! [4]     size           длина полезной нагрузки
//! [5..7]  offset LE      смещение в блоке данных
//! [8..]   payload        до 56 байт
//! ```

pub const VID: u16 = 0x320F;
pub const PID: u16 = 0x505B;

/// Вендорский интерфейс клавиатуры.
pub const USAGE_PAGE: u16 = 0xFF1C;
pub const USAGE: u16 = 0x0092;
/// Запасной признак, если hidapi не сообщает usage page.
pub const INTERFACE: i32 = 1;

pub const REPORT_ID: u8 = 0x04;
pub const PACKET: usize = 64;
pub const MAX_CHUNK: usize = 56;

/// Длина блока конфигурации.
pub const CONFIG_LEN: usize = 29;
/// Индекс яркости в конфигурации: `0` — подсветка выключена, `4` — максимум.
pub const BRIGHTNESS_INDEX: usize = 2;

pub mod cmd {
    pub const BEGIN: u8 = 0x01;
    pub const END: u8 = 0x02;
    pub const READ_CONFIG: u8 = 0x05;
    pub const WRITE_CONFIG: u8 = 0x06;
}

pub type Packet = [u8; PACKET];

/// 16-битная сумма байтов.
pub fn sum16(data: &[u8]) -> u16 {
    data.iter().fold(0u16, |acc, &b| acc.wrapping_add(b as u16))
}

/// Собирает пакет с корректной контрольной суммой.
pub fn packet(command: u8, size: u8, offset: u16, payload: &[u8]) -> Packet {
    let mut p = [0u8; PACKET];
    p[0] = REPORT_ID;
    p[3] = command;
    p[4] = size;
    p[5..7].copy_from_slice(&offset.to_le_bytes());
    let n = payload.len().min(MAX_CHUNK);
    p[8..8 + n].copy_from_slice(&payload[..n]);
    let sum = sum16(&p[3..]);
    p[1..3].copy_from_slice(&sum.to_le_bytes());
    p
}

/// Пакеты записи блока данных, разбитого по 56 байт.
pub fn write_packets(command: u8, base_offset: u16, data: &[u8]) -> Vec<Packet> {
    data.chunks(MAX_CHUNK)
        .enumerate()
        .map(|(i, chunk)| packet(command, chunk.len() as u8, base_offset + (i * MAX_CHUNK) as u16, chunk))
        .collect()
}

/// `(пакет-запрос, длина фрагмента)` для чтения `total_len` байт по 56 байт.
pub fn read_requests(command: u8, base_offset: u16, total_len: usize) -> Vec<(Packet, usize)> {
    let mut out = Vec::new();
    let mut pos = 0;
    while pos < total_len {
        let len = (total_len - pos).min(MAX_CHUNK);
        out.push((packet(command, len as u8, base_offset + pos as u16, &[]), len));
        pos += len;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checksum_covers_command_and_payload() {
        let p = packet(cmd::READ_CONFIG, 0, 0, &[]);
        assert_eq!(p[0], 0x04);
        assert_eq!(p[3], cmd::READ_CONFIG);
        assert_eq!(u16::from_le_bytes([p[1], p[2]]), sum16(&p[3..]));
    }

    #[test]
    fn chunking_splits_by_56() {
        let data = vec![0u8; 100];
        assert_eq!(write_packets(cmd::WRITE_CONFIG, 0, &data).len(), 2);
    }
}
