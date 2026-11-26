# VIctory

Runs Grand Theft Auto VI (Xbox Series X|S) natively on Windows, from files
the user supplies.

The title's x86-64 code runs unchanged inside our process; VIctory supplies
the platform underneath it and converts the title's data, ahead of time where
it can (shaders and pipelines too, see Getting started) and on first use
otherwise. See [docs/architecture.md](docs/architecture.md).

## Status (2026-11-25)

**Not playable yet, because of the framerate: about 1 frame per second in the
world.** Nothing else keeps the game from working: the title boots, runs its
menus at its own 30 fps, loads the world, plays the intro cutscene and
reaches the **prologue**, the mission after it, with the graphics fully
displayed. 

| Stage | State |
|-------|-------|
| Boot, first-boot screens (accessibility, language, settings, legal) | works, 30 fps |
| Intro video with sound | works |
| Start screen, profile and saves | works |
| Loading into the world (about a minute) | works |
| The intro cutscene | plays at about 1 fps (0.9-1.0 s a frame, warm cache, quiet machine): textured and lit characters with whole hair, buildings, sky, depth of field, the title card, credits and interface. Missing: ray-traced shadows and indirect light, the local-light shadow mask. Seen in some shots: a pink band over nearer bodies, a stand-in head on a secondary character for a few frames |
| The prologue | reached; the graphics are fully displayed, at about 1 fps |

In the intro cutscene at 1 fps the opening credits land over other shots than
on a console: they follow the frame count, the shots follow the clock.

Tested on one machine: Windows 11, AMD RX 6700 XT (12 GB).

> **Warning.** Entering the world on a real GPU hung it in earlier builds,
> and Windows then reset the display driver (every monitor goes black for a
> moment). The latest runs, several minutes each in the world, did not, but
> it is not ruled out; a run can also end on a GPU fault (`crashes.log`). A
> diagnostics build with `VICTORY_D3D12_WARP=1` renders on the software
> adapter instead (too slow to reach the world in minutes).

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
| GPU: world | The scene is drawn and presented: mesh pipelines, indirect execution, views the shaders pick from the heap themselves, title memory read by address, depth written from compute, lists that wait for values other queues write, textures made when a shader first reads them, pipelines pre-built or made by worker threads and kept on disk, depth bounds on supporting adapters (synthetic tests) | The title's ray-tracing structures are never built (no ray-traced shadows or indirect light in the intro cutscene); about 1 s a frame; draws of untranslated shaders are dropped; real queries, HDR and scaling are missing |
| Shaders | Console GPU code recompiled to DXIL, ahead of time (`translate --cache`) or at first use, cached on disk. Menus: all. World: all but under 300 of 24,387 programs | Merged geometry stages (195 programs) and under 100 others; fused multiply-adds follow the host GPU; some compute programs need a GPU with 64-lane waves |
| Textures | Console tiled layouts and detiling for the modes the title uses | Render-target metadata planes |
| Series S mode | Menus, loading and the world run with the PC upscaler (`VICTORY_FIDELITYFX_DIR`, about 1 s a frame) | The title's own upscaler DLLs: six of their passes are untranslated, so their picture is wrong |

## Getting started

Requirements: Windows 10/11 x64, a CPU with AVX2, a Direct3D 12 GPU with
shader model 6.6 (and mesh shaders for the world), Rust (pinned in
`rust-toolchain.toml`).

