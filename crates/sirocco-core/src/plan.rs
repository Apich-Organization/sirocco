//! Job description types shared by the scanner, the engine and the CLI.

use std::path::PathBuf;
use std::time::SystemTime;

/// What the run should do.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Mode {
    /// Copy source to destination, leaving the source alone.
    #[default]
    Copy,
    /// Copy then delete the source (same-volume renames are used when possible).
    Move,
    /// Delete the matched entries at the source.
    Delete,
    /// Mirror: copy what is missing or different, delete what is extra.
    Sync,
}

impl Mode {
    /// The spelling used by `--mode` and by the JSON report.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            | Self::Copy => "copy",
            | Self::Move => "move",
            | Self::Delete => "delete",
            | Self::Sync => "sync",
        }
    }

    /// Parse a mode name or its common alias. `None` means "not a mode".
    #[must_use]
    pub fn parses(s: &str) -> Option<Self> {
        Some(match s {
            | "copy" | "cp" => Self::Copy,
            | "move" | "mv" => Self::Move,
            | "delete" | "rm" | "del" => Self::Delete,
            | "sync" | "mirror" => Self::Sync,
            | _ => return None,
        })
    }
}

/// When to replace an existing destination file.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Overwrite {
    /// Replace when the size or the modification time differ.
    #[default]
    IfDifferent,
    /// Never touch an existing destination.
    Never,
    /// Always rewrite.
    Always,
    /// Replace only when the source is newer.
    IfNewer,
    /// Replace only when the source is older (restore an old snapshot).
    IfOlder,
    /// Replace only when sizes differ.
    IfDifferentSize,
    /// Compare contents (hashes) and replace only when they differ.
    IfContentDiffers,
}

impl Overwrite {
    /// The spelling used by `--overwrite` and by the JSON report.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            | Self::IfDifferent => "if-different",
            | Self::Never => "never",
            | Self::Always => "always",
            | Self::IfNewer => "if-newer",
            | Self::IfOlder => "if-older",
            | Self::IfDifferentSize => "if-size",
            | Self::IfContentDiffers => "if-content",
        }
    }

    /// Parse a policy name or its common alias. `None` means "not a policy".
    #[must_use]
    pub fn parses(s: &str) -> Option<Self> {
        Some(match s {
            | "if-different" | "different" | "default" => Self::IfDifferent,
            | "never" | "skip" => Self::Never,
            | "always" | "force" => Self::Always,
            | "if-newer" | "newer" | "update" => Self::IfNewer,
            | "if-older" | "older" => Self::IfOlder,
            | "if-size" | "size" => Self::IfDifferentSize,
            | "if-content" | "content" | "checksum" => Self::IfContentDiffers,
            | _ => return None,
        })
    }
}

/// How thoroughly to verify the result.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum VerifyMode {
    /// Trust the write path; check nothing.
    #[default]
    None,
    /// Compare final file sizes only (free).
    Size,
    /// Compare size and modification time.
    SizeMtime,
    /// Re-read the destination and compare a strong hash of the whole file.
    Hash,
    /// Hash the first, middle and last chunk of every file.
    Sample,
}

impl VerifyMode {
    /// The spelling used by `--verify` and by the JSON report.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            | Self::None => "none",
            | Self::Size => "size",
            | Self::SizeMtime => "size-mtime",
            | Self::Hash => "hash",
            | Self::Sample => "sample",
        }
    }

    /// Parse a verification level or its common alias.
    #[must_use]
    pub fn parses(s: &str) -> Option<Self> {
        Some(match s {
            | "none" | "off" => Self::None,
            | "size" => Self::Size,
            | "size-mtime" | "mtime" | "meta" => Self::SizeMtime,
            | "hash" | "full" | "on" => Self::Hash,
            | "sample" => Self::Sample,
            | _ => return None,
        })
    }

    /// True when the mode has to read the destination back, which is what makes
    /// it incompatible with the kernel-copy fast path.
    #[must_use]
    pub const fn reads_dest(self) -> bool {
        matches!(self, Self::Hash | Self::Sample)
    }
}

/// What a scanned path is. Determines which copy path it takes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryKind {
    /// A regular file: copied through the pipeline or the kernel fast path.
    File,
    /// A directory: created, never read.
    Dir,
    /// A symbolic link: recreated as a link when symlinks are not followed.
    Symlink,
    /// A device, socket, FIFO or anything else the engine will not copy.
    Other,
}

/// One filesystem object to process.
#[derive(Clone, Debug)]
pub struct Entry {
    /// Absolute source path.
    pub src: PathBuf,
    /// Absolute destination path, already mapped through the source root.
    pub dst: PathBuf,
    /// Path relative to the source root (used for filters and reporting).
    pub rel: PathBuf,
    /// Size in bytes. Zero for anything that is not a regular file.
    pub size: u64,
    /// Modification time, when the filesystem reports one.
    pub mtime: Option<SystemTime>,
    /// What this path is.
    pub kind: EntryKind,
    /// Ordering key: MFT record index / inode / physical block, depending on
    /// `--order`. Zero when unused.
    pub order: u64,
    /// File identity within the volume (inode / NTFS file id). 0 when unknown.
    pub file_id: u64,
    /// Hard link count. > 1 means other paths share this inode.
    pub links: u32,
    /// Permission bits (Unix mode) or Windows attributes.
    pub mode: u32,
}

