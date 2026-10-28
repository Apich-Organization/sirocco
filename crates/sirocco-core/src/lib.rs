//! Sirocco core: the algorithms that are independent of the OS and the I/O path.
//!
//! * [`delta`] — decides between full copy, block delta and append-only update
//!   from a small number of probes, using a cost model built from measured
//!   throughputs and a Wilson confidence bound.
//! * [`lz`] — an LZ4-block-compatible codec with reusable compressor state.
//! * [`entropy`] — measures whether compression actually pays.
//! * [`hash`] — XXH64 and hardware CRC32C.
//! * [`queue`] — bounded MPMC ring used for pipeline backpressure.
//! * [`plan`] — job description types shared with the CLI.
//! * [`pattern`] — `--include` / `--exclude` matching.
//!
//! ---
//!
//! # Lint exceptions
//!
//! Four clippy lints are allowed for the whole crate, because they fight the
//! kind of code this crate is made of:
//!
//! * `arithmetic_side_effects` — the hashes, the LZ4 match finder and the MPMC
//!   ring index with `wrapping_add` and `%` by design. Rewriting every `+` as a
//!   checked operation would add a branch per byte.
//! * `indexing_slicing` — parsing a block means `data[i..i + 8]`. Bounds are
//!   established by the loop condition immediately above each slice.
//! * `single_call_fn` — helpers that exist to name a concept are worth their
//!   one call site.
//! * `cast_precision_loss` — byte counts become `f64` only to compute a rate or
//!   a break-even ratio. The 2^53-byte threshold where this would lose precision
//!   is 8 PiB, three orders of magnitude past any file this engine will see.
//!
//! Everything else in the workspace profile is enforced here, including in the
//! unit tests below — with two exceptions that apply to test builds only, and
//! are declared as such rather than widening the crate-level set.

#![allow(clippy::arithmetic_side_effects)]
#![allow(clippy::indexing_slicing)]
#![allow(clippy::single_call_fn)]
#![allow(clippy::cast_precision_loss)]
// Test code is not shipped. An assertion that ends in `unwrap()` states exactly
// what it expects, and the fixtures are full of literal byte patterns whose
// separators would only make them harder to read against the reference vectors.
#![cfg_attr(
    test,
    allow(clippy::unwrap_used, clippy::expect_used, clippy::unreadable_literal)
)]

pub mod delta;
pub mod entropy;
pub mod error;
pub mod hash;
pub mod human;
pub mod lz;
pub mod pattern;
pub mod plan;
pub mod queue;
pub mod stats;

pub use error::Error;
pub use error::Result;
