//! An exit that skips the process's native teardown.

/// Ends the process with exit status `status` without the native teardown
/// a return from `main` runs: C `atexit` handlers and C++ static
/// destructors. Flushes stdout and stderr first, as Rust's own exit flushes
/// stdout.
pub fn exit_without_native_teardown(status: u8) -> ! {
    use std::io::Write;
    // A failed flush leaves rearview nowhere to report it.
    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();
    platform::exit(status)
}

#[cfg(unix)]
mod platform {
    pub fn exit(status: u8) -> ! {
        // SAFETY: `_exit` ends the process at once; nothing runs after it.
        unsafe { libc::_exit(status.into()) }
    }
}

#[cfg(windows)]
mod platform {
    /// `ExitProcess` ends every other thread before any DLL's teardown runs,
    /// and doesn't run the executable's `atexit` handlers or static
    /// destructors.
    pub fn exit(status: u8) -> ! {
        // SAFETY: `ExitProcess` accepts any exit status and doesn't return.
        unsafe { windows_sys::Win32::System::Threading::ExitProcess(status.into()) }
    }
}
