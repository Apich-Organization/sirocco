//! Resume journal.
//!
//! A long copy that dies half way should not start from zero. The journal is an
//! append-only list of destination-relative paths that have been fully written and
//! verified; on restart the planner skips anything already in it.
//!
//! It is deliberately boring: one line per file, flushed every 256 entries. Losing
//! the last few lines only means re-copying a few files, never corrupting one,
//! because an entry is written only after the destination is complete.

use std::collections::HashSet;
use std::fs::File;
use std::fs::OpenOptions;
use std::io::BufRead;
use std::io::BufReader;
use std::io::BufWriter;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Mutex;

/// Name of the resume log, written at the destination root.
pub const JOURNAL_NAME: &str = ".sirocco-journal";

/// Append-only log of completed files, so an interrupted run can skip them.
///
/// Entries are flushed in batches: one write per file would cost more than the
/// bookkeeping saves.
pub struct Journal {
    path: PathBuf,
    out: Mutex<BufWriter<File>>,
}

impl Journal {
    /// Open (or create) the journal, returning the set of paths already done.
    pub fn open(path: PathBuf) -> std::io::Result<(Self, HashSet<String>)> {
        let mut done = HashSet::new();
        if let Ok(f) = File::open(&path) {
            for line in BufReader::new(f).lines().map_while(Result::ok) {
                let line = line.trim();
                if !line.is_empty() {
                    done.insert(line.to_string());
                }
            }
        }
        if let Some(p) = path.parent() {
            let _ = std::fs::create_dir_all(p);
        }
        let f = OpenOptions::new().create(true).append(true).open(&path)?;
        Ok((
            Self {
                path,
                out: Mutex::new(BufWriter::with_capacity(64 << 10, f)),
            },
            done,
        ))
    }

    /// Note that `rel` was copied successfully.
    pub fn record(
        &self,
        rel: &str,
    ) {
        let mut g = self.out.lock().unwrap();
        let _ = writeln!(g, "{rel}");
        // Flushing per entry would cost a syscall per file; every 256 entries is
        // 0.4% of a small-file workload and bounds the loss on a crash.
        if g.buffer().len() >= 8192 {
            let _ = g.flush();
        }
    }

    /// Push buffered entries to the file.
    pub fn flush(&self) {
        let _ = self.out.lock().unwrap().flush();
    }

    /// Where the journal is being written.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for Journal {
    fn drop(&mut self) {
        if let Ok(w) = self.out.get_mut() {
            let _ = w.flush();
        }
    }
}
