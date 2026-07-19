use std::fs;
use std::path::{Path, PathBuf};

use crossbeam_channel::Receiver;
use tokio::task;

use super::traits::{CommitReport, VirtualEnvConfig, VirtualizerError, WorkspaceVirtualizer};

/// Cross-platform default virtualizer.
///
/// On macOS it uses APFS `clonefile(2)` and an FSEvents stream over `upper_dir`.
/// On Windows it uses the Windows fallback hard-link CoW engine.
/// On Unix/Linux it uses `std::fs::hard_link` with a copy fallback.
pub struct DefaultVirtualizer {
    #[cfg(target_os = "macos")]
    watcher: std::sync::Mutex<Option<super::macos_fsevents::WatcherHandle>>,
    #[cfg(target_os = "macos")]
    rx: std::sync::Mutex<Option<Receiver<PathBuf>>>,
    #[cfg(not(target_os = "macos"))]
    _private: (),
}

impl DefaultVirtualizer {
    pub fn new() -> Self {
        DefaultVirtualizer {
            #[cfg(target_os = "macos")]
            watcher: std::sync::Mutex::new(None),
            #[cfg(target_os = "macos")]
            rx: std::sync::Mutex::new(None),
            #[cfg(not(target_os = "macos"))]
            _private: (),
        }
    }
}

#[allow(async_fn_in_trait)]
impl WorkspaceVirtualizer for DefaultVirtualizer {
    async fn initialize(&self, config: &VirtualEnvConfig) -> Result<(), VirtualizerError> {
        let config = config.clone();
        task::spawn_blocking(move || CowEngine::initialize(&config))
            .await
            .map_err(|e| VirtualizerError::SystemFault(e.to_string()))?
    }

    async fn mount(&self, config: &VirtualEnvConfig) -> Result<(), VirtualizerError> {
        #[cfg(target_os = "macos")]
        let upper_dir = config.upper_dir.clone();

        #[cfg(windows)]
        {
            let config = config.clone();
            let result =
                task::spawn_blocking(move || super::windows_fallback::mount_with_projfs(&config))
                    .await
                    .map_err(|e| VirtualizerError::SystemFault(e.to_string()))?;
            match result {
                Ok(()) => return Ok(()),
                Err(VirtualizerError::AccessDenied) => {}
                Err(e) => return Err(e),
            }
        }

        let config = config.clone();
        task::spawn_blocking(move || CowEngine::mount(&config))
            .await
            .map_err(|e| VirtualizerError::SystemFault(e.to_string()))??;

        #[cfg(target_os = "macos")]
        {
            let (handle, rx) = super::macos_fsevents::WatcherHandle::start(&upper_dir)?;
            *self.watcher.lock().unwrap() = Some(handle);
            *self.rx.lock().unwrap() = Some(rx);
        }

        Ok(())
    }

    async fn synchronize_upper(&self, config: &VirtualEnvConfig) -> Result<(), VirtualizerError> {
        #[cfg(target_os = "macos")]
        let rx = self.rx.lock().unwrap().as_ref().cloned();
        #[cfg(not(target_os = "macos"))]
        let rx: Option<Receiver<PathBuf>> = None;

        let config = config.clone();
        task::spawn_blocking(move || CowEngine::synchronize_upper(&config, rx.as_ref()))
            .await
            .map_err(|e| VirtualizerError::SystemFault(e.to_string()))?
    }

    async fn commit(&self, config: &VirtualEnvConfig) -> Result<CommitReport, VirtualizerError> {
        let config = config.clone();
        task::spawn_blocking(move || CowEngine::commit(&config))
            .await
            .map_err(|e| VirtualizerError::SystemFault(e.to_string()))?
    }

    async fn teardown(&self, config: &VirtualEnvConfig) -> Result<(), VirtualizerError> {
        #[cfg(target_os = "macos")]
        {
            *self.watcher.lock().unwrap() = None;
            *self.rx.lock().unwrap() = None;
        }

        let config = config.clone();
        task::spawn_blocking(move || CowEngine::teardown(&config))
            .await
            .map_err(|e| VirtualizerError::SystemFault(e.to_string()))?
    }
}

pub(crate) struct CowEngine;

