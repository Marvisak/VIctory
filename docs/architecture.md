# Architecture

```
 user's files (outside repo)             VIctory
 ───────────────────────────             ─────────────────────────────────────────
 game assets ──────────────────────────► victory-pipeline + victory-game
                                         (victory-formats, victory-shader)
                                                    │
                                                    ▼
                                         cache_dir: converted data
                                                    │
 game executable ─► victory-loader ────► title code (native x86-64)
                    prepare, map, bind      │ imports
                                            ▼
                                         victory-hle ── host Windows (shared APIs)
                                            │ D3D12.X
                                            ▼
                                         victory-gpu ─► host D3D12
                                            │ shaders at pipeline creation
                                            ▼
                                         victory-shader ◄─► cache_dir
```

Shaders are translated when the title creates a pipeline, from the bytes it
passes, wherever it loaded them from (executable, archive, memory).
Converting them ahead of time from files (`victory translate --cache`) is
only a warm-up: it fills the same cache, under the same key. With
`--pipelines` it also makes the host pipelines of the title's shader pack
into the pipeline library, from the creation streams the engine would pass
(`victory-game` builds them, `victory-gpu` reads them as it reads the
title's), so a run finds them under the names it gives them itself.

## Crates

| Crate | Role |
|-------|------|
| `victory-core` | config, asset-policy guard, logging |
| `victory-pe` | read-only PE32+ and import-library analysis |
| `victory-loader` | prepare, map, bind and start the title image |
| `victory-hle` | console API implementations, host pass-through policy |
| `victory-gpu` | D3D12.X objects, command lists and GPU memory → host D3D12 |
| `victory-shader` | Xbox shader containers → host shaders |
| `victory-formats` | the GPU's texture layouts (sizes, tiling, detiling), BCPack, audio codecs |
| `victory-pipeline` | converter trait, content-addressed cache |
| `victory-game` | Grand Theft Auto VI-specific formats, build identities, hooks |
| `apps/victory` | command-line front end |
| `xtask` | asset-policy check, git hooks |
| `tests/fixtures/hle-probe` | a stand-in "title" (our own code) for loader tests |

Everything except `victory-game` is platform-level and can be tested with
any Xbox Series title.

## Import resolution

For every `dll!function` the title imports, in order:

1. our implementation in `victory-hle::modules` (always wins);
2. the host's own function, if `HostPolicy` allows that DLL;
3. an export of a DLL the title ships beside its executable, mapped like
   the executable (only DLLs neither of the above serves);
4. a generated stub that names the import when called (`--unimplemented
   trap` breaks into the debugger and aborts; `zero` logs once and returns 0).

`LoadLibrary*` of a console-only DLL we implement (e.g. `xgameruntime.dll`)
returns a pseudo handle; `GetProcAddress` on it finds our functions.
`LoadLibrary*` of a title DLL returns its mapped image, mapping it first if
nothing imports it. By name, as by import, a title gets only host DLLs
`HostPolicy` passes through, and `GetProcAddress` on one returns our
override where import binding would.
`QueryApiImpl` hands out our runtime objects by provider and interface id
(`victory-hle::modules::xgameruntime` lists the areas); online areas answer
as an offline console. Game saves are files under `VICTORY_DATA_DIR`.

Title storage is a drive on the console, and titles build paths on that.
`victory-hle::modules::files` binds every path-taking title import
(kernel32 and the C/C++ runtime) and rewrites paths on a virtual drive to
the host folder behind it; persistent local storage is one such drive over
a folder under `VICTORY_DATA_DIR`. Other paths reach the host unchanged.

An exe's code reaches its static TLS as slot 0; mapped as a DLL it gets
another slot, so the loader redirects those loads (`victory-loader::tls`).
`victory run --execute` installs a crash reporter that names faults as
`module+rva` and unwinds the faulting thread's stack
(`victory-loader::crash`).

`victory census` shows this classification without running anything (it
does not map title DLLs; `victory run` without `--execute` does).

## Shaders

`victory_shader::ir::build` turns a native program into SSA, one
instruction per machine instruction. `victory_shader::structure` then
removes EXEC: lane masks become per-lane booleans, wave-wide mask tests
become per-lane branches, and vector instructions keep a guard only where
a lane can reach them with its EXEC bit clear. `victory_shader::range::build`
runs both, again where a bound on the index of a register-relative move
(`v_movrel*`) is proven on the per-lane form, so the move names only the
registers it can reach. `victory_shader::resource`
traces each descriptor and scalar memory address to the root parameter it
comes from (table, heap, root descriptor or constant). A descriptor a
waterfall loop reads from the first lane (`v_readfirstlane_b32`) is traced
to the per-lane load or choice that gives each lane its own (a mask's
`v_cndmask_b32`, or an if/else whose arms each set it: traced on the paths
the lanes take, from structurisation), and accessed with a non-uniform heap
index in one pass. `victory translate`
runs every shader it finds (containers, SGD5 files) through these and
lists what they cannot handle yet.

The code cites three design documents, "decision 0003" (the shader
translation), "decision 0005" (title memory), and "decision 0007" (the
host GPU), which are not in the repo. Until they are, the module docs
are the current record of each decision's shape:

**0003 — shader translation** (`victory-shader`): the stages, in
order, are SSA (`ir.rs`), control-flow structurisation without EXEC
(`structure.rs`), I/O raising (`io.rs`), whole-wave sections
(`emit/wave.rs`), lowering one machine instruction at a time
(`emit/lower.rs`), resource tracking (`resource.rs`), and DXIL
emission with the host bindings (`emit/mod.rs`). Translated shaders
are kept on disk keyed by the container's content and the translator
build that made them (`victory-gpu::shader_cache`).

**0005 — title memory**: shaders that read or write memory at
addresses they compute look each address up in a page table over the
host copies (`victory-gpu/src/memory.rs`; the contract is in
`victory-shader`'s `emit/mod.rs`), and BVH ray tests read their nodes
the same way (`emit/lower/bvh.rs`).

**0007 — host GPU** (`victory-gpu`): one device and a direct queue
(`host.rs`, `queue.rs`), with every submission waited for. Title
buffers get host copies whose read ranges are refreshed from title
memory before each submission and whose written ranges go back after
it (`shadow.rs`, write-watched in `watch.rs`); textures the title
writes are detiled into their host copies when their content changed
(`texture.rs`). Pipelines are made by workers, not by the title
thread that first records a draw with them, within a per-frame wait
budget; past it a draw is dropped, as an untranslated one is
(`pipeline/make.rs`). Compiled host pipelines go into the host's
pipeline library under the cache folder, and the title pipelines a
run used are listed for early making on the next run
(`pipeline/library.rs`).