To install Rust, use rustup (`winget install --id Rustlang.Rustup -e`, or
the installer from https://rustup.rs). The first `cargo` command in this
repository then fetches the pinned toolchain. Building needs the MSVC C++
build tools: Visual Studio or its Build Tools with the "Desktop development
with C++" workload.

Keep the build paths short. From a checkout about 170 characters deep, the
`blake3` dependency's build script fails in `ml64.exe` with
`A1009: line too long`, even with long paths enabled in Windows, because
its object files land under `target`. Set `CARGO_TARGET_DIR` to a short
absolute path (`$env:CARGO_TARGET_DIR = 'C:\vt'`; `victory.exe` is then
in `C:\vt\release` instead of `target\release`), or check out the
repository nearer the drive's root.

A run in the world takes about 33 GB of Windows commit (RAM plus paging
file) and about 10 GB of video memory. The tested machine has 32 GB of RAM
and a commit limit of about 80 GB. Check **Committed** under **Task Manager →
Performance → Memory**. If `crashes.log` reports Windows error 1455, close
other applications or increase the Windows paging file size, then retry.

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
estimated at 20 minutes on 16 threads, but about 2 pipelines a second after a
translator change that touches most programs, which is hours), so a run
compiles none of those either; the library belongs to the GPU and driver it
was made on. Run again, it skips what is there in seconds; stopped
early, it keeps what it made. Without it, a run makes the pipelines it needs
on worker threads and keeps them, so the first runs after a translator
change are slower and show missing draws for a while.

To get into the game, press A (Enter) on each first-boot screen and again on
the start screen. Loading the world takes about a minute; the intro cutscene
plays, then the prologue starts.

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

Set `VICTORY_WINDOW_MAX=1920x1080` before launching to limit the title's
top-level window to that bounding box. Smaller windows keep their size,
larger windows keep their proportions, and the monitor's work area still
limits them. This scales the displayed image; it does not change the
console render resolution or the swap-chain buffers. With the variable
unset, window sizing is unchanged.

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

## Tests

