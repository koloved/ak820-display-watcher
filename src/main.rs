#![windows_subsystem = "windows"]

//! Фоновый сервис: гасит подсветку AJAZZ AK820, когда изображение на экранах пропадает,
//! и возвращает прежний вид подсветки, когда экран снова включается.
//!
//! Устройство:
//! - окно сообщений принимает события Windows и следит за собственным существованием;
//! - поток наблюдения независимо опрашивает мониторы по DDC/CI и состояние топологии;
//! - поток HID держит клавиатуру и выполняет команды.
//!
//! Состояние хранится в атомиках, поэтому вердикт можно принять из любого потока:
//! решение не зависит от того, живо ли окно сообщений.

mod ddc;
#[path = "protocol.rs"]
mod protocol;

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::sync::Mutex;
use std::thread::{sleep, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use hidapi::{HidApi, HidDevice};
use protocol::{cmd, packet, read_requests, write_packets, CONFIG_LEN, INTERFACE, PID, USAGE, USAGE_PAGE, VID};
use windows_sys::core::GUID;
use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, HINSTANCE, HWND, LPARAM, LRESULT, WAIT_OBJECT_0, WPARAM, ERROR_ALREADY_EXISTS,
};
use windows_sys::Win32::System::Power::{RegisterPowerSettingNotification, POWERBROADCAST_SETTING};
use windows_sys::Win32::System::RemoteDesktop::{
    WTSRegisterSessionNotification, WTSUnRegisterSessionNotification, NOTIFY_FOR_THIS_SESSION,
};
use windows_sys::Win32::System::Threading::CreateMutexW;
use windows_sys::Win32::System::{Memory::SetProcessWorkingSetSizeEx, Threading::GetCurrentProcess};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GetSystemMetrics, IsWindow,
    MsgWaitForMultipleObjectsEx, PeekMessageW, PostQuitMessage, RegisterClassW, TranslateMessage,
    DEVICE_NOTIFY_WINDOW_HANDLE, HWND_MESSAGE, MSG, MWMO_INPUTAVAILABLE, PM_REMOVE, QS_ALLINPUT,
    SM_CMONITORS, WM_DESTROY, WM_DISPLAYCHANGE, WM_POWERBROADCAST, WM_QUIT, WM_WTSSESSION_CHANGE, WNDCLASSW,
    PBT_APMRESUMEAUTOMATIC, PBT_APMSUSPEND, PBT_POWERSETTINGCHANGE, WTS_SESSION_LOCK, WTS_SESSION_UNLOCK,
};

const MUTEX_NAME: &str = "AK820_DisplayPowerWatcher_Mutex";
const WINDOW_CLASS: &str = "AK820_DisplayPowerWatcher_Window";
const LOG_FILE_NAME: &str = "ak820-display-watcher.log";
const LOG_MAX_BYTES: u64 = 1024 * 1024;

/// Период опроса мониторов. События питания приходят не всегда: на этой системе
/// уход экранов в сон по таймауту не сопровождался ни одним уведомлением, поэтому
/// реакция целиком зависит от опроса. Замерено, что вызов DDC блокируется на ожидании
/// шины и не занимает процессор, так что частый опрос стоит примерно ничего.
const WATCH_INTERVAL: Duration = Duration::from_secs(3);
/// Сколько раз подряд переспрашивать, прежде чем признать мониторы погасшими.
/// DDC/CI иногда сбоит на ровном месте, одиночному отказу доверять нельзя.
const DDC_ATTEMPTS: usize = 4;
/// Пауза между повторами внутри одной проверки.
const DDC_RETRY_GAP: Duration = Duration::from_millis(200);
/// Сколько опросов подряд должны молчать все мониторы, чтобы поверить в это.
///
/// DDC/CI сбоит и на исправном мониторе: в замере одиночные отказы появлялись несколько раз
/// за минуту, а при опросе каждые 3 секунды один такой провал гасил подсветку и через
/// мгновение возвращал её. Решение принимается только после устойчивой картины.
const OFF_CONFIRM_POLLS: u32 = 3;
/// Сколько опросов подряд должны отвечать мониторы, чтобы вернуть подсветку.
const ON_CONFIRM_POLLS: u32 = 2;
/// Период проверки живости окна сообщений. Ожидание блокирующее, процессор не занимает.
const HEALTH_CHECK_MS: u32 = 1000;
/// Размер стека вспомогательных потоков: по умолчанию Rust резервирует 2 МБ на поток.
const STACK_SIZE: usize = 128 * 1024;

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
// Состояние: доступно из всех потоков
// ---------------------------------------------------------------------------

