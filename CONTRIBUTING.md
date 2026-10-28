# Contributing to Sirocco

Thanks for taking the time to look. Sirocco is a copy engine, which means two
things for anyone sending a patch: the hot path matters, and the correctness bar
is higher than usual — a copy tool that silently drops a byte is worse than one
that is slow.

## Code of Conduct

See [CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md). Short version: be decent, argue
about the code and not the person, and do not post anyone's private information.

## Getting set up

```bash
cargo build --release                 # CLI: target/release/sirocco
cargo build --release -p sirocco-gui  # GUI, built separately (see below)
cargo test                            # unit and integration tests
python tests/e2e.py                   # end-to-end acceptance
```

The GUI is deliberately **not** a default workspace member. `iced` pulls in wgpu,
winit, Wayland and X11, which a headless build machine cannot satisfy; keeping it
out means `cargo build` and `cargo test` stay dependency-free on Linux (only
`libc`) and truly dependency-free on Windows.

### What a full check looks like

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test
python tests/e2e.py
```

The last one is not optional for a change that touches the copy path. It hashes
the destination tree with Python's `hashlib` and compares it against the source,
independently of anything the engine reports about itself.

## Ground rules

**No new runtime dependencies.** Windows uses hand-written FFI, Linux uses
`libc` and raw syscalls, and the CLI parses its own arguments. This is not
minimalism for its own sake: the whole point is to know exactly which syscall is
issued and when. `sirocco-gui` is the only crate allowed third-party code, and it
is not in the default build.

**`unsafe` is confined to `sirocco-sys`** (plus the one shared-memory handoff in
the engine and the CLI, each of which says so at the crate root). Every `unsafe`
block needs a `// SAFETY:` comment naming the invariant that makes it sound, and
the invariant has to be true of the surrounding code, not just plausible.

**No blocking work on an I/O thread** and no allocation on a per-chunk path. If a
patch adds a `Vec` per chunk or a syscall per file, say why in the PR — it will be
asked about.

**Platform differences are explicit.** If a change touches alignment, offsets,
allocation or metadata, it has to hold on Windows and Linux, on x86_64 and
aarch64. Syscall numbers come from `libc::SYS_*`; never hardcode one.

## Performance changes

A PR that claims a speed-up needs a number, and the number needs a method:

```bash
cargo build --release
sirocco copy <src> <dst> --no-delta --verify=none   # repeat 3×, report the median
```

State the hardware, the filesystem, the file-size mix, and whether the page cache
was warm. A result measured once, on a warm cache, on a dataset of one file, is
not a result. Regressions in exchange for readability are sometimes right; say so
and let the maintainers decide.

## Tests

* New behaviour gets a test. New failure modes get a test that fails without the
  fix.
* The unit tests live next to the code; `tests/engine.rs` and
  `tests/platform.rs` drive the public API against real files.
* `tests/e2e.py` covers the combinations that only break end to end — empty
  files, 4 KiB boundaries, `1 MiB ± 1`, all-zero content, appends, mid-file
  rewrites, mtime preservation, hard links, sparse files, the blocking backend,
  and copies with direct I/O disabled.
* Tests may `unwrap()`. They may not `#[ignore]` a failure without an issue link.

## Lint policy

The workspace enforces `clippy::all`, `pedantic`, `nursery` and `missing_docs` at
`deny`, plus `unwrap_used`, `expect_used`, `indexing_slicing`,
`arithmetic_side_effects` and `single_call_fn`. That is a strict set for a
systems crate, and it is deliberate: it is what keeps the reporting path honest.

Where a lint fights the domain, the exception is written down rather than
worked around:

* at the **crate root**, with a doc comment saying which lint and why, when it
  applies to the whole crate (`arithmetic_side_effects` in the hashes, for
  example);
* at the **item**, with a one-line comment, when it is one function.

A blanket `#[allow]` without a reason will be sent back. Note that
`nonstandard_style` is `deny` rather than `forbid` precisely so the FFI module can
keep `CreateFileW` spelled the way the header spells it.

## Documentation

* Comments and doc comments are in English.
* User-facing docs are split by language, one file each, linked from the top of
  the other: `README.md` is the English README and the one GitHub shows by
  default; `README.zh-CN.md` is its Chinese translation. `DESIGN.md` is Chinese
  only. A change to one README is a change to both — a stale translation is worse
  than no translation.
* Every public item is documented — `missing_docs` is an error, not a suggestion.
* A doc comment explains *why*, not *what*. `/// Opens the file.` on
  `fn open` is noise; `/// Opens without buffering so the read reaches the device
  instead of the page cache.` is the reason the function exists.
* If you discover a platform quirk the hard way, write it down in `DESIGN.md` with
  the measurement that exposed it. Several of the current defaults exist only
  because someone measured the alternative.

## Submitting

1. Fork, branch from `main`.
2. Keep commits focused; a formatting-only change goes in its own commit.
3. Run the full check above.
4. Open a PR describing the problem, the change, and how you verified it. If it
   touches the copy path, include before/after numbers.

## AI-assisted contributions

Welcome, with the same rule as any other contribution: you are responsible for
it. Read the generated code, run the tests, and be able to explain every line you
submit. "The model wrote it" is not a defence for a bug that loses data.

## License

By contributing you agree that your work is dual licensed under the MIT License
and the Apache License, Version 2.0, without additional terms.
