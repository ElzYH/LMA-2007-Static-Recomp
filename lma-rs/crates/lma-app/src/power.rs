//! Whether the computer runs on its battery, for the battery saver (F2, #844; the maintainer, 26 Oct: "at one
//! point on this laptop it would kill battery in an hour"). macOS asks IOKit's power sources,
//! Windows GetSystemPowerStatus; elsewhere, or when the answer is unknown, it counts as mains.

/// True while the machine draws on its battery.
pub fn on_battery() -> bool {
    imp::on_battery()
}

#[cfg(target_os = "macos")]
mod imp {
    use std::ffi::c_void;

    type CFTypeRef = *const c_void;

    #[link(name = "IOKit", kind = "framework")]
    extern "C" {
        fn IOPSCopyPowerSourcesInfo() -> CFTypeRef;
        /// "AC Power", "Battery Power" or "UPS Power"; not owned (get rule).
        fn IOPSGetProvidingPowerSourceType(snapshot: CFTypeRef) -> CFTypeRef;
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        fn CFRelease(cf: CFTypeRef);
        fn CFStringGetCString(s: CFTypeRef, buf: *mut u8, len: isize, encoding: u32) -> u8;
    }

    const UTF8: u32 = 0x0800_0100;

    pub fn on_battery() -> bool {
        // SAFETY: the snapshot is released once read; the type string belongs to it
        unsafe {
            let snap = IOPSCopyPowerSourcesInfo();
            if snap.is_null() {
                return false;
            }
            let kind = IOPSGetProvidingPowerSourceType(snap);
            let mut buf = [0u8; 64];
            let ok = !kind.is_null() && CFStringGetCString(kind, buf.as_mut_ptr(), buf.len() as isize, UTF8) != 0;
            CFRelease(snap);
            ok && buf.starts_with(b"Battery Power")
        }
    }
}

#[cfg(windows)]
mod imp {
    #[repr(C)]
    #[derive(Default)]
    struct SystemPowerStatus {
        ac_line_status: u8,
        battery_flag: u8,
        battery_life_percent: u8,
        system_status_flag: u8,
        battery_life_time: u32,
        battery_full_life_time: u32,
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn GetSystemPowerStatus(status: *mut SystemPowerStatus) -> i32;
    }

    pub fn on_battery() -> bool {
        let mut s = SystemPowerStatus::default();
        // SAFETY: a plain struct the call fills in; ACLineStatus 0 is offline (on battery), 255 unknown
        unsafe { GetSystemPowerStatus(&mut s) != 0 && s.ac_line_status == 0 }
    }
}

#[cfg(not(any(target_os = "macos", windows)))]
mod imp {
    pub fn on_battery() -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    /// The query answers without crashing, quickly enough to poll every few seconds.
    #[test]
    fn the_power_source_answers_quickly() {
        let t = std::time::Instant::now();
        for _ in 0..10 {
            let _ = super::on_battery();
        }
        assert!(t.elapsed() < std::time::Duration::from_millis(500), "{:?} for 10 queries", t.elapsed());
    }
}