/// ОС сообщила, что дисплей погашен.
static DISPLAY_SLEEPING: AtomicBool = AtomicBool::new(false);
/// Сеанс заблокирован (`Win + L`). Влияет только на журнал.
static SESSION_LOCKED: AtomicBool = AtomicBool::new(false);
/// `SM_CMONITORS == 0` — мониторов нет вовсе.
static MONITORS_GONE: AtomicBool = AtomicBool::new(false);
/// Ни один монитор не ответил по DDC/CI — погашены все.
static MONITOR_SILENT: AtomicBool = AtomicBool::new(false);
/// Состояние, уже отправленное в поток HID.
static APPLIED_OFF: AtomicBool = AtomicBool::new(false);
/// Топология изменилась: handles мониторов нужно пересоздать.
static RESET_MONITORS: AtomicBool = AtomicBool::new(false);

static WORKER_TX: Mutex<Option<Sender<bool>>> = Mutex::new(None);
static WATCH_TX: Mutex<Option<Sender<()>>> = Mutex::new(None);
/// Сериализует выдачу команд, чтобы два потока не отправили их одновременно.
static RECONCILE: Mutex<()> = Mutex::new(());

/// Подсветка гасится только тогда, когда изображения нет ни на одном экране.
///
/// Блокировка сеанса намеренно не учитывается: на экране ввода пароля подсветка нужна,
/// иначе в тёмной комнате клавиатуру не видно.
fn should_be_off() -> bool {
    DISPLAY_SLEEPING.load(Ordering::Acquire)
        || MONITORS_GONE.load(Ordering::Acquire)
        || MONITOR_SILENT.load(Ordering::Acquire)
}

/// Отправляет команду, только если результирующее состояние изменилось.
fn reconcile() {
    let _guard = RECONCILE.lock().unwrap_or_else(|e| e.into_inner());
    let want_off = should_be_off();
    if want_off == APPLIED_OFF.load(Ordering::Acquire) {
        return;
    }
    log(&format!(
        "Состояние: сон={} блокировка={} мониторы_отключены={} мониторы_молчат={} → выключить={want_off}",
        DISPLAY_SLEEPING.load(Ordering::Acquire),
        SESSION_LOCKED.load(Ordering::Acquire),
        MONITORS_GONE.load(Ordering::Acquire),
        MONITOR_SILENT.load(Ordering::Acquire),
    ));
    if let Ok(guard) = WORKER_TX.lock() {
        if let Some(tx) = guard.as_ref() {
            let _ = tx.send(want_off);
        }
    }
    APPLIED_OFF.store(want_off, Ordering::Release);
}

/// Экран снова включился: прежний вывод DDC устарел, подсветка возвращается сразу.
fn display_on() {
    DISPLAY_SLEEPING.store(false, Ordering::Release);
    MONITOR_SILENT.store(false, Ordering::Release);
}

/// Будит поток наблюдения, чтобы он проверил мониторы немедленно.
fn nudge_watch() {
    if let Ok(guard) = WATCH_TX.lock() {
        if let Some(tx) = guard.as_ref() {
            let _ = tx.send(());
        }
    }
}

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
        // Список всех HID-устройств держать незачем: он нужен только для поиска, а
        // открытый дескриптор от него не зависит.
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
        let available = resp.len().saturating_sub(8).min(len);
        if available < CONFIG_LEN {
            return Err(format!("короткий ответ: {available} байт"));
        }
        let mut cfg = [0u8; CONFIG_LEN];
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
    /// Если конфиг не снимался, подсветку и не гасили — ничего не трогаем.
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
// Поток HID
// ---------------------------------------------------------------------------

