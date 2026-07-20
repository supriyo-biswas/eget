# Development

This is a Rust 2024 command-line application. Before handing off changes, run:

```console
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
```

Archive and executable fixtures are committed under `tests/fixtures`. Recreate
them deterministically with `python3 misc/regenerate_fixtures.py`, then rerun
the tests and inspect the resulting changes before committing.

Implementation details for source detection, package identifiers, storage,
updates, and removal are documented in
[docs/specs/eget.md](docs/specs/eget.md).

# Guidelines

- When using the Context7 MCP, remember that it can help with framework/library
questions only, so break down your problem into specific questions and only
then hand it off.
