//! What this process is actually allowed to do.
//!
//! Every interesting OS feature is gated on a privilege: instant allocation needs
//! `SeManageVolumePrivilege`, large pages need `SeLockMemoryPrivilege`, reading a
//! file with a restrictive ACL needs `SeBackupPrivilege`, realtime I/O needs
//! `CAP_SYS_ADMIN`. Rather than making the user remember which flags to pass, we
//! probe once at startup and pick the best path that is actually available.
//!
//! The probe is a real attempt, not a guess: on Windows it enables the privileges
//! on the process token and records which ones stuck; on Linux it reads the
//! effective capability set.

use std::fmt;

#[derive(Clone, Copy, Debug, Default)]
/// What this process is actually allowed to do, probed once at startup.
pub struct Caps {
    /// The process is elevated (admin / uid 0).
    pub elevated: bool,
    /// Extending a file costs nothing: `SetFileValidData` or a `fallocate` that
    /// does not have to zero-fill.
    pub instant_alloc: bool,
    /// 2 MiB pages for the buffer pool.
    pub large_pages: bool,
    /// Can open files that the DACL would otherwise deny.
    pub backup: bool,
    /// Can write files whose DACL would otherwise deny, and restore ownership.
    pub restore: bool,
    /// Realtime I/O class.
    pub rt_io: bool,
    /// Realtime thread priority.
    pub rt_priority: bool,
    /// Pin pages into physical memory.
    pub mlock: bool,
    /// `O_NOATIME` / equivalent: read without touching atime.
    pub noatime: bool,
}

impl Caps {
    /// Human-readable tier used in the run banner.
    #[must_use]
    pub const fn tier(&self) -> &'static str {
        if self.elevated && self.instant_alloc && self.large_pages {
            "privileged"
        } else if self.elevated || self.backup || self.restore {
            "elevated"
        } else {
            "unprivileged"
        }
    }

    /// The features that are on, for the banner.
    #[must_use]
    pub fn enabled(&self) -> Vec<&'static str> {
        let mut v = Vec::new();
        if self.instant_alloc {
            v.push("instant-alloc");
        }
        if self.large_pages {
            v.push("large-pages");
        }
        if self.backup {
            v.push("backup-semantics");
        }
        if self.restore {
            v.push("restore-ownership");
        }
        if self.rt_io {
            v.push("rt-io");
        }
        if self.rt_priority {
            v.push("rt-priority");
        }
        if self.mlock {
            v.push("mlock");
        }
        if self.noatime {
            v.push("noatime");
        }
        v
    }
}

impl fmt::Display for Caps {
    fn fmt(
        &self,
        f: &mut fmt::Formatter<'_>,
    ) -> fmt::Result {
        let e = self.enabled();
        if e.is_empty() {
            write!(f, "{}", self.tier())
        } else {
            write!(f, "{} [{}]", self.tier(), e.join(", "))
        }
    }
}

static CAPS: std::sync::OnceLock<Caps> = std::sync::OnceLock::new();

/// Detect (once) and return the process capabilities.
pub fn get() -> &'static Caps {
    CAPS.get_or_init(crate::plat::detect_caps)
}

/// Run the detection eagerly, so the privileges are enabled before the first
/// `CreateFileW` that needs them.
#[must_use]
pub fn init() -> &'static Caps {
    get()
}
