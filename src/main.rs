#![windows_subsystem = "windows"]

//! Фоновый сервис: гасит подсветку AJAZZ AK820, когда изображение на экранах пропадает,
//! и возвращает прежний вид подсветки, когда экран снова включается.
//!
//! События Windows приходят в окно сообщений; работа с HID и опрос DDC/CI выполняются
//! в отдельных потоках, чтобы блокирующие вызовы не останавливали очередь сообщений.

mod ddc;
#[path = "protocol.rs"]
mod protocol;

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::thread::{sleep, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use hidapi::{HidApi, HidDevice};
use protocol::{cmd, packet, read_requests, write_packets, CONFIG_LEN, INTERFACE, PID, USAGE, USAGE_PAGE, VID};
use windows_sys::core::GUID;
use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, HWND, LPARAM, LRESULT, WPARAM,
};
use windows_sys::Win32::System::Power::{RegisterPowerSettingNotification, POWERBROADCAST_SETTING};
use windows_sys::Win32::System::RemoteDesktop::{
    WTSRegisterSessionNotification, WTSUnRegisterSessionNotification, NOTIFY_FOR_THIS_SESSION,
};
use windows_sys::Win32::System::Threading::CreateMutexW;
use windows_sys::Win32::System::{Memory::SetProcessWorkingSetSizeEx, Threading::GetCurrentProcess};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, GetSystemMetrics, PostMessageW,
    PostQuitMessage, RegisterClassW, TranslateMessage, DEVICE_NOTIFY_WINDOW_HANDLE, HWND_MESSAGE, MSG,
    SM_CMONITORS, WM_APP, WM_DESTROY, WM_DISPLAYCHANGE, WM_POWERBROADCAST, WM_WTSSESSION_CHANGE, WNDCLASSW,
    PBT_APMRESUMEAUTOMATIC, PBT_APMSUSPEND, PBT_POWERSETTINGCHANGE, WTS_SESSION_LOCK, WTS_SESSION_UNLOCK,
};

const MUTEX_NAME: &str = "AK820_DisplayPowerWatcher_Mutex";
const WINDOW_CLASS: &str = "AK820_DisplayPowerWatcher_Window";
const LOG_FILE_NAME: &str = "ak820-display-watcher.log";
const LOG_MAX_BYTES: u64 = 1024 * 1024;

/// Сообщение от потока DDC: `wparam` = 1, если монитор отвечает.
const WM_APP_DDC: u32 = WM_APP + 1;

/// Период опроса DDC/CI. Опрос нужен только для монитора, выключенного кнопкой:
/// остальные случаи Windows сообщает событиями.
const DDC_INTERVAL: Duration = Duration::from_secs(10);
/// Размер стека вспомогательных потоков. По умолчанию Rust резервирует 2 МБ на поток,
/// а этим потокам хватает нескольких десятков килобайт.
const STACK_SIZE: usize = 128 * 1024;
/// Сколько раз подряд переспрашивать, прежде чем признать монитор выключенным.
/// DDC/CI иногда сбоит на ровном месте, одиночному отказу доверять нельзя.
const DDC_ATTEMPTS: usize = 4;
/// Пауза между повторами внутри одной проверки.
const DDC_RETRY_GAP: Duration = Duration::from_millis(250);

/// Считать ли состояние «Dimmed» (`GUID_CONSOLE_DISPLAY_STATE` = 2) выключенным экраном.
///
/// На практике монитор, уходя в энергосбережение по DPMS, нередко рапортует `2`, а не `0`:
/// изображения на нём уже нет, а Windows продолжает считать дисплей «затемнённым».
/// Обратный случай — плавное снижение яркости планом питания при простое, когда картинка
/// ещё видна. Для задачи «гасить подсветку, когда экран тёмный» верно первое, поэтому `2`
/// трактуется как выключенный экран. Если понадобится обратное поведение, поставьте `false`.
const DIMMED_MEANS_OFF: bool = true;