fn spawn_worker() -> (Sender<bool>, JoinHandle<()>) {
    let (tx, rx): (Sender<bool>, Receiver<bool>) = channel();
    let handle = std::thread::Builder::new()
        .name("hid".into())
        .stack_size(STACK_SIZE)
        .spawn(move || {
            let mut kb = Keyboard::new();
            let mut off = false;

            // Подключаемся заранее: перечисление HID занимает до 10 секунд, и делать его
            // в момент, когда экран уже погас, нельзя.
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
// Поток наблюдения: топология и DDC/CI
// ---------------------------------------------------------------------------

/// Раз в `WATCH_INTERVAL` (или немедленно по запросу) проверяет, есть ли изображение.
///
/// Здесь же ловится монитор, выключенный кнопкой: Windows об этом не сообщает вообще,
/// единственный признак — монитор перестаёт отвечать на DDC-запрос.
fn spawn_watchdog() -> (Sender<()>, JoinHandle<()>) {
    let (tx, rx) = channel::<()>();
    let handle = std::thread::Builder::new()
        .name("watch".into())
        .stack_size(STACK_SIZE)
        .spawn(move || {
            let mut monitors: Option<ddc::Monitors> = None;
            let mut acquire_failed = false;
            // Счётчики подряд идущих опросов для защиты от дребезга DDC/CI.
            let mut silent_streak: u32 = 0;
            let mut alive_streak: u32 = 0;
            while matches!(rx.recv_timeout(WATCH_INTERVAL), Ok(()) | Err(RecvTimeoutError::Timeout)) {
                // Топология изменилась — handles могли устареть.
                if RESET_MONITORS.swap(false, Ordering::AcqRel) {
                    monitors = None;
                }
                if monitors.is_none() {
                    monitors = ddc::Monitors::acquire();
                    // Молча ничего не проверять нельзя: без этого «не удалось перечислить»
                    // выглядело бы как «всё хорошо».
                    if monitors.is_none() && !acquire_failed {
                        log("DDC: не удалось перечислить мониторы");
                        acquire_failed = true;
                    } else if monitors.is_some() && acquire_failed {
                        log("DDC: мониторы снова перечисляются");
                        acquire_failed = false;
                    }
                }

                let gone = unsafe { GetSystemMetrics(SM_CMONITORS) == 0 };
                MONITORS_GONE.store(gone, Ordering::Release);

                // Если ОС уже знает, что дисплей погашен, опрос ничего не добавит.
                let paused = DISPLAY_SLEEPING.load(Ordering::Acquire) || gone;
                if paused {
                    MONITOR_SILENT.store(false, Ordering::Release);
                    silent_streak = 0;
                    alive_streak = 0;
                } else {
                    // Результат этого опроса: true — хотя бы один монитор ещё отвечает.
                    let responsive = match monitors.as_ref() {
                        Some(current) => {
                            let mut r = current.responsive();
                            if !r {
                                for _ in 1..DDC_ATTEMPTS {
                                    sleep(DDC_RETRY_GAP);
                                    if current.responsive() {
                                        r = true;
                                        break;
                                    }
                                }
                            }
                            if !r {
                                // Handles могли умереть вместе с линией — перечислим заново.
                                monitors = None;
                            }
                            r
                        }
                        // Перечислить мониторы не удалось: при погашенных экранах это норма,
                        // поэтому считаем опрос «молчащим», но решение примет счётчик.
                        None => false,
                    };

                    if responsive {
                        alive_streak += 1;
                        silent_streak = 0;
                    } else {
                        silent_streak += 1;
                        alive_streak = 0;
                    }

                    // Подсветку гасим только по устойчивой картине, а возвращаем быстрее:
                    // пусть лучше лишний раз погаснет, чем останется включённой при погасшем
                    // экране — но и мигать из-за сбоя DDC она не должна.
                    let silent = if silent_streak >= OFF_CONFIRM_POLLS {
                        true
                    } else if alive_streak >= ON_CONFIRM_POLLS {
                        false
                    } else {
                        MONITOR_SILENT.load(Ordering::Acquire)
                    };
                    MONITOR_SILENT.store(silent, Ordering::Release);
                }

                reconcile();
            }
            log("Поток наблюдения завершён");
        })
        .expect("не удалось запустить поток наблюдения");
    (tx, handle)
}

// ---------------------------------------------------------------------------
// Окно сообщений
// ---------------------------------------------------------------------------

/// Создаёт message-only окно и подписывается на все нужные уведомления.
fn create_message_window(hinstance: HINSTANCE) -> HWND {
    unsafe {
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
        // Класс регистрируется один раз; повторная регистрация просто вернёт ошибку.
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
            return hwnd;
        }

        for guid in [
            &GUID_CONSOLE_DISPLAY_STATE,
            &GUID_MONITOR_POWER_ON,
            &GUID_SESSION_DISPLAY_STATUS,
        ] {
            RegisterPowerSettingNotification(hwnd as _, guid, DEVICE_NOTIFY_WINDOW_HANDLE);
        }
        WTSRegisterSessionNotification(hwnd, NOTIFY_FOR_THIS_SESSION);
        hwnd
    }
}

unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_WTSSESSION_CHANGE => {
            match wparam as u32 {
                WTS_SESSION_LOCK => {
                    SESSION_LOCKED.store(true, Ordering::Release);
                    // Подсветку не трогаем: на экране ввода пароля она нужна.
                    log("Сеанс заблокирован — подсветка остаётся включённой");
                }
                WTS_SESSION_UNLOCK => {
                    SESSION_LOCKED.store(false, Ordering::Release);
                    log("Сеанс разблокирован");
                }
                _ => {}
            }
            0
        }
        WM_POWERBROADCAST => {
            match wparam as u32 {
                PBT_APMSUSPEND => {
                    DISPLAY_SLEEPING.store(true, Ordering::Release);
                    reconcile();
                }
                PBT_APMRESUMEAUTOMATIC => {
                    display_on();
                    reconcile();
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
                        // Решение принимается только по состоянию самого дисплея:
                        // session_display_status описывает доступность дисплея сеансу.
                        let is_display = guid_eq(&s.PowerSetting, &GUID_CONSOLE_DISPLAY_STATE)
                            || guid_eq(&s.PowerSetting, &GUID_MONITOR_POWER_ON);
                        if is_display {
                            match value {
                                0 => {
                                    DISPLAY_SLEEPING.store(true, Ordering::Release);
                                    reconcile();
                                }
                                1 => {
                                    display_on();
                                    reconcile();
                                }
                                _ => {
                                    // «Затемнён» — неоднозначно: погаснуть могли не все мониторы.
                                    // Будим наблюдение, пусть решает опрос DDC.
                                    nudge_watch();
                                }
                            }
                        }
                    }
                }
                _ => {}
            }
            1
        }
        WM_DISPLAYCHANGE => {
            log("Смена конфигурации дисплеев");
            RESET_MONITORS.store(true, Ordering::Release);
            MONITORS_GONE.store(GetSystemMetrics(SM_CMONITORS) == 0, Ordering::Release);
            log_display_topology();
            nudge_watch();
            reconcile();
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
/// `user32`, `combase` и прочие. После инициализации они не нужны. Это не освобождает
/// память системы (страницы общие и остаются резидентными для других процессов),
/// но снимает с процесса обязанность держать их в своём рабочем наборе.
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

    let (worker_tx, worker) = spawn_worker();
    let (watch_tx, watch) = spawn_watchdog();
    *WORKER_TX.lock().unwrap_or_else(|e| e.into_inner()) = Some(worker_tx.clone());
    *WATCH_TX.lock().unwrap_or_else(|e| e.into_inner()) = Some(watch_tx.clone());

    unsafe {
        let hinstance = windows_sys::Win32::System::LibraryLoader::GetModuleHandleW(ptr::null());
        let mut hwnd = create_message_window(hinstance);
        if hwnd.is_null() {
            log("Не удалось создать окно сообщений");
        } else {
            log(&format!("Окно сообщений создано: {hwnd:p}"));
        }

        log("Начальная топология");
        log_display_topology();
        MONITORS_GONE.store(GetSystemMetrics(SM_CMONITORS) == 0, Ordering::Release);
        reconcile();
        trim_working_set();

        // Цикл сообщений. Ожидание блокирующее, но с таймаутом: раз в секунду проверяем,
        // что окно живо. Если оно пропало, события питания перестали бы приходить вовсе,
        // поэтому окно пересоздаётся и подписки восстанавливаются.
        let mut running = true;
        while running {
            let wait = MsgWaitForMultipleObjectsEx(0, ptr::null(), HEALTH_CHECK_MS, QS_ALLINPUT, MWMO_INPUTAVAILABLE);
            if wait == WAIT_OBJECT_0 {
                loop {
                    let mut msg: MSG = std::mem::zeroed();
                    if PeekMessageW(&mut msg, ptr::null_mut(), 0, 0, PM_REMOVE) == 0 {
                        break;
                    }
                    if msg.message == WM_QUIT {
                        running = false;
                        break;
                    }
                    TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
            if !running {
                break;
            }
            if hwnd.is_null() || IsWindow(hwnd) == 0 {
                log("Окно сообщений пропало — создаю заново");
                hwnd = create_message_window(hinstance);
                if hwnd.is_null() {
                    log("Пересоздать окно не удалось");
                } else {
                    log(&format!("Окно сообщений создано: {hwnd:p}"));
                    // Состояние могло измениться, пока события не приходили.
                    nudge_watch();
                }
            }
        }

        if !hwnd.is_null() {
            WTSUnRegisterSessionNotification(hwnd);
        }
        CloseHandle(mutex);
    }

    // Закрываем каналы: потоки завершатся, поток HID вернёт подсветку.
    *WORKER_TX.lock().unwrap_or_else(|e| e.into_inner()) = None;
    *WATCH_TX.lock().unwrap_or_else(|e| e.into_inner()) = None;
    let _ = watch.join();
    let _ = worker.join();
    log("Остановка сервиса");
}
