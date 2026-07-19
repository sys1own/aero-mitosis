pub mod traits;
pub use traits::*;

#[cfg(target_os = "macos")]
pub mod macos_apfs;
#[cfg(target_os = "macos")]
pub mod macos_fsevents;

#[cfg(windows)]
pub mod windows_fallback;
#[cfg(windows)]
pub mod windows_projfs;

mod engine;
pub use engine::DefaultVirtualizer;
