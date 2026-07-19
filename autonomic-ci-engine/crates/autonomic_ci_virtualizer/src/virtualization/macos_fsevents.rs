//! Apple FSEvents-backed file monitoring for `upper_dir`.
//!
//! Uses the `notify` crate's FSEvents backend on macOS to stream real-time
//! file changes into a lock-free `crossbeam_channel::Sender<PathBuf>`.

use std::path::{Path, PathBuf};

use crossbeam_channel::{Receiver, Sender};
use notify::{Config, Event, RecursiveMode, Watcher};

use super::traits::VirtualizerError;

/// Handle that keeps the FSEvent stream alive and exposes the receiving end.
pub struct WatcherHandle {
    _watcher: notify::RecommendedWatcher,
    _tx: Sender<PathBuf>,
    rx: Receiver<PathBuf>,
}

impl WatcherHandle {
    /// Start recursively watching `path` and return a handle plus a `Receiver`
    /// that yields absolute `PathBuf`s for changed files.
    pub fn start(path: &Path) -> Result<Self, VirtualizerError> {
        let (tx, rx): (Sender<PathBuf>, Receiver<PathBuf>) = crossbeam_channel::unbounded();

        let tx_clone = tx.clone();
        let mut watcher = notify::RecommendedWatcher::new(
            move |res: notify::Result<Event>| {
                if let Ok(event) = res {
                    for p in event.paths {
                        let _ = tx_clone.send(p);
                    }
                }
            },
            Config::default(),
        )
        .map_err(|e| VirtualizerError::SystemFault(e.to_string()))?;

        watcher
            .watch(path, RecursiveMode::Recursive)
            .map_err(|e| VirtualizerError::SystemFault(e.to_string()))?;

        Ok(WatcherHandle {
            _watcher: watcher,
            _tx: tx,
            rx,
        })
    }

    pub fn receiver(&self) -> &Receiver<PathBuf> {
        &self.rx
    }
}
