//! Error type.

use std::fmt;
use std::io;
use std::path::Path;
use std::path::PathBuf;

/// Everything that can go wrong, with enough context to name the file and the
/// operation in a one-line message.
#[derive(Debug)]
pub enum Error {
    /// A system call failed on a specific path.
    Io {
        /// The file the operation was aimed at.
        path: PathBuf,
        /// The operation, as a short verb (`open`, `read`, `truncate`).
        op: &'static str,
        /// The underlying OS error, kept for [`Error::is_transient`].
        source: io::Error,
    },
    /// The request does not make sense before any I/O starts.
    Config(String),
    /// The destination does not match the source after the copy.
    Verify {
        /// The destination that failed the check.
        path: PathBuf,
        /// What did not match, in enough detail to tell the modes apart.
        detail: String,
    },
    /// The run was interrupted by a signal or `Ctrl-C`.
    Aborted,
    /// Anything else, already formatted.
    Other(String),
}

/// The crate's result alias.
pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    /// Wrap an OS error with the path and the operation that produced it.
    pub fn io(
        path: impl AsRef<Path>,
        op: &'static str,
        source: io::Error,
    ) -> Self {
        Self::Io {
            path: path.as_ref().to_path_buf(),
            op,
            source,
        }
    }

    /// A request that cannot be honoured: a missing source, a destination that
    /// is also the source, an impossible combination of options.
    pub fn config(msg: impl Into<String>) -> Self {
        Self::Config(msg.into())
    }

    /// The copy completed but the destination does not match the source.
    pub fn verify(
        path: impl AsRef<Path>,
        detail: impl Into<String>,
    ) -> Self {
        Self::Verify {
            path: path.as_ref().to_path_buf(),
            detail: detail.into(),
        }
    }

    /// A failure that does not fit the other variants, already formatted.
    pub fn other(msg: impl Into<String>) -> Self {
        Self::Other(msg.into())
    }

    /// True when the failure is worth retrying (transient device trouble).
    #[must_use]
    pub fn is_transient(&self) -> bool {
        match self {
            | Self::Io { source, .. } => {
                matches!(
                    source.kind(),
                    io::ErrorKind::Interrupted
                        | io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                        | io::ErrorKind::ResourceBusy
                )
            },
            | _ => false,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(
        &self,
        f: &mut fmt::Formatter<'_>,
    ) -> fmt::Result {
        match self {
            | Self::Io { path, op, source } => {
                write!(f, "{op} {}: {source}", path.display())
            },
            | Self::Config(m) => write!(f, "configuration: {m}"),
            | Self::Verify { path, detail } => {
                write!(f, "verification failed for {}: {detail}", path.display())
            },
            | Self::Aborted => write!(f, "aborted"),
            | Self::Other(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            | Self::Io { source, .. } => Some(source),
            | _ => None,
        }
    }
}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Self::Other(e.to_string())
    }
}