impl Entry {
    /// True for a regular file.
    #[must_use]
    pub fn is_file(&self) -> bool {
        self.kind == EntryKind::File
    }
}

/// Order in which files are fed to the pipeline.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Order {
    /// Leave the directory-walk order alone.
    #[default]
    Walk,
    /// Small files first — drains the aggregation batch early.
    SizeAsc,
    /// Large files first — keeps the queue deep for the whole run.
    SizeDesc,
    /// Physical layout order (MFT record index on NTFS, inode on ext4).
    Physical,
    /// Lexicographic.
    Name,
}

impl Order {
    /// The spelling used by `--order` and by the JSON report.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            | Self::Walk => "walk",
            | Self::SizeAsc => "size-asc",
            | Self::SizeDesc => "size-desc",
            | Self::Physical => "physical",
            | Self::Name => "name",
        }
    }

    /// Parse an ordering name or its common alias.
    #[must_use]
    pub fn parses(s: &str) -> Option<Self> {
        Some(match s {
            | "walk" | "none" => Self::Walk,
            | "size-asc" | "size" | "small" => Self::SizeAsc,
            | "size-desc" | "large" => Self::SizeDesc,
            | "physical" | "mft" | "disk" => Self::Physical,
            | "name" => Self::Name,
            | _ => return None,
        })
    }
}

/// Scanner configuration.
#[derive(Clone, Debug)]
#[allow(clippy::struct_excessive_bools)]
pub struct ScanCfg {
    /// Descend into symbolic links instead of recreating them.
    pub follow_symlinks: bool,
    /// Stay on the source volume; skip mount points inside the tree.
    pub one_filesystem: bool,
    /// Glob patterns to skip. A pattern without `/` is also tried against the
    /// file name alone.
    pub exclude: Vec<String>,
    /// Glob patterns to keep. Empty means "everything not excluded".
    pub include: Vec<String>,
    /// Directory depth limit below the source root.
    pub max_depth: usize,
    /// Order in which entries are fed to the pipeline.
    pub order: Order,
    /// Keep directories that contain no files. Off mirrors `cp`'s pruning.
    pub include_empty_dirs: bool,
    /// Collect hard link identity during the scan. On Windows that costs one
    /// `CreateFileW` per file, so it is only done when links are preserved.
    pub collect_links: bool,
}

impl Default for ScanCfg {
    fn default() -> Self {
        Self {
            follow_symlinks: false,
            one_filesystem: false,
            exclude: Vec::new(),
            include: Vec::new(),
            max_depth: usize::MAX,
            order: Order::Walk,
            include_empty_dirs: true,
            collect_links: true,
        }
    }
}

/// A scanned set of entries plus a few aggregates.
#[derive(Clone, Debug, Default)]
pub struct Plan {
    /// Everything the walk found, in walk order until [`Plan::sort`] runs.
    pub entries: Vec<Entry>,
    /// Sum of the file sizes, used for the progress denominator.
    pub total_bytes: u64,
    /// Number of regular files.
    pub total_files: u64,
    /// Number of directories, including the root.
    pub total_dirs: u64,
    /// Entries the walk could not read. Non-zero means the plan is incomplete,
    /// which matters for `--delete-extra`: an unseen directory must never be
    /// mistaken for one that no longer exists.
    pub scan_errors: u64,
}

impl Plan {
    /// Append an entry and keep the aggregates in step with it.
    pub fn push(
        &mut self,
        e: Entry,
    ) {
        match e.kind {
            | EntryKind::File => {
                self.total_bytes += e.size;
                self.total_files += 1;
            },
            | EntryKind::Dir => self.total_dirs += 1,
            | _ => {},
        }
        self.entries.push(e);
    }

    /// Reorder in place. `Physical` sorts by the pre-computed ordering key so the
    /// reads land on the platter in the same order the data sits on it.
    pub fn sort(
        &mut self,
        order: Order,
    ) {
        match order {
            | Order::Walk => {},
            | Order::SizeAsc => self.entries.sort_by_key(|e| e.size),
            | Order::SizeDesc => self.entries.sort_by_key(|e| std::cmp::Reverse(e.size)),
            | Order::Physical => self.entries.sort_by_key(|e| e.order),
            | Order::Name => self.entries.sort_by(|a, b| a.rel.cmp(&b.rel)),
        }
        // Directories must still be created before their contents.
        self.entries.sort_by_key(|e| (e.kind != EntryKind::Dir, 0));
    }
}
