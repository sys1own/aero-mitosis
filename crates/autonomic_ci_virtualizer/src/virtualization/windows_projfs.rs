//! Windows Projected File System (ProjFS) integration.
//!
//! This module is only compiled on Windows. It references the ProjFS API via
//! `windows-sys` and attempts to start a virtualization instance at the merged
//! directory. In this milestone the callback provider is intentionally minimal;
//! the production-grade provider logic lives in `windows_fallback.rs` which
//! falls back to NTFS hard-link CoW when ProjFS is unavailable.

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use std::ptr;

use windows_sys::Win32::Foundation::{E_ACCESSDENIED, GUID, HRESULT, PCWSTR};
use windows_sys::Win32::Storage::ProjectedFileSystem::{
    PrjMarkDirectoryAsPlaceholder, PrjStartVirtualizing, PrjStopVirtualizing, PRJ_CALLBACKS,
    PRJ_CALLBACK_DATA, PRJ_DIR_ENTRY_BUFFER_HANDLE, PRJ_NAMESPACE_VIRTUALIZATION_CONTEXT,
};

use super::traits::{VirtualEnvConfig, VirtualizerError};

pub struct ProjFsHandle {
    ctx: PRJ_NAMESPACE_VIRTUALIZATION_CONTEXT,
}

unsafe impl Send for ProjFsHandle {}
unsafe impl Sync for ProjFsHandle {}

impl ProjFsHandle {
    pub fn stop(&self) {
        unsafe { PrjStopVirtualizing(self.ctx) };
    }
}

fn path_to_pcwstr(path: &Path) -> Vec<u16> {
    OsStr::new(path).encode_wide().chain(Some(0)).collect()
}

const S_OK: HRESULT = 0;
const E_INVALIDARG: HRESULT = 0x80070057u32 as i32;

unsafe extern "system" fn start_enum_cb(
    _callbackdata: *const PRJ_CALLBACK_DATA,
    _enumerationid: *const GUID,
) -> HRESULT {
    S_OK
}

unsafe extern "system" fn end_enum_cb(
    _callbackdata: *const PRJ_CALLBACK_DATA,
    _enumerationid: *const GUID,
) -> HRESULT {
    S_OK
}

unsafe extern "system" fn get_enum_cb(
    _callbackdata: *const PRJ_CALLBACK_DATA,
    _enumerationid: *const GUID,
    _searchexpression: PCWSTR,
    _direntrybufferhandle: PRJ_DIR_ENTRY_BUFFER_HANDLE,
) -> HRESULT {
    E_INVALIDARG
}

unsafe extern "system" fn get_placeholder_cb(_callbackdata: *const PRJ_CALLBACK_DATA) -> HRESULT {
    E_INVALIDARG
}

unsafe extern "system" fn get_file_data_cb(
    _callbackdata: *const PRJ_CALLBACK_DATA,
    _byteoffset: u64,
    _length: u32,
) -> HRESULT {
    E_INVALIDARG
}

/// Attempt to start a ProjFS virtualization instance at `config.merged_dir`.
///
/// Returns `VirtualizerError::AccessDenied` if Windows rejects the request due
/// to missing privileges, allowing the caller to fall back to the hard-link CoW
/// engine. Other ProjFS errors are reported as `SystemFault`.
pub unsafe fn try_start(config: &VirtualEnvConfig) -> Result<ProjFsHandle, VirtualizerError> {
    let root = path_to_pcwstr(&config.merged_dir);

    let hr = PrjMarkDirectoryAsPlaceholder(
        PCWSTR(root.as_ptr()),
        PCWSTR(ptr::null()),
        ptr::null(),
        ptr::null(),
    );
    if hr != S_OK && hr != E_ACCESSDENIED {
        return Err(VirtualizerError::SystemFault(format!(
            "PrjMarkDirectoryAsPlaceholder failed: {hr:#x}"
        )));
    }

    let callbacks = PRJ_CALLBACKS {
        StartDirectoryEnumerationCallback: Some(start_enum_cb),
        EndDirectoryEnumerationCallback: Some(end_enum_cb),
        GetDirectoryEnumerationCallback: Some(get_enum_cb),
        GetPlaceholderInfoCallback: Some(get_placeholder_cb),
        GetFileDataCallback: Some(get_file_data_cb),
        QueryFileNameCallback: None,
        NotificationCallback: None,
        CancelCommandCallback: None,
    };

    let mut ctx: PRJ_NAMESPACE_VIRTUALIZATION_CONTEXT = ptr::null_mut();
    let hr = PrjStartVirtualizing(
        PCWSTR(root.as_ptr()),
        &callbacks,
        ptr::null(),
        ptr::null(),
        &mut ctx,
    );

    if hr == S_OK {
        Ok(ProjFsHandle { ctx })
    } else if hr == E_ACCESSDENIED {
        Err(VirtualizerError::AccessDenied)
    } else {
        Err(VirtualizerError::SystemFault(format!(
            "PrjStartVirtualizing failed: {hr:#x}"
        )))
    }
}
