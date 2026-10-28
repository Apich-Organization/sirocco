//! Integration tests for the platform layer.
//!
//! These are straight-line assertions against real files and a real reactor, not
//! shipped code, so the lints that ask production code to be explicit about
//! failure are turned off for the file: a test that cannot set up its fixture
//! has failed anyway, and `unwrap()` says so in one word. The same goes for the
//! offset arithmetic and slicing — those *are* the subject of the test.

#![allow(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::assert_is_empty,
    clippy::many_single_char_names,
    clippy::redundant_closure_for_method_calls,
    clippy::ref_as_ptr,
    clippy::semicolon_if_nothing_returned,
    clippy::single_match_else,
    clippy::too_many_lines,
    clippy::unreadable_literal,
    clippy::use_self,
    unsafe_code
)]

use std::io::Write;
use std::path::PathBuf;

use sirocco_sys::Access;
use sirocco_sys::Create;
use sirocco_sys::OpenOpts;
use sirocco_sys::RawFile;
use sirocco_sys::ReactorKind;
use sirocco_sys::align_up;
use sirocco_sys::buf::BufferPool;

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "sirocco-sys-{}-{}-{tag}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&p).unwrap();
        TempDir(p)
    }

    fn join(
        &self,
        name: &str,
    ) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn pattern(
    n: usize,
    seed: u64,
) -> Vec<u8> {
    let mut s = seed | 1;
    (0..n)
        .map(|_| {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            s as u8
        })
        .collect()
}

