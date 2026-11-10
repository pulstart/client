#[cfg(target_os = "linux")]
use std::process::Command;

/// Refresh rate for a stream: the env override, else the rate of the monitor
/// the window is known to be on, else the fastest of all monitors (the window
/// may be on any of them), else whatever a probe finds.
pub fn refresh_for_monitor(monitor: Option<&LocalMonitor>, all: &[LocalMonitor]) -> Option<u32> {
    env_override_refresh_millihz()
        .or_else(|| monitor.and_then(|m| m.refresh_millihz))
        .or_else(|| all.iter().filter_map(|m| m.refresh_millihz).max())
        .or_else(detect_platform_refresh_millihz)
}

pub fn desired_present_refresh_millihz(
    display_refresh_millihz: Option<u32>,
    stream_fps: u16,
) -> Option<u32> {
    env_override_present_refresh_millihz().or_else(|| {
        let stream_refresh = normalize_refresh_millihz(u32::from(stream_fps).saturating_mul(1000))?;
        Some(
            display_refresh_millihz
                .map(|display_refresh| display_refresh.min(stream_refresh))
                .unwrap_or(stream_refresh),
        )
    })
}

pub fn vsync_enabled() -> bool {
    env_override_vsync().unwrap_or(cfg!(target_os = "macos"))
}

/// `ST_CLIENT_VSYNC` pins vsync; the renderer then leaves it alone.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn vsync_forced() -> bool {
    env_override_vsync().is_some()
}

fn env_override_refresh_millihz() -> Option<u32> {
    if let Ok(value) = std::env::var("ST_CLIENT_REFRESH_MILLIHZ") {
        if let Ok(parsed) = value.parse::<u32>() {
            return normalize_refresh_millihz(parsed);
        }
    }

    std::env::var("ST_CLIENT_FPS")
        .ok()
        .and_then(|value| value.parse::<f64>().ok())
        .and_then(normalize_refresh_hz)
}

fn env_override_present_refresh_millihz() -> Option<u32> {
    if let Ok(value) = std::env::var("ST_CLIENT_PRESENT_MILLIHZ") {
        if let Ok(parsed) = value.parse::<u32>() {
            return normalize_refresh_millihz(parsed);
        }
    }

    std::env::var("ST_CLIENT_PRESENT_FPS")
        .ok()
        .and_then(|value| value.parse::<f64>().ok())
        .and_then(normalize_refresh_hz)
}

fn env_override_vsync() -> Option<bool> {
    std::env::var("ST_CLIENT_VSYNC").ok().and_then(|value| {
        match value.trim().to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => Some(true),
            "0" | "false" | "no" | "off" => Some(false),
            _ => None,
        }
    })
}

#[cfg(target_os = "linux")]
fn detect_platform_refresh_millihz() -> Option<u32> {
    detect_linux_refresh_millihz()
}

#[cfg(target_os = "macos")]
fn detect_platform_refresh_millihz() -> Option<u32> {
    detect_macos_refresh_millihz()
}

