# VIctory

I run Grand Theft Auto VI (Xbox Series X|S) on Windows and Linux/Proton,
from files the user supplies.

I keep the title's x86-64 code unchanged inside the VIctory process. VIctory
supplies the platform underneath it and converts the title's data, ahead of
time where it can (shaders and pipelines too, see Getting started) and on
first use otherwise. See [docs/architecture.md](docs/architecture.md).

## Windows status (2026-11-25)

**Not playable yet, but I can get into the game.** I observed the title boot,
run its menus at its own 30 fps, load the world, play the intro cutscene and
reach the **prologue**, the mission after it, with the graphics fully
displayed, at about **1 frame per second**.
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

## Linux / Proton status (2026-11-25)

**Not playable yet, but I got the title to a partial visible world under
Proton.** I am treating Linux/Proton as a major supported target of this
fork, not an aside.

### My measured visible-world run

In controlled run `20261125_220632`, I reached a **partial visible world** on
2026-11-25. I saw Lucia, grass, ground, a fence, lighting, LOD structures,
trees, a car scene, a left-side GUI, and the minimap. The copy on the left
later looked as though it had rendered into a better picture, but everything
on screen was still **STATIC**; it did not behave like a normal animated
world. Rendering remained choppy and broken-looking, square artifacts were
visible, audio became silent, and the late scene ran at about 0.1-0.2 FPS.
I stopped the still-running process with the enforced 480-second cap (exit
`124`), so this was not a spontaneous crash.

- Exact source: `facfb30b829083ac9a86212ef4222ec06e607d39`, clean tree.
- Exact MSVC binary SHA-256:
  `0c6b1215a551e2b9af6777a186ec797a811ceddb9376e7dc3e2734a522d3d87a`.
- Measured stack: NVIDIA GeForce RTX 4060 Ti, Proton Experimental build
  `26621173`, Wine `11.0`, DXVK `v3.1.1-209-g1039f682058fa71`, and
  vkd3d-proton `3.2.0`.

| Metric | Linux/Proton run | MSVC baseline `20261125_094753` |
|--------|------------------|---------------|
| Shader translation failures | 4 | 439 |
| Host pipeline refusals | 4 | 117 |
| Descriptor heap caps | 1 | 1 |
| Descriptor slot overruns | 507,329 | 310,720 |
| Device removals | 0 | 0 |
| Panics | 0 | 0 |
| FPS | median 0.1 overall; about 0.2 in the late scene | median 0.2 |

### My current limitations

- I have **not** proven stable, correct, and playable visible-world rendering.
- Descriptor slot overruns remain at 507,329.
- Late frames are dominated by host-side texture refresh, title-memory remap
  waits, preparation, and copy-back; the GPU is mostly idle.
- What I saw was choppy/broken, the scene stayed static, and audio became
  silent late in the run.

## What I added in this fork

My current branch adds five Linux/Proton code commits on top of the tracked
`theDVNVS/main` baseline:

| Commit | Linux/Proton work |
|--------|-------------------|
| `95d1f4f` | Compiles the indirect-execution clamp with Wine's shader compiler instead of assuming the native compiler's byte-address-buffer support. |
| `2e74fc8` | Requests an empty host pipeline library with a null blob, which vkd3d-proton accepts. |
| `3dde136` | Fails missing `WSAConnectByList` and `GetAddrInfoExA` calls cleanly instead of panicking when Wine does not export them. |
| `185373d` | Lowers shaders to one shared return block, avoiding invalid SPIR-V structured exits under vkd3d-proton. |
| `f9a4cbc` | Adds `scripts/proton-run.sh` to run the MSVC build and test suite under Proton's Wine with vkd3d-proton and DXVK. |

At the revisions I audited for this README, none of those five commits was
contained in `origin/main`, `fxceless/main`, `victory-perf/main`, or
`theDVNVS/main`. This describes my exact patch combination; it is not a
claim that another project cannot implement an equivalent fix.

I also prepared a separate branch, `experiment/pr2121-nemotron` at commit
`66c0992`, which adapts upstream PR #2121's 256-slot view-heap paging onto
`facfb30`. It keeps 1,245,184 title slots logical while mapping them through
pages on hosts with a smaller physical descriptor heap, and is enabled with
`VICTORY_VIEW_PAGES=1`. Focused paging tests, 224 `victory-shader` tests, and
156 `victory-gpu` tests pass. This branch is prepared but **not live-validated**
and is intentionally not merged into my current Linux/Proton branch.

What each part does today and what it still needs:

| Subsystem | Today | Needs |
|-----------|-------|-------|
| Loader | Runs the title's executable and its DLLs in my process; crash reports name the faulting code and are kept in `crashes.log` | – |
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