#[test]
fn open_write_read_roundtrip() {
    let d = TempDir::new("rw");
    let p = d.join("a.bin");
    let data = pattern(200_000, 42);
    {
        let f = RawFile::open(
            &p,
            &OpenOpts {
                access: Access::Write,
                create: Create::Create,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(f.pwrite(&data, 0).unwrap(), data.len());
        assert_eq!(f.size().unwrap(), data.len() as u64);
        f.flush().unwrap();
    }
    let f = RawFile::open(&p, &OpenOpts::read()).unwrap();
    let mut got = vec![0u8; data.len()];
    assert_eq!(f.pread(&mut got, 0).unwrap(), data.len());
    assert_eq!(got, data);
    // Read past EOF yields zero bytes rather than an error.
    assert_eq!(f.pread(&mut got, data.len() as u64).unwrap(), 0);
}

#[test]
fn direct_io_requires_alignment_but_works() {
    let d = TempDir::new("direct");
    let p = d.join("d.bin");
    let size = 4 << 20;
    let data = pattern(size, 7);
    let align = 4096usize;

    // A page-aligned scratch buffer, as the engine's pool provides.
    let pool = BufferPool::new(size, 2, align, false).unwrap();
    let mut buf = pool.try_take().unwrap();
    buf.as_mut_slice()[..size].copy_from_slice(&data);

    {
        let f = RawFile::open(
            &p,
            &OpenOpts {
                access: Access::Write,
                create: Create::Create,
                direct: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(f.is_direct());
        assert_eq!(f.alignment(), align);
        f.set_len(size as u64).unwrap();
        assert_eq!(f.pwrite(&buf.as_slice()[..size], 0).unwrap(), size);
    }

    let f = RawFile::open(
        &p,
        &OpenOpts {
            access: Access::Read,
            create: Create::Open,
            direct: true,
            ..Default::default()
        },
    )
    .unwrap();
    let mut out = pool.try_take().unwrap();
    assert_eq!(f.pread(&mut out.as_mut_slice()[..size], 0).unwrap(), size);
    assert_eq!(&out.as_slice()[..size], &data[..]);
}

#[test]
fn preallocate_then_truncate() {
    let d = TempDir::new("prealloc");
    let p = d.join("p.bin");
    let f = RawFile::open(
        &p,
        &OpenOpts {
            access: Access::Write,
            create: Create::Create,
            ..Default::default()
        },
    )
    .unwrap();
    let want = 1 << 20;
    // Either instant allocation or the caller falls back; both must report cleanly.
    let _ = f.prealloc_valid(want);
    f.set_len(want).unwrap();
    assert_eq!(f.size().unwrap(), want);
    f.set_len(1234).unwrap();
    assert_eq!(f.size().unwrap(), 1234);
}

#[test]
fn sparse_holes_and_allocated_ranges() {
    let d = TempDir::new("sparse");
    let p = d.join("s.bin");
    let f = RawFile::open(
        &p,
        &OpenOpts {
            access: Access::ReadWrite,
            create: Create::Create,
            sparse: true,
            ..Default::default()
        },
    )
    .unwrap();
    let size = 8 << 20;
    f.set_sparse_flag(true).unwrap();
    f.set_len(size).unwrap();
    f.pwrite(&[0xAB; 4096], 1 << 20).unwrap();
    f.flush().unwrap();

    let ranges = f.allocated_ranges(size).unwrap();
    let total: u64 = ranges.iter().map(|(_, l)| *l).sum();
    assert!(
        total < size / 2,
        "expected the file to stay mostly holey, allocated {total} of {size}"
    );
    // The hole still reads as zeros.
    let mut z = vec![0u8; 4096];
    f.pread(&mut z, 0).unwrap();
    assert!(z.iter().all(|&b| b == 0));
    f.set_sparse_flag(false).unwrap();
}

#[test]
fn extents_are_reported() {
    let d = TempDir::new("extents");
    let p = d.join("e.bin");
    let mut f = std::fs::File::create(&p).unwrap();
    f.write_all(&pattern(1 << 20, 3)).unwrap();
    f.sync_all().unwrap();
    drop(f);
    let rf = RawFile::open(&p, &OpenOpts::read()).unwrap();
    match rf.extents() {
        | Ok(e) => {
            // Non-empty on NTFS and on Linux filesystems with FIEMAP support.
            if !e.is_empty() {
                assert!(e.iter().all(|x| x.len > 0));
            }
        },
        | Err(_) => { /* filesystem without physical mapping; acceptable */ },
    }
}

#[test]
fn volume_info_is_sane() {
    let d = TempDir::new("vol");
    let v = sirocco_sys::volume_info(&d.0).unwrap();
    assert!(v.logical_sector >= 512);
    assert!(v.physical_sector >= v.logical_sector);
    assert!(v.total_bytes > 0);
    assert!(v.free_bytes <= v.total_bytes);
}

#[test]
fn topology_has_consistent_cores() {
    let t = sirocco_sys::Topology::detect();
    assert!(!t.cpus.is_empty());
    assert!(!t.cores.is_empty());
    let total: usize = t.cores.iter().map(|c| c.len()).sum();
    assert_eq!(total, t.cpus.len(), "every logical cpu belongs to one core");
    assert!(t.physical_count() <= t.logical_count());
}

fn reactor_roundtrip(kind: ReactorKind) {
    let d = TempDir::new(&format!("reactor-{kind:?}"));
    let p = d.join("r.bin");
    let align = 4096usize;
    let block = 64 << 10;
    let blocks = 16;
    let total = block * blocks;
    let data = pattern(total, 99);

    let pool = BufferPool::new(block, blocks * 2 + 4, align, false).unwrap();
    let mut rx = sirocco_sys::make_reactor(kind, 32, 4).unwrap();
    let mut comps = Vec::new();

    // --- write
    let wf = rx
        .open(
            &p,
            &OpenOpts {
                access: Access::Write,
                create: Create::Create,
                direct: true,
                ..Default::default()
            },
        )
        .unwrap();
    wf.set_len(align_up(total as u64, align as u64)).unwrap();

    let mut held = Vec::new();
    for i in 0..blocks {
        let mut b = pool.try_take().unwrap();
        b.as_mut_slice()[..block].copy_from_slice(&data[i * block..(i + 1) * block]);
        let ptr = b.ptr();
        unsafe {
            rx.write(&wf, ptr, block, (i * block) as u64, i as u64)
                .unwrap()
        };
        held.push(b);
        if held.len() == 8 {
            comps.clear();
            while comps.len() < held.len() {
                let n = rx.poll(held.len() - comps.len(), &mut comps).unwrap();
                if n == 0 {
                    break;
                }
            }
            assert_eq!(comps.len(), held.len());
            assert!(comps.iter().all(|c| c.is_ok()));
            held.clear();
        }
    }
    if !held.is_empty() {
        comps.clear();
        while comps.len() < held.len() {
            let n = rx.poll(held.len() - comps.len(), &mut comps).unwrap();
            if n == 0 {
                break;
            }
        }
        assert_eq!(comps.len(), held.len());
        assert!(comps.iter().all(|c| c.is_ok()));
    }
    drop(wf);
    std::fs::OpenOptions::new()
        .write(true)
        .open(&p)
        .unwrap()
        .set_len(total as u64)
        .unwrap();

    // --- read back, out of order, and compare
    let rf = rx
        .open(
            &p,
            &OpenOpts {
                access: Access::Read,
                create: Create::Open,
                direct: true,
                ..Default::default()
            },
        )
        .unwrap();
    let order: Vec<usize> = (0..blocks).rev().collect();
    let bufs: Vec<_> = (0..blocks).map(|_| pool.try_take().unwrap()).collect();
    for (n, &i) in order.iter().enumerate() {
        let ptr = bufs[n].ptr();
        unsafe {
            rx.read(&rf, ptr, block, (i * block) as u64, i as u64)
                .unwrap()
        };
    }
    // `poll` returns as soon as the queue is momentarily empty, so accumulate.
    comps.clear();
    while comps.len() < blocks {
        let n = rx.poll(blocks - comps.len(), &mut comps).unwrap();
        if n == 0 {
            break;
        }
    }
    assert_eq!(comps.len(), blocks, "{kind:?}: expected every completion");
    for c in &comps {
        assert!(c.is_ok(), "{kind:?}: completion failed: {c:?}");
        assert_eq!(c.bytes(), block);
    }
    for (n, &i) in order.iter().enumerate() {
        assert_eq!(
            &bufs[n].as_slice()[..block],
            &data[i * block..(i + 1) * block],
            "{kind:?}: block {i} differs"
        );
    }
}

#[test]
fn native_reactor_roundtrip() {
    reactor_roundtrip(ReactorKind::Native);
}

#[test]
fn blocking_reactor_roundtrip() {
    reactor_roundtrip(ReactorKind::Blocking);
}

#[test]
fn buffer_pool_exhausts_and_recovers() {
    let p = BufferPool::new(4096, 3, 4096, false).unwrap();
    let a = p.try_take().unwrap();
    let b = p.try_take().unwrap();
    let c = p.try_take().unwrap();
    assert!(p.try_take().is_none());
    drop(b);
    let d = p.try_take().unwrap();
    assert_ne!(d.ptr(), a.ptr());
    drop(a);
    drop(c);
    drop(d);
    assert_eq!(p.available(), 3);
}