static DDC_PAUSED: AtomicBool = AtomicBool::new(false);
static DDC_RESET: AtomicBool = AtomicBool::new(false);
static DDC_HWND: AtomicIsize = AtomicIsize::new(0);

// GUID-ы питания дисплеев.
const GUID_CONSOLE_DISPLAY_STATE: GUID = GUID {
    data1: 0x6fe69556,
    data2: 0x704a,
    data3: 0x47a0,
    data4: [0x8f, 0x24, 0xc2, 0x8d, 0x93, 0x6f, 0xda, 0x47],
};
const GUID_MONITOR_POWER_ON: GUID = GUID {
    data1: 0x02731015,
    data2: 0x4510,
    data3: 0x4526,
    data4: [0x99, 0xe6, 0xe5, 0xa1, 0x7e, 0xbd, 0x1a, 0xea],
};
const GUID_SESSION_DISPLAY_STATUS: GUID = GUID {
    data1: 0x2b84c20e,
    data2: 0xad23,
    data3: 0x4ddf,
    data4: [0x93, 0xdb, 0x05, 0xff, 0xbd, 0x7e, 0xfc, 0xa5],
};

// ---------------------------------------------------------------------------
// Журнал
// ---------------------------------------------------------------------------

fn log_path() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join(LOG_FILE_NAME)))
        .unwrap_or_else(|| PathBuf::from(LOG_FILE_NAME))
}

fn log(msg: &str) {
    let path = log_path();
    if fs::metadata(&path).map(|m| m.len() > LOG_MAX_BYTES).unwrap_or(false) {
        let _ = fs::remove_file(&path);
    }
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(f, "[{ts}] {msg}");
    }
}

// ---------------------------------------------------------------------------
// Клавиатура: один долгоживущий HID-дескриптор
// ---------------------------------------------------------------------------

struct Keyboard {
    api: Option<HidApi>,
    device: Option<HidDevice>,
    /// Конфигурация, снятая перед выключением подсветки.
    saved: Option<[u8; CONFIG_LEN]>,
}

impl Keyboard {
    fn new() -> Self {
        Keyboard {
            api: None,
            device: None,
            saved: None,
        }
    }

    fn connect(&mut self) -> bool {
        if self.device.is_some() {
            return true;
        }
        if self.api.is_none() {
            let started = std::time::Instant::now();
            match HidApi::new() {
                Ok(api) => {
                    self.api = Some(api);
                    log(&format!("Перечисление HID: {} мс", started.elapsed().as_millis()));
                }
                Err(e) => {
                    log(&format!("HidApi::new: {e}"));
                    return false;
                }
            }
        }
        let api = self.api.as_ref().expect("api создан выше");
        let found = api
            .device_list()
            .filter(|d| d.vendor_id() == VID && d.product_id() == PID)
            .find(|d| d.usage_page() == USAGE_PAGE && d.usage() == USAGE)
            .or_else(|| {
                api.device_list()
                    .filter(|d| d.vendor_id() == VID && d.product_id() == PID)
                    .find(|d| d.interface_number() == INTERFACE)
            })
            .map(|info| {
                (
                    info.path().to_owned(),
                    info.interface_number(),
                    info.usage_page(),
                )
            });
        let Some((path, interface, usage_page)) = found else {
            // Список устарел (клавиатуру могли подключить) — в следующий раз перечислим заново.
            self.api = None;
            return false;
        };

        let started_open = std::time::Instant::now();
        let opened = api.open_path(&path);
        // Список всех HID-устройств системы держать незачем: он нужен только для поиска.
        // Дескриптор устройства живёт независимо, поэтому перечисление освобождаем.
        self.api = None;
        match opened {
            Ok(dev) => {
                log(&format!(
                    "Клавиатура подключена (if={interface}, usage_page=0x{usage_page:04X}, открытие {} мс)",
                    started_open.elapsed().as_millis()
                ));
                self.device = Some(dev);
                true
            }
            Err(e) => {
                log(&format!("Не удалось открыть клавиатуру: {e}"));
                false
            }
        }
    }