#[cfg(target_os = "windows")]
fn detect_platform_refresh_millihz() -> Option<u32> {
    detect_windows_refresh_millihz()
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
fn detect_platform_refresh_millihz() -> Option<u32> {
    None
}

fn normalize_refresh_millihz(value: u32) -> Option<u32> {
    if (20_000..=360_000).contains(&value) {
        Some(value)
    } else {
        None
    }
}

fn normalize_refresh_hz(value: f64) -> Option<u32> {
    if !(20.0..=360.0).contains(&value) {
        return None;
    }

    normalize_refresh_millihz((value * 1000.0).round() as u32)
}

#[cfg(target_os = "linux")]
fn run_command(program: &str, args: &[&str]) -> Option<String> {
    let output = Command::new(program).args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(any(target_os = "linux", test))]
fn parse_rate_token_hz(token: &str) -> Option<u32> {
    let value: String = token
        .chars()
        .filter(|ch| ch.is_ascii_digit() || *ch == '.')
        .collect();
    if value.is_empty() {
        return None;
    }
    value.parse::<f64>().ok().and_then(normalize_refresh_hz)
}

#[cfg(any(target_os = "linux", test))]
fn parse_lines_with_current_hz(text: &str) -> Option<u32> {
    let mut best: Option<u32> = None;
    for line in text.lines() {
        if !line.contains("current") || !line.contains("Hz") {
            continue;
        }

        let parts: Vec<&str> = line.split_whitespace().collect();
        for (idx, chunk) in parts.iter().enumerate() {
            if chunk.eq_ignore_ascii_case("Hz") {
                if let Some(prev) = idx.checked_sub(1).and_then(|prev| parts.get(prev)) {
                    if let Some(rate) = parse_rate_token_hz(prev) {
                        best = Some(best.map_or(rate, |current| current.max(rate)));
                    }
                }
            } else if chunk.contains("Hz") {
                let trimmed = chunk.trim_end_matches("Hz").trim_end_matches("hz");
                if let Some(rate) = parse_rate_token_hz(trimmed) {
                    best = Some(best.map_or(rate, |current| current.max(rate)));
                }
            }
        }
    }
    best
}

#[cfg(any(target_os = "linux", test))]
fn parse_named_rate_values(text: &str, key: &str) -> Option<u32> {
    let mut best: Option<u32> = None;
    let mut rest = text;

    while let Some(idx) = rest.find(key) {
        let after_key = &rest[idx + key.len()..];
        let colon = match after_key.find(':') {
            Some(colon) => colon,
            None => break,
        };
        let mut number = String::new();
        let mut started = false;
        for ch in after_key[colon + 1..].chars() {
            if ch.is_ascii_digit() || ch == '.' {
                number.push(ch);
                started = true;
            } else if started {
                break;
            }
        }

        if let Ok(value) = number.parse::<f64>() {
            let rate = if value > 1000.0 {
                normalize_refresh_millihz(value.round() as u32)
            } else {
                normalize_refresh_hz(value)
            };
            if let Some(rate) = rate {
                best = Some(best.map_or(rate, |current| current.max(rate)));
            }
        }

        rest = &after_key[colon + 1..];
    }

    best
}

#[cfg(target_os = "linux")]
fn detect_linux_refresh_millihz() -> Option<u32> {
    detect_xrandr_refresh_millihz()
        .or_else(detect_hyprland_refresh_millihz)
        .or_else(detect_sway_refresh_millihz)
        .or_else(detect_wlr_randr_refresh_millihz)
        .or_else(detect_wayland_info_refresh_millihz)
}

#[cfg(target_os = "linux")]
fn detect_xrandr_refresh_millihz() -> Option<u32> {
    let output = run_command("xrandr", &["--current"])?;
    let mut best: Option<u32> = None;

    for line in output.lines() {
        if !line.contains('*') {
            continue;
        }

        for token in line.split_whitespace() {
            if token.contains('*') {
                if let Some(rate) = parse_rate_token_hz(token) {
                    best = Some(best.map_or(rate, |current| current.max(rate)));
                }
            }
        }
    }

    best
}

#[cfg(target_os = "linux")]
fn detect_hyprland_refresh_millihz() -> Option<u32> {
    let output = run_command("hyprctl", &["-j", "monitors"])?;
    parse_named_rate_values(&output, "\"refreshRate\"")
}

#[cfg(target_os = "linux")]
fn detect_sway_refresh_millihz() -> Option<u32> {
    let output = run_command("swaymsg", &["-t", "get_outputs", "-r"])?;
    parse_named_rate_values(&output, "\"refresh\"")
}

#[cfg(target_os = "linux")]
fn detect_wlr_randr_refresh_millihz() -> Option<u32> {
    let output = run_command("wlr-randr", &[])?;
    parse_lines_with_current_hz(&output)
}

#[cfg(target_os = "linux")]
fn detect_wayland_info_refresh_millihz() -> Option<u32> {
    let output = run_command("wayland-info", &[])?;
    parse_named_rate_values(&output, "refresh")
}

#[cfg(target_os = "macos")]
fn detect_macos_refresh_millihz() -> Option<u32> {
    use std::ffi::c_void;

    type CGDirectDisplayID = u32;
    type CGDisplayModeRef = *const c_void;
    type CGError = i32;

    #[link(name = "CoreGraphics", kind = "framework")]
    extern "C" {
        fn CGGetActiveDisplayList(
            max_displays: u32,
            active_displays: *mut CGDirectDisplayID,
            display_count: *mut u32,
        ) -> CGError;
        fn CGDisplayCopyDisplayMode(display: CGDirectDisplayID) -> CGDisplayModeRef;
        fn CGDisplayModeGetRefreshRate(mode: CGDisplayModeRef) -> f64;
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        fn CFRelease(cf: *const c_void);
    }

    let mut count = 0u32;
    if unsafe { CGGetActiveDisplayList(0, std::ptr::null_mut(), &mut count) } != 0 || count == 0 {
        return None;
    }

    let mut displays = vec![0u32; count as usize];
    if unsafe { CGGetActiveDisplayList(count, displays.as_mut_ptr(), &mut count) } != 0 {
        return None;
    }

    let mut best: Option<u32> = None;
    for display in displays.into_iter().take(count as usize) {
        let mode = unsafe { CGDisplayCopyDisplayMode(display) };
        if mode.is_null() {
            continue;
        }
        let refresh = unsafe { CGDisplayModeGetRefreshRate(mode) };
        unsafe {
            CFRelease(mode);
        }
        if let Some(rate) = normalize_refresh_hz(refresh) {
            best = Some(best.map_or(rate, |current: u32| current.max(rate)));
        }
    }

    best
}

/// The one place that declares `EnumDisplaySettingsW`.
#[cfg(target_os = "windows")]
mod windows_modes {
    use std::mem::size_of;

    const ENUM_CURRENT_SETTINGS: u32 = 0xFFFF_FFFF;

    #[repr(C)]
    struct DevModeW {
        device_name: [u16; 32],
        spec_version: u16,
        driver_version: u16,
        size: u16,
        driver_extra: u16,
        fields: u32,
        union1: [u8; 16],
        color: i16,
        duplex: i16,
        y_resolution: i16,
        tt_option: i16,
        collate: i16,
        form_name: [u16; 32],
        log_pixels: u16,
        bits_per_pel: u32,
        pels_width: u32,
        pels_height: u32,
        union2: [u8; 4],
        display_frequency: u32,
        icm_method: u32,
        icm_intent: u32,
        media_type: u32,
        dither_type: u32,
        reserved1: u32,
        reserved2: u32,
        panning_width: u32,
        panning_height: u32,
    }

    #[link(name = "user32")]
    extern "system" {
        fn EnumDisplaySettingsW(
            device_name: *const u16,
            mode_num: u32,
            dev_mode: *mut DevModeW,
        ) -> i32;
    }

    /// Current refresh rate in Hz of the display device named `device_name`.
    pub fn current_refresh_hz(device_name: &[u16; 32]) -> Option<f64> {
        let mut mode = DevModeW {
            device_name: [0; 32],
            spec_version: 0,
            driver_version: 0,
            size: size_of::<DevModeW>() as u16,
            driver_extra: 0,
            fields: 0,
            union1: [0; 16],
            color: 0,
            duplex: 0,
            y_resolution: 0,
            tt_option: 0,
            collate: 0,
            form_name: [0; 32],
            log_pixels: 0,
            bits_per_pel: 0,
            pels_width: 0,
            pels_height: 0,
            union2: [0; 4],
            display_frequency: 0,
            icm_method: 0,
            icm_intent: 0,
            media_type: 0,
            dither_type: 0,
            reserved1: 0,
            reserved2: 0,
            panning_width: 0,
            panning_height: 0,
        };
        let ok =
            unsafe { EnumDisplaySettingsW(device_name.as_ptr(), ENUM_CURRENT_SETTINGS, &mut mode) };
        (ok != 0).then_some(f64::from(mode.display_frequency))
    }
}

#[cfg(target_os = "windows")]
fn detect_windows_refresh_millihz() -> Option<u32> {
    use std::mem::size_of;

    const DISPLAY_DEVICE_ACTIVE: u32 = 0x0000_0001;

    #[repr(C)]
    struct DisplayDeviceW {
        cb: u32,
        device_name: [u16; 32],
        device_string: [u16; 128],
        state_flags: u32,
        device_id: [u16; 128],
        device_key: [u16; 128],
    }

    #[link(name = "user32")]
    extern "system" {
        fn EnumDisplayDevicesW(
            device: *const u16,
            dev_num: u32,
            display_device: *mut DisplayDeviceW,
            flags: u32,
        ) -> i32;
    }

    let mut best: Option<u32> = None;
    let mut index = 0u32;

    loop {
        let mut display = DisplayDeviceW {
            cb: size_of::<DisplayDeviceW>() as u32,
            device_name: [0; 32],
            device_string: [0; 128],
            state_flags: 0,
            device_id: [0; 128],
            device_key: [0; 128],
        };
        let ok = unsafe { EnumDisplayDevicesW(std::ptr::null(), index, &mut display, 0) };
        if ok == 0 {
            break;
        }
        index += 1;

        if display.state_flags & DISPLAY_DEVICE_ACTIVE == 0 {
            continue;
        }

        let Some(hz) = windows_modes::current_refresh_hz(&display.device_name) else {
            continue;
        };

        if let Some(rate) = normalize_refresh_hz(hz) {
            best = Some(best.map_or(rate, |current| current.max(rate)));
        }
    }

    best
}

/// One monitor of the local desktop, as the window system lays it out.
#[derive(Clone, Debug, PartialEq)]
pub struct LocalMonitor {
    pub name: String,
    /// Top-left corner in the desktop's coordinate space (scaled units on
    /// compositors that scale, pixels elsewhere).
    pub x: i32,
    pub y: i32,
    /// Upright pixel size.
    pub width: u32,
    pub height: u32,
    pub refresh_millihz: Option<u32>,
    pub scale: f32,
    pub primary: bool,
}

impl LocalMonitor {
    /// Size in desktop coordinate units (pixels divided by `scale`).
    pub fn logical_size(&self) -> (f32, f32) {
        let scale = if self.scale > 0.0 { self.scale } else { 1.0 };
        (self.width as f32 / scale, self.height as f32 / scale)
    }

    pub fn contains(&self, x: f32, y: f32) -> bool {
        let (w, h) = self.logical_size();
        x >= self.x as f32 && x < self.x as f32 + w && y >= self.y as f32 && y < self.y as f32 + h
    }
}

/// Monitors left to right, then top to bottom.
pub fn sort_monitors(monitors: &mut [LocalMonitor]) {
    monitors.sort_by(|a, b| (a.x, a.y, &a.name).cmp(&(b.x, b.y, &b.name)));
}

/// The monitor flagged primary, else the first one.
pub fn primary_monitor(monitors: &[LocalMonitor]) -> Option<usize> {
    monitors
        .iter()
        .position(|m| m.primary)
        .or((!monitors.is_empty()).then_some(0))
}

/// The monitor under a point in desktop coordinates.
pub fn monitor_at(monitors: &[LocalMonitor], x: f32, y: f32) -> Option<usize> {
    monitors.iter().position(|m| m.contains(x, y))
}

pub fn list_local_monitors() -> Vec<LocalMonitor> {
    let mut monitors = platform_monitors();
    sort_monitors(&mut monitors);
    monitors
}

/// Whether the app can choose where its windows open. Wayland compositors
/// place toplevel windows themselves.
pub fn can_place_windows() -> bool {
    #[cfg(target_os = "linux")]
    {
        let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some_and(|v| !v.is_empty());
        let forced_x11 = std::env::var("WINIT_UNIX_BACKEND").is_ok_and(|v| v == "x11");
        !wayland || forced_x11
    }
    #[cfg(not(target_os = "linux"))]
    {
        true
    }
}

#[cfg(target_os = "linux")]
fn platform_monitors() -> Vec<LocalMonitor> {
    run_command("kscreen-doctor", &["-j"])
        .map(|out| parse_kscreen_monitors(&out))
        .filter(|m| !m.is_empty())
        .or_else(|| {
            run_command("hyprctl", &["-j", "monitors"])
                .map(|out| parse_hyprland_monitors(&out))
                .filter(|m| !m.is_empty())
        })
        .or_else(|| {
            run_command("swaymsg", &["-t", "get_outputs", "-r"])
                .map(|out| parse_sway_monitors(&out))
                .filter(|m| !m.is_empty())
        })
        .or_else(|| {
            run_command("wlr-randr", &[])
                .map(|out| parse_wlr_randr_monitors(&out))
                .filter(|m| !m.is_empty())
        })
        .or_else(|| {
            run_command("xrandr", &["--current"])
                .map(|out| parse_xrandr_monitors(&out))
                .filter(|m| !m.is_empty())
        })
        .unwrap_or_default()
}

#[cfg(any(target_os = "linux", test))]
fn json_i32(value: &serde_json::Value) -> Option<i32> {
    value.as_f64().map(|v| v.round() as i32)
}

#[cfg(any(target_os = "linux", test))]
fn json_millihz(value: &serde_json::Value) -> Option<u32> {
    let hz = value.as_f64()?;
    // sway reports millihertz, the others hertz.
    if hz > 1000.0 {
        normalize_refresh_millihz(hz.round() as u32)
    } else {
        normalize_refresh_hz(hz)
    }
}

/// `kscreen-doctor -j`: KDE, X11 and Wayland. `pos` is in scaled desktop
/// units, `priority == 1` is the primary output, and `rotation` is Qt's
/// bitmask (2 = left, 8 = right turn the mode sideways).
#[cfg(any(target_os = "linux", test))]
fn parse_kscreen_monitors(text: &str) -> Vec<LocalMonitor> {
    let Ok(root) = serde_json::from_str::<serde_json::Value>(text) else {
        return Vec::new();
    };
    let Some(outputs) = root["outputs"].as_array() else {
        return Vec::new();
    };
    let live: Vec<&serde_json::Value> = outputs
        .iter()
        .filter(|o| o["enabled"].as_bool() == Some(true) && o["connected"].as_bool() != Some(false))
        .collect();
    let primary_priority = live.iter().filter_map(|o| o["priority"].as_i64()).min();
    live.into_iter()
        .filter_map(|o| {
            let current = o["currentModeId"].as_str()?;
            let mode = o["modes"]
                .as_array()?
                .iter()
                .find(|m| m["id"].as_str() == Some(current))?;
            let (mut width, mut height) = (
                mode["size"]["width"].as_u64()? as u32,
                mode["size"]["height"].as_u64()? as u32,
            );
            if matches!(o["rotation"].as_i64(), Some(2 | 8)) {
                std::mem::swap(&mut width, &mut height);
            }
            Some(LocalMonitor {
                name: o["name"].as_str()?.to_string(),
                x: json_i32(&o["pos"]["x"])?,
                y: json_i32(&o["pos"]["y"])?,
                width,
                height,
                refresh_millihz: json_millihz(&mode["refreshRate"]),
                scale: o["scale"].as_f64().unwrap_or(1.0) as f32,
                primary: o["priority"].as_i64().is_some()
                    && o["priority"].as_i64() == primary_priority,
            })
        })
        .collect()
}

/// `hyprctl -j monitors`: `transform` 1, 3, 5, 7 turn the mode sideways.
#[cfg(any(target_os = "linux", test))]
fn parse_hyprland_monitors(text: &str) -> Vec<LocalMonitor> {
    let Ok(root) = serde_json::from_str::<serde_json::Value>(text) else {
        return Vec::new();
    };
    root.as_array()
        .into_iter()
        .flatten()
        .filter(|o| o["disabled"].as_bool() != Some(true))
        .filter_map(|o| {
            let (mut width, mut height) =
                (o["width"].as_u64()? as u32, o["height"].as_u64()? as u32);
            if o["transform"].as_i64().unwrap_or(0) & 1 == 1 {
                std::mem::swap(&mut width, &mut height);
            }
            Some(LocalMonitor {
                name: o["name"].as_str()?.to_string(),
                x: json_i32(&o["x"])?,
                y: json_i32(&o["y"])?,
                width,
                height,
                refresh_millihz: json_millihz(&o["refreshRate"]),
                scale: o["scale"].as_f64().unwrap_or(1.0) as f32,
                primary: false,
            })
        })
        .collect()
}

/// `swaymsg -t get_outputs -r`: `rect` is scaled and already rotated, the
/// pixel size comes from the current mode.
#[cfg(any(target_os = "linux", test))]
fn parse_sway_monitors(text: &str) -> Vec<LocalMonitor> {
    let Ok(root) = serde_json::from_str::<serde_json::Value>(text) else {
        return Vec::new();
    };
    root.as_array()
        .into_iter()
        .flatten()
        .filter(|o| o["active"].as_bool() != Some(false))
        .filter_map(|o| {
            let mode = &o["current_mode"];
            let (mut width, mut height) = (
                mode["width"].as_u64()? as u32,
                mode["height"].as_u64()? as u32,
            );
            if matches!(o["transform"].as_str(), Some(t) if t.contains("90") || t.contains("270")) {
                std::mem::swap(&mut width, &mut height);
            }
            Some(LocalMonitor {
                name: o["name"].as_str()?.to_string(),
                x: json_i32(&o["rect"]["x"])?,
                y: json_i32(&o["rect"]["y"])?,
                width,
                height,
                refresh_millihz: json_millihz(&mode["refresh"]),
                scale: o["scale"].as_f64().unwrap_or(1.0) as f32,
                primary: false,
            })
        })
        .collect()
}

/// `wlr-randr` text output: one unindented header line per output.
#[cfg(any(target_os = "linux", test))]
fn parse_wlr_randr_monitors(text: &str) -> Vec<LocalMonitor> {
    struct Block {
        name: String,
        enabled: bool,
        mode: Option<(u32, u32, Option<u32>)>,
        position: (i32, i32),
        sideways: bool,
        scale: f32,
    }
    fn finish(block: Option<Block>, out: &mut Vec<LocalMonitor>) {
        let Some(block) = block else { return };
        let Some((mut width, mut height, refresh_millihz)) = block.mode else {
            return;
        };
        if !block.enabled {
            return;
        }
        if block.sideways {
            std::mem::swap(&mut width, &mut height);
        }
        out.push(LocalMonitor {
            name: block.name,
            x: block.position.0,
            y: block.position.1,
            width,
            height,
            refresh_millihz,
            scale: block.scale,
            primary: false,
        });
    }

    let mut out = Vec::new();
    let mut block: Option<Block> = None;
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        if !line.starts_with(char::is_whitespace) {
            finish(block.take(), &mut out);
            block = line.split_whitespace().next().map(|name| Block {
                name: name.to_string(),
                enabled: true,
                mode: None,
                position: (0, 0),
                sideways: false,
                scale: 1.0,
            });
            continue;
        }
        let Some(block) = block.as_mut() else {
            continue;
        };
        let trimmed = line.trim();
        if let Some(value) = trimmed.strip_prefix("Enabled:") {
            block.enabled = value.trim() == "yes";
        } else if let Some(value) = trimmed.strip_prefix("Position:") {
            let mut parts = value.trim().split(',');
            if let (Some(x), Some(y)) = (
                parts.next().and_then(|v| v.trim().parse().ok()),
                parts.next().and_then(|v| v.trim().parse().ok()),
            ) {
                block.position = (x, y);
            }
        } else if let Some(value) = trimmed.strip_prefix("Transform:") {
            let value = value.trim();
            block.sideways = value.contains("90") || value.contains("270");
        } else if let Some(value) = trimmed.strip_prefix("Scale:") {
            block.scale = value.trim().parse().unwrap_or(1.0);
        } else if trimmed.contains("current") && trimmed.contains(" px,") {
            let mut tokens = trimmed.split_whitespace();
            let size = tokens.next().and_then(|t| t.split_once('x'));
            if let Some((w, h)) = size {
                if let (Ok(w), Ok(h)) = (w.parse(), h.parse()) {
                    block.mode = Some((
                        w,
                        h,
                        parse_lines_with_current_hz(&format!("current {trimmed}")),
                    ));
                }
            }
        }
    }
    finish(block, &mut out);
    out
}

