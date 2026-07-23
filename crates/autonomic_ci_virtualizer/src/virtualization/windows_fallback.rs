//! Zero-privilege Simulated Copy-on-Write (S-CoW) fallback for Windows.
//!
//! When the Windows Projected File System cannot be initialized (typically due
//! to `E_ACCESSDENIED`), this engine replicates the directory topology using
//! NTFS hard-links. Upper-layer changes are written directly to `upper_dir`;
//! `CowEngine::synchronize_upper` breaks the hard-link in `merged_dir` and
//! copies the baseline data from `upper_dir`, tracking variations cleanly.

use std::ffi::OsStr;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use std::ptr;

use windows_sys::Win32::Storage::FileSystem::CreateHardLinkW;

use super::traits::{VirtualEnvConfig, VirtualizerError};

/// Create an NTFS hard link from `src` (existing) to `dst` (new link).
pub fn create_hardlink(src: &Path, dst: &Path) -> io::Result<()> {
    let dst_w: Vec<u16> = OsStr::new(dst).encode_wide().chain(Some(0)).collect();
    let src_w: Vec<u16> = OsStr::new(src).encode_wide().chain(Some(0)).collect();

    let res = unsafe { CreateHardLinkW(dst_w.as_ptr(), src_w.as_ptr(), ptr::null()) };

    if res != 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Attempt ProjFS; if it fails with a privilege denial, signal the caller to
/// fall back to the hard-link CoW engine. If it succeeds, stop the instance
/// immediately because this milestone does not yet include a full ProjFS
/// callback provider.
pub fn mount_with_projfs(config: &VirtualEnvConfig) -> Result<(), VirtualizerError> {
    unsafe {
        match super::windows_projfs::try_start(config) {
            Ok(handle) => {
                handle.stop();
                // A complete provider would retain the handle. For now we
                // deliberately fall back so the cross-platform CoW engine owns
                // the workspace view.
                Err(VirtualizerError::AccessDenied)
            }
            Err(VirtualizerError::AccessDenied) => Err(VirtualizerError::AccessDenied),
            Err(e) => Err(e),
        }
    }
}
