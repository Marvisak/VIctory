# Running under Wine/Proton on Linux

VIctory is a Windows program. On Linux it runs under Wine, in practice a
Proton build with vkd3d-proton for Direct3D 12. This is not a supported
platform; these notes are what has worked for testers.

## Setup

- Run `victory.exe` with `proton runinprefix`, in a prefix of its own.
  Tested: proton-cachyos 11.0 with the `d3d12.dll` and `d3d12core.dll` of
  Proton Experimental 11.0 (vkd3d-proton 3.2.0) next to `victory.exe`.
- Use those native Direct3D 12 DLLs and **`d3dcompiler_47.dll` from a Windows
  10/11 SDK** (10.0.x; not winetricks' 6.3.9600 build, which lacks `cs_5_1`),
  next to `victory.exe`:

  ```
  WINEDLLOVERRIDES="d3d12,d3d12core,dxgi,d3dcompiler_47=n,b"
  ```

  Wine's own `d3dcompiler_47` refuses the helper shaders VIctory compiles at
  run time (`E5005: Method 'Load' is not defined on type
  'RWByteAddressBuffer'`). The log then says `no argument clamp: indirect
  execution dropped`, and every indirect draw and dispatch is skipped: most
  of the world is not drawn.

## Common snags

- **The start screen waits for ever without a pad.** Wine's XInput can report
  a "pad" that never presses anything (here a mouse's HID interface), and the
  keyboard stands in for a pad only while there is none. The log shows
  `input device device=0 connected=true` for that phantom pad.
  `SDL_GAMECONTROLLER_IGNORE_DEVICES_EXCEPT=0000/0000` hides every pad from
  Wine, and Return gets past the start screen.
- **`VKD3D_SHADER_CACHE_PATH` is a folder, not a file.** Given a file,
  vkd3d-proton logs `Failed to map read-only cache` and compiles every
  pipeline again on every run.

## NVIDIA

- Some translated shaders crash the driver's compiler, which ends the
  process. With `VICTORY_PIPELINE_JOURNAL=1` (#491, not merged yet), a
  pipeline whose creation ended a run is refused from the next run on.
- Set `VKD3D_DISABLE_EXTENSIONS=VK_NV_raw_access_chains`. With vkd3d-proton's
  raw access chains, the title's hair passes hang the GPU (RTX 5080, driver
  615.71); without them they run, and the rest is drawn as before.
- Some of the title's compute passes take the GPU past the driver's timeout
  (`NVRM: Xid 109 ... CTX SWITCH TIMEOUT` in the kernel log, then the title's
  own watchdog). `VICTORY_SKIP_PIPELINES=<hash>,...` (a diagnostics build)
  drops a pass. Which passes, and what fixes them, is tracked in
  [#844](https://github.com/Marvisak/VIctory/issues/844#issuecomment-7159554401)
  and [#2220](https://github.com/Marvisak/VIctory/issues/2220#issuecomment-7159553404).

## Faster: the wineserver patch

Under Wine, much of a frame's time goes into the wineserver, which keeps each
process's mapped views in a list and walks it on every memory query. VIctory
asks many such queries per frame. ajsem1337's change
([#340](https://github.com/Marvisak/VIctory/issues/340#issuecomment-7126653768),
with tests and contributions by
[neonbyte1](https://github.com/Marvisak/VIctory/issues/340#issuecomment-7136559706))
keeps them in a tree. With it, a world frame went from about 4 s to 1 s
here, and from about 3 s to 1 s on an RX 9070.

It is proposed to Wine ([bug 60868](https://bugs.winehq.org/show_bug.cgi?id=60868))
and to wine-cachyos ([CachyOS/wine-cachyos#50](https://github.com/CachyOS/wine-cachyos/pull/50)).
Until a Proton release ships it, the wineserver has to be built from the
Wine source of the exact Proton you run, with the patch applied. Ports of
the patch, scripts that build it and notes for several Protons are kept
[outside this repository](https://github.com/desplmfao/VIctory/blob/renderer/docs/linux-wineserver.md).