/// `xrandr --current`: the geometry is already rotated; the active refresh
/// rate is the starred token of the mode line under a connected output.
#[cfg(any(target_os = "linux", test))]
fn parse_xrandr_monitors(text: &str) -> Vec<LocalMonitor> {
    fn parse_geometry(token: &str) -> Option<(u32, u32, i32, i32)> {
        let (size, rest) = token.split_once('+')?;
        let (w, h) = size.split_once('x')?;
        let (x, y) = rest.split_once('+')?;
        Some((
            w.parse().ok()?,
            h.parse().ok()?,
            x.parse().ok()?,
            y.parse().ok()?,
        ))
    }

    let mut out: Vec<LocalMonitor> = Vec::new();
    let mut collecting = false;
    for line in text.lines() {
        if !line.starts_with(char::is_whitespace) {
            collecting = false;
            let mut tokens = line.split_whitespace();
            let (Some(name), Some("connected")) = (tokens.next(), tokens.next()) else {
                continue;
            };
            let rest: Vec<&str> = tokens.collect();
            let primary = rest.first() == Some(&"primary");
            let Some((width, height, x, y)) = rest.iter().find_map(|t| parse_geometry(t)) else {
                continue;
            };
            out.push(LocalMonitor {
                name: name.to_string(),
                x,
                y,
                width,
                height,
                refresh_millihz: None,
                scale: 1.0,
                primary,
            });
            collecting = true;
        } else if collecting {
            if let Some(monitor) = out.last_mut() {
                if monitor.refresh_millihz.is_none() {
                    monitor.refresh_millihz = line
                        .split_whitespace()
                        .skip(1)
                        .find(|token| token.contains('*'))
                        .and_then(parse_rate_token_hz);
                }
            }
        }
    }
    out
}

