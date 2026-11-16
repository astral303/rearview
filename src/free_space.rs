//! The free space on the drive a cache grows on.

use std::path::Path;

/// The least free space a write leaves on a cache's drive: 1 GB.
pub(crate) const FREE_SPACE_FLOOR: u64 = 1_000_000_000;

/// The free space on the drive a path is on, as rearview asks for it.
pub trait DiskSpace: Send + Sync {
    /// The bytes this user may still write on the drive holding `path`;
    /// `None` when the system does not answer.
    fn available_bytes(&self, path: &Path) -> Option<u64>;
}

/// The system's answer for free space.
pub struct SystemDiskSpace;

impl DiskSpace for SystemDiskSpace {
    fn available_bytes(&self, path: &Path) -> Option<u64> {
        available_bytes(path)
    }
}

/// The bytes this user may still write on the drive holding `path`, or its
/// nearest directory that exists. `None` on a platform without the call, or
/// when the system does not answer.
fn available_bytes(path: &Path) -> Option<u64> {
    let existing = path.ancestors().find(|ancestor| ancestor.exists())?;
    platform::available_bytes(existing)
}

/// A drive whose free space stays at the bytes it holds.
#[cfg(test)]
pub(crate) struct FixedFreeSpace(pub(crate) u64);

#[cfg(test)]
impl DiskSpace for FixedFreeSpace {
    fn available_bytes(&self, _path: &Path) -> Option<u64> {
        Some(self.0)
    }
}

/// A drive with room for the first `roomy_answers` free-space checks and
/// 0.9 GB free from then on, calling `on_running_out` before each of those
/// later answers.
#[cfg(test)]
pub(crate) struct DriveRunningOut {
    roomy_answers: usize,
    answers: std::sync::atomic::AtomicUsize,
    on_running_out: Box<dyn Fn() + Send + Sync>,
}

#[cfg(test)]
impl DriveRunningOut {
    pub(crate) fn after(
        roomy_answers: usize,
        on_running_out: impl Fn() + Send + Sync + 'static,
    ) -> Self {
        Self {
            roomy_answers,
            answers: std::sync::atomic::AtomicUsize::new(0),
            on_running_out: Box::new(on_running_out),
        }
    }

    /// The free-space checks this drive answered.
    pub(crate) fn answers(&self) -> usize {
        self.answers.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// The `roomy_answers` that run the drive out at the first, the middle
    /// and the last of `checks` free-space checks.
    pub(crate) fn check_points(checks: usize) -> Vec<usize> {
        let mut points = vec![0, checks / 2, checks - 1];
        points.dedup();
        points
    }
}

#[cfg(test)]
impl DiskSpace for DriveRunningOut {
    fn available_bytes(&self, _path: &Path) -> Option<u64> {
        let answer = self
            .answers
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if answer < self.roomy_answers {
            return Some(10 * FREE_SPACE_FLOOR);
        }
        (self.on_running_out)();
        Some(900_000_000)
    }
}

#[cfg(test)]
pub(crate) struct UnreportedFreeSpace;

#[cfg(test)]
impl DiskSpace for UnreportedFreeSpace {
    fn available_bytes(&self, _path: &Path) -> Option<u64> {
        None
    }
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
        // `u64::from` widens `f_bavail`, a `u32` on macOS, and `f_frsize`, a
        // `c_ulong` of 32 bits on some targets; on 64-bit Linux both are
        // already `u64`.
        #[allow(clippy::useless_conversion)]
        let (available_blocks, block_size) = (u64::from(stat.f_bavail), u64::from(stat.f_frsize));
        available_blocks.checked_mul(block_size)
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
