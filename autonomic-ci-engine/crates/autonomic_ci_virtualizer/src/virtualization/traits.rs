use std::error::Error;
use std::fmt;
use std::io;
use std::path::PathBuf;

/// Configuration for an overlay-style virtual workspace.
#[derive(Debug, Clone, Default)]
pub struct VirtualEnvConfig {
    pub lower_dir: PathBuf,
    pub upper_dir: PathBuf,
    pub merged_dir: PathBuf,
    pub work_dir: PathBuf,
}

/// Report produced after a successful commit of upper-layer changes.
#[derive(Debug, Clone, Default)]
pub struct CommitReport {
    pub files_written: Vec<PathBuf>,
    pub bytes_mutated: u64,
}

/// Error type representing I/O failures and low-level system faults.
#[derive(Debug)]
pub enum VirtualizerError {
    Io(io::Error),
    SystemFault(String),
}

impl fmt::Display for VirtualizerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            VirtualizerError::Io(e) => write!(f, "virtualizer I/O error: {e}"),
            VirtualizerError::SystemFault(msg) => write!(f, "virtualizer system fault: {msg}"),
        }
    }
}

impl Error for VirtualizerError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            VirtualizerError::Io(e) => Some(e),
            VirtualizerError::SystemFault(_) => None,
        }
    }
}

impl From<io::Error> for VirtualizerError {
    fn from(err: io::Error) -> Self {
        VirtualizerError::Io(err)
    }
}

/// Primary contract for mounting, synchronizing, and tearing down a virtual workspace.
#[allow(async_fn_in_trait)]
pub trait WorkspaceVirtualizer {
    async fn initialize(&self, config: &VirtualEnvConfig) -> Result<(), VirtualizerError>;
    async fn mount(&self, config: &VirtualEnvConfig) -> Result<(), VirtualizerError>;
    async fn synchronize_upper(&self, config: &VirtualEnvConfig) -> Result<(), VirtualizerError>;
    async fn commit(&self, config: &VirtualEnvConfig) -> Result<CommitReport, VirtualizerError>;
    async fn teardown(&self, config: &VirtualEnvConfig) -> Result<(), VirtualizerError>;
}
