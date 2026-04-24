pub struct KeepAwakeController {
    active: bool,
    inhibitor: Option<platform::Inhibitor>,
}

impl KeepAwakeController {
    pub fn new() -> Self {
        Self {
            active: false,
            inhibitor: None,
        }
    }

    pub fn set_active(&mut self, active: bool) {
        if self.active == active {
            return;
        }

        self.active = active;
        if active {
            match platform::Inhibitor::acquire() {
                Ok(inhibitor) => {
                    self.inhibitor = Some(inhibitor);
                    eprintln!("[power] display sleep inhibited");
                }
                Err(err) => {
                    self.inhibitor = None;
                    eprintln!("[power] failed to inhibit display sleep: {err}");
                }
            }
        } else if self.inhibitor.take().is_some() {
            eprintln!("[power] display sleep inhibitor released");
        }
    }
}

#[cfg(target_os = "linux")]
mod platform {
    use std::fs::File;
    use std::os::fd::OwnedFd;
    use zbus::blocking::{Connection, Proxy};
    use zbus::zvariant::OwnedFd as ZbusOwnedFd;

    pub enum Inhibitor {
        ScreenSaver { connection: Connection, cookie: u32 },
        Login1 { _connection: Connection, _fd: File },
    }

    impl Inhibitor {
        pub fn acquire() -> Result<Self, String> {
            acquire_screensaver().or_else(|screen_err| {
                acquire_login1().map_err(|login_err| {
                    format!("{screen_err}; fallback login1 failed: {login_err}")
                })
            })
        }
    }

    impl Drop for Inhibitor {
        fn drop(&mut self) {
            if let Self::ScreenSaver { connection, cookie } = self {
                if let Ok(proxy) = Proxy::new(
                    connection,
                    "org.freedesktop.ScreenSaver",
                    "/org/freedesktop/ScreenSaver",
                    "org.freedesktop.ScreenSaver",
                ) {
                    let _: zbus::Result<()> = proxy.call("UnInhibit", &(*cookie,));
                }
            }
        }
    }

    fn acquire_screensaver() -> Result<Inhibitor, String> {
        let connection =
            Connection::session().map_err(|err| format!("session bus unavailable: {err}"))?;
        let proxy = Proxy::new(
            &connection,
            "org.freedesktop.ScreenSaver",
            "/org/freedesktop/ScreenSaver",
            "org.freedesktop.ScreenSaver",
        )
        .map_err(|err| format!("screen saver proxy unavailable: {err}"))?;
        let cookie: u32 = proxy
            .call("Inhibit", &("st-client", "Streaming active"))
            .map_err(|err| format!("screen saver inhibit failed: {err}"))?;
        Ok(Inhibitor::ScreenSaver { connection, cookie })
    }

    fn acquire_login1() -> Result<Inhibitor, String> {
        let connection =
            Connection::system().map_err(|err| format!("system bus unavailable: {err}"))?;
        let proxy = Proxy::new(
            &connection,
            "org.freedesktop.login1",
            "/org/freedesktop/login1",
            "org.freedesktop.login1.Manager",
        )
        .map_err(|err| format!("login1 proxy unavailable: {err}"))?;
        let fd: ZbusOwnedFd = proxy
            .call(
                "Inhibit",
                &("idle", "st-client", "Streaming active", "block"),
            )
            .map_err(|err| format!("login1 inhibit failed: {err}"))?;
        let fd = OwnedFd::from(fd);
        Ok(Inhibitor::Login1 {
            _connection: connection,
            _fd: File::from(fd),
        })
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use core_foundation::base::TCFType;
    use core_foundation::string::{CFString, CFStringRef};

    pub struct Inhibitor {
        assertion_id: u32,
    }

    impl Inhibitor {
        pub fn acquire() -> Result<Self, String> {
            let assertion_type = CFString::from_static_string("PreventUserIdleDisplaySleep");
            let reason = CFString::from_static_string("Streaming active");
            let mut assertion_id = 0u32;
            let status = unsafe {
                IOPMAssertionCreateWithName(
                    assertion_type.as_concrete_TypeRef(),
                    K_IOPM_ASSERTION_LEVEL_ON,
                    reason.as_concrete_TypeRef(),
                    &mut assertion_id,
                )
            };
            if status == 0 {
                Ok(Self { assertion_id })
            } else {
                Err(format!("IOPMAssertionCreateWithName failed: {status}"))
            }
        }
    }

    impl Drop for Inhibitor {
        fn drop(&mut self) {
            unsafe {
                let _ = IOPMAssertionRelease(self.assertion_id);
            }
        }
    }

    type IOPMAssertionID = u32;
    type IOPMAssertionLevel = u32;
    type IOReturn = i32;

    const K_IOPM_ASSERTION_LEVEL_ON: IOPMAssertionLevel = 255;

    #[link(name = "IOKit", kind = "framework")]
    unsafe extern "C" {
        fn IOPMAssertionCreateWithName(
            assertion_type: CFStringRef,
            assertion_level: IOPMAssertionLevel,
            assertion_name: CFStringRef,
            assertion_id: *mut IOPMAssertionID,
        ) -> IOReturn;
        fn IOPMAssertionRelease(assertion_id: IOPMAssertionID) -> IOReturn;
    }
}

#[cfg(target_os = "windows")]
mod platform {
    use windows_sys::Win32::System::Power::{
        SetThreadExecutionState, ES_CONTINUOUS, ES_DISPLAY_REQUIRED, EXECUTION_STATE,
    };

    pub struct Inhibitor;

    impl Inhibitor {
        pub fn acquire() -> Result<Self, String> {
            let previous = unsafe {
                SetThreadExecutionState((ES_CONTINUOUS | ES_DISPLAY_REQUIRED) as EXECUTION_STATE)
            };
            if previous == 0 {
                Err("SetThreadExecutionState failed".to_string())
            } else {
                Ok(Self)
            }
        }
    }

    impl Drop for Inhibitor {
        fn drop(&mut self) {
            unsafe {
                let _ = SetThreadExecutionState(ES_CONTINUOUS as EXECUTION_STATE);
            }
        }
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
mod platform {
    pub struct Inhibitor;

    impl Inhibitor {
        pub fn acquire() -> Result<Self, String> {
            Ok(Self)
        }
    }
}