impl CowEngine {
    pub(crate) fn initialize(config: &VirtualEnvConfig) -> Result<(), VirtualizerError> {
        if !config.lower_dir.exists() {
            return Err(VirtualizerError::SystemFault(format!(
                "lower directory does not exist: {}",
                config.lower_dir.display()
            )));
        }
        fs::create_dir_all(&config.upper_dir)?;
        fs::create_dir_all(&config.merged_dir)?;
        fs::create_dir_all(&config.work_dir)?;
        Ok(())
    }

    pub(crate) fn mount(config: &VirtualEnvConfig) -> Result<(), VirtualizerError> {
        Self::initialize(config)?;

        if config.merged_dir.exists() {
            fs::remove_dir_all(&config.merged_dir)?;
        }
        fs::create_dir_all(&config.merged_dir)?;

        Self::mirror_links(&config.lower_dir, &config.merged_dir)?;
        Self::sync_overlay_dir(&config.upper_dir, &config.merged_dir, false)?;
        Ok(())
    }

    pub(crate) fn synchronize_upper(
        config: &VirtualEnvConfig,
        events: Option<&Receiver<PathBuf>>,
    ) -> Result<(), VirtualizerError> {
        if let Some(rx) = events {
            while let Ok(path) = rx.try_recv() {
                if path.is_file() {
                    if let Ok(rel) = path.strip_prefix(&config.upper_dir) {
                        let merged = config.merged_dir.join(rel);
                        if let Some(parent) = merged.parent() {
                            fs::create_dir_all(parent)?;
                        }
                        Self::copy_file_atomic(&path, &merged)?;
                    }
                }
            }
        }

        Self::sync_overlay_dir(&config.upper_dir, &config.merged_dir, true)?;
        Ok(())
    }

    pub(crate) fn commit(config: &VirtualEnvConfig) -> Result<CommitReport, VirtualizerError> {
        let mut report = CommitReport::default();
        Self::apply_upper_to_lower_and_merged(
            config,
            &config.upper_dir,
            &config.merged_dir,
            &mut report,
        )?;

        if config.upper_dir.exists() {
            fs::remove_dir_all(&config.upper_dir)?;
            fs::create_dir_all(&config.upper_dir)?;
        }
        if config.work_dir.exists() {
            fs::remove_dir_all(&config.work_dir)?;
            fs::create_dir_all(&config.work_dir)?;
        }

        Ok(report)
    }

    pub(crate) fn teardown(config: &VirtualEnvConfig) -> Result<(), VirtualizerError> {
        if config.merged_dir.exists() {
            fs::remove_dir_all(&config.merged_dir)?;
        }
        if config.work_dir.exists() {
            fs::remove_dir_all(&config.work_dir)?;
        }
        Ok(())
    }

    fn mirror_links(src: &Path, dst: &Path) -> Result<(), VirtualizerError> {
        fs::create_dir_all(dst)?;
        for entry in fs::read_dir(src)? {
            let entry = entry?;
            let src_path = entry.path();
            let dst_path = dst.join(entry.file_name());
            let file_type = entry.file_type()?;

            if file_type.is_dir() {
                Self::mirror_links(&src_path, &dst_path)?;
            } else if file_type.is_file() {
                Self::link_or_copy_file(&src_path, &dst_path)?;
            }
        }
        Ok(())
    }

    fn sync_overlay_dir(src: &Path, dst: &Path, compare: bool) -> Result<(), VirtualizerError> {
        if !src.exists() {
            return Ok(());
        }
        fs::create_dir_all(dst)?;

        for entry in fs::read_dir(src)? {
            let entry = entry?;
            let src_path = entry.path();
            let dst_path = dst.join(entry.file_name());
            let file_type = entry.file_type()?;

            if file_type.is_dir() {
                Self::sync_overlay_dir(&src_path, &dst_path, compare)?;
            } else if file_type.is_file() {
                let should_copy = if compare && dst_path.exists() {
                    Self::needs_update(&src_path, &dst_path)?
                } else {
                    true
                };
                if should_copy {
                    Self::copy_file_atomic(&src_path, &dst_path)?;
                }
            }
        }
        Ok(())
    }