    /// Отправляет пакет и ждёт подтверждения.
    fn send(&mut self, pkt: &[u8; protocol::PACKET]) -> Result<Vec<u8>, String> {
        if !self.connect() {
            return Err("клавиатура не подключена".into());
        }
        let dev = self.device.as_ref().expect("устройство открыто");
        let result = (|| {
            dev.write(pkt).map_err(|e| e.to_string())?;
            let mut resp = [0u8; protocol::PACKET];
            let n = dev.read_timeout(&mut resp, 1000).map_err(|e| e.to_string())?;
            if n == 0 {
                return Err("нет ответа".to_string());
            }
            if resp[7] == 0xFF || resp[7] == 0xFE {
                return Err(format!("команда отклонена (0x{:02X})", resp[7]));
            }
            Ok(resp[..n].to_vec())
        })();
        if result.is_err() {
            // Разрыв связи: дескриптор сбрасывается, подключение — при следующей команде.
            self.device = None;
        }
        result
    }

    fn read_config(&mut self) -> Result<[u8; CONFIG_LEN], String> {
        let (req, len) = read_requests(cmd::READ_CONFIG, 0, CONFIG_LEN)
            .into_iter()
            .next()
            .expect("один запрос на 29 байт");
        let resp = self.send(&req)?;
        let mut cfg = [0u8; CONFIG_LEN];
        let available = resp.len().saturating_sub(8).min(len);
        if available < CONFIG_LEN {
            return Err(format!("короткий ответ: {available} байт"));
        }
        cfg.copy_from_slice(&resp[8..8 + CONFIG_LEN]);
        Ok(cfg)
    }

    fn write_config(&mut self, cfg: &[u8; CONFIG_LEN]) -> Result<(), String> {
        self.send(&packet(cmd::BEGIN, 0, 0, &[]))?;
        for p in write_packets(cmd::WRITE_CONFIG, 0, cfg) {
            self.send(&p)?;
        }
        sleep(Duration::from_millis(10));
        self.send(&packet(cmd::END, 0, 0, &[]))?;
        Ok(())
    }

    /// Гасит подсветку, запомнив текущую конфигурацию.
    fn turn_off(&mut self) -> bool {
        if self.saved.is_none() {
            match self.read_config() {
                Ok(cfg) => self.saved = Some(cfg),
                Err(e) => {
                    log(&format!("Выключение: не удалось прочитать конфиг ({e})"));
                    return false;
                }
            }
        }
        let Some(mut cfg) = self.saved else {
            return false;
        };
        cfg[protocol::BRIGHTNESS_INDEX] = 0;
        match self.write_config(&cfg) {
            Ok(()) => {
                log("Подсветка выключена");
                true
            }
            Err(e) => {
                log(&format!("Выключение не удалось: {e}"));
                false
            }
        }
    }

