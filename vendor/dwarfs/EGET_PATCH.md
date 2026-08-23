# Vendored changes

This directory contains `dwarfs` 0.2.1 from crates.io, licensed under MIT or
Apache-2.0.

It includes the sparse-file reader changes from upstream pull request
[oxalica/dwarfs-rs#1](https://github.com/oxalica/dwarfs-rs/pull/1), commit
`c008c05d232dde4f24280f0458cdbbf70e170f88`. The patch teaches the reader about
the `sparsefiles` feature and the metadata fields used by DwarFS 0.14 and newer.
