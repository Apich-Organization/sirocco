//! Sirocco engine: the pipeline that actually moves bytes.
//!
//! ```text
//!   scan ──▶ decide ──▶ schedule ──▶ [reader × R] ──▶ [writer × W] ──▶ verify
//!             │                          │                │
//!        delta/append                buffer pool      sparse holes
//! ```
//!
//! * [`config`] — every knob, in one place.
//! * [`scan`] — parallel directory walk.
//! * [`pipeline`] — the reader/writer threads and their reactors.
//! * [`session`] — orchestration: what to copy, in which order, and the report.
//!
//! # Lint exceptions
//!
//! * `arithmetic_side_effects` — offsets, chunk counts and block indices are
//!   computed per chunk; making each `+` a checked operation would put a branch
//!   on the hot path for a condition that cannot occur (offsets are bounded by
//!   the file size).
//! * `indexing_slicing` — `buf[..len]` is how a chunk is handed to the reactor.
//! * `unwrap_used` / `expect_used` — the remaining sites lock a mutex or read a
//!   value the surrounding code just inserted.
//! * `single_call_fn` — helpers exist to name a step of the pipeline.

#![allow(clippy::arithmetic_side_effects)]
#![allow(clippy::indexing_slicing)]
#![allow(clippy::single_call_fn)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]
// The reader and writer loops take their `Arc<Shared>` by value because they are
// `move` closures handed to `thread::spawn`; `too_many_lines` and
// `struct_excessive_bools` describe the pipeline and the config honestly.
#![allow(clippy::needless_pass_by_value)]
#![allow(clippy::too_many_lines)]
#![allow(clippy::struct_excessive_bools)]
// Panics here are index and `unwrap` panics inside code whose invariant is
// stated one line above; the crate documents that instead of per-function.
#![allow(clippy::missing_panics_doc)]
// `Error` already carries the path and the operation, so a per-function
// `# Errors` section would restate the type. The crate documents the two
// kinds of failure (request rejected vs per-file failure) once.
#![allow(clippy::missing_errors_doc)]
// Byte counts become `f64` only for rate and break-even arithmetic.
#![allow(clippy::cast_precision_loss)]
// The only `unsafe` in this crate is the shared-memory progress block: a mapping
// created by `sirocco-sys` is handed to `Stats::from_shared` as a raw pointer,
// which is what makes cross-process progress cost zero syscalls. The single call
// site carries the SAFETY comment; a safe wrapper would have to be `unsafe`
// itself, since the caller is the one that knows the mapping outlives the run.
#![allow(unsafe_code)]
// Test code is not shipped: the fixtures describe block sizes and offsets as
// plain numbers, and the shared-memory tests have to reach through a raw pointer
// to prove the mapping is readable from a second process.
#![cfg_attr(
    test,
    allow(clippy::unreadable_literal, clippy::cast_possible_truncation)
)]

pub mod config;
pub mod index;
pub mod journal;
pub mod pipeline;
pub mod scan;
pub mod session;
pub mod types;

pub use config::CompressMode;
pub use config::EngineConfig;
pub use config::Event;
pub use session::Summary;
pub use session::partition;
pub use session::print_summary;
pub use session::run;
pub use session::run_with;
