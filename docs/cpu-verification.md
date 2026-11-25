# CPU-only source verification

Run the reviewed component checks without launching the title or supplying its
files:

```powershell
cargo xtask verify-cpu --list
cargo xtask verify-cpu --jobs 2 --report target/cpu-verification.md
```

The report records the source commit, working-tree state, host platform,
compiler, selected commands, test summaries, and full output. The command
returns an error if a check fails or if a selected test filter unexpectedly
matches no passing tests. The report's parent directory must already exist.
On Windows x64, checks default to `x86_64-pc-windows-msvc`, matching the
repository's supported target; `CARGO_BUILD_TARGET` overrides that choice.
The synthetic loader fixture's nested Cargo build uses the same target. Run
from a Visual Studio Developer shell (or another environment with the MSVC
linker on `PATH`).

The `compile` check type-checks all workspace targets without running their
test or application binaries. The command then reuses existing tests for
PE/formats/codecs, synthetic game containers,
loader preparation and TLS, selected HLE files/storage/runtime/async/save/memory
paths, and shader analysis and lowering. On Windows it also builds and calls
the repository's synthetic `hle-probe` fixture. GPU execution tests and title
execution/data are not included. Windows-only checks are reported as skipped
on other platforms, not counted as passes.

This is a curated test selection, not a sandbox or a replacement for the full
test suite. Review it when tests are added or moved. A passing report only
describes the listed checks on the recorded host; it does not establish GPU
behavior, title compatibility, or performance. Run `cargo test --workspace`
for the complete workspace test suite (which also does not need game data).
