//! APFS copy-on-write support via `clonefile(2)`.
//!
//! This module is only compiled on macOS. It projects a `lower_dir` workspace
//! layout into `merged_dir` using APFS clone extents, sharing data blocks until
//! a write triggers copy-on-write.

use std::ffi::CString;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use super::traits::VirtualizerError;

/// Do not copy ownership information from the source.
pub const CLONE_NOOWNERCOPY: u32 = 0x0002;

extern "C" {
    /// macOS `clonefile(2)` creates a copy-on-write clone of `src` at `dst`.
    fn clonefile(
        src: *const libc::c_char,
        dst: *const libc::c_char,
        flags: u32,
    ) -> libc::c_int;
}

/// Clone a single file from `src` to `dst` using APFS copy-on-write extents.
pub fn clone_file(src: &Path, dst: &Path) -> io::Result<()> {
    let src_c = CString::new(src.as_os_str().as_bytes())?;
    let dst_c = CString::new(dst.as_os_str().as_bytes())?;

    let rc = unsafe { clonefile(src_c.as_ptr(), dst_c.as_ptr(), CLONE_NOOWNERCOPY) };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Recursively clone `lower_dir` into `merged_dir`.
///
/// Directories are created normally; each regular file is cloned with
/// `clonefile(2)`. This gives a zero-cost projected workspace on APFS.
pub fn clone_tree(lower_dir: &Path, merged_dir: &Path) -> Result<(), VirtualizerError> {
    std::fs::create_dir_all(merged_dir)?;

    for entry in std::fs::read_dir(lower_dir)? {
        let entry = entry?;
        let src = entry.path();
        let dst = merged_dir.join(entry.file_name());
        let file_type = entry.file_type()?;

        if file_type.is_dir() {
            clone_tree(&src, &dst)?;
        } else if file_type.is_file() {
            clone_file(&src, &dst)?;
        }
    }

    Ok(())
}
