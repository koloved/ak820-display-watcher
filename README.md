# ak820-display-watcher

A tiny headless Windows service that turns the backlight of an **AJAZZ AK820** keyboard
(`320F:505B`) off when the screen image goes away, and puts it back exactly as it was when the
screen returns.

No window, no console, no tray icon, no configuration files. One `.exe`, about 200 KB, ~1.9 MB
of resident memory and 0 ms of CPU when idle.

*Русское описание: [README.ru.md](README.ru.md)*

---

## Why this exists

If you keep a mechanical keyboard with per-key RGB, the backlight stays on all night while the
monitors are asleep. It lights up the whole room, wakes you up, and burns LEDs for nothing.

The obvious fix — "turn the backlight off when the session is locked" — is wrong for a different
reason. In the morning you come to a locked screen in a dark room and need to type a password on
a keyboard you cannot see. The lock screen is precisely the moment the backlight matters.

So the rule implemented here is narrower and more useful:

> **The keyboard is dark only when there is no image on the screen.**

Lock the workstation with `Win + L`: the backlight stays on, you can log in. Leave the PC and let
the monitor time out: everything goes dark. Move the mouse in the morning: the screen comes back
and the keyboard lights up with it, exactly in the mode, color and speed you had.

Turning the monitor off with its **physical power button** is a separate problem — Windows does
not report it at all (see [DDC/CI polling](#ddcci-polling)). This application detects it anyway.

## Behavior

| What happens | Backlight |
|---|---|
| Monitor timeout, DPMS off, system sleep | **off** |
| Wake by mouse/keyboard, display back on | **restored** |
| Monitor switched off with its physical button | **off** within ~11 s |
| Monitor switched back on | **restored** |
| All monitors physically disconnected | **off** |
| `Win + L` (session locked, screen still on) | **stays on** |
| Unlock | unchanged |

"Restored" means the exact previous state: the keyboard's 29-byte configuration block is read
before switching off and written back afterwards, so effect, brightness, speed, direction and
color all return as they were.

## Features

- **0% CPU when idle.** Events arrive through a blocking `GetMessageW` loop; no polling timer.
- **Single instance.** A named mutex stops a second copy from starting.
- **Silent.** Built with `#![windows_subsystem = "windows"]` — no console window flashes on start.
- **Survives unplugging.** A lost keyboard never crashes the service; it reconnects on the next
  event.
- **Logs to a file** next to the executable, capped at 1 MB.
- **Restores the backlight on exit**, so stopping the service never leaves you in the dark.

## Requirements

- Windows 10 or 11, x64.
- Rust toolchain with the MSVC target.
- Visual Studio Build Tools (or Visual Studio) — `hidapi` compiles a small C file.

## Build

```cmd
build.cmd
```

The script locates Visual Studio with `vswhere`, sets up `vcvars64.bat` and runs
`cargo build --release`.

Output: `target\release\ak820-display-watcher.exe`.

Building by hand works only from a Visual Studio environment. Without it the C part of `hidapi`
fails with MSVC error `D8050`. The **debug** profile does not build here at all, because `cc-rs`
adds `-Z7`; use `--release`.

## Run

Start `ak820-display-watcher.exe`. Nothing appears — it is a GUI-subsystem binary with no UI.
A log file is created next to it.

### Autostart

Run `autostart.bat` and pick:

```
[1] Enable autostart   (start with Windows)
[2] Disable autostart  (remove from startup)
```

It creates or removes a shortcut in your per-user Startup folder, so no administrator rights are
needed. The script also accepts `autostart.bat enable`, `autostart.bat disable` and
`autostart.bat status` for unattended use.

The shortcut appears in Task Manager under the *Startup* tab, where you can disable it later
without running the script.

## How it works

### Event sources

A hidden **message-only window** (`HWND_MESSAGE`) receives:

| Source | Purpose |
|---|---|
| `GUID_CONSOLE_DISPLAY_STATE` | display on/off (main signal) |
| `GUID_MONITOR_POWER_ON` | same, legacy equivalent |
| `GUID_SESSION_DISPLAY_STATUS` | log only — see the note below |
| `WTSRegisterSessionNotification` | lock/unlock, log only |
| `WM_DISPLAYCHANGE` | monitor topology changed |
| `WM_POWERBROADCAST` | system suspend/resume |

`HWND_MESSAGE` matters: an ordinary hidden window never gets its `WM_PAINT` validated and burns
a whole CPU core at 100%. A message-only window is not painted at all.

`GUID_SESSION_DISPLAY_STATUS` is deliberately **not** used for the decision. It reports whether
the display is available *to the session* and can stay `1` while the screen itself is already
off. Handling it like the others made the state flap — off, on, off within one second.

### State machine

Only three independent flags can switch the backlight off:

```rust
display_sleeping        // the OS says the display is off
monitors_disconnected   // GetSystemMetrics(SM_CMONITORS) == 0
monitor_unresponsive    // the monitor stopped answering DDC/CI
```

A command is sent **only when the combined result changes**, so a burst of related events causes
one write, not five.

`session_locked` is tracked but intentionally excluded from that condition — that is the whole
point of the project.

### DDC/CI polling

When you switch a monitor off with its own button, Windows reports **nothing**: no
`WM_DISPLAYCHANGE`, no power event, `SM_CMONITORS` unchanged, the monitor still counted as
attached. There is no event path for it.

The only remaining signal is that the monitor stops answering DDC/CI. A dedicated thread asks
each physical monitor for VCP code `0xD6` (power mode) every 10 seconds:

- monitor on → answers `1` in ~55 ms;
- monitor switched off → the request fails.

**False positives.** DDC/CI is noisy: on a healthy monitor, isolated failures appeared 6 times in
45 seconds. One failure therefore means nothing. The monitor is declared off only after 4
consecutive failures, 250 ms apart. Retries are cheap because they happen only on suspicion.

**Cost.** Measured: 15.6 ms of CPU over 115 seconds, including 90 seconds with the monitor off —
and all of it went into the single moment the switch-off was detected. In steady state the growth
is zero, because a failed DDC request returns instantly without burning CPU.

**Skipping work.** While the OS already reports the display as off, polling pauses: it would add
nothing. Monitor handles are re-created when the topology changes, so the service cannot get stuck
on a stale handle.

### Threads

| Thread | Job |
|---|---|
| main | window procedure, message loop |
| `hid` | keyboard I/O, blocks on a channel |
| `ddc` | DDC/CI polling, blocks on `recv_timeout` |

All three sleep in system calls when nothing happens. Stack size is set to 128 KB instead of the
2 MB default.

`HidApi::new()` (HID enumeration) takes about **10 seconds** on a busy system, while opening the
device itself takes ~5 ms. Connection is therefore established at startup in the worker thread,
never at the moment the screen goes dark — otherwise the command would arrive after the screen was
already back.

## Keyboard protocol

The AK820 exposes a vendor HID interface at `Usage Page 0xFF1C / Usage 0x0092` (interface 1).
A packet is 64 bytes, including the report ID:

| Byte | Meaning |
|------|---------|
| 0 | `0x04` — report ID |
| 1–2 | checksum: 16-bit sum of bytes 3..63, little-endian |
| 3 | command code |
| 4 | payload length |
| 5–6 | offset, little-endian |
| 7 | `0x00` |
| 8..63 | payload, up to 56 bytes |

Commands: `BEGIN` `0x01`, `END` `0x02`, `READ_CONFIG` `0x05`, `WRITE_CONFIG` `0x06`.

The configuration block is 29 bytes; byte 2 is brightness (`0x00` = off, `0x04` = maximum), byte 1
is the effect, byte 5 tells whether a rainbow color cycle is used, bytes 6–8 are RGB.

Every packet is acknowledged: after `write` the code performs `read_timeout` with a 1000 ms
timeout. Byte 7 of the reply equal to `0xFF` or `0xFE` means the keyboard rejected the command.

Writing the configuration is a sequence: `BEGIN` → `WRITE_CONFIG` → 10 ms pause → `END`.

## Diagnostics

```cmd
cargo run --release --example probe
```

Opens the keyboard, prints every HID interface with its usage page, reads the configuration,
switches the backlight off for two seconds and restores the original block. Useful to check the
protocol on a new revision of the hardware.

```cmd
cargo run --release --example ddc -- 45
```

Polls the monitor over DDC/CI for 45 seconds and prints the power state. Press the monitor button
while it runs: if the state stops answering, this monitor supports detection.

The technical specification (in Russian) is in [TA.txt](TA.txt).

## Memory

Executable size and memory use are unrelated: the process maps system DLLs (`SETUPAPI`, `user32`,
`combase`, `ntdll` …) whose combined image here is ~31 MB. Those pages are **shared** and already
resident for other processes.

| Metric | Value |
|---|---|
| Working set (Task Manager) | ~1.9 MB |
| Private memory | ~2.4 MB |
| CPU when idle | 0 ms |

What produced the numbers:

- **Working-set trim** (`SetProcessWorkingSetSizeEx`) after initialization and after every
  command: 9.2 MB → 1.9 MB. It evicts pages the process no longer touches. This does not free
  system memory — the pages are shared — they are simply faulted back in on next use.
- **Thread stacks** reduced from 2 MB to 128 KB.
- **The HID device list is dropped** after opening the keyboard: `HidApi` keeps an enumeration of
  every HID device in the system, while the opened `HidDevice` does not depend on it.

The 2.4 MB of private memory is the floor without giving up HID enumeration and a message window.

If `RTSSHooks64.dll` appears in the module list, that is RivaTuner / MSI Afterburner injecting
itself; it adds ~2.5 MB and has nothing to do with this application. Add the exe to its exclusion
list.

## Scope

Written and tested against the **AK820 wired** (`320F:505B`). The `AK820 Pro` uses a different
controller and protocol and is out of scope.

The service is a plain executable, not a Windows service: it runs in your session, which is what
the power and session notifications require.

## License

MIT
