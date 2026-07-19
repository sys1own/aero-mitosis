pub mod traits;
pub use traits::*;

#[cfg(target_os = "macos")]
pub mod macos_apfs;
#[cfg(target_os = "macos")]
pub mod macos_fsevents;

#[cfg(windows)]
pub mod windows_projfs;
#[cfg(windows)]
pub mod windows_fallback;

mod engine;
pub use engine::DefaultVirtualizer;