#[cfg(target_os = "macos")]
fn platform_monitors() -> Vec<LocalMonitor> {
    use std::ffi::c_void;

    type CGDirectDisplayID = u32;
    type CGDisplayModeRef = *const c_void;
    type CGError = i32;

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct CGPoint {
        x: f64,
        y: f64,
    }
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct CGSize {
        width: f64,
        height: f64,
    }
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct CGRect {
        origin: CGPoint,
        size: CGSize,
    }

    #[link(name = "CoreGraphics", kind = "framework")]
    extern "C" {
        fn CGGetActiveDisplayList(
            max_displays: u32,
            active_displays: *mut CGDirectDisplayID,
            display_count: *mut u32,
        ) -> CGError;
        fn CGDisplayBounds(display: CGDirectDisplayID) -> CGRect;
        fn CGDisplayPixelsWide(display: CGDirectDisplayID) -> usize;
        fn CGDisplayPixelsHigh(display: CGDirectDisplayID) -> usize;
        fn CGDisplayIsMain(display: CGDirectDisplayID) -> u32;
        fn CGDisplayUnitNumber(display: CGDirectDisplayID) -> u32;
        fn CGDisplayCopyDisplayMode(display: CGDirectDisplayID) -> CGDisplayModeRef;
        fn CGDisplayModeGetRefreshRate(mode: CGDisplayModeRef) -> f64;
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        fn CFRelease(cf: *const c_void);
    }

    let mut count = 0u32;
    if unsafe { CGGetActiveDisplayList(0, std::ptr::null_mut(), &mut count) } != 0 || count == 0 {
        return Vec::new();
    }
    let mut displays = vec![0u32; count as usize];
    if unsafe { CGGetActiveDisplayList(count, displays.as_mut_ptr(), &mut count) } != 0 {
        return Vec::new();
    }
    displays
        .into_iter()
        .take(count as usize)
        .map(|display| {
            // Bounds are in points; the pixel size is what the stream must match.
            let bounds = unsafe { CGDisplayBounds(display) };
            let width = unsafe { CGDisplayPixelsWide(display) } as u32;
            let height = unsafe { CGDisplayPixelsHigh(display) } as u32;
            let refresh_millihz = {
                let mode = unsafe { CGDisplayCopyDisplayMode(display) };
                if mode.is_null() {
                    None
                } else {
                    let refresh = unsafe { CGDisplayModeGetRefreshRate(mode) };
                    unsafe { CFRelease(mode) };
                    normalize_refresh_hz(refresh)
                }
            };
            let scale = if bounds.size.width > 0.0 {
                (f64::from(width) / bounds.size.width) as f32
            } else {
                1.0
            };
            LocalMonitor {
                name: format!("Display {}", unsafe { CGDisplayUnitNumber(display) } + 1),
                x: bounds.origin.x.round() as i32,
                y: bounds.origin.y.round() as i32,
                width,
                height,
                refresh_millihz,
                scale,
                primary: unsafe { CGDisplayIsMain(display) } != 0,
            }
        })
        .collect()
}

