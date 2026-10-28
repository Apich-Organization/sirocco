//! Portable fallback reactor: a small thread pool doing positioned I/O.
//!
//! Used when the native backend is unavailable (old kernel, no `io_uring`,
//! sandboxed environment) and as the reference implementation the fast paths are
//! tested against.
//!
//! Each worker owns its *own* file handles. That is not an optimisation, it is a
//! correctness requirement: several Windows storage stacks (cloud-attached
//! volumes, some minifilters) return zero bytes when two threads issue overlapped
//! `ReadFile` calls on the same handle at different offsets — the offset in
//! `OVERLAPPED` ends up ignored and the shared file pointer is used instead. One
//! handle per thread sidesteps the whole class of problem, and it is what the IOCP
//! backend does anyway.

use std::collections::HashMap;
use std::collections::VecDeque;
use std::io;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Condvar;
use std::sync::Mutex;
use std::thread::JoinHandle;

use crate::Access;
use crate::Completion;
use crate::Create;
use crate::OpenOpts;
use crate::RawFile;
use crate::Reactor;

#[derive(Clone, PartialEq, Eq, Hash)]
struct HandleKey {
    path: PathBuf,
    write: bool,
    direct: bool,
}

struct Job {
    path: PathBuf,
    direct: bool,
    buf: *mut u8,
    len: usize,
    off: u64,
    user: u64,
    write: bool,
}

// SAFETY: the caller guarantees the buffer stays alive until the completion is
// reaped, which is the documented contract of `Reactor`.
unsafe impl Send for Job {}

struct Shared {
    jobs: Mutex<VecDeque<Job>>,
    jobs_cv: Condvar,
    done: Mutex<VecDeque<Completion>>,
    done_cv: Condvar,
    stop: Mutex<bool>,
    in_flight: Mutex<usize>,
    /// One handle cache per worker thread.
    handles: Vec<Mutex<HashMap<HandleKey, RawFile>>>,
}

const HANDLES_PER_WORKER: usize = 64;

impl Shared {
    fn wake_workers(&self) {
        let _g = self.jobs.lock().unwrap();
        self.jobs_cv.notify_all();
    }
}

/// Portable backend: a small thread pool doing `pread`/`pwrite`.
///
/// Always available, never the fastest. Used on filesystems where the native
/// backend cannot work (some network mounts reject `O_DIRECT` or `io_uring`), and
/// as the reference implementation the tests compare against.
pub struct BlockingReactor {
    shared: Arc<Shared>,
    threads: Vec<JoinHandle<()>>,
}

impl BlockingReactor {
    /// Start a pool of `threads` workers (at least one).
    #[must_use]
    pub fn new(threads: usize) -> Self {
        let threads = threads.max(1);
        let shared = Arc::new(Shared {
            jobs: Mutex::new(VecDeque::new()),
            jobs_cv: Condvar::new(),
            done: Mutex::new(VecDeque::new()),
            done_cv: Condvar::new(),
            stop: Mutex::new(false),
            in_flight: Mutex::new(0),
            handles: (0..threads).map(|_| Mutex::new(HashMap::new())).collect(),
        });
        let mut hs = Vec::with_capacity(threads);
        for w in 0..threads {
            let s = shared.clone();
            let h = std::thread::Builder::new()
                .name(format!("sirocco-blk-{w}"))
                .spawn(move || worker(&s, w))
                .expect("spawn blocking io worker");
            hs.push(h);
        }
        Self { shared, threads: hs }
    }

    fn enqueue(
        &self,
        job: Job,
    ) {
        *self.shared.in_flight.lock().unwrap() += 1;
        self.shared.jobs.lock().unwrap().push_back(job);
        self.shared.jobs_cv.notify_one();
    }
}

fn worker(
    s: &Arc<Shared>,
    w: usize,
) {
    loop {
        let job = {
            let mut q = s.jobs.lock().unwrap();
            loop {
                if *s.stop.lock().unwrap() {
                    return;
                }
                if let Some(j) = q.pop_front() {
                    break j;
                }
                q = s.jobs_cv.wait(q).unwrap();
            }
        };
        let completion = run_job(s, w, &job);
        s.done.lock().unwrap().push_back(completion);
        s.done_cv.notify_one();
    }
}

