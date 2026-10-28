//! Print the CPU topology Sirocco sees: logical CPUs, physical cores and NUMA
//! nodes, in the order the placement planner uses them.
//!
//! Run with `cargo run -p sirocco-sys --example topo`. Useful when a placement
//! decision looks wrong: this is exactly the input the planner got.

use sirocco_sys::Topology;

fn main() {
    let t = Topology::detect();
    println!(
        "cpus={} cores={} nodes={}",
        t.cpus.len(),
        t.cores.len(),
        t.nodes.len()
    );
    for (i, c) in t.cpus.iter().enumerate() {
        println!(
            "  cpu {i}: group={} idx={} id={} node={} core={}",
            c.group, c.index, c.id, c.node, c.core
        );
    }
    for (i, core) in t.cores.iter().enumerate() {
        println!("  core {i}: {core:?}");
    }
}