#[cfg(target_os = "windows")]
fn platform_monitors() -> Vec<LocalMonitor> {
    use std::ffi::c_void;
    use std::mem::size_of;

    const MONITORINFOF_PRIMARY: u32 = 1;

    #[repr(C)]
    #[derive(Default, Clone, Copy)]
    struct Rect {
        left: i32,
        top: i32,
        right: i32,
        bottom: i32,
    }

    #[repr(C)]
    struct MonitorInfoExW {
        size: u32,
        monitor: Rect,
        work: Rect,
        flags: u32,
        device: [u16; 32],
    }

    type MonitorEnumProc =
        unsafe extern "system" fn(*mut c_void, *mut c_void, *mut Rect, isize) -> i32;

    #[link(name = "user32")]
    extern "system" {
        fn EnumDisplayMonitors(
            hdc: *mut c_void,
            clip: *const Rect,
            callback: MonitorEnumProc,
            data: isize,
        ) -> i32;
        fn GetMonitorInfoW(monitor: *mut c_void, info: *mut MonitorInfoExW) -> i32;
    }

    unsafe extern "system" fn collect(
        monitor: *mut c_void,
        _hdc: *mut c_void,
        _rect: *mut Rect,
        data: isize,
    ) -> i32 {
        let monitors = &mut *(data as *mut Vec<LocalMonitor>);
        let mut info = MonitorInfoExW {
            size: size_of::<MonitorInfoExW>() as u32,
            monitor: Rect::default(),
            work: Rect::default(),
            flags: 0,
            device: [0; 32],
        };
        if GetMonitorInfoW(monitor, &mut info) == 0 {
            return 1;
        }
        let refresh_millihz =
            windows_modes::current_refresh_hz(&info.device).and_then(normalize_refresh_hz);
        let len = info
            .device
            .iter()
            .position(|c| *c == 0)
            .unwrap_or(info.device.len());
        monitors.push(LocalMonitor {
            name: String::from_utf16_lossy(&info.device[..len]),
            x: info.monitor.left,
            y: info.monitor.top,
            width: (info.monitor.right - info.monitor.left).max(0) as u32,
            height: (info.monitor.bottom - info.monitor.top).max(0) as u32,
            refresh_millihz,
            scale: 1.0,
            primary: info.flags & MONITORINFOF_PRIMARY != 0,
        });
        1
    }

    let mut monitors: Vec<LocalMonitor> = Vec::new();
    unsafe {
        EnumDisplayMonitors(
            std::ptr::null_mut(),
            std::ptr::null(),
            collect,
            &mut monitors as *mut Vec<LocalMonitor> as isize,
        );
    }
    monitors
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
fn platform_monitors() -> Vec<LocalMonitor> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::{
        monitor_at, parse_hyprland_monitors, parse_kscreen_monitors, parse_lines_with_current_hz,
        parse_named_rate_values, parse_rate_token_hz, parse_sway_monitors,
        parse_wlr_randr_monitors, parse_xrandr_monitors, primary_monitor, refresh_for_monitor,
        sort_monitors, LocalMonitor,
    };

    /// `kscreen-doctor -j` from a two-monitor KDE Wayland desktop, modes
    /// trimmed to the current one plus a spare: DP-2 landscape and primary,
    /// HDMI-A-1 turned 270 degrees (rotation 8).
    const KSCREEN_TWO_MONITORS: &str = r#"{
        "outputs": [
            {"connected": true, "currentModeId": "1", "enabled": true, "id": 1, "name": "DP-2",
             "pos": {"x": 0, "y": 579}, "priority": 1, "rotation": 1, "scale": 1,
             "size": {"height": 1440, "width": 2560},
             "modes": [
                {"id": "1", "name": "2560x1440@144", "refreshRate": 144, "size": {"height": 1440, "width": 2560}},
                {"id": "10", "name": "1600x900@60", "refreshRate": 60, "size": {"height": 900, "width": 1600}}]},
            {"connected": true, "currentModeId": "27", "enabled": true, "id": 2, "name": "HDMI-A-1",
             "pos": {"x": 2560, "y": 0}, "priority": 2, "rotation": 8, "scale": 1,
             "size": {"height": 2560, "width": 1440},
             "modes": [
                {"id": "27", "name": "2560x1440@75", "refreshRate": 74.96800231933594, "size": {"height": 1440, "width": 2560}}]},
            {"connected": true, "currentModeId": "1", "enabled": false, "id": 3, "name": "DP-3",
             "pos": {"x": 0, "y": 0}, "priority": 0, "rotation": 1, "scale": 1,
             "modes": [{"id": "1", "refreshRate": 60, "size": {"height": 1080, "width": 1920}}]}
        ]
    }"#;

    fn monitor(name: &str, x: i32, w: u32, primary: bool) -> LocalMonitor {
        LocalMonitor {
            name: name.into(),
            x,
            y: 0,
            width: w,
            height: 1080,
            refresh_millihz: None,
            scale: 1.0,
            primary,
        }
    }

    #[test]
    fn parses_kscreen_doctor_with_rotated_monitor() {
        let monitors = parse_kscreen_monitors(KSCREEN_TWO_MONITORS);
        assert_eq!(monitors.len(), 2, "the disabled output is skipped");
        let dp = &monitors[0];
        assert_eq!((dp.name.as_str(), dp.x, dp.y), ("DP-2", 0, 579));
        assert_eq!((dp.width, dp.height), (2560, 1440));
        assert_eq!(dp.refresh_millihz, Some(144_000));
        assert!(dp.primary);
        let hdmi = &monitors[1];
        assert_eq!(
            (hdmi.width, hdmi.height),
            (1440, 2560),
            "rotation 8 stands the mode up"
        );
        assert_eq!(hdmi.refresh_millihz, Some(74_968));
        assert!(!hdmi.primary);
    }

    #[test]
    fn kscreen_garbage_yields_no_monitors() {
        assert!(parse_kscreen_monitors("not json").is_empty());
        assert!(parse_kscreen_monitors("{}").is_empty());
    }

    #[test]
    fn parses_xrandr_with_rotated_and_primary_outputs() {
        let text = "\
Screen 0: minimum 16 x 16, current 4000 x 2560, maximum 32767 x 32767
DP-2 connected primary 2560x1440+0+579 (normal left inverted right x axis y axis) 597mm x 336mm
   2560x1440     143.86*+ 119.99  
   1920x1080      60.00  
HDMI-A-1 connected 1440x2560+2560+0 right (normal left inverted right x axis y axis) 597mm x 336mm
   2560x1440     74.89*+
   1920x1440     74.89  
DP-3 disconnected (normal left inverted right x axis y axis)
";
        let monitors = parse_xrandr_monitors(text);
        assert_eq!(monitors.len(), 2);
        assert_eq!(monitors[0].name, "DP-2");
        assert!(monitors[0].primary);
        assert_eq!((monitors[0].x, monitors[0].y), (0, 579));
        assert_eq!(monitors[0].refresh_millihz, Some(143_860));
        assert_eq!((monitors[1].width, monitors[1].height), (1440, 2560));
        assert_eq!((monitors[1].x, monitors[1].y), (2560, 0));
        assert_eq!(monitors[1].refresh_millihz, Some(74_890));
    }

    #[test]
    fn parses_wlr_randr_text() {
        let text = "\
DP-1 \"Dell Inc. DELL U2720Q (DP-1)\"
  Make: Dell Inc.
  Enabled: yes
  Modes:
    3840x2160 px, 59.997002 Hz (preferred)
    2560x1440 px, 143.856003 Hz (current)
  Position: 0,0
  Transform: normal
  Scale: 1.000000
HDMI-A-1 \"Acme (HDMI-A-1)\"
  Enabled: yes
  Modes:
    1920x1080 px, 60.000000 Hz (preferred, current)
  Position: 2560,0
  Transform: 90
  Scale: 2.000000
DP-2 \"Off (DP-2)\"
  Enabled: no
  Modes:
    1920x1080 px, 60.000000 Hz (current)
";
        let monitors = parse_wlr_randr_monitors(text);
        assert_eq!(monitors.len(), 2);
        assert_eq!((monitors[0].width, monitors[0].height), (2560, 1440));
        assert_eq!(monitors[0].refresh_millihz, Some(143_856));
        assert_eq!((monitors[1].width, monitors[1].height), (1080, 1920));
        assert_eq!((monitors[1].x, monitors[1].scale), (2560, 2.0));
    }

    #[test]
    fn parses_sway_and_hyprland_json() {
        let sway = r#"[
            {"name": "DP-1", "active": true, "scale": 1.0, "transform": "normal",
             "rect": {"x": 0, "y": 0, "width": 2560, "height": 1440},
             "current_mode": {"width": 2560, "height": 1440, "refresh": 144000}},
            {"name": "HDMI-A-1", "active": true, "scale": 1.0, "transform": "270",
             "rect": {"x": 2560, "y": 0, "width": 1080, "height": 1920},
             "current_mode": {"width": 1920, "height": 1080, "refresh": 60000}}]"#;
        let monitors = parse_sway_monitors(sway);
        assert_eq!(monitors.len(), 2);
        assert_eq!(monitors[0].refresh_millihz, Some(144_000));
        assert_eq!((monitors[1].width, monitors[1].height), (1080, 1920));

        let hypr = r#"[
            {"name": "DP-1", "width": 2560, "height": 1440, "refreshRate": 143.856003,
             "x": 0, "y": 0, "scale": 1.0, "transform": 0, "disabled": false},
            {"name": "HDMI-A-1", "width": 1920, "height": 1080, "refreshRate": 60.0,
             "x": 2560, "y": 0, "scale": 1.5, "transform": 3, "disabled": false}]"#;
        let monitors = parse_hyprland_monitors(hypr);
        assert_eq!(monitors.len(), 2);
        assert_eq!(monitors[0].refresh_millihz, Some(143_856));
        assert_eq!((monitors[1].width, monitors[1].height), (1080, 1920));
        assert_eq!(monitors[1].scale, 1.5);
    }

    #[test]
    fn monitors_sort_left_to_right_and_find_primary() {
        let mut monitors = vec![
            monitor("c", 3840, 1920, false),
            monitor("a", 0, 1920, false),
            monitor("b", 1920, 1920, true),
        ];
        sort_monitors(&mut monitors);
        let names: Vec<_> = monitors.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names, ["a", "b", "c"]);
        assert_eq!(primary_monitor(&monitors), Some(1));
        monitors[1].primary = false;
        assert_eq!(
            primary_monitor(&monitors),
            Some(0),
            "falls back to the first"
        );
        assert_eq!(primary_monitor(&[]), None);
    }

    #[test]
    fn monitor_lookup_uses_scaled_extent_and_knows_when_the_point_is_nowhere() {
        let mut hidpi = monitor("hidpi", 0, 3840, false);
        hidpi.scale = 2.0;
        let monitors = vec![hidpi, monitor("right", 1920, 1920, true)];
        assert_eq!(monitor_at(&monitors, 100.0, 100.0), Some(0));
        assert_eq!(monitor_at(&monitors, 1930.0, 10.0), Some(1));
        assert_eq!(monitor_at(&monitors, -50.0, 10.0), None);
    }

    #[test]
    fn refresh_uses_the_known_monitor_else_the_fastest_one() {
        let rated = |name: &str, x: i32, millihz: u32| LocalMonitor {
            refresh_millihz: Some(millihz),
            ..monitor(name, x, 1920, false)
        };
        let monitors = vec![rated("fast", 0, 144_000), rated("primary", 1920, 60_000)];
        assert_eq!(
            refresh_for_monitor(Some(&monitors[1]), &monitors),
            Some(60_000),
            "a known window monitor decides"
        );
        assert_eq!(
            refresh_for_monitor(None, &monitors),
            Some(144_000),
            "an unknown one must not cap the stream at the primary's rate"
        );
    }

    #[test]
    fn parses_xrandr_rate_tokens() {
        assert_eq!(parse_rate_token_hz("143.86*+"), Some(143_860));
        assert_eq!(parse_rate_token_hz("59.94"), Some(59_940));
    }

    #[test]
    fn parses_current_hz_lines() {
        let text =
            "DP-1 current 2560x1440 px, 143.856003 Hz\nHDMI-A-1 current 1920x1080 px, 60.000000 Hz";
        assert_eq!(parse_lines_with_current_hz(text), Some(143_856));
    }

    #[test]
    fn parses_named_rate_values() {
        let hypr = r#"[{"refreshRate": 143.856003}, {"refreshRate": 60.0}]"#;
        assert_eq!(
            parse_named_rate_values(hypr, "\"refreshRate\""),
            Some(143_856)
        );

        let sway = r#"[{"current_mode":{"refresh":144000}}, {"current_mode":{"refresh":60000}}]"#;
        assert_eq!(parse_named_rate_values(sway, "\"refresh\""), Some(144_000));
    }
}
