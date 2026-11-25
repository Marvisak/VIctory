# Vice-Tay

I develop and test Vice-Tay on my Ryzen 9 9950X3D and RTX 4090 Windows PC. Vice-Tay is my fork of [VIctory](https://github.com/Marvisak/VIctory) for the Xbox Series build of Grand Theft Auto VI. It runs the title's x86-64 code natively, provides Xbox platform services, and translates console GPU programs to DXIL for Direct3D 12. The runtime operates offline.

## My PC

| Component | My configuration |
| --- | --- |
| Processor | AMD Ryzen 9 9950X3D, 16 cores and 32 threads |
| Motherboard | MSI MAG X870 TOMAHAWK WIFI (MS-7E51), revision 1.0 |
| BIOS | MSI 1.A93 |
| Graphics card | NVIDIA GeForce RTX 4090 with 24 GB GDDR6X |
| NVIDIA driver | 621.40 (`32.0.16.2140`) |
| Integrated graphics | AMD Radeon Graphics on the Ryzen CPU |
| Memory | 48 GiB DDR5 (2 × 24 GiB), configured for 8000 MT/s |
| System and benchmark SSD | Samsung 990 PRO 4 TB NVMe |
| Additional SSDs | Samsung 990 PRO 2 TB NVMe and Samsung 970 PRO 1 TB NVMe |
| Display | Samsung Odyssey OLED G8 G81SF, 3840 × 2160, 240 Hz class |
| Operating system | Windows 11 Pro 26H2 x64, build 26300.10075 |
| Windows paging file | 32 GiB allocated on the system SSD |

I recorded this configuration on 25 November 2026. Vice-Tay rendered on the RTX 4090 in the tests below. Each run records its own window, render or output setting where measured.

## Local test results

### 30-minute play session · 25 November 2026

I played [`386e885`](https://github.com/TaynanGT/Vice-Tay/commit/386e885) for **30 minutes 12 seconds in the world without a crash**, then closed the window myself. The process exited with code 0. Release build, Series X profile, `VICTORY_RELAY=2` with its snapshot ring, `VICTORY_BACK_ALIAS=1`, `VICTORY_COPY_NO_WAIT=1`, and the wave-wide form for four compute programs (`VICTORY_WAVE_WIDE_ONLY`). The pipeline library was warm; the shader cache was not.

The session started from a save in the prologue and went from the cell, through the corridor and the common room, into the robbery, with the player turning the camera and walking every minute. The renderer logged one frame line a second.

| Stretch | Median FPS | Frame time |
| --- | ---: | ---: |
| Whole session, 1,629 one-second samples | **7.0** | 143 ms |
| Corridor and common room | 7.4–9.5 | 105–135 ms |
| Robbery, under control | 6.3–7.8 | 129–160 ms |
| Scene change into the robbery, about two minutes | 1.9–3.8 | 264–537 ms |

Across the session the 25th and 75th percentiles were 4.6 and 7.9 FPS, the 90th 8.8, and the highest one-second sample 11.3.

I ended the session at 30 minutes because the process had grown to 36 GB of private memory and the PC had about 1 GB of commit left. That leak is the next thing to fix; a longer session needs it fixed first.

### First launch with an empty shader cache · 25 November 2026

I tested [`b033e17a`](https://github.com/TaynanGT/Vice-Tay/commit/b033e17a) on the system above in Series X mode. The diagnostics build used `VICTORY_RELAY=2`, frame capture, and a 1920 × 1080 window limit. I started with an existing D3D12 pipeline library and an empty shader cache. Shader compilation continued through loading and into the cinematic.

| Segment | UTC sample times | Samples | Median FPS | FPS range | Median frame time |
| --- | --- | ---: | ---: | ---: | ---: |
| Early startup and loading | 20:40:25–20:41:01 | 8 | 28.0 | 25.5–30.7 | 35.7 ms |
| Later loading intervals | 20:41:06–20:41:39 | 7 | 15.2 | 0.6–22.3 | 65.6 ms |
| Opening 3D cinematic | 20:41:46–20:43:01 | 11 | 0.8 | 0.1–1.1 | 1,207 ms |

These are medians of the renderer's periodic samples. I used intervals with at least 500 draw calls per frame for the cinematic row. The first qualifying sample appeared **88.5 seconds** after the first log entry. I closed the window during the cinematic at **2 minutes 56 seconds**. The process exited with code 0.

### Prologue under player control · 24 November 2026

I measured the prologue on the same RTX 4090 PC using [`05a78e3`](https://github.com/TaynanGT/Vice-Tay/commit/05a78e37) and the Series X profile. This run used 2560 × 1440 internal rendering, 3840 × 2160 output, and command relay level 2. Texture prefetching and the ray-tracing structure builder were disabled.

| What I measured | Method | FPS | Frame time |
| --- | --- | ---: | --- |
| Camera stationary in the first room | PresentMon, 268 presents over 35.76 s | 7.47 | 132.2 ms median, 157.2 ms at the 95th percentile |
| Turning the camera in the first room | Renderer log, 248 frames over 41.2 s | 6.01 | 166.3 ms mean |
| Scene transition | PresentMon, 155 presents over 35.51 s | 4.34 | 152.9 ms median, 521.7 ms at the 95th percentile |

The PresentMon figures use the intervals between presents. The prologue and cinematic runs use different revisions, scenes and settings. A matched run is still needed to measure the performance effect of the latest renderer changes.

## What I have verified on this PC

- The prologue has reached player control. I could turn the camera in the first room and record a scene transition.
- The current renderer build reached the 3D opening cinematic. Some figures appeared as dark silhouettes. In the earlier prologue run, the sky and window were overexposed, shadow edges were jagged, and some details were missing.
- A 30-minute session ran without a crash from the cell into the robbery. Memory grows by about 1 GB a minute, which limits sessions on this 48 GB PC.
- Campaign completion remains unverified on this setup.

The latest renderer changes recheck texture updates after queue overflow, reuse upload buffers for texture copies, and keep each pipeline and root signature once per command list. See [PR #14](https://github.com/TaynanGT/Vice-Tay/pull/14) for the changes and credits.

I ran the local graphics suite on [`b033e17a`](https://github.com/TaynanGT/Vice-Tay/commit/b033e17a). It reported **262 passed**, **0 failed**, and **9 ignored**. The Series X load-only check resolved every import in the executable and its four DLLs. [Windows CI](https://github.com/TaynanGT/Vice-Tay/actions/runs/45694584789) passed the workspace tests and both Clippy configurations.

## How I build and run Vice-Tay

I use Windows x64, the Visual Studio C++ build tools and the Rust toolchain pinned in [`rust-toolchain.toml`](rust-toolchain.toml). The RTX 4090 provides Direct3D 12, Shader Model 6.6 and mesh shader support for the world renderer.

Copy [`victory.example.toml`](victory.example.toml) and set the game, cache and save paths. Use your own compatible Xbox Series files and keep them outside the repository. The paths below are examples on the system drive.

```powershell
Copy-Item .\victory.example.toml .\victory.toml
cargo build --profile diag --locked -p victory --features diagnostics
$victory = '.\target\diag\victory.exe'
$title = 'C:\Vice-TayData\game\game.exe'
& $victory config
& $victory run $title --console series-x
```

The last command checks loading and import resolution. The launch below uses the same console profile, command relay and window limit as my 25 November benchmark.

```powershell
$env:VICTORY_RELAY = '2'
$env:VICTORY_WINDOW_MAX = '1920x1080'
& $victory run $title --console series-x --execute
```

For a release build, use `cargo build --release --locked -p victory` and `target\release\victory.exe`. The window limit preserves aspect ratio and applies to the displayed window. I record console profile, window size, internal resolution and output resolution separately where available.

The cache and save folders default to `%LOCALAPPDATA%\victory\cache` and `%LOCALAPPDATA%\victory\data`. Use `VICTORY_CONFIG` to select an isolated configuration for benchmarking, and `VICTORY_GAME_DIR`, `VICTORY_CACHE_DIR` and `VICTORY_DATA_DIR` to override individual paths.

Use `translate --cache --pipelines` with your shader inputs to prebuild the data that `run` uses. Record the shader and pipeline cache states when measuring. The host pipeline library belongs to the GPU and driver that built it.

```powershell
$game = Split-Path -Parent $title
& $victory translate --cache --pipelines "$game\shaders.rpf" $title `
    "$game\XboxBvhBuilder.dll" "$game\amd_fidelityfx_upscaler_xbox.dll"
```

Series X is the profile used for the numbers above. For a separate Series S test, select `--console series-s`. To use the PC upscaler with that profile, point `VICTORY_FIDELITYFX_DIR` at the [AMD FidelityFX SDK](https://github.com/GPUOpen-LibrariesAndSDKs/FidelityFX-SDK) directory containing `amd_fidelityfx_upscaler_dx12.dll`.

### Controls

Use an XInput controller or these keyboard mappings while the window has focus.

| Keyboard | Gamepad |
| --- | --- |
| `W A S D` / `I J K L` | Left / right stick |
| Arrow keys | D-pad |
| `Enter`, `Backspace`, `X`, `Y` | A, B, X, Y |
| `Q` / `E`, `Z` / `C` | Shoulders, triggers |
| `F` / `H` | Stick clicks |
| `Esc`, `Tab` | Menu, View |

## Measuring and reporting a run

I record the revision, hardware, driver, console profile, cache state, scene and sample interval alongside every result. If you can reproduce an issue on your own setup, include those fields with the failure mode and the relevant lines from `crashes.log`. Captures and logs containing game-derived data belong outside the repository. See the [asset policy](docs/asset-policy.md).

| Diagnostics setting | What I use it for |
| --- | --- |
| `VICTORY_LOG` | Select logging levels, for example `victory_loader=trace,info`. |
| `VICTORY_KEYS` | Schedule repeatable input, for example `30:Enter,35:X`. |
| `VICTORY_CAPTURE` | Save selected presented frames outside the repository. Use `VICTORY_CAPTURE_FRAMES` to select frames. |
| `VICTORY_CAPTURE_WORLD` | Capture frames relative to the first frame with 500 draw calls. |
| `VICTORY_D3D12_DEBUG=1` | Enable the D3D12 debug layer. |
| `VICTORY_D3D12_DRED=1` | Record device removal information. |
| `VICTORY_D3D12_WARP=1` | Repeat a run on the software D3D12 adapter. |
| `VICTORY_CRASH_BREAKPOINTS=1` | Record first-chance breakpoints in `crashes.log`. |

The repository test suites use synthetic fixtures. Run `cargo test --workspace --locked` for the full suite, or `cargo test -p victory-gpu --lib --locked` for the graphics crate. Use a current x64 [DXIL validator](https://github.com/microsoft/DirectXShaderCompiler/releases) through `VICTORY_DXIL` if the installed validator rejects Shader Model 6.6. See [architecture](docs/architecture.md) for how the runtime is put together.

## Credits and licence

I build on [Marvisak's VIctory](https://github.com/Marvisak/VIctory) and adapt contributions from [victory-fork](https://github.com/fxceless/victory-fork), [VItoriaBr](https://github.com/uiliannn/VItoriaBr), [KLBF Lab](https://github.com/PastiiJay/klbf-lab), and [VIctory-Belobug-AMD](https://github.com/Belobug/VIctory-Belobug-AMD). The commit history records individual contributions.

Vice-Tay is an independent project. This repository contains source code and synthetic test fixtures. The source is licensed under GPL-3.0-or-later. See [LICENSE](LICENSE).
