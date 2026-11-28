# VIctory

Runs Grand Theft Auto VI (Xbox Series X|S) natively on Windows, from files
the user supplies.

The title's x86-64 code runs unchanged inside our process; VIctory supplies
the platform underneath it and converts the title's data, ahead of time where
it can (shaders and pipelines too, see Getting started) and on first use
otherwise. See [docs/architecture.md](docs/architecture.md).

## Status (2026-11-25)

**Not playable yet, because of the framerate: about 1 frame per second in the
world.** On the maintainer's tested RX 6700 XT, the title boots, runs its
menus at 30 fps, loads the world, plays the intro cutscene and reaches the
**prologue** with the graphics fully displayed.

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

Maintainer baseline: Windows 11, AMD RX 6700 XT (12 GB).

> **Warning.** Entering the world on a real GPU hung it in earlier builds,
> and Windows then reset the display driver (every monitor goes black for a
> moment). The latest runs, several minutes each in the world, did not, but
> it is not ruled out; a run can also end on a GPU fault (`crashes.log`). A
> diagnostics build with `VICTORY_D3D12_WARP=1` renders on the software
> adapter instead (too slow to reach the world in minutes).

### RX 5700 XT / Ryzen 3900X diagnostics (2026-11-29)

This separate 8 GB GPU test reaches world rendering through the experimental
compute-to-vertex mesh fallback. The latest strict run showed the player,
vegetation and buildings, but the foreground remained dark and later views
still had rectangular artifacts. It measured about 1.3 fps, recorded 3,069
used texture nulls and 721 budget buffer nulls, and had no GPU removal.
These observations do not establish a usable picture, world interaction, or
playability on this card. The launch menu and cooked-profile path below add
local controls and selective diagnostics; they do not resolve those rendering
and allocation problems by themselves.

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
| Series S mode | Menus, loading and the world run with the PC upscaler (`VICTORY_FIDELITYFX_DIR`, about 1 s a frame on the RX 6700 XT baseline) | The title's own upscaler DLLs have six untranslated passes, so their picture is wrong |

## Getting started

