//! The free space on the drive the session cache database grows on.

use std::path::Path;

/// The bytes this user may still write on the drive holding `path`, or its
/// nearest directory that exists. `None` on a platform without the call, or
/// when the system does not answer.
pub(super) fn available_bytes(path: &Path) -> Option<u64> {
    let existing = path.ancestors().find(|ancestor| ancestor.exists())?;
    platform::available_bytes(existing)
}

#[cfg(windows)]
mod platform {
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;

    pub fn available_bytes(path: &Path) -> Option<u64> {
        let wide: Vec<u16> = path
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let mut available = 0u64;
        // SAFETY: `wide` is a NUL-terminated path, and the call writes the
        // one count it is given a pointer to; the other two may be null.
        let answered = unsafe {
            GetDiskFreeSpaceExW(
                wide.as_ptr(),
                &mut available,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        (answered != 0).then_some(available)
    }
}

#[cfg(unix)]
mod platform {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;

    pub fn available_bytes(path: &Path) -> Option<u64> {
        let path = CString::new(path.as_os_str().as_bytes()).ok()?;
        let mut stat = std::mem::MaybeUninit::<libc::statvfs>::zeroed();
        // SAFETY: `path` is NUL-terminated, and `statvfs` fills the struct
        // it is pointed at when it returns 0.
        if unsafe { libc::statvfs(path.as_ptr(), stat.as_mut_ptr()) } != 0 {
            return None;
        }
        // SAFETY: `statvfs` returned 0, so `stat` is filled.
        let stat = unsafe { stat.assume_init() };
        u64::try_from(stat.f_bavail)
            .ok()?
            .checked_mul(u64::try_from(stat.f_frsize).ok()?)
    }
}

#[cfg(not(any(windows, unix)))]
mod platform {
    use std::path::Path;

    pub fn available_bytes(_path: &Path) -> Option<u64> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn temporary_directorys_drive_has_free_space() {
        let directory = tempfile::tempdir().unwrap();

        assert!(
            available_bytes(&directory.path().join("not/yet/made")).is_some_and(|free| free > 0)
        );
    }
}
