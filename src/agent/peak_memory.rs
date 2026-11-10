//! The most memory this process has held at once, for `perf memory`.

/// The process's peak physical memory in bytes: the peak working set on
/// Windows, `VmHWM` on Linux, `ru_maxrss` on macOS. `None` on another
/// platform, or when the system does not report it.
pub fn peak_bytes() -> Option<u64> {
    platform::peak_bytes()
}

#[cfg(windows)]
mod platform {
    use windows_sys::Win32::System::ProcessStatus::{
        K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;

    pub fn peak_bytes() -> Option<u64> {
        let mut counters = PROCESS_MEMORY_COUNTERS::default();
        let size = u32::try_from(std::mem::size_of::<PROCESS_MEMORY_COUNTERS>()).ok()?;
        // SAFETY: `GetCurrentProcess` returns a pseudo-handle that needs no
        // closing, and `counters` is a writable struct of `size` bytes.
        let filled = unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), &mut counters, size) };
        (filled != 0).then_some(counters.PeakWorkingSetSize as u64)
    }
}

#[cfg(target_os = "linux")]
mod platform {
    pub fn peak_bytes() -> Option<u64> {
        super::vm_hwm_bytes(&std::fs::read_to_string("/proc/self/status").ok()?)
    }
}

#[cfg(target_os = "macos")]
mod platform {
    pub fn peak_bytes() -> Option<u64> {
        let mut usage = std::mem::MaybeUninit::<libc::rusage>::zeroed();
        // SAFETY: `getrusage` fills the struct it is pointed at when it
        // returns 0.
        if unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) } != 0 {
            return None;
        }
        // SAFETY: `getrusage` returned 0, so `usage` is filled.
        let usage = unsafe { usage.assume_init() };
        // In bytes on macOS; Linux reports kilobytes.
        u64::try_from(usage.ru_maxrss).ok()
    }
}

#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
mod platform {
    pub fn peak_bytes() -> Option<u64> {
        None
    }
}

/// `VmHWM`, the peak resident set size, from the text of `/proc/self/status`,
/// which reports it in kilobytes.
#[cfg(any(target_os = "linux", test))]
fn vm_hwm_bytes(status: &str) -> Option<u64> {
    let value = status
        .lines()
        .find_map(|line| line.strip_prefix("VmHWM:"))?;
    let kilobytes: u64 = value.trim().strip_suffix("kB")?.trim().parse().ok()?;
    Some(kilobytes * 1024)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vm_hwm_is_read_in_kilobytes() {
        let status =
            "Name:\trearview\nVmPeak:\t  900000 kB\nVmHWM:\t  204800 kB\nVmRSS:\t  102400 kB\n";

        assert_eq!(vm_hwm_bytes(status), Some(204_800 * 1024));
    }

    #[test]
    fn a_status_without_vm_hwm_has_no_peak() {
        assert_eq!(vm_hwm_bytes("Name:\trearview\n"), None);
    }

    #[cfg(any(windows, target_os = "linux", target_os = "macos"))]
    #[test]
    fn this_platform_reports_a_peak() {
        assert!(peak_bytes().is_some_and(|bytes| bytes > 0));
    }
}