Requirements: Windows 10/11 x64, a CPU with AVX2, a Direct3D 12 GPU with
shader model 6.6 (mesh shaders for the maintainer-tested world path; an
experimental compute-to-vertex fallback exists for cards without them), Rust (pinned in
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
`Kits/FidelityFX/signedbin` folder, which holds both
`amd_fidelityfx_loader_dx12.dll` and `amd_fidelityfx_upscaler_dx12.dll`.
Without them, or if the loader offers no
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

`victory run --resolution 1080p --quality medium` (also `720p`,
`WIDTHxHEIGHT`, `low` / `high` / `extreme`, `--aa off|msaa`,
`--anisotropic 0|2|4|8|16`) writes those fields into the title's
`settings.xml` before it starts and advertises that size as the console
mode. `victory graphics` does the same edit without starting the title.
The requested size does not guarantee a matching internal render size:
in a 640×360 test the title still allocated observed 1280×720 render
textures and the GPU reset before gameplay. Use 720p as the lowest
tested preset for now.
The title's FidelityFX context reports a 1280×720 scene and a 1920×1080
upscaled output, while PresentX supplies a separate 3840×2160 overlay.
`VICTORY_PRESENT_MAX=1280x720` optionally scales those planes into a
1280×720 host swap chain. This reduces final compositing and swap-chain
memory; the title still creates and renders its original textures.
The texture edge limit drops existing large mips from ordinary sampled
colour textures. It does not resize render targets, depth textures, or
unordered textures that shaders write; those sizes come from the title's
resource descriptions and its shader coordinates.
On an 8 GB card, start with Series S, `--resolution 720p`, `--quality low`,
`--aa off`, and `--anisotropic 0`. The RX 5700 XT has no mesh shaders;
this build translates supported mesh work into a compute-to-vertex path.
Some mesh work is still dropped, and reaching the world does not yet mean
its image is correct on that card.

### Windows launch menu

Double-click [Launch VIctory.cmd](Launch%20VIctory.cmd) to open the local
launcher. It has Game, Graphics, Performance, and Debugging tabs. The Game tab
shows a detected graphics adapter and offers 720p, 1080p, native, and custom
resolution profiles. A profile fills the render, final composite, and streamed
texture edge controls; changing one of those controls selects custom. A
one-time shader/pipeline prebuild caches missing work for the GPU and driver
used by D3D12. The Graphics tab offers 720p through 1440p,
low/medium/high/extreme title presets, AA,
anisotropic filtering, a frame limit, a master disable-shadows switch, and switches for motion blur,
reflections, variable rate shading, VSync, HDR, sharpening, particle and
long shadows, and tree tessellation. The Performance tab controls the
streaming budget, local VRAM threshold, texture mip limit, final composite
size, and related
fallbacks. The defaults favor an 8 GB RX 5700 XT: Series S, 720p, a 720p
final composite, low,
30 fps, 85% of the current DXGI local budget, 512-pixel texture mips, and
expensive effects off. The master switch defaults to on and overrides the
title's ray-traced, screen-space, world-height, particle, long, and alpha-tinted
shadow settings even when a higher graphics preset is selected. It also turns
off depth shading in the ambient fallback. The launcher enables fixed ambient
light by default so foreground objects remain visible without ray-traced
indirect light. A saved selection takes precedence over defaults.

Ray tracing is off while the master shadow switch is on. With that switch off,
ray tracing can be explicitly selected. The launcher sets both
title RT-shadow switches to false, and the port suppresses the console
BVH builder's `DispatchX` calls and any translated shader that contains
a reachable native BVH ray-traversal instruction. The same off mode is the
default for direct `victory run` launches. Enabling the toggle is experimental;
the port does not have a complete ray-tracing path. The title preset controls
its shader and rendering choices, but the port has no separate compiler
precision setting for “shader quality.”

Settings are saved locally at `%LOCALAPPDATA%\VIctory\launcher.json`.
Each launch writes its plan and logs under `%LOCALAPPDATA%\VIctory\logs`.
The launcher rejects network drives, UNC paths, and reparse points for its
state, settings, logs, game executable, and selected cooked inputs. It sets
cache, data, and an explicit local config file before shader prebuild or game
start, so a `victory.toml` in the game folder is not read by this launcher.
The launcher builds with Cargo's offline mode, then checks WFP and
Windows Firewall inbound, outbound, and loopback blocks for the exact
`victory.exe` path before starting the title. If isolation fails or UAC
is declined, the title does not start. `scripts\launch-victory.ps1
-ValidateOnly` prints the plan without building or launching; add
`-ValidateUi` to check that the form and selected controls construct.
The Debugging tab has local shader-hash traces and source dumps, streamed
texture traces, frame and render-target captures for UI/sprite inspection,
coverage, GPU timing, D3D12 debug/DRED, and full-resolution presented-frame
capture. Captures require a diagnostics build and write into the launch log
session. The render-target capture covers all targets at selected frames; it
is not a sprite-only trace. Sliders control the DXGI local VRAM streaming
threshold and root-buffer system RAM budget.

For a tighter VRAM budget, set `VICTORY_STREAM_MAX_EDGE=512` before
launching. This keeps only existing mips at or below 512 pixels on the
longest side of ordinary sampled colour textures. The default keeps the
current mip policy (2048-side textures start at 1024; 4096-side textures
also start at 1024). The setting leaves render targets, depth and
unordered-access textures at the title's requested dimensions. When the
streaming budget temporarily prevents another host texture, the next use
retries after memory becomes available. `VICTORY_STREAM_MB` sets the
combined local and non-local video-memory streaming threshold in MB
(default 9000); it does not cap the title's other GPU allocations.
`VICTORY_STREAM_LOCAL_PERCENT` sets the local-memory streaming threshold as a
percentage of Windows' current DXGI budget (default 85%). The budget can
change while the game runs, so this limit follows it rather than assuming the
card's full physical capacity is available.

For extracted, explicitly classified 2D source images, the offline
[`scripts/cook-textures.py`](scripts/cook-textures.py) pipeline writes separate
1080p and 720p images, semantic mip chains, and BC-compressed DDS files. The
launch menu's **Cook verified textures...** action requires a classified
manifest, extracted source-image folder, independently verified original title
mip identities, Texconv, Texassemble, and a separate output folder. It runs the
cooker, builds a runtime profile, validates its DDS files, and keeps local logs.
An optional 720p or 1080p profile then substitutes only listed, matching BC
mips during host uploads. Original RPF9 assets remain untouched; unlisted or
mismatched textures keep their original uploads. A built profile does not prove
that a game run used any listed texture. See
[`docs/texture-cooking.md`](docs/texture-cooking.md) for the evidence and format
limits. `scripts\launch-victory.ps1 -ValidateCookedOnly` checks a selected
profile without launching the game, and `-CookOnly` runs the saved one-time
cook inputs without opening the menu.

For a diagnostic indirect-light fallback, `VICTORY_AMBIENT=1` raises the
screen-probe light after the title's resolve (0.6 is the current level).
It uses a fixed level by default; `VICTORY_AMBIENT_SKY=on` follows the rendered
sky dome, and `VICTORY_AMBIENT_SKY=gain,least,power` tunes that response.
The launcher enables the fixed fallback by default. It is experimental and
has not been validated for a full play session on the RX 5700 XT.

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
            [--console series-x|series-s]
            [--resolution 720p|1080p|WxH] [--quality low|medium|high|extreme]
            [--aa off|msaa] [--anisotropic 0|2|4|8|16]
            [-- <title arguments>]
victory graphics [--resolution 720p|1080p|WxH] [--quality low|medium|high|extreme]
            [--aa off|msaa] [--anisotropic 0|2|4|8|16]
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
| `VICTORY_KEYS=30:Enter,35:W:3` | presses keys at those seconds, focus or not; optional final number holds a key for that many seconds (default 0.2, maximum 30) |
| `VICTORY_CRASH_BREAKPOINTS=1` | appends first-chance breakpoints to `crashes.log`, including ones a handler may recover from |
| `VICTORY_CRASH_SAVE=<rva>:<len>,...` | saves title memory to the cache folder on a crash |
| `VICTORY_CAPTURE=<dir>` | saves presented frames (powers of two, or `VICTORY_CAPTURE_FRAMES=<list>`) |
| `VICTORY_DUMP_TARGETS=1` | with the two above, saves every render target of those frames |
| `VICTORY_CAPTURE_WORLD=<list>` | with `VICTORY_CAPTURE`, also saves (and with `VICTORY_DUMP_TARGETS` dumps) frames at these offsets from the first frame that ran 500 draws; this is a heavy-render marker, not proof of interactive gameplay |
| `VICTORY_DUMP_WRITERS=<hash>,...` | with the two above, also the textures a pipeline containing a listed shader hash writes, the first frame it runs, and for its buffers a log line (dwords changed, dwords not zero) |
| `VICTORY_PEEK=<hex address>[:<bytes>],...` | in listed frames, those bytes of title memory logged after each submission. Listed frames also log targets of 16 texels or fewer as values, each table view's view words and whether its host slot is null, the host copy of every buffer of 64 bytes or fewer a draw reads (at submission and after its list), and each 1D or 3D texture a dispatch writes, read back right after it |
| `VICTORY_D3D12_DEBUG=1` | host debug layer |
| `VICTORY_D3D12_GBV=1` | with the above, GPU-based validation (bad descriptor use reported, not faulted; slow) |
| `VICTORY_D3D12_DRED=1` | on a GPU failure, the allocation hit and the commands in flight |
| `VICTORY_DISABLE_SHADOWS=1` | override all exposed title shadow settings and suppress BVH traversal, even with a higher quality preset |
| `VICTORY_SKIP_PIPELINES=<hash>,...` | skip host pipeline creation and omit draws and dispatches, including indirect ones, when the pipeline contains a listed 16-digit shader hash |
| `VICTORY_DISABLE_731_COMPOSITE=1` | release-capable visual fallback: skip the world composite that introduces coarse sky tiles on the RX 5700 XT; some post-lighting detail will be absent while its width/input mapping is investigated |
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
