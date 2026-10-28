//! CPU placement.
//!
//! Getting this right is mostly about three rules, in order of importance:
//!
//! 1. **Never put a reader and a writer on the same physical core.** They compete
//!    for the same execution ports, and the pipeline stalls exactly when both
//!    halves are busy — which is always.
//! 2. **Keep them on the NUMA node that owns the device.** Crossing a socket costs
//!    more than the I/O itself on a 2-socket box.
//! 3. **Prefer performance cores on hybrid CPUs.** The I/O threads do almost no
//!    compute, but the submission path is latency-sensitive, and an E-core wakes up
//!    slower.
//!
//! SMT siblings are used only after every physical core in the node is taken, and
//! even then a sibling is only given to a thread of the *same* role, so rule 1
//! still holds.

use crate::Topology;

/// Where one pipeline thread should run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CpuTarget {
    /// Index into [`Topology::cpus`].
    pub cpu: usize,
    /// NUMA node that owns the CPU.
    pub node: u16,
    /// 0 for the first thread on a core, 1 for its SMT sibling, and so on.
    pub smt_level: u8,
}

/// A complete placement, one entry per thread.
#[derive(Clone, Debug, Default)]
pub struct AffinityPlan {
    /// Where each reader should be pinned.
    pub readers: Vec<CpuTarget>,
    /// Where each writer should be pinned.
    pub writers: Vec<CpuTarget>,
    /// Node the placement was restricted to, if any.
    pub node: Option<u16>,
    /// True when the request could not be satisfied without sharing a logical CPU.
    pub oversubscribed: bool,
}

impl AffinityPlan {
    /// True when nothing could be placed (no topology, or zero threads asked for).
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.readers.is_empty() && self.writers.is_empty()
    }
}

/// Build a placement for `readers` + `writers` threads.
///
/// `node` restricts the search to one NUMA node; `cpu_list` (if given) restricts
/// it to an explicit set of logical CPUs, which wins over everything else.
#[must_use]
pub fn plan(
    topo: &Topology,
    readers: usize,
    writers: usize,
    node: Option<u16>,
    cpu_list: Option<&[usize]>,
) -> AffinityPlan {
    let want_r = readers;
    let want_w = writers;
    if want_r + want_w == 0 || topo.cores.is_empty() {
        return AffinityPlan::default();
    }

    // --- candidate cores, filtered and ordered
    let allowed: Option<Vec<bool>> = cpu_list.map(|list| {
        let mut v = vec![false; topo.cpus.len()];
        for &c in list {
            if c < v.len() {
                v[c] = true;
            }
        }
        v
    });

    let mut cores: Vec<usize> = (0..topo.cores.len())
        .filter(|&ci| {
            let cpus = &topo.cores[ci];
            if cpus.is_empty() {
                return false;
            }
            if let Some(n) = node
                && topo.cpus[cpus[0]].node != n
            {
                return false;
            }
            if let Some(a) = &allowed
                && !cpus.iter().any(|&c| a[c])
            {
                return false;
            }
            true
        })
        .collect();

    if cores.is_empty() {
        // Nothing matched: fall back to every core so we still place the threads.
        cores = (0..topo.cores.len())
            .filter(|&c| !topo.cores[c].is_empty())
            .collect();
    }

    // Performance cores first (EfficiencyClass is 0 on non-hybrid CPUs, so this
    // is a no-op there), then by node locality, then by index for determinism.
    let eff = |ci: usize| -> u8 { topo.core_efficiency.get(ci).copied().unwrap_or(0) };
    cores.sort_by_key(|&ci| {
        let c = &topo.cores[ci];
        (std::cmp::Reverse(eff(ci)), topo.cpus[c[0]].node, c[0])
    });

    // --- split the cores between the two roles
    let n_cores = cores.len();
    let total = want_r + want_w;
    let reader_cores_n = if want_w == 0 {
        n_cores
    } else if want_r == 0 {
        0
    } else if total <= n_cores {
        want_r.min(n_cores - 1)
    } else if n_cores <= 1 {
        // One core: both roles share it, there is nothing to split.
        n_cores
    } else {
        // Fewer cores than threads: give each role at least one core, split
        // proportionally.
        ((n_cores * want_r) / total).clamp(1, n_cores - 1)
    };
    let (reader_cores, writer_cores) = cores.split_at(reader_cores_n.min(n_cores));

    // --- fill level by level: all level-0 slots first, then SMT siblings
    let mut out = AffinityPlan {
        node,
        ..AffinityPlan::default()
    };
    let max_level = topo.cores.iter().map(std::vec::Vec::len).max().unwrap_or(1);
    let mut oversubscribed = false;

    for level in 0..max_level {
        for &ci in reader_cores {
            if out.readers.len() >= want_r {
                break;
            }
            if let Some(&cpu) = topo.cores[ci].get(level) {
                out.readers.push(CpuTarget {
                    cpu,
                    node: topo.cpus[cpu].node,
                    smt_level: level as u8,
                });
            }
        }
        for &ci in writer_cores {
            if out.writers.len() >= want_w {
                break;
            }
            if let Some(&cpu) = topo.cores[ci].get(level) {
                out.writers.push(CpuTarget {
                    cpu,
                    node: topo.cpus[cpu].node,
                    smt_level: level as u8,
                });
            }
        }
    }

    // --- still short? wrap around logical CPUs (sharing a core, last resort)
    let mut next = 0usize;
    while out.readers.len() < want_r {
        let ci = cores[next % n_cores];
        let cpu = topo.cores[ci][0];
        out.readers.push(CpuTarget {
            cpu,
            node: topo.cpus[cpu].node,
            smt_level: 0,
        });
        oversubscribed = true;
        next += 1;
    }
    next = 0;
    while out.writers.len() < want_w {
        let ci = cores[n_cores - 1 - (next % n_cores)];
        let cpu = topo.cores[ci][0];
        out.writers.push(CpuTarget {
            cpu,
            node: topo.cpus[cpu].node,
            smt_level: 0,
        });
        oversubscribed = true;
        next += 1;
    }

    out.oversubscribed = oversubscribed;
    out
}