    fn apply_upper_to_lower_and_merged(
        config: &VirtualEnvConfig,
        src_dir: &Path,
        _merged_root: &Path,
        report: &mut CommitReport,
    ) -> Result<(), VirtualizerError> {
        for entry in fs::read_dir(src_dir)? {
            let entry = entry?;
            let src = entry.path();
            let rel = src.strip_prefix(&config.upper_dir).unwrap_or(&src);
            let file_type = entry.file_type()?;

            if file_type.is_dir() {
                let lower_path = config.lower_dir.join(rel);
                let merged_path = config.merged_dir.join(rel);
                fs::create_dir_all(&lower_path)?;
                fs::create_dir_all(&merged_path)?;
                Self::apply_upper_to_lower_and_merged(config, &src, _merged_root, report)?;
            } else if file_type.is_file() {
                let lower_path = config.lower_dir.join(rel);
                let merged_path = config.merged_dir.join(rel);
                if let Some(parent) = lower_path.parent() {
                    fs::create_dir_all(parent)?;
                }
                if let Some(parent) = merged_path.parent() {
                    fs::create_dir_all(parent)?;
                }

                let bytes_lower = fs::copy(&src, &lower_path)?;
                report.files_written.push(rel.to_path_buf());
                report.bytes_mutated += bytes_lower;

                if merged_path.exists() {
                    fs::remove_file(&merged_path)?;
                }
                let bytes_merged = fs::copy(&src, &merged_path)?;
                report.bytes_mutated += bytes_merged;
            }
        }
        Ok(())
    }

    fn copy_file_atomic(src: &Path, dst: &Path) -> Result<(), VirtualizerError> {
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent)?;
        }
        if dst.exists() {
            fs::remove_file(dst)?;
        }
        fs::copy(src, dst)?;
        Ok(())
    }

    fn needs_update(src: &Path, dst: &Path) -> Result<bool, VirtualizerError> {
        let src_meta = fs::metadata(src)?;
        let dst_meta = fs::metadata(dst)?;

        if src_meta.len() != dst_meta.len() {
            return Ok(true);
        }

        match (src_meta.modified(), dst_meta.modified()) {
            (Ok(src_t), Ok(dst_t)) if src_t != dst_t => Ok(true),
            (Ok(_), Err(_)) | (Err(_), Ok(_)) | (Err(_), Err(_)) => Ok(true),
            _ => Ok(false),
        }
    }

    fn link_or_copy_file(src: &Path, dst: &Path) -> Result<(), VirtualizerError> {
        #[cfg(target_os = "macos")]
        if super::macos_apfs::clone_file(src, dst).is_ok() {
            return Ok(());
        }

        #[cfg(windows)]
        if super::windows_fallback::create_hardlink(src, dst).is_ok() {
            return Ok(());
        }

        #[cfg(unix)]
        if std::fs::hard_link(src, dst).is_ok() {
            return Ok(());
        }

        fs::copy(src, dst)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_config(base: &Path) -> VirtualEnvConfig {
        VirtualEnvConfig {
            lower_dir: base.join("lower"),
            upper_dir: base.join("upper"),
            merged_dir: base.join("merged"),
            work_dir: base.join("work"),
        }
    }

    #[test]
    fn cow_engine_mount_sync_and_commit() {
        let base = std::env::temp_dir().join(format!("acv_test_{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);

        let config = make_config(&base);
        fs::create_dir_all(&config.lower_dir).unwrap();
        fs::write(config.lower_dir.join("foo.txt"), "lower\n").unwrap();

        fs::create_dir_all(&config.upper_dir).unwrap();
        fs::write(config.upper_dir.join("bar.txt"), "upper\n").unwrap();

        CowEngine::mount(&config).unwrap();

        assert_eq!(
            fs::read_to_string(config.merged_dir.join("foo.txt")).unwrap(),
            "lower\n"
        );
        assert_eq!(
            fs::read_to_string(config.merged_dir.join("bar.txt")).unwrap(),
            "upper\n"
        );

        fs::write(config.upper_dir.join("foo.txt"), "changed\n").unwrap();
        CowEngine::synchronize_upper(&config, None).unwrap();

        assert_eq!(
            fs::read_to_string(config.merged_dir.join("foo.txt")).unwrap(),
            "changed\n"
        );

        let report = CowEngine::commit(&config).unwrap();
        assert!(report
            .files_written
            .iter()
            .any(|p| p == Path::new("foo.txt")));
        assert!(report.bytes_mutated > 0);

        assert_eq!(
            fs::read_to_string(config.lower_dir.join("foo.txt")).unwrap(),
            "changed\n"
        );
        assert_eq!(
            fs::read_to_string(config.lower_dir.join("bar.txt")).unwrap(),
            "upper\n"
        );

        CowEngine::teardown(&config).unwrap();
        let _ = fs::remove_dir_all(&base);
    }
}