Everything below runs without game data. `cargo test --workspace` runs the
unit tests of every crate and the acceptance suite; the shader tests run one
at a time (`cargo test -p victory-shader -- --test-threads=1`, see
[the validator](#windows-shader-test-validator)).

### The conformance title (acceptance suite)

`tests/fixtures/conformance` is a small title of our own, built from source
by the test and loaded through VIctory like a real one. It calls the
console runtime the way a title does and checks the answers against what
the public platform documentation says, citing the page per check. It links
no SDK and holds no game data.

```powershell
cargo conformance          # or: cargo test -p victory-hle --test conformance -- --nocapture
```

It reports one TAP line per check (189 of them):

| Area | What it checks |
|---|---|
| Runtime | loading `xgameruntime.dll`, initialising, reaching each area's object |
| Task queues and async calls | dispatch modes, cancellation, status, a provider's call order |
| System information | console id, analytics info |
| Memory | physical pages mapped at two addresses reading back the same bytes (invariants: XMem's pages are not public) |
| DirectStorage | completion order, automatic submission, capacity and size limits, cancel by tag |
| Game saves | update and quota limits, atomic updates, container names |
| User | default user, gamertags, ids, handles |
| Networking, package, display | connectivity hint, package, HDR mode |
| Files | the package drive (read-only) and temporary storage, with Win32 and C runtime semantics |
| Persistent local storage | path size and path |
| GPU frame | see below |

Checks VIctory is known to answer differently are listed with the reason in
`KNOWN` (`crates/victory-hle/tests/conformance.rs`): reported, not failed. A
listed check that starts passing fails the test, so the list stays true.

### The GPU frame (a drawing test title)

The same title draws a frame through VIctory's console D3D12, as a game
would: device, queue, command lists, descriptor heaps, root signatures, a
graphics and a compute pipeline whose shaders are hand-assembled console
(GFX10) programs, vertex, index and constant buffers in title memory, a
texture uploaded by a copy, and a depth buffer. The scene is a ground plane,
two cubes and a glowing pyramid, lit by a directional light, textured from a
checkerboard, then blurred by a compute pass (512x288). The test draws the
same scene natively on WARP from equivalent HLSL and compares the two
images (`the_gpu_frame_matches_warp`).

```powershell
# Watch it: present the frame, animated, in a window for 10 seconds.
$env:VICTORY_CONFORMANCE_WINDOW = 10
cargo test -p victory-hle --test conformance the_gpu_frame_matches_warp -- --nocapture
# Save VIctory's and WARP's frames as victory.ppm and warp.ppm.
$env:VICTORY_CONFORMANCE_DUMP = "frames"
```

### The demo

`cargo demo` shows the GPU frame, animated, in a window until you close it,
with VIctory's overlay (frame rate and frame times) in its top left corner:
to leave running, or to show. The window's frames add hills of Perlin
noise around the ground; the checks still run on the 512x288 frame,
without them.

`--scale <n>` is a stress test of VIctory itself, not of the GPU: each
step past 1 adds 64 small objects to a frame, each a draw with its
constants written into title memory and its view made in the descriptor
heap that frame, and one more submission, as a title's frame grows. Every
5 seconds it prints the frame rate and the work per frame, so runs at
different scales compare.

```powershell
cargo demo                          # 1440p until the window is closed
cargo demo --resolution 4k          # 720p, 1080p, 1440p, 4k, or <width>x<height>
cargo demo --scale 64               # 4,037 draws and 64 submissions a frame
cargo demo --seconds 30 --dump frames   # then save the last frame as frames/window.ppm
cargo conformance --window 10 --resolution 1080p   # the whole suite, then 10 s of frames
```

Both build the test in release (`--debug` for the dev profile, whose
frames are much slower).

### On macOS

`cargo conformance` and `cargo demo` also run on a Mac with Apple silicon,
under [CrossOver](https://www.codeweavers.com/crossover) (Wine, with Apple's
D3DMetal for Direct3D 12, and Rosetta 2 for the x86-64 code). The title and
the test are cross-compiled for Windows with
[cargo-xwin](https://github.com/rust-cross/cargo-xwin), and run in a
CrossOver bottle named `VIctory`, made on first use:

```sh
cargo install --locked cargo-xwin   # once; its first build downloads Microsoft's CRT and Windows SDK
cargo demo
```

`VICTORY_CROSSOVER_BIN` names CrossOver's `bin` folder if the app is not in
`/Applications`. There is no WARP under D3DMetal, so the comparison with
it is skipped; every other check runs. The library crates' own tests run
natively (`cargo test --workspace --exclude victory`); the `victory`
executable, the game itself and GPU timings need Windows.

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

`translate --jobs N` requires a positive worker count. Omit it to use the
default worker count.

Logging: `VICTORY_LOG=victory_loader=trace,info` (a release build logs
info and above).

### Windows shader-test validator

The shader tests use `dxil.dll` to validate Shader Model 6.6 containers.
An older Windows SDK validator can load successfully but reject them with
`Unknown shader model 'ps_6_6'`. If this happens, use a current x64 validator
from [Microsoft's DirectX Shader Compiler releases](https://github.com/microsoft/DirectXShaderCompiler/releases)
or a current Windows SDK. Set its path for the test session:

```powershell
$env:VICTORY_DXIL = 'C:\tools\dxc\bin\x64\dxil.dll'
cargo test -p victory-shader --lib --locked -- --test-threads=1
```

Without this override, the validator loader selects the newest installed
Windows SDK's `bin/<version>/x64/dxil.dll`, then falls back to the DLL search
path when no SDK copy is found. This is a test/offline-validation dependency;
the translated shaders do not require it at runtime.

## Debugging

A release build has no debugging aids. Build them in with

```powershell
cargo build --profile diag -p victory --features diagnostics   # target\diag\victory.exe
```

The on/off switches below (`...=1`) are off when unset or set to `0`.

| Variable | Effect |
|----------|--------|
| `VICTORY_LOG=<target>=debug,...` | debug and trace output |
| `VICTORY_STACKS=20,60` | every thread's stack at those seconds |
| `VICTORY_WATCH=<rva>,...` | logs a title u32 when it changes |
| `VICTORY_POKE=<rva>:<byte>,...` | writes a title byte twice a second (a hang watchdog's tick, say) |
| `VICTORY_KEYS=30:Enter,35:X` | presses keys at those seconds, focus or not |
| `VICTORY_CRASH_BREAKPOINTS=1` | appends first-chance breakpoints to `crashes.log`, including ones a handler may recover from |
| `VICTORY_CRASH_SAVE=<rva>:<len>,...` | saves title memory to the cache folder on a crash |
| `VICTORY_CAPTURE=<dir>` | saves presented frames (powers of two, or `VICTORY_CAPTURE_FRAMES=<list>`) |
| `VICTORY_DUMP_TARGETS=1` | with the two above, saves every render target of those frames |
| `VICTORY_CAPTURE_WORLD=<list>` | with `VICTORY_CAPTURE`, also saves (and with `VICTORY_DUMP_TARGETS` dumps) the frames at these offsets from the first frame that ran 500 draws (the world's first; its number depends on how fast the title loaded) |
| `VICTORY_DUMP_WRITERS=<hash>,...` | with the two above, also the textures a pipeline containing a listed shader hash writes, the first frame it runs, and for its buffers a log line (dwords changed, dwords not zero) |
| `VICTORY_PEEK=<hex address>[:<bytes>],...` | in listed frames, those bytes of title memory logged after each submission. Listed frames also log targets of 16 texels or fewer as values, each table view's view words and whether its host slot is null, the host copy of every buffer of 64 bytes or fewer a draw reads (at submission and after its list), and each 1D or 3D texture a dispatch writes, read back right after it |
| `VICTORY_D3D12_DEBUG=1` | host debug layer |
| `VICTORY_D3D12_GBV=1` | with the above, GPU-based validation (bad descriptor use reported, not faulted; slow) |
| `VICTORY_D3D12_DRED=1` | on a GPU failure, the allocation hit and the commands in flight |
| `VICTORY_SKIP_PIPELINES=<hash>,...` | skip host pipeline creation and omit draws and dispatches, including indirect ones, when the pipeline contains a listed 16-digit shader hash |
| `VICTORY_GPU_TIMING=1` | with the above, each list of a submission run and timed on its own; slow lists logged with their slowest commands; a device removal names the list it happened in and lists all its commands |
| `VICTORY_TRACE_SHADERS=<hash>,...` | with `VICTORY_D3D12_DRED`, every draw and dispatch of a listed shader logged with what it binds (user data, heap slots, root views and their first dwords; the first 500, and all of them in listed capture frames); the records of its indirect executions are read back and logged in every frame (records with work, the first four, the frame) |
| `VICTORY_D3D12_WARP=1` | host device on the software adapter (slow; cannot reset the driver) |
| `VICTORY_PIPELINE_LOG=<file>` | a line per title pipeline created (key, shaders, state), first used, and made or loaded (library name); `translate --cache --pipelines` writes the last kind too |
| `VICTORY_PREBUILD_KEYS=<file>` | `translate --cache --pipelines` makes nothing and lists each pack record's key, shaders and state (or why it is skipped) |
| `VICTORY_SHADER_LOOP_LIMIT=<n>` | translated shader loops stop after `n` iterations per lane instead of 2^20 (own shader cache entries); if a lower `n` makes world frames much faster, loops hitting the limit are what makes them slow |
| `VICTORY_AUDIO_CAPTURE=<file.wav>` | records what the title plays (our process only) |
| `VICTORY_XMA_DUMP=<dir>` | saves the first 32 XMA voices' context and input |

Every build appends unhandled exceptions, panics, and host GPU failures to
`crashes.log` in the title's data folder.

For a startup breakpoint (`0x80000003`), run the diagnostics build with
`VICTORY_CRASH_BREAKPOINTS=1`. Keep the console output along with the new
`crashes.log` entries. A host DLL refusal includes the caller stack in the
console output. The refusal is expected for DLLs outside the host policy,
including `rpcrt4.dll`.

On Series 1.0.2.1, a missing `appdata0.rpf` reproduced the breakpoint at
`+0x2237178` before the first window. Restore the archive from the same
package beside the title executable. `run --execute` checks that it can
read the RPF9 header before starting this build.

To compare hardware and software rendering, repeat with
`VICTORY_D3D12_WARP=1` and check the console for
`host device on the software adapter (WARP)` to confirm the WARP device was
created. Compare the exception's module and offset, stack frames, and
preceding messages across the two runs.

## License

GPL-3.0-or-later. See [LICENSE](LICENSE).
