//! Resume journal.
//!
//! A long copy that dies half way should not start from zero. The journal is an
//! append-only list of destination-relative paths that have been fully written and
//! verified; on restart the planner skips anything already in it.
//!
//! It is deliberately boring: one line per file, flushed every 256 entries. Losing
//! the last few lines only means re-copying a few files, never corrupting one,
//! because an entry is written only after the destination is complete.
//!
//! Each entry carries the source's size and mtime, and the file starts with the
//! source root it belongs to. A path alone is not enough: a source edited in place
//! between the two runs keeps its size, and a different job writing into the same
//! destination would otherwise inherit entries for files it never copied.

use std::collections::HashMap;
use std::fs::File;
use std::fs::OpenOptions;
use std::io::BufRead;
use std::io::BufReader;
use std::io::BufWriter;
use std::io::Seek;
use std::io::SeekFrom;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Mutex;

/// Name of the resume log, written at the destination root.
pub const JOURNAL_NAME: &str = ".sirocco-journal";

/// First line of a journal. The root follows it, tab separated.
const HEADER: &str = "#sirocco-journal 2";

/// What an entry remembers about the source it was copied from.
pub type Entry = (u64, u128);

/// Append-only log of completed files, so an interrupted run can skip them.
///
/// Entries are flushed in batches: one write per file would cost more than the
/// bookkeeping saves.
pub struct Journal {
    path: PathBuf,
    out: Mutex<BufWriter<File>>,
}

impl Journal {
    /// Open (or create) the journal, returning the entries already done.
    ///
    /// A journal written for a different source root is discarded rather than
    /// merged: its paths describe a tree this run knows nothing about.
    pub fn open(
        path: PathBuf,
        src_root: &Path,
    ) -> std::io::Result<(Self, HashMap<String, Entry>)> {
        let mut done = HashMap::new();
        let mut mine = false;
        if let Ok(f) = File::open(&path) {
            let mut lines = BufReader::new(f).lines().map_while(Result::ok);
            if let Some(first) = lines.next() {
                let first = first.trim_end();
                let root = first
                    .strip_prefix(HEADER)
                    .map(|rest| rest.trim_start_matches('\t'));
                mine = root == Some(src_root.to_string_lossy().as_ref());
            }
            if mine {
                for line in lines {
                    let mut parts = line.trim_end().split('\t');
                    let (Some(rel), Some(size), Some(mtime)) =
                        (parts.next(), parts.next(), parts.next())
                    else {
                        continue;
                    };
                    if rel.is_empty() {
                        continue;
                    }
                    if let (Ok(size), Ok(mtime)) = (size.parse::<u64>(), mtime.parse::<u128>()) {
                        done.insert(rel.to_string(), (size, mtime));
                    }
                }
            }
        }
        if let Some(p) = path.parent() {
            let _ = std::fs::create_dir_all(p);
        }
        // Not `append(true)`: on Windows that opens with FILE_APPEND_DATA only, and
        // `SetEndOfFile` (needed to discard a journal from another run) is refused.
        // Seeking to the end once gives the same behaviour with a normal handle.
        let mut f = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&path)?;
        if !mine {
            // Start over: the old lines describe a tree this run does not have,
            // and appending to them would mix two jobs in one file.
            f.set_len(0)?;
            done.clear();
            let mut g = BufWriter::new(&f);
            let _ = writeln!(g, "{HEADER}\t{}", src_root.display());
            let _ = g.flush();
        }
        f.seek(SeekFrom::End(0))?;
        Ok((
            Self {
                path,
                out: Mutex::new(BufWriter::with_capacity(64 << 10, f)),
            },
            done,
        ))
    }

    /// Note that `rel` was copied successfully, from a source of this size and
    /// mtime.
    pub fn record(
        &self,
        rel: &str,
        size: u64,
        mtime_ns: u128,
    ) {
        let mut g = self.out.lock().unwrap();
        let _ = writeln!(g, "{rel}\t{size}\t{mtime_ns}");
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

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_journal(tag: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("sirocco-journal-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_file(&p);
        p
    }

    /// The entries have to survive a reopen for the same source, or resume does
    /// nothing at all.
    #[test]
    fn entries_survive_a_reopen() {
        let path = temp_journal("reopen");
        let root = Path::new("/src");
        {
            let (j, done) = Journal::open(path.clone(), root).unwrap();
            assert!(done.is_empty());
            j.record("a/b.bin", 42, 1234);
            j.record("c.bin", 7, 99);
            j.flush();
        }
        let (j, done) = Journal::open(path.clone(), root).unwrap();
        assert_eq!(done.get("a/b.bin"), Some(&(42, 1234)));
        assert_eq!(done.get("c.bin"), Some(&(7, 99)));
        drop(j);
        let _ = std::fs::remove_file(&path);
    }

    /// A journal written for another source root describes a tree this run does
    /// not have; inheriting its entries would skip files that were never copied.
    #[test]
    fn a_journal_for_another_root_is_discarded() {
        let path = temp_journal("other-root");
        {
            let (j, _) = Journal::open(path.clone(), Path::new("/one")).unwrap();
            j.record("x.bin", 1, 2);
            j.flush();
        }
        let (j, done) = Journal::open(path.clone(), Path::new("/two")).unwrap();
        assert!(done.is_empty(), "entries leaked across source roots");
        drop(j);
        let _ = std::fs::remove_file(&path);
    }
}
