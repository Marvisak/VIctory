# Offline texture cooking status

## Selective runtime profiles

The runtime can now use a **verified subset** of the cooked BC textures at
launch. The cooker still needs extracted and explicitly classified source
images. The original game archives stay untouched, so a 720p profile, a 1080p
profile, or the original texture can be selected on later launches. A profile
does not automatically cover every archive texture or sprite. The selected
host upload must have the same DXGI BC format, dimensions, and mip mapping as
the cooked DDS. The runtime hashes the original detiled blocks of each
selected mip before substitution; any mismatch leaves the original upload in
place. Format-changing normal maps and HDR/array/cube resources remain
unsupported by this bridge.

After running `cook-textures.py` with `--texconv` and `--texassemble`, prepare
an identity JSON from independently verified title-resource metadata and the
`BLAKE3` hashes of the **original detiled BC mip bytes**. The hashes are not
PNG hashes, archive hashes, or DDS hashes. `VICTORY_TRACE_STREAMED_TEXTURES=1`
can log the retained mip hash for a candidate that the game actually uploads;
the identity still needs a checked material/source match. Provide one hash
for every mip the selected host texture retains:

```json
{
  "assets": [
    {
      "profile": "720p",
      "output_name": "tele_beach_07_diffuse2.png",
      "title_width": 4096,
      "title_height": 2048,
      "title_mips": 3,
      "title_format": 72,
      "dropped": 2,
      "source_blake3": ["dc860eeb40128b375a6fa26e0793ef4781ddbb5adc6f764d72fc2eed68d0f8d1"]
    }
  ]
}
```

