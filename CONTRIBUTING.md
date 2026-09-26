# Contributing

Keep model-specific Epson capabilities in `src/capabilities.rs`, with real
identity fixtures and offline protocol tests. See `docs/adding-scanners.md`.
Document hardware observations separately from simulated tests. Do not copy
external driver implementations; acknowledge protocol references in NOTICE.md
and docs/sources.md, including the exact revisions consulted.

Before proposing a change:

```sh
cargo fmt --all -- --check
cargo test --locked --all-targets --features cli
cargo test --locked --doc
cargo clippy --locked --all-targets --all-features -- -D warnings
```

Changes to Python bindings should update `epscan.pyi`, PYTHON.md and their tests.
Disclose AI assistance in contributions and review generated code before
submitting it. No hardware tests run by default; never change an installed
scanner driver as an incidental part of testing.