    /// Возвращает подсветку к сохранённой конфигурации.
    ///
    /// Если конфиг не снимался, значит подсветку и не гасили — ничего не трогаем,
    /// чтобы не изменить настройки пользователя.
    fn restore(&mut self) -> bool {
        let Some(cfg) = self.saved else {
            return false;
        };
        match self.write_config(&cfg) {
            Ok(()) => {
                self.saved = None;
                log("Подсветка восстановлена");
                true
            }
            Err(e) => {
                // Конфиг сохраняем: следующая попытка восстановления ещё возможна.
                log(&format!("Восстановление не удалось: {e}"));
                false
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Рабочий поток: очередь команд к клавиатуре
// ---------------------------------------------------------------------------

/// Поток, который держит HID-соединение и обрабатывает команды по очереди.
fn spawn_worker() -> (Sender<bool>, std::thread::JoinHandle<()>) {
    let (tx, rx): (Sender<bool>, Receiver<bool>) = channel();
    let handle = std::thread::Builder::new()
        .name("hid".into())
        .stack_size(STACK_SIZE)
        .spawn(move || {
        let mut kb = Keyboard::new();
        let mut off = false;

        // Подключаемся заранее: первое перечисление HID занимает до 10 секунд,
        // и делать его в момент блокировки экрана нельзя.
        let started = std::time::Instant::now();
        if kb.connect() {
            log(&format!("Готов к работе (подключение {} мс)", started.elapsed().as_millis()));
        } else {
            log("Клавиатура не найдена, подключусь при первом событии");
        }

        // Блокирующее ожидание: без опроса, поток спит пока нет команд.
        while let Ok(first) = rx.recv() {
            let mut want_off = first;
            // Пока поток был занят, события могли накопиться: берём последнее,
            // чтобы не мигать подсветкой серией устаревших команд.
            while let Ok(v) = rx.try_recv() {
                want_off = v;
            }
            off = want_off;
            if want_off {
                kb.turn_off();
            } else {
                kb.restore();
            }
            // Команда выполнена, до следующего события процесс ничего не делает.
            trim_working_set();
        }

        // Сервис останавливают — подсветку не оставляем погашенной.
        if off {
            kb.restore();
        }
        log("Рабочий поток завершён");
        })
        .expect("не удалось запустить поток HID");
    (tx, handle)
}

// ---------------------------------------------------------------------------
// Поток DDC/CI: замечает монитор, выключенный кнопкой
// ---------------------------------------------------------------------------

/// Раз в `DDC_INTERVAL` спрашивает монитор, отвечает ли он, и сообщает результат окну.
///
/// Между опросами поток спит в `recv_timeout`, поэтому в простое не занимает процессор:
/// одна проверка — один вызов драйвера, всё остальное время поток заблокирован.
fn spawn_ddc_watcher() -> (Sender<()>, JoinHandle<()>) {
    let (tx, rx) = channel::<()>();
    let handle = std::thread::Builder::new()
        .name("ddc".into())
        .stack_size(STACK_SIZE)
        .spawn(move || {
        let mut monitors: Option<ddc::Monitors> = None;
        // recv_timeout ждёт с таймаутом: поток спит между опросами и просыпается
        // сразу, когда канал закрывают при остановке сервиса.
        while matches!(rx.recv_timeout(DDC_INTERVAL), Ok(()) | Err(RecvTimeoutError::Timeout)) {
            // ОС уже сообщила, что экран погашен — события расскажут о пробуждении.
            if DDC_PAUSED.load(Ordering::Relaxed) {
                monitors = None;
                continue;
            }
            // Топология изменилась: handles могли устареть.
            if DDC_RESET.swap(false, Ordering::Relaxed) {
                monitors = None;
            }
            if monitors.is_none() {
                monitors = ddc::Monitors::acquire();
            }
            let Some(current) = monitors.as_ref() else {
                // Мониторов не видно — решает проверка топологии, а не DDC.
                continue;
            };

            let mut responsive = current.responsive();
            if !responsive {
                // Одиночный сбой DDC/CI ничего не значит: переспрашиваем несколько раз
                // подряд. Повторы стоят дёшево и случаются только при подозрении.
                for _ in 1..DDC_ATTEMPTS {
                    sleep(DDC_RETRY_GAP);
                    if current.responsive() {
                        responsive = true;
                        break;
                    }
                }
            }
            if !responsive {
                // Монитор молчит — handles могли умереть вместе с линией.
                // Следующий цикл перечислит их заново, чтобы не застрять в «выключено».
                monitors = None;
            }

            let hwnd = DDC_HWND.load(Ordering::Relaxed);
            if hwnd != 0 {
                unsafe {
                    PostMessageW(hwnd as HWND, WM_APP_DDC, usize::from(responsive), 0);
                }
            }
        }
        log("Поток DDC завершён");
        })
        .expect("не удалось запустить поток DDC");
    (tx, handle)
}

// ---------------------------------------------------------------------------
// Состояние
// ---------------------------------------------------------------------------

struct State {
    /// ОС сообщила, что дисплей погашен (таймаут, сон системы).
    display_sleeping: bool,
    /// Сеанс заблокирован (`Win + L`). Влияет только на журнал.
    session_locked: bool,
    /// `SM_CMONITORS == 0` — мониторов нет вовсе.
    monitors_disconnected: bool,
    /// Монитор перестал отвечать по DDC/CI: выключен кнопкой.
    monitor_unresponsive: bool,
    /// Состояние, уже отправленное в рабочий поток.
    applied_off: bool,
    tx: Sender<bool>,
}

impl State {
    /// Подсветка гасится только тогда, когда изображения на экранах нет.
    ///
    /// Блокировка сеанса намеренно не учитывается: на экране ввода пароля подсветка
    /// нужна оставаться включённой, иначе в тёмной комнате клавиатуру не видно.
    /// Как только экран гаснет (по таймауту или кнопкой), подсветка выключается.
    fn should_be_off(&self) -> bool {
        self.display_sleeping || self.monitors_disconnected || self.monitor_unresponsive
    }

    /// Экран снова включился.
    ///
    /// Вывод DDC после пробуждения устарел: пока экран был погашен, опрос не шёл,
    /// и флаг «монитор молчит» мог остаться с прошлого раза. Сбрасываем его
    /// оптимистично, чтобы подсветка вернулась сразу, а не ждала следующего опроса.
    /// Если монитор действительно выключен, ближайшая проверка вернёт всё обратно.
    fn display_on(&mut self) {
        self.display_sleeping = false;
        self.monitor_unresponsive = false;
    }

    /// Отправляет команду только при изменении результирующего состояния.
    fn reconcile(&mut self) {
        // Пока ОС сама знает, что экран погашен, опрос DDC не нужен: он ничего
        // не добавит, а стоит вызова драйвера.
        DDC_PAUSED.store(self.display_sleeping || self.monitors_disconnected, Ordering::Relaxed);

        let want_off = self.should_be_off();
        if want_off == self.applied_off {
            return;
        }
        log(&format!(
            "Состояние: сон={} блокировка={} мониторы_отключены={} монитор_молчит={} → выключить={want_off}",
            self.display_sleeping, self.session_locked, self.monitors_disconnected, self.monitor_unresponsive
        ));
        if self.tx.send(want_off).is_err() {
            log("Рабочий поток недоступен");
        }
        self.applied_off = want_off;
    }
}

// ---------------------------------------------------------------------------
// Оконная процедура
// ---------------------------------------------------------------------------

static mut STATE: *mut State = ptr::null_mut();

fn state() -> &'static mut State {
    // SAFETY: STATE создаётся в main до окна и живёт до его закрытия;
    // оконная процедура вызывается только из потока сообщений.
    unsafe { &mut *STATE }
}

unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_WTSSESSION_CHANGE => {
            let st = state();
            match wparam as u32 {
                WTS_SESSION_LOCK => {
                    st.session_locked = true;
                    // Подсветку не трогаем: на экране ввода пароля она нужна.
                    log("Сеанс заблокирован — подсветка остаётся включённой");
                }
                WTS_SESSION_UNLOCK => {
                    st.session_locked = false;
                    log("Сеанс разблокирован");
                }
                _ => {}
            }
            0
        }
        WM_POWERBROADCAST => {
            let st = state();
            match wparam as u32 {
                PBT_APMSUSPEND => {
                    st.display_sleeping = true;
                    st.reconcile();
                }
                PBT_APMRESUMEAUTOMATIC => {
                    st.display_on();
                    st.reconcile();
                }
                PBT_POWERSETTINGCHANGE => {
                    let setting = lparam as *const POWERBROADCAST_SETTING;
                    if !setting.is_null() {
                        let s = &*setting;
                        // 0 = выключен, 1 = включён, 2 = затемнён.
                        let value = *(s.Data.as_ptr() as *const u32);
                        let name = if guid_eq(&s.PowerSetting, &GUID_CONSOLE_DISPLAY_STATE) {
                            Some("console_display_state")
                        } else if guid_eq(&s.PowerSetting, &GUID_MONITOR_POWER_ON) {
                            Some("monitor_power_on")
                        } else if guid_eq(&s.PowerSetting, &GUID_SESSION_DISPLAY_STATUS) {
                            Some("session_display_status")
                        } else {
                            None
                        };
                        if let Some(name) = name {
                            log(&format!("Питание дисплея: {name} = {value}"));
                        }
                        // Решение принимается только по состоянию самого дисплея.
                        // session_display_status описывает доступность дисплея сеансу и
                        // остаётся 1 при погашенном экране, из-за чего состояние дёргалось.
                        let is_display = guid_eq(&s.PowerSetting, &GUID_CONSOLE_DISPLAY_STATE)
                            || guid_eq(&s.PowerSetting, &GUID_MONITOR_POWER_ON);
                        if is_display {
                            // 0 = выключен, 1 = включён, 2 = затемнён.
                            let off = match value {
                                0 => true,
                                2 => DIMMED_MEANS_OFF,
                                _ => false,
                            };
                            if off {
                                st.display_sleeping = true;
                            } else {
                                st.display_on();
                            }
                            st.reconcile();
                        }
                    }
                }
                _ => {}
            }
            1
        }
        WM_DISPLAYCHANGE => {
            let st = state();
            log("Смена конфигурации дисплеев");
            DDC_RESET.store(true, Ordering::Relaxed);
            st.monitors_disconnected = GetSystemMetrics(SM_CMONITORS) == 0;
            log_display_topology();
            st.reconcile();
            0
        }
        WM_APP_DDC => {
            // Пришло от потока DDC: монитор ответил (1) или промолчал (0).
            let st = state();
            let unresponsive = wparam == 0;
            if st.monitor_unresponsive != unresponsive {
                st.monitor_unresponsive = unresponsive;
                log(if unresponsive {
                    "DDC: монитор не отвечает — считаю выключенным"
                } else {
                    "DDC: монитор снова отвечает"
                });
                st.reconcile();
            }
            0
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            0
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

fn guid_eq(a: &GUID, b: &GUID) -> bool {
    a.data1 == b.data1 && a.data2 == b.data2 && a.data3 == b.data3 && a.data4 == b.data4
}

/// Пишет в журнал, какие дисплеи Windows сейчас считает подключёнными.
/// Нужно для диагностики: включённый и выключенный физически монитор
/// различаются только по флагам состояния.
fn log_display_topology() {
    unsafe {
        use windows_sys::Win32::Graphics::Gdi::{EnumDisplayDevicesW, DISPLAY_DEVICEW};
        let mut monitors = 0u32;
        let mut i = 0u32;
        loop {
            let mut adapter: DISPLAY_DEVICEW = std::mem::zeroed();
            adapter.cb = std::mem::size_of::<DISPLAY_DEVICEW>() as u32;
            if EnumDisplayDevicesW(ptr::null(), i, &mut adapter, 0) == 0 {
                break;
            }
            let mut j = 0u32;
            loop {
                let mut monitor: DISPLAY_DEVICEW = std::mem::zeroed();
                monitor.cb = std::mem::size_of::<DISPLAY_DEVICEW>() as u32;
                if EnumDisplayDevicesW(adapter.DeviceName.as_ptr(), j, &mut monitor, 0) == 0 {
                    break;
                }
                let name: String = monitor
                    .DeviceString
                    .iter()
                    .take_while(|&&c| c != 0)
                    .map(|&c| char::from_u32(c as u32).unwrap_or('?'))
                    .collect();
                log(&format!("  дисплей: {name:?} flags=0x{:08X}", monitor.StateFlags));
                monitors += 1;
                j += 1;
            }
            i += 1;
        }
        log(&format!(
            "Топология: SM_CMONITORS={} найдено={monitors}",
            GetSystemMetrics(SM_CMONITORS)
        ));
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Отдаёт системе страницы, к которым процесс больше не будет обращаться.
///
/// Основную часть рабочего набора занимают отображённые библиотеки: `SETUPAPI`,
/// `user32`, `combase` и прочие. После инициализации они не нужны, и их можно выгрузить.
/// Это не освобождает память системы (страницы общие и остаются резидентными для других
/// процессов), но снимает с процесса обязанность держать их в своём рабочем наборе.
/// При следующем обращении страницы подгружаются обратно автоматически.
fn trim_working_set() {
    unsafe {
        SetProcessWorkingSetSizeEx(GetCurrentProcess(), usize::MAX, usize::MAX, 0);
    }
}

fn main() {
    // Одиночный экземпляр.
    let mutex_name = wide(MUTEX_NAME);
    let mutex = unsafe { CreateMutexW(ptr::null(), 0, mutex_name.as_ptr()) };
    if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        unsafe { CloseHandle(mutex) };
        return;
    }

    log("Запуск сервиса");

    let (tx, worker) = spawn_worker();
    let mut st = Box::new(State {
        display_sleeping: false,
        session_locked: false,
        monitors_disconnected: false,
        monitor_unresponsive: false,
        applied_off: false,
        tx,
    });
    unsafe { STATE = &mut *st as *mut State };

    unsafe {
        let hinstance = windows_sys::Win32::System::LibraryLoader::GetModuleHandleW(ptr::null());
        let class_name = wide(WINDOW_CLASS);

        let wc = WNDCLASSW {
            style: 0,
            lpfnWndProc: Some(wnd_proc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: hinstance,
            hIcon: ptr::null_mut(),
            hCursor: ptr::null_mut(),
            hbrBackground: ptr::null_mut(),
            lpszMenuName: ptr::null(),
            lpszClassName: class_name.as_ptr(),
        };
        RegisterClassW(&wc);

        // Message-only окно: не рисуется, поэтому нагрузка в простое нулевая.
        let hwnd = CreateWindowExW(
            0,
            class_name.as_ptr(),
            class_name.as_ptr(),
            0,
            0,
            0,
            0,
            0,
            HWND_MESSAGE,
            ptr::null_mut(),
            hinstance,
            ptr::null(),
        );
        if hwnd.is_null() {
            log("Не удалось создать окно сообщений");
            return;
        }

        for guid in [&GUID_CONSOLE_DISPLAY_STATE, &GUID_MONITOR_POWER_ON, &GUID_SESSION_DISPLAY_STATUS] {
            RegisterPowerSettingNotification(hwnd as _, guid, DEVICE_NOTIFY_WINDOW_HANDLE);
        }
        WTSRegisterSessionNotification(hwnd, NOTIFY_FOR_THIS_SESSION);

        // Поток DDC узнаёт, куда сообщать результат.
        DDC_HWND.store(hwnd as isize, Ordering::Relaxed);
        let (ddc_tx, ddc_thread) = spawn_ddc_watcher();

        // Начальное состояние: проверяем текущую топологию мониторов.
        st.monitors_disconnected = GetSystemMetrics(SM_CMONITORS) == 0;
        log("Начальная топология");
        log_display_topology();
        st.reconcile();
        // Инициализация закончена: выгружаем из рабочего набора всё лишнее.
        trim_working_set();

        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, ptr::null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }

        WTSUnRegisterSessionNotification(hwnd);
        DDC_HWND.store(0, Ordering::Relaxed);
        drop(ddc_tx);
        let _ = ddc_thread.join();
        CloseHandle(mutex);
    }

    // Закрываем канал: рабочий поток вернёт подсветку и завершится.
    unsafe { STATE = ptr::null_mut() };
    drop(st);
    let _ = worker.join();
    log("Остановка сервиса");
}