/// Parse a `--cpu-list` value such as `0-3,8,10-11`.
#[must_use]
pub fn parse_cpu_list(s: &str) -> Option<Vec<usize>> {
    let mut out = Vec::new();
    for part in s.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        match part.split_once('-') {
            | Some((a, b)) => {
                let a: usize = a.trim().parse().ok()?;
                let b: usize = b.trim().parse().ok()?;
                if a > b || b - a > 4096 {
                    return None;
                }
                out.extend(a..=b);
            },
            | None => out.push(part.parse().ok()?),
        }
    }
    out.sort_unstable();
    out.dedup();
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CpuInfo;

    /// 4 physical cores, 2-way SMT, one NUMA node.
    fn fake(
        smt: usize,
        nodes: u16,
    ) -> Topology {
        let mut t = Topology::default();
        let mut cpus = Vec::new();
        let mut cores = Vec::new();
        for ci in 0..4 {
            let mut group = Vec::new();
            for s in 0..smt {
                group.push(cpus.len());
                cpus.push(CpuInfo {
                    group: 0,
                    index: (ci * smt + s) as u8,
                    id: (ci * smt + s) as u16,
                    node: (ci as u16) % nodes,
                    core: ci as u16,
                });
            }
            cores.push(group);
        }
        t.cpus = cpus;
        t.cores = cores;
        t.nodes = vec![Vec::new(); nodes as usize];
        for (i, c) in t.cpus.iter().enumerate() {
            t.nodes[c.node as usize].push(i);
        }
        t.core_efficiency = vec![0; 4];
        t
    }

    #[test]
    fn readers_and_writers_never_share_a_core() {
        for (r, w) in [(1, 1), (2, 2), (3, 1), (1, 3), (4, 0), (0, 4)] {
            let t = fake(2, 1);
            let p = plan(&t, r, w, None, None);
            assert_eq!(p.readers.len(), r);
            assert_eq!(p.writers.len(), w);
            let core_of = |cpu: usize| t.cpus[cpu].core;
            for a in &p.readers {
                for b in &p.writers {
                    assert_ne!(
                        core_of(a.cpu),
                        core_of(b.cpu),
                        "r={r} w={w}: cpu {} and {} share core {}",
                        a.cpu,
                        b.cpu,
                        core_of(a.cpu)
                    );
                }
            }
        }
    }

    #[test]
    fn uses_smt_siblings_before_sharing() {
        let t = fake(2, 1);
        // 8 threads, 4 cores, 8 logical: exactly one per logical CPU, no sharing.
        let p = plan(&t, 4, 4, None, None);
        assert!(!p.oversubscribed);
        let mut all: Vec<usize> = p
            .readers
            .iter()
            .chain(p.writers.iter())
            .map(|c| c.cpu)
            .collect();
        all.sort_unstable();
        assert_eq!(all, vec![0, 1, 2, 3, 4, 5, 6, 7]);
    }

    #[test]
    fn respects_the_node_filter() {
        let t = fake(2, 2);
        let p = plan(&t, 1, 1, Some(1), None);
        assert!(p.readers.iter().all(|c| c.node == 1));
        assert!(p.writers.iter().all(|c| c.node == 1));
    }

    #[test]
    fn respects_an_explicit_cpu_list() {
        let t = fake(1, 1);
        let p = plan(&t, 1, 1, None, Some(&[2, 3]));
        assert_eq!(p.readers[0].cpu, 2);
        assert_eq!(p.writers[0].cpu, 3);
    }

    #[test]
    fn oversubscribes_rather_than_dropping_threads() {
        let t = fake(1, 1);
        let p = plan(&t, 4, 4, None, None);
        assert_eq!(p.readers.len(), 4);
        assert_eq!(p.writers.len(), 4);
        assert!(p.oversubscribed);
    }

    #[test]
    fn parses_cpu_lists() {
        assert_eq!(parse_cpu_list("0-3,8"), Some(vec![0, 1, 2, 3, 8]));
        assert_eq!(parse_cpu_list("5"), Some(vec![5]));
        assert_eq!(parse_cpu_list(""), None);
        assert_eq!(parse_cpu_list("x"), None);
        assert_eq!(parse_cpu_list("3-1"), None);
    }
}