// The cache guard has to live as long as the `&RawFile` borrowed from it, which is
// until the I/O finishes; tightening the drop would not compile.
#[allow(clippy::significant_drop_tightening)]
fn run_job(
    s: &Shared,
    w: usize,
    job: &Job,
) -> Completion {
    let key = HandleKey {
        path: job.path.clone(),
        write: job.write,
        direct: job.direct,
    };
    // The guard is held across the open below on purpose: the cache must not
    // hand the same handle to two workers, and opening is fast.
    let mut cache = s.handles[w].lock().unwrap();
    if !cache.contains_key(&key) {
        if cache.len() >= HANDLES_PER_WORKER {
            cache.clear();
        }
        let opts = OpenOpts {
            access: if job.write {
                Access::Write
            } else {
                Access::Read
            },
            create: Create::Open,
            direct: job.direct,
            sequential: true,
            ..Default::default()
        };
        match RawFile::open(&job.path, &opts) {
            | Ok(f) => {
                cache.insert(key.clone(), f);
            },
            | Err(e) => {
                return Completion {
                    user: job.user,
                    res: -i64::from(e.raw_os_error().unwrap_or(5).max(1)),
                };
            },
        }
    }
    let f = cache.get(&key).unwrap();
    let buf = unsafe { std::slice::from_raw_parts_mut(job.buf, job.len) };
    let res = if job.write {
        f.pwrite(buf, job.off)
    } else {
        f.pread(buf, job.off)
    };
    match res {
        | Ok(n) => {
            Completion {
                user: job.user,
                res: n as i64,
            }
        },
        | Err(e) => {
            Completion {
                user: job.user,
                res: -i64::from(e.raw_os_error().unwrap_or(5).max(1)),
            }
        },
    }
}

impl Reactor for BlockingReactor {
    fn open(
        &mut self,
        path: &Path,
        opts: &OpenOpts,
    ) -> io::Result<RawFile> {
        RawFile::open(path, opts)
    }

    unsafe fn read(
        &mut self,
        f: &RawFile,
        buf: *mut u8,
        len: usize,
        off: u64,
        user: u64,
    ) -> io::Result<()> {
        self.enqueue(Job {
            path: f.path().to_path_buf(),
            direct: f.is_direct(),
            buf,
            len,
            off,
            user,
            write: false,
        });
        Ok(())
    }

    unsafe fn write(
        &mut self,
        f: &RawFile,
        buf: *const u8,
        len: usize,
        off: u64,
        user: u64,
    ) -> io::Result<()> {
        self.enqueue(Job {
            path: f.path().to_path_buf(),
            direct: f.is_direct(),
            buf: buf.cast_mut(),
            len,
            off,
            user,
            write: true,
        });
        Ok(())
    }

    fn poll(
        &mut self,
        want: usize,
        out: &mut Vec<Completion>,
    ) -> io::Result<usize> {
        let mut total = 0;
        loop {
            {
                let mut d = self.shared.done.lock().unwrap();
                while let Some(c) = d.pop_front() {
                    *self.shared.in_flight.lock().unwrap() -= 1;
                    out.push(c);
                    total += 1;
                }
            }
            if total >= want || *self.shared.in_flight.lock().unwrap() == 0 {
                break;
            }
            let d = self.shared.done.lock().unwrap();
            let _ = self
                .shared
                .done_cv
                .wait_timeout(d, std::time::Duration::from_millis(20))
                .unwrap();
        }
        Ok(total)
    }

    fn in_flight(&self) -> usize {
        *self.shared.in_flight.lock().unwrap()
    }

    fn name(&self) -> &'static str {
        "blocking"
    }
}

impl Drop for BlockingReactor {
    fn drop(&mut self) {
        *self.shared.stop.lock().unwrap() = true;
        self.shared.wake_workers();
        for h in self.threads.drain(..) {
            let _ = h.join();
        }
    }
}
