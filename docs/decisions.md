# Implementation decision references

An index of the inline `decision 000X` references in the source tree. This is
a pointer list, not the decisions themselves: the numbered documents are not
in this repository, so no links are invented here.

- Snapshot: `main` `63bcca43c6a00a758510398fa43096ce7e4f7df0`.
- 39 references across 28 files.
- Counts and line numbers drift as the tree moves; re-generate before relying on them.
- Decision numbers seen: 0001, 0003, 0005, 0007.

## By decision number

| Decision | Where | Inline note |
| --- | --- | --- |
| `0001` | `crates/victory-hle/src/modules/xmem.rs:77` | with no field). Decision 0001. |
| `0003` | `apps/victory/src/translate.rs:2` | what it cannot handle yet is tallied (decision 0003). |
| `0003` | `apps/victory/src/translate.rs:321` | the shader cache `run` reads (`<cache>/shaders`, decision 0003), by the |
| `0003` | `crates/victory-gpu/src/pipeline.rs:10` | use (decision 0003). |
| `0003` | `crates/victory-gpu/src/shader_cache.rs:1` | Translated shaders kept on disk (decision 0003): under the configured |
| `0003` | `crates/victory-shader/src/io.rs:1` | I/O raising (decision 0003): what a program's input registers, its |
| `0003` | `crates/victory-shader/src/ir.rs:1` | One-to-one SSA form of a native program (decision 0003, step 1). |
| `0003` | `crates/victory-shader/src/resource.rs:1` | Resource tracking (decision 0003): where each descriptor a program uses |
| `0003` | `crates/victory-shader/src/structure.rs:1` | Control-flow structurisation (decision 0003): the program as one lane |
| `0003` | `crates/victory-shader/src/emit/lower.rs:1` | IR to DXIL, one machine instruction at a time (decision 0003). |
| `0003` | `crates/victory-shader/src/emit/mod.rs:1` | DXIL emission (decision 0003, step 3): a console vertex, pixel, compute |
| `0003` | `crates/victory-shader/src/emit/wave.rs:1` | Whole-wave sections (decision 0003): code between `s_or_saveexec -1` |
| `0005` | `crates/victory-gpu/src/memory.rs:1` | Title memory at raw GPU addresses (decision 0005, the unified-memory |
| `0005` | `crates/victory-shader/src/resource.rs:44` | instruction's own operands (decision 0005, title memory). |
| `0005` | `crates/victory-shader/src/emit/lower.rs:4641` | Title memory at a raw GPU address (decision 0005: the unified-memory |
| `0005` | `crates/victory-shader/src/emit/memory_tests.rs:1` | Title memory at addresses the program computes (decision 0005, the |
| `0005` | `crates/victory-shader/src/emit/mod.rs:122` | * **Title memory** (added 2026-11-24, decision 0005's unified-memory |
| `0005` | `crates/victory-shader/src/emit/lower/bvh.rs:2` | (decision 0005): one ray against one node of a BVH the console wrote, the |
| `0007` | `crates/victory-gpu/src/bindless.rs:1` | Views shaders reach through the resource heap itself (decision 0007, |
| `0007` | `crates/victory-gpu/src/descriptor.rs:56` | descriptor order when a draw first uses them (decision 0007, step |
| `0007` | `crates/victory-gpu/src/draw.rs:1` | Draws on the host GPU (decision 0007, step 3). A list keeps the state |
| `0007` | `crates/victory-gpu/src/hold.rs:1` | Work a queue holds back (decision 0007, "Queues wait on memory values"). |
| `0007` | `crates/victory-gpu/src/host.rs:1` | The host GPU: one D3D12 device and direct queue (decision 0007), and a |
| `0007` | `crates/victory-gpu/src/list.rs:9` | (decision 0007); draw state lives in `crate::draw`. Methods not |
| `0007` | `crates/victory-gpu/src/list.rs:116` | calls record into (decision 0007), and the draw state the title set. |
| `0007` | `crates/victory-gpu/src/list.rs:1333` | The title's draw state (decision 0007, step 3), kept for its draws or |
| `0007` | `crates/victory-gpu/src/list.rs:1586` | shortcut: dropped until their shaders translate (decision 0007). |
| `0007` | `crates/victory-gpu/src/pipeline.rs:8` | seconds and draws with a few dozen (decision 0007, step 3). Shader |
| `0007` | `crates/victory-gpu/src/queue.rs:114` | may run (`crate::hold`; decision 0007). |
| `0007` | `crates/victory-gpu/src/resource.rs:780` | Textures also live on the host GPU (decision 0007), made on first use |
| `0007` | `crates/victory-gpu/src/resource.rs:797` | One per texture memory (decision 0007, "One host texture per texture |
| `0007` | `crates/victory-gpu/src/resource.rs:1476` | of the title's textures are never used (decision 0007, "Host |
| `0007` | `crates/victory-gpu/src/shadow.rs:1` | Host copies of title buffers (decision 0007, step 3 and "Copies of title |
| `0007` | `crates/victory-gpu/src/texture.rs:1` | Texture contents the title writes itself (decision 0007, "Texture |
| `0007` | `crates/victory-gpu/src/watch.rs:1` | Which title memory changed since a host copy took it (decision 0007, |
| `0007` | `crates/victory-gpu/src/pipeline/host.rs:1` | Host pipelines for title pipelines (decision 0007, step 3): the |
| `0007` | `crates/victory-gpu/src/pipeline/library.rs:2` | starting early (decision 0007, "Pipeline cache"). |
| `0007` | `crates/victory-gpu/src/pipeline/make.rs:7` | title's GPU watchdog ended the run (decision 0007, "Pipelines made off |
| `0007` | `crates/victory-hle/src/modules/xmem.rs:133` | copy (`victory_gpu`, decision 0007); placeholders cannot be |

## Canonical documents

Every reference in this snapshot points at `decision 0001`, `0003`, `0005` or
`0007`. The numbered documents themselves are not in this repository, and no
in-repo index of them exists at this snapshot. Whether they live in Issues,
Discussions or an external tracker is `TBD` until a maintainer confirms it;
this page intentionally invents no links. If a comment later cites a different
number or an external issue, add it here with its `file:line` and keep the
canonical column `TBD` until confirmed.