`output_name` must equal the matching entry in `cook-report.json`. The sample
hash above applies only to the verified `tele_beach_07` retained mip; it is
not a generic identity for other 4K images. The profile builder checks the
cooker report's SHA-256 and the DDS header, full payload, format, dimensions,
and retained mip count. It emits `runtime_720p.toml` and/or
`runtime_1080p.toml` beside `cook-report.json` only for the provided identity
records. It refuses a profile whose cooked format differs from the title's.
With Python 3.10+, `blake3`, and `tomli` (or Python 3.11+'s `tomllib`):

```powershell
python scripts/build-runtime-texture-profile.py --cooked-root E:\cooked-textures --identities E:\verified-identities.json
python scripts/build-runtime-texture-profile.py --validate-manifest E:\cooked-textures\runtime_720p.toml --profile 720p
python scripts/test-runtime-texture-profile.py
```

The launch menu can use the matching manifest when the cooked root is set.
For direct launches, set `VICTORY_COOKED_PROFILE_MANIFEST` to the absolute
fixed-drive path of `runtime_720p.toml` or `runtime_1080p.toml`. The runtime
checks local paths without reparse points, indexes at most 16,384 verified mip
entries from a 4 MiB manifest, reads DDS files of at most 64 MiB, verifies
their BLAKE3, and keeps at most 128 MiB of DDS data in its cache. It logs
`verified cooked texture mip substituted into host upload` when a match is
actually used. A loaded manifest by itself does not prove that the game
requested any listed texture. A bad DDS logs a warning; an upload whose
identity is absent keeps the original title bytes. The existing single-texture
`VICTORY_COOKED_DIFFUSE_DDS` experiment remains independent; the profile
manifest takes priority if both target the same mip.

This bridge replaces BC blocks during a host upload. GPU-written render
targets, title virtual memory, shader programs, game files, and sprite
semantics are unchanged. The 720p/1080p launcher setting separately controls
runtime dimensions, streamed texture edge cap, and one-time shader/pipeline
cache prebuild on this GPU. It does not turn every 4K asset into a new 720p
asset without a classified cook and verified identity record.

`scripts/cook-textures.py` makes **separate 1080p and 720p outputs from
explicitly classified, extracted 2D images**. It does not modify the original
RPF9 archives. Its ordinary output is a preparation stage. Only explicitly
verified same-format BC mips listed in an optional runtime manifest can enter
the title's host upload path. No game speed or image gain from the profile has
been demonstrated yet.

## Inputs and output

Install Python, Pillow, and NumPy. Export an asset to PNG/TIFF using a decoder
that handles Xbox tiling and BCPack. Never classify a texture from BC format
alone: a BC5 texture can be a normal, a two-channel data map, or something
else. Verify the title's material slot or inspect the texture before assigning
its role. Arrays, cube maps, 3D textures, HDR textures, and palette/lookup
textures are deliberately outside this cooker's 2D image path.
Check exporter metadata for a `texture_fallback` or low-mip warning; a flat
fallback PNG cannot recover missing full-resolution detail by cooking it.
If you build [SixNG](https://github.com/holdmysocks/SixNG) separately, run
the following to inventory an indexed texture archive:

```powershell
python scripts/census-sixng-textures.py --sixng <sixng.exe> --game <game-dir> --work <index-dir> --archive textures --output <census.csv>
```

Its `role` and `tier` columns stay blank until
material-slot evidence or manual inspection justifies a classification.
For indexed `levels.rpf` drawables, `sixng texture-users levels` writes a
material-slot index. Join it with the map-store census using
`scripts/join-material-roles.py --census <world-maps-census.csv> --users
<index/texture_users.jsonl.gz> --output <world-role-candidates.csv>`.
This labels unambiguous `diffusetex`/`diffusetex2` and
`bumptex`/`bumptexture`/`bumptexture2` slots as albedo and normal
*candidates*. It exposes conflicting slots and leaves atlas layout,
normal encoding, alpha semantics, and screen-coverage tier for review before
constructing a cook manifest.

Example manifest (`manifest.json`):

```json
{
  "assets": [
    {"source": "wall_color.png", "kind": "albedo", "tier": "environment"},
    {"source": "wall_normal.png", "kind": "normal", "tier": "environment", "normal_encoding": "xy"},
    {"source": "wall_roughness.png", "kind": "roughness", "tier": "environment", "normal_source": "wall_normal.png", "normal_encoding": "xy"},
    {"source": "leaves.png", "kind": "mask", "tier": "prop", "channel": "a", "alpha_cutoff": 0.5},
    {"source": "packed_orm.png", "output": "metalness.png", "kind": "metalness", "tier": "prop", "channel": "b"},
    {"source": "trim.png", "kind": "albedo", "tier": "environment", "atlas": true, "rects": [[0,0,1024,1024],[2048,0,1024,1024]], "gutter": 4}
  ]
}
```

Run:

```powershell
python scripts/cook-textures.py --manifest manifest.json --input E:\exported-textures --output E:\cooked-textures
```

The script emits `assets_1080p/`, `assets_720p/`, semantic mip PNGs under
`mips_1080p/` and `mips_720p/`, and `cook-report.json` with source/output
SHA-256 hashes, semantic settings, DDS format, byte size, dimensions, and
mip count. It never upscales small source
images. The target longest edges are 2048/1024 for hero and environment,
1024/512 for props, and 2048/1024 for UI. Roughness and metalness are capped
at 1024/512. Alpha masks can be single-channel or explicitly select a packed
channel (`r`, `g`, `b`, or `a`). Packed roughness/metalness also require a
channel. Normal maps need their `normal_encoding` set to `xy` if Z is
reconstructed from RG, or `xy_ag` for a verified DXT5nm A/G packing; the
default is three stored components. A BC3 format or purple preview alone does
not establish the packing. Albedo and UI default to sRGB; set
`"color_space": "linear"` for a verified linear-color input. Normal and scalar
maps always use linear values.
Packed sources can appear more than once if each entry has a distinct
`output` PNG path.

Albedo is filtered in linear light, then encoded to sRGB. Normal vectors are
filtered as signed vectors and normalized. Roughness paired with a normal map
gets a Toksvig-style variance adjustment. Mask filtering preserves the
specified cutoff's covered fraction as closely as the target pixel grid
allows. For cutout alpha stored in an albedo RGBA image, set `alpha_cutoff`
on that albedo entry as well; this preserves coverage in its alpha channel
while filtering color in linear light. Atlas rectangles are filtered
independently; optional gutters copy the closest island's edge pixels only
outside declared islands. Unknown atlas geometry
fails rather than risking UV bleed.

With Microsoft's `texconv.exe`, add `--texconv C:\path\to\texconv.exe` to
write BC-compressed DDS files with the custom mip chain (BC1 sRGB for opaque albedo, BC7 sRGB
for albedo with alpha and UI, BC7 UNORM for full XYZ normals, BC5 for
verified XY normals, and BC4 scalar). BC5 cannot preserve a negative Z in
a source XYZ normal. The cooker verifies the
DX10 DDS format, dimensions, mip count, and payload length. The full chain
needs `texassemble.exe`; pass `--texassemble C:\path\to\texassemble.exe` or
put it on PATH. The shorter axis limits the final assembled mip level (for
example 2048×1024 ends at 2×1) because DirectXTex `from-mips` rejects the
next level. Atlas chains stop earlier if an island would collapse or overlap;
the report records this. Repacking into RPF9 remains unimplemented. The
separate PNGs remain the reference output. `texconv` is given the sRGB input
flag for color textures: a gray code value 128 otherwise comes back near 188
after BC compression instead of staying near 128.

Run `python scripts/test-cook-textures.py` for synthetic checks. A game test
using cooked assets requires an extractor, verified asset identities and
material roles, a title-compatible substitution path, and a render comparison.

On 2026-11-28, the locally built SixNG CLI indexed the supplied
`textures.rpf` (1,370 entries, 1,344 named) and exported an actual
`overlay_cctv_generic04` image. This cooker produced a 2048×1024 1080p
reference and a 1024×512 720p reference from that source. The mipmapped BC7
DDS files passed header/payload validation and the 720p PNG was inspected visually. This
checks the export → classify → cook → compress path for **one UI asset**;
neither cooked file was injected into the game.
The exported `non_break_glass_decals` alpha channel was also cooked from
1024×1024 to 512×512 and compressed as BC4 with semantic mips. At the 0.5
cutoff, its covered fraction was 0.05026 in the source and cooked PNG, and
0.05000 after BC4 decoding. The BC7 overlay's 720p top level decoded with a
1.02 code-value RMSE across RGBA versus the cooked PNG after the linear-light
UI filter update. Both checks concern
extracted files only.

The archive also contains a 4096×1024 `ffx_titles_chapter4b` UI image tagged
BC7 UNORM. With an explicit linear color-space manifest, it cooked to
2048×512 (1080p, 10 mips) and 1024×256 (720p, 9 mips). The 720p BC7 file is
349,700 bytes; its decoded top level was visually readable and had 2.23
code-value RMSE across RGBA versus the cooked PNG. This is a useful 4K source
check, though a title card does not stand in for world materials.

A complete metadata census of `textures.rpf` found 4,604 textures in 1,356
texture dictionaries, with zero dictionary inspection errors. Only eight
have a 4096-pixel edge, all in the same chapter-title dictionary; 78 have a
2048-pixel edge or larger. The dominant formats are 2,634 BC6H UF16 (HDR)
and 1,434 BC7 UNORM. 4,510 textures have just one mip. This archive is not
a safe target for a blanket "4K to 720p" rule, and the current 8-bit cooker
deliberately excludes the BC6H/HDR data.

The eight indexed `data/ttdb/maps/ttdb_maps_0_7.ttpf` through
`ttdb_maps_7_7.ttpf` stores contain 140,727 texture records in total. Only
976 have an edge of 2048 pixels or more, and four reach 4096 pixels
(4096×2048). All eight stores listed without errors. Generate the reproducible
CSV and summary with `scripts/census-ttpf-textures.py` and a repeated
`--store data/ttdb/maps/ttdb_maps_N_7.ttpf` argument for each indexed store.
These are metadata counts, not confirmed material roles or proof that every
texture is visible in the current world scene. They strengthen the case for
fixing the title's 4K GPU render targets and missing draw bindings before
blanket world-texture downsampling.

As a material-linked world check, `levels.rpf@f9c05010` (an interior ceiling
drawable) identifies texture hash `a64047bd240e2b87` in its `diffusetex`
base-color slot. SixNG decoded the matching 1024×1024 BC1 sRGB texture from
`ttdb_maps_7_7.ttpf` with no missing bytes. The cooker wrote and validated a
full 11-mip BC1 sRGB DDS for both profiles; decoded RGB differs from the
cooked top-level PNG by 2.30 code values RMS. The texture is already at the
720p environment cap, so this validates semantic selection and compression,
not a size reduction or a game speedup.

The completed `levels.rpf` material index contains 315,577 models, 1,064,123
distinct material records, and 374,212 referenced texture hashes. Joining
this index to the eight map stores gives 44,312 albedo candidates, 25,974
normal candidates, 70,437 other or unresolved records, and four role
conflicts. The four 4096×2048 map textures are two `diffusetex2` and
`bumptexture2` pairs used by the `tele_beach_07` drawable. The previewed
normal has a sampled negative-Z fraction near 3.9%, so a BC5 output that
reconstructs positive Z would be destructive. Full XYZ normals stay in
BC7 UNORM; only verified XY/AG normals use BC5.
One verified material pair (`9a1e3a19154449aa` diffuse and
`e8f2e32fdd42f10f` normal) was exported without missing bytes and cooked
from 4096×2048 to 2048×1024 and 1024×512 with full semantic mip chains.
The 720p BC1 sRGB diffuse DDS is 349,684 bytes and the BC7 UNORM XYZ normal
DDS is 699,220 bytes. Their decoded top levels differ from the cooked PNGs by
6.77 and 6.58 code values RGB RMS, respectively. Negative Z is present in
3.32% of cooked normal pixels and 3.20% after BC7 decode; about 0.56% of
pixels differ in Z sign near zero after compression. These are offline
texture results, not evidence that the title uses the cooked files yet.

## Heap accounting when cooked assets are installed

The current runtime already allocates a smaller host D3D12 resource for a
streamed texture after dropping leading mips. It keeps the title's original
texture description and memory footprint for reads, translates the upload
subresource indices, and clamps the host SRV to the retained mip chain. Host
allocation size comes from `GetResourceAllocationInfo` for that smaller
resource, including D3D12 alignment; the title's virtual heap and source
offsets must continue to use the original layout. Small eligible sampled
textures request 4 KiB alignment, with a fallback to 64 KiB placement.

The offline cooked DDS files are **not installed** in the title's archive or
substituted by default. The optional profile above can replace exact verified
same-format host uploads. This does not reduce the host texture's allocation
beyond the existing retained-mip size; only a smaller host resource changes
that allocation. A format-changing profile (BC1 to BC7 or BC5) needs separate
host resource support and is rejected. A 720p swapchain does not make
GPU-written 3840×2160 render targets smaller; those need a separate render
target and viewport change. Count GPU heap savings from the host allocation
reported by the driver, not from the cooked DDS file size.

### Original single-texture upload proof

The earlier single-file runtime bridge is deliberately limited to the verified
`tele_beach_07` diffuse material. The title source is 4096×2048 BC1 sRGB
with three mips. The existing streaming policy keeps its 1024×512 mip 2,
which matches the 720p cooked BC1 sRGB DDS top level. Set
`VICTORY_COOKED_DIFFUSE_DDS` to the absolute path of that exact DDS before
launching the game. The bridge verifies a fixed local volume with no reparse
points, the DDS header, full mip payload and file hash, then matches the
detiled source mip's BLAKE3 before substituting the cooked blocks in the host
upload. It leaves title virtual memory, other materials, and shader formats
untouched. Look for `verified cooked diffuse substituted into host upload` in
the local log; absent that line, the title is still using its own bytes.
This is an experimental single-texture path, not yet a tested game image or
speed improvement. The paired title normal is BC1 while the offline cooked
normal is BC7, so it cannot enter this same-format bridge.

The isolated 720p test `rx5700-offline-20261128-085737Z` reached the world
plus 60 frames with no cooked candidate or substitution log. This prologue
scene did not upload the `tele_beach_07` diffuse source, so it provides no
visual or performance evidence for this override. To identify a material
actually loaded in that scene, set `VICTORY_TRACE_STREAMED_TEXTURES=1` in a
diagnostics build. The trace reserves 4 preworld and 8 world entries for
all-zero sources, separately from 24 preworld and 128 world populated
texture (address, content hash) pairs. Reused addresses whose bytes changed
can appear again. It records original and host shape/format/mips, dropped mip
count, and BLAKE3 of the tightly packed detiled retained mip. It logs no
pixels and writes no external file. A second line confirms when
`CopyTextureRegion` for that exact traced subresource was recorded; it does
not assert that the GPU completed the command. A bounded audit on the first
presented frame at or after world frame + 60
rechecks at most 12 sources that were uploaded while empty. It reports
whether title memory has since become nonzero and whether that exact host
subresource had another recorded copy. `stale_candidate=true` means a source
became nonzero without a later upload; it does not prove that a world draw
actually sampled the host texture. Join an observed nonzero hash
to an extracted TTPF source before extending the cooked substitution list.
`python scripts/analyze-streamed-texture-trace.py <stderr.log>` counts zero
and populated uploads and same-shape address recoveries. The first trace run
(`rx5700-offline-20261128-090756Z`) recorded 152 detiles, 52 all-zero
(41 in the world phase), and 20 addresses where a zero mip was later detiled
with nonzero bytes. It did not record per-texture upload confirmation, so
whether those zero mips reached the host texture remains to be measured with
the added copy log.
The next run (`rx5700-offline-20261128-091836Z`) confirmed 12 empty and 50
populated `CopyTextureRegion` commands, one empty source followed by a
populated copy at the same address and shape, and no later copy for the other
11 empty sources during that run. All 21 nonzero world 2048×2048 BC1 mip-1
hashes were matched to `data/ttdb/props/octa_imposters.ttpf`; the exported
source is a multi-view tree atlas with packed-looking color, so it has not
been classified as ordinary diffuse color or cooked for substitution.
The longer isolated run (`rx5700-offline-20261128-093726Z`) recorded 12
empty and 152 populated upload commands. At the first presented frame at or
after world + 60, 11 of the 12 watched title sources were still zero and one
could not be read. Six of those 11 had received at least one later copy,
including populated copies in the log; their title memory had returned to
zero by the audit. No watched source was nonzero without a later copy. This
does not justify changing the refresh path on the current evidence. It also
does not prove whether the zero-filled host textures were sampled in a world
draw: the bindless heap may prepare textures that no shader uses that frame.

## Why resolution still needs runtime work

The 720p launch still creates GPU-written 3840x2160 targets. Static tracing
found one target creation path reading width at title VA `0x14a9438ec` and
height at `0x14a9438f0`; those globals are not simply the 720p setting in the
current run. Offline cooking sampled images cannot resize a GPU-written target
or change its viewport, dispatch dimensions, and coordinate calculations.
Shader simplification also needs per-pass evidence: a compiled shader has no
general "4K" parameter to downsample. POM, SSR, and similar changes should
be tied to identified passes and compared with images and GPU timings before
enabling them in a profile.

The prior capped 720p world capture reported a 2.3 fps median, 181.4 ms
median CPU preparation, 88.7 ms median copy work, and 108.3 ms median GPU
work. It also dropped many draws when root views exceeded the nonlocal copy
admission cap. Texture cooking has not been linked to that run, and reducing
sampled texture dimensions alone cannot repair those root-view failures or
reach a 33.3 ms frame budget.
