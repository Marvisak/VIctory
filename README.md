# VIctory

Runs Grand Theft Auto VI (Xbox Series X|S) natively on Windows, from files
the user supplies.

The title's x86-64 code runs unchanged inside our process; VIctory supplies
the platform underneath it and converts the title's data, ahead of time where
it can (shaders and pipelines too, see Getting started) and on first use
otherwise. See [docs/architecture.md](docs/architecture.md).

## Status (2026-11-24)

**Not playable yet.** The title boots, runs its menus at its own 30 fps,
loads the world and shows it: slowly, and with rendering defects.

| Stage | State |
|-------|-------|
| Boot, first-boot screens (accessibility, language, settings, legal) | works, 30 fps |
| Intro video with sound | works |
| Start screen, profile and saves | works |
| Loading into the world (about 50 s) | works |
| The world (first mission's intro) | drawn and shown: streets, buildings, vehicles, characters, the title card and credits; 1-2 s per frame. Surfaces are unlit, a band of distant geometry is missing, characters come out broken in some shots |

Tested on one machine: Windows 11, AMD RX 6700 XT.

> **Warning.** Entering the world on a real GPU has hung it in earlier
> builds, and Windows then resets the display driver (every monitor goes
> black for a moment). The latest runs did not, but it is not ruled out; a
> run can also end on a GPU fault (`crashes.log`). A diagnostics build with
> `VICTORY_D3D12_WARP=1` renders on the software adapter instead (too slow
> to reach the world in minutes).

What each part does today and what it still needs:

| Subsystem | Today | Needs |
|-----------|-------|-------|
| Loader | Runs the title's executable and its DLLs in our process; crash reports name the faulting code and are kept in `crashes.log` | – |
| Runtime services | One signed-in offline user, licences, packages, saves that survive a kill at any step, text entry dialog | Speech is silent; the dialog and relaunch are untested live |
| Network | Offline by design: the title cannot reach the network | – |
| Files and streaming | Archives and streamed data read, zlib and BCPack decoded in parallel | Loading is limited by writes into title memory |
| Input | XInput pads, keyboard, keyboard stand-in pad | Trigger rumble |
| Audio | Menu, video and loading sound; XMA voices and Opus streams decode | Reverb is silent; Opus results unverified against the title |
| GPU: menus | Draws, compute and presentation on host D3D12; only changed title memory is copied | Frame pacing drifts on one screen in release builds |
| GPU: world | The scene is drawn and presented: mesh pipelines, indirect execution, views the shaders pick from the heap themselves, title memory read by address (ray-tracing structures), depth written from compute, pipelines pre-built or made by worker threads and kept on disk | Lighting (surfaces are unlit) and some missing geometry; frames take 1-2 s; draws of untranslated shaders are dropped; real queries, depth bounds, HDR and scaling are missing |
| Shaders | Console GPU code recompiled to DXIL, ahead of time (`translate --cache`) or at first use, cached on disk. Menus: all. World: mesh 8,765 of 8,765, vertex/pixel 12,550 of 12,592, compute 2,579 of 2,637 | Merged geometry stages (195 programs) and about 100 others; fused multiply-adds follow the host GPU; some compute programs need a GPU with 64-lane waves |
| Textures | Console tiled layouts and detiling for the modes the title uses | Render-target metadata planes |
| Series S mode | Runs through the menus to the loading screen | The world, as above |

## Getting started

Requirements: Windows 10/11 x64, a CPU with AVX2, a Direct3D 12 GPU with
shader model 6.6 (and mesh shaders for the world), Rust (pinned in
`rust-toolchain.toml`).

The title also needs several GiB of available Windows commit at startup.
Check **Committed** under **Task Manager → Performance → Memory**. If
`crashes.log` reports Windows error 1455, close other applications or increase
the Windows paging file size, then retry.

```powershell
cargo xtask install-hooks                # asset-policy check on every commit
copy victory.example.toml victory.toml   # then set your paths (outside the repo)
cargo test --workspace                   # needs no game data
cargo run -p victory -- config           # check your paths
```

Run the title:

```powershell
cargo build --release -p victory
target\release\victory.exe translate --cache --pipelines <game dir>\shaders.rpf <game dir>\<the title's exe> <game dir>\XboxBvhBuilder.dll <game dir>\amd_fidelityfx_upscaler_xbox.dll
target\release\victory.exe run <game dir>\<the title's exe> --execute
```

The `translate --cache` step (minutes; again after a change of the shader
translator) translates every shader the title can ask for into the cache
(about 1.3 GB), so a run does not translate them while it plays; without it
they are translated at first use, and their draws wait. With `--pipelines`
it then makes every pipeline in the title's shader pack (inside
`shaders.rpf`) into the host pipeline library (about 27,000 pipelines, 4 MB;
some 20 minutes on 16 threads, most of it the 2,700 compute ones), so a run
compiles none of those either; the library belongs to the GPU and driver it
was made on. Run again, it skips what is there in seconds; stopped
early, it keeps what it made.

