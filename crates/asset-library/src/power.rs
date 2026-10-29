//! Being a good background citizen: lower thread priority, and detect battery power.

/// Lower the calling thread's scheduling priority. Threads it creates afterwards inherit
/// this on Linux (nice) and macOS (QoS), so ONNX Runtime's workers are covered there too.
pub fn lower_current_thread_priority() {
    #[cfg(target_os = "linux")]
    // SAFETY: plain syscalls on the calling thread; failure leaves priority unchanged.
    unsafe {
        let tid = libc::syscall(libc::SYS_gettid) as libc::id_t;
        libc::setpriority(libc::PRIO_PROCESS, tid, 10);
    }
    #[cfg(target_os = "macos")]
    // SAFETY: sets the QoS class of the calling thread only.
    unsafe {
        libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_UTILITY, 0);
    }
    #[cfg(windows)]
    // SAFETY: the pseudo-handle refers to the calling thread and needs no closing.
    unsafe {
        use windows_sys::Win32::System::Threading::{GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_BELOW_NORMAL};
        SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_BELOW_NORMAL);
    }
}

/// True when the machine is running on battery. Unknown counts as mains power.
pub fn on_battery() -> bool {
    #[cfg(target_os = "linux")]
    {
        // Any "Mains"/"USB" supply that is online means we're plugged in; a battery that is
        // discharging with no online supply means we're not.
        let Ok(entries) = std::fs::read_dir("/sys/class/power_supply") else { return false };
        let (mut has_battery, mut online) = (false, false);
        for e in entries.flatten() {
            let read = |f: &str| std::fs::read_to_string(e.path().join(f)).unwrap_or_default().trim().to_string();
            match read("type").as_str() {
                "Battery" => has_battery |= read("status") == "Discharging",
                "Mains" | "USB" => online |= read("online") == "1",
                _ => {}
            }
        }
        has_battery && !online
    }
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("pmset")
            .args(["-g", "batt"])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).contains("'Battery Power'"))
            .unwrap_or(false)
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};
        let mut s: SYSTEM_POWER_STATUS = unsafe { std::mem::zeroed() };
        // SAFETY: writes into the struct we pass. ACLineStatus 0 = offline (on battery).
        unsafe { GetSystemPowerStatus(&mut s) != 0 && s.ACLineStatus == 0 }
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    {
        false
    }
}
