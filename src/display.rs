#[cfg(target_os = "linux")]
use std::process::Command;

pub fn detect_max_refresh_millihz() -> Option<u32> {
    env_override_refresh_millihz().or_else(detect_platform_refresh_millihz)
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

#[cfg(target_os = "windows")]
fn detect_windows_refresh_millihz() -> Option<u32> {
    use std::mem::size_of;

    const DISPLAY_DEVICE_ACTIVE: u32 = 0x0000_0001;
    const ENUM_CURRENT_SETTINGS: u32 = 0xFFFF_FFFF;

    #[repr(C)]
    struct DisplayDeviceW {
        cb: u32,
        device_name: [u16; 32],
        device_string: [u16; 128],
        state_flags: u32,
        device_id: [u16; 128],
        device_key: [u16; 128],
    }

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
        fn EnumDisplayDevicesW(
            device: *const u16,
            dev_num: u32,
            display_device: *mut DisplayDeviceW,
            flags: u32,
        ) -> i32;
        fn EnumDisplaySettingsW(
            device_name: *const u16,
            mode_num: u32,
            dev_mode: *mut DevModeW,
        ) -> i32;
    }

    let mut best = None;
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

        let ok = unsafe {
            EnumDisplaySettingsW(
                display.device_name.as_ptr(),
                ENUM_CURRENT_SETTINGS,
                &mut mode,
            )
        };
        if ok == 0 {
            continue;
        }

        if let Some(rate) = normalize_refresh_hz(mode.display_frequency as f64) {
            best = Some(best.map_or(rate, |current| current.max(rate)));
        }
    }

    best
}

#[cfg(test)]
mod tests {
    use super::{parse_lines_with_current_hz, parse_named_rate_values, parse_rate_token_hz};

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