Upscaling (Series S mode): the title upscales with AMD FidelityFX. To run
it with the PC build on your GPU instead of the console build, get the
[FidelityFX SDK](https://github.com/GPUOpen-LibrariesAndSDKs/FidelityFX-SDK)
and point `VICTORY_FIDELITYFX_DIR` (or `[paths].fidelityfx_dir`) at its
`Kits/FidelityFX/signedbin` folder, which holds
`amd_fidelityfx_upscaler_dx12.dll`. Without it, or if that DLL offers no
upscaler for your GPU (the log says so), the title's own DLLs run.

The window caption shows the framerate. Saves, settings and `crashes.log`
are kept under the data folder (default `%LOCALAPPDATA%\victory\data`), derived
data under the cache folder.

Input: XInput pads. With none connected the keyboard is a pad while the
title's window has focus:

| Keys | Pad |
|------|-----|
| W A S D / I J K L | left / right stick |
| arrows | D-pad |
| Enter, Backspace, X, Y | A, B, X, Y |
| Q / E, Z / C | shoulders, triggers |
| F / H | stick clicks |
| Esc, Tab | Menu, View |

## Commands

```
victory inspect <exe|dll> [--imports] [--exports]
victory census [paths...] [--missing] [--strict]
victory lib-exports <import.lib> [--list]
victory shader-info [paths...] [--verbose] [--disasm] [--effects]
victory translate [paths...] [--validate] [--pipelines] [--list] [--jobs N]
victory translate [paths...] --cache [--pipelines] [--jobs N]
victory run <exe> [--execute] [--unimplemented trap|zero] [--strict]
            [--console series-x|series-s] [-- <title arguments>]
victory config
```

Logging: `VICTORY_LOG=victory_loader=trace,info` (a release build logs
info and above).

## Debugging

A release build has no debugging aids. Build them in with

```powershell
cargo build --profile diag -p victory --features diagnostics   # target\diag\victory.exe
```

| Variable | Effect |
|----------|--------|
| `VICTORY_LOG=<target>=debug,...` | debug and trace output |
| `VICTORY_STACKS=20,60` | every thread's stack at those seconds |
| `VICTORY_WATCH=<rva>,...` | logs a title u32 when it changes |
| `VICTORY_POKE=<rva>:<byte>,...` | writes a title byte twice a second (a hang watchdog's tick, say) |
| `VICTORY_KEYS=30:Enter,35:X` | presses keys at those seconds, focus or not |
| `VICTORY_CRASH_SAVE=<rva>:<len>,...` | saves title memory to the cache folder on a crash |
| `VICTORY_CAPTURE=<dir>` | saves presented frames (powers of two, or `VICTORY_CAPTURE_FRAMES=<list>`) |
| `VICTORY_DUMP_TARGETS=1` | with the two above, saves every render target of those frames |
| `VICTORY_CAPTURE_WORLD=<list>` | with `VICTORY_CAPTURE`, also saves (and with `VICTORY_DUMP_TARGETS` dumps) the frames at these offsets from the first frame that ran 500 draws (the world's first; its number depends on how fast the title loaded) |
| `VICTORY_DUMP_WRITERS=<hash>,...` | with the two above, also the textures a pipeline containing a listed shader hash writes, the first frame it runs, and for its buffers a log line (dwords changed, dwords not zero) |
| `VICTORY_PEEK=<hex address>[:<bytes>],...` | in listed frames, those bytes of title memory logged after each submission. Listed frames also log targets of 16 texels or fewer as values, each table view's view words and whether its host slot is null, the host copy of every buffer of 64 bytes or fewer a draw reads (at submission and after its list), and each 1D or 3D texture a dispatch writes, read back right after it |
| `VICTORY_D3D12_DEBUG=1` | host debug layer |
| `VICTORY_D3D12_GBV=1` | with the above, GPU-based validation (bad descriptor use reported, not faulted; slow) |
| `VICTORY_D3D12_DRED=1` | on a GPU failure, the allocation hit and the commands in flight |
| `VICTORY_SKIP_PIPELINES=<hash>,...` | omit draws and dispatches, including indirect ones, whose pipeline contains a listed 16-digit shader hash |
| `VICTORY_GPU_TIMING=1` | with the above, each list of a submission run and timed on its own; slow lists logged with their slowest commands; a device removal names the list it happened in and lists all its commands |
| `VICTORY_TRACE_SHADERS=<hash>,...` | with `VICTORY_D3D12_DRED`, every draw and dispatch of a listed shader logged with what it binds (user data, heap slots, root views and their first dwords; the first 500) |
| `VICTORY_D3D12_WARP=1` | host device on the software adapter (slow; cannot reset the driver) |
| `VICTORY_PIPELINE_LOG=<file>` | a line per title pipeline created (key, shaders, state), first used, and made or loaded (library name); `translate --cache --pipelines` writes the last kind too |
| `VICTORY_PREBUILD_KEYS=<file>` | `translate --cache --pipelines` makes nothing and lists each pack record's key, shaders and state (or why it is skipped) |
| `VICTORY_SHADER_LOOP_LIMIT=<n>` | translated shader loops stop after `n` iterations per lane instead of 2^20 (own shader cache entries); if a lower `n` makes world frames much faster, loops hitting the limit are what makes them slow |
| `VICTORY_AUDIO_CAPTURE=<file.wav>` | records what the title plays (our process only) |
| `VICTORY_XMA_DUMP=<dir>` | saves the first 32 XMA voices' context and input |
| `VICTORY_PROFILE=1` or `=<dir>` | any build: frame times, memory, caches and loads; see [Profiling](#profiling) |
| `VICTORY_PROFILE_OVERLAY=1` | any build: the same with an on-screen panel, F3 to hide and show it; see [Profiling](#profiling) |
| `VICTORY_PROFILE_LIVE=1` | any build: a line a second on the console and in a file synced to disk, with warnings for slow work and GPU stalls; see [Profiling](#profiling) |

Every build appends crashes and host GPU failures to `crashes.log` in the
title's data folder.

## Profiling

Any build, release included, profiles a run with `VICTORY_PROFILE` set;
unset, it costs one flag test per hook.

| Variable | Effect |
|----------|--------|
| `VICTORY_PROFILE=1` | a summary in the log (target `victory_profile`) every 5 s |
| `VICTORY_PROFILE=<dir>` | the same, plus `<dir>\frames.csv` and `<dir>\trace.json` (outside the repository) |
| `VICTORY_PROFILE_OVERLAY=1` or `VICTORY_PROFILE=overlay` | the summaries, plus a panel in the window's top left corner (with `VICTORY_PROFILE=<dir>`, the files too) |
| `VICTORY_PROFILE_INTERVAL=<seconds>` | time between summaries |
| `VICTORY_PROFILE_HITCH_MS=<ms>` | a frame interval above this is a hitch (default 50) |
| `VICTORY_PROFILE_TRACE_US=<us>` | timed scopes shorter than this are counted, not traced (default 100) |
| `VICTORY_PROFILE_DUMP=30,90` | a full report at those seconds (with every thread's stack in a diagnostics build); one is also logged when the host GPU fails |
| `VICTORY_PROFILE_LIVE=1`, `=frame` or `=<file>` | live lines, each second or after each frame, on the console and in `<dir>\live.log` (or `%TEMP%\victory-live.log`, or `<file>`), synced to disk at each line |
| `VICTORY_PROFILE_LIVE_SLOW_MS=<ms>` | a command or submission slower than this is warned of at once (default 100) |
| `VICTORY_PROFILE_LIVE_STALL_MS=<ms>` | the GPU on one marker this long is warned of at once, then at each doubling (default 1000) |

A summary has the frame rate and 1% low, frame interval avg, p50, p95,
p99 and max, hitches and stutters (over twice the median); per frame, the
time preparing submissions, waiting for the GPU and presenting, and the
submissions, draws and dispatches, and those dropped (not run on the
host) or of zero groups; the slowest submission and the pipelines it
bound; process memory, video memory and its
budget, and resident bytes (GPU memory, host textures, shader containers,
the default allocator); cache hits and misses (shader containers, the
shader disk cache, host pipelines, detiled textures); the timed scopes that took longest
(shader translation, host pipelines, `ReadFile`, `XMemAlloc`, audio
decoding...); and loads (file reads, DirectStorage requests) with the
biggest and slowest of the period.

`frames.csv` has one row per presented frame. Its `frame` column is the
present count `VICTORY_CAPTURE` names its files by, so a captured frame
finds its row. `trace.json` opens in <https://ui.perfetto.dev> or
`chrome://tracing`: a row of frames, each thread's timed scopes and loads,
and counters for frame times and memory. It stays readable when a run
ends abruptly. Timing a new place is one line:

```rust
victory_profile::scope!("gpu", "my pass", bytes); // times the rest of the block
```

The overlay shows, redrawn four times a second: the frame rate, average
frame time, 1% low and longest frame of the last 120 frames, with a
graph of them (newest at the right, red above `VICTORY_PROFILE_HITCH_MS`,
a line at it); the GPU wait, draws and submissions per frame; the
process's working set and commit; video memory used, its budget and
shared memory; each cache's hit rate (shader containers, the shader disk
cache, host pipelines, detiled textures); and file and DirectStorage
read rates in MB/s over the last second. F3, while the window has focus,
hides and shows it; the title sees the key as well. It is copied into the
back buffer after the frame (no shaders), so `VICTORY_CAPTURE` images do
not have it, and it scales with the window: twice as big from 1440 lines.

### GPU hangs and slow GPU work

With a diagnostics build, `VICTORY_D3D12_DRED=1` and `VICTORY_GPU_TIMING=1`
(lists then run one at a time, so a list's time is its own), the summaries
add the commands that took longest on the GPU: each by its marker text cut
to the command, its groups and its pipeline's shader hashes (`dispatch
64x1x1 [compute 8bbcbda6008eaa37]`, `mesh 3 [mesh ..., pixel ...]`,
`indirect kind 2 max 1 [...]`), with its longest, median and total time,
its runs, and its list's time when it took longest. Addresses and other
hexadecimal values are left out, so the lines can be pasted as they are.
A dump (`VICTORY_PROFILE_DUMP`, or a host failure) has them since the
start.

`VICTORY_PROFILE_LIVE=1` writes one line a second:

```text
[  95.0 s] frames 3 (last 4210) | cpu 41.3 ms gpu 290.0 ms /frame | slowest submission 812.4 ms (11 lists) [compute ...], ... | slowest command 387.8 ms (list 412.0) mesh 3 [mesh ..., pixel ...] | last marker done: dispatch 4x1x1 [compute ...] | dropped 12.0 zero-group 3.0 /frame | vram 6.12 GB / 7.50 GB, shared 250.0 MB / 16.00 GB
```

and `WARNING` lines at once: `SLOW COMMAND` and `SLOW SUBMISSION` over
`VICTORY_PROFILE_LIVE_SLOW_MS`, and `STALL` when the GPU has not passed a
marker for `VICTORY_PROFILE_LIVE_STALL_MS`, naming the list, the commands
of it done, the last one done and the one it is in. Those come from two
words per list in read-back memory the GPU writes before each noted
command and at the end (`WriteBufferImmediate`), read while the submission
runs (a diagnostics build with `VICTORY_D3D12_DRED=1`: the noted commands
are its markers). The device-removed data names the same commands only
after a removal; the live file has them on disk before, and a `FAILURE`
line when the host GPU fails. The writes wait for the work before them,
so the GPU runs slower with them. Without markers a stall still names the
submission and its pipelines.

What a dropped count does not say is why: `VICTORY_D3D12_DRED` notes each
dropped command (`DROPPED dispatch (why) ...`), and the reasons are in the
log. Zero-group work is counted for direct draws and dispatches; for
indirect ones, a frame listed in `VICTORY_CAPTURE_FRAMES` with
`VICTORY_DUMP_TARGETS` logs how many records asked for work.

`victory_profile`'s documentation lists the other hooks (`count!`,
`cache!`, `resident!`, `load`).

## License

GPL-3.0-or-later. See [LICENSE](LICENSE).