I install Rust with rustup (`winget install --id Rustlang.Rustup -e`, or the
installer from https://rustup.rs). The first `cargo` command in this
repository then fetches the pinned toolchain. On Windows, the build needs the
MSVC C++ build tools: Visual Studio or its Build Tools with the "Desktop
development with C++" workload.

Keep the build paths short. From a checkout about 170 characters deep, the
`blake3` dependency's build script fails in `ml64.exe` with
`A1009: line too long`, even with long paths enabled in Windows, because
its object files land under `target`. Set `CARGO_TARGET_DIR` to a short
absolute path (`$env:CARGO_TARGET_DIR = 'C:\vt'`; `victory.exe` is then
in `C:\vt\release` instead of `target\release`), or check out the
repository nearer the drive's root.

A run in the world takes about 33 GB of Windows commit (RAM plus paging
file) and about 10 GB of video memory. My tested Windows machine has 32 GB of
RAM and a commit limit of about 80 GB. I check **Committed** under **Task
Manager → Performance → Memory**. If `crashes.log` reports Windows error
1455, I close other applications or increase the Windows paging file size,
then retry.

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

To get into the game, I press A (Enter) on each first-boot screen and again on
the start screen. Loading the world takes about a minute; the intro cutscene
plays, then the prologue starts.

For Series S-mode upscaling, I use AMD FidelityFX. When I want to run it
with the PC build on my GPU instead of the console build, I get the
[FidelityFX SDK](https://github.com/GPUOpen-LibrariesAndSDKs/FidelityFX-SDK)
and point `VICTORY_FIDELITYFX_DIR` (or `[paths].fidelityfx_dir`) at its
`Kits/FidelityFX/signedbin` folder, which holds
`amd_fidelityfx_upscaler_dx12.dll`. Without it, or if that DLL offers no
upscaler for my GPU (the log says so), the title's own DLLs run.

The window caption shows the framerate. I keep saves, settings, and
`crashes.log` under the data folder (default `%LOCALAPPDATA%\victory\data`),
with derived data under the cache folder.

I set `VICTORY_WINDOW_MAX=1920x1080` before launching when I want to limit
the title's top-level window to that bounding box. Smaller windows keep their
size, larger windows keep their proportions, and the monitor's work area
still limits them. This scales the displayed image; it does not change the
console render resolution or the swap-chain buffers. With the variable
unset, window sizing is unchanged.

I use XInput pads. With none connected, I can use the keyboard as a pad
while the title's window has focus:

| Keys | Pad |
|------|-----|
| W A S D / I J K L | left / right stick |
| arrows | D-pad |
| Enter, Backspace, X, Y | A, B, X, Y |
| Q / E, Z / C | shoulders, triggers |
| F / H | stick clicks |
| Esc, Tab | Menu, View |

## Run it under Linux / Proton

I build the Windows binary from Linux with `cargo xwin`, then run it under
Proton's Wine with `scripts/proton-run.sh`.

I need the pinned Rust toolchain, `cargo-xwin`, a Proton installation, and
the user's game files outside this repository. My measured run used Proton
Experimental build `26621173` from this path:

```bash
export PROTON="/mnt/ssd2/SteamLibrary/steamapps/common/Proton - Experimental"
```

I build the exact locked MSVC release with:

```bash
cargo xwin build --locked --release -p victory \
  --target x86_64-pc-windows-msvc
```

I can use the runner's defaults, or override the Proton folder, prefix, and
VIctory executable:

```bash
# Defaults:
# PROTON: Proton - Experimental in a standard Steam/Flatpak location
# PREFIX: ~/.local/share/victory/proton
# VICTORY_EXE: target/x86_64-pc-windows-msvc/release/victory.exe
scripts/proton-run.sh config

# Or set them explicitly:
PROTON="/path/to/Proton - Experimental" \
PREFIX="$HOME/.local/share/victory/proton" \
VICTORY_EXE="$PWD/target/x86_64-pc-windows-msvc/release/victory.exe" \
  scripts/proton-run.sh config
```

I launch the title through VIctory, not directly through Wine:

```bash
PROTON="/path/to/Proton - Experimental" \
PREFIX="$HOME/.local/share/victory/proton" \
VICTORY_EXE="$PWD/target/x86_64-pc-windows-msvc/release/victory.exe" \
VICTORY_KEYBOARD_ONLY=1 \
  scripts/proton-run.sh run "/path/to/game/game_scarlett_master.exe" --execute
```

For an exact run in the same style as my measured build, I also used
`DXVK_CONFIG=dxgi_custom_device_id=10de:2803` for the RTX 4060 Ti.

I use the same runner for the Windows test suite:

```bash
CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_RUNNER="$PWD/scripts/proton-run.sh" \
  cargo xwin test --locked --workspace \
  --target x86_64-pc-windows-msvc
```

The Windows translation and run commands above work under Proton as well;
I substitute the runner for the direct `victory.exe` invocation.

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

A release build has no debugging aids. I build them in with

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
`crashes.log` in the title's data folder, so I keep it with the console
output.

For a startup breakpoint (`0x80000003`), I run the diagnostics build with
`VICTORY_CRASH_BREAKPOINTS=1` and keep the console output along with the new
`crashes.log` entries. A host DLL refusal includes the caller stack in the
console output. The refusal is expected for DLLs outside the host policy,
including `rpcrt4.dll`.

On Series 1.0.2.1, a missing `appdata0.rpf` reproduced the breakpoint at
`+0x2237178` before the first window. Restore the archive from the same
package beside the title executable. `run --execute` checks that it can
read the RPF9 header before starting this build.

To compare hardware and software rendering, I repeat the run with
`VICTORY_D3D12_WARP=1` and check the console for
`host device on the software adapter (WARP)` to confirm the WARP device was
created. I compare the exception's module and offset, stack frames, and
preceding messages across the two runs.

## License

GPL-3.0-or-later. See [LICENSE](LICENSE).
