# Fuzz targets

Coverage-guided fuzzing of `victory-formats` with [cargo-fuzz](https://github.com/rust-fuzz/cargo-fuzz). No game data is needed. This crate has its own workspace, so the main build, tests and CI never build it.

| Target | What it exercises |
|---|---|
| `bcpack_tables` | `bcpack::Tables::parse` on raw bytes and on the shipped tables with bytes overwritten, then `decode` with any tables that still parse |
| `bcpack_decode` | `bcpack::decode` for every supported BC format with the shipped tables |
| `xma_frames` | XMA `packet`, `read_frame` and `Decoder::decode` / `flush` |

## Running

libFuzzer needs a nightly toolchain and works best on Linux (WSL is fine). From the repository root:

```sh
cargo install cargo-fuzz
cargo +nightly fuzz run bcpack_decode --debug-assertions -- -max_total_time=1800
cargo +nightly fuzz run xma_frames --debug-assertions -- -max_len=16385 -max_total_time=1800
```

`--debug-assertions` keeps integer overflow checks on. A crash is saved under `fuzz/artifacts/<target>/`. Minimise it with `cargo +nightly fuzz tmin <target> <file>`, then turn it into a regression test in the module's `#[cfg(test)]` block. Corpora and artifacts are not committed.
