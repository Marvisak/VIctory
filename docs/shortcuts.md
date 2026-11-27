# Deliberate shortcuts

An index of the inline `shortcut:` comments: places where the implementation
deliberately does less than the full behaviour, usually because the title has
not needed it yet. This is a pointer list, not a to-do list; it does not
claim any of these are bugs.

- Snapshot: `main` `63bcca43c6a00a758510398fa43096ce7e4f7df0`.
- 135 comments across 43 files.
- The asset-policy note is tracked by #2246; see the last section.

## By file


### `crates/victory-formats/src/bcpack.rs`

- `crates/victory-formats/src/bcpack.rs:403` - shortcut: one lane step at a time (BC1 roughly 30-60 MB/s, BC7 15-20 MB/s

### `crates/victory-formats/src/texture.rs`

- `crates/victory-formats/src/texture.rs:532` - shortcut: no addressing for the XOR modes (`_X`, `_T`) and MSAA;
- `crates/victory-formats/src/texture.rs:757` - shortcut: those formats have no layout here; add them when a title uses
- `crates/victory-formats/src/texture.rs:808` - shortcut: no compression metadata planes (HTILE, CMASK, FMASK, DCC);

### `crates/victory-gpu/src/bindless.rs`

- `crates/victory-gpu/src/bindless.rs:196` - shortcut: a title copy into texture memory recorded in the same
- `crates/victory-gpu/src/bindless.rs:249` - shortcut: a texture read again after a gap is brought up after
- `crates/victory-gpu/src/bindless.rs:306` - shortcut: what shaders write through the heap's unordered access views

### `crates/victory-gpu/src/console.rs`

- `crates/victory-gpu/src/console.rs:19` - shortcut: later system updates gave Series S titles more (amount not

### `crates/victory-gpu/src/descriptor.rs`

- `crates/victory-gpu/src/descriptor.rs:452` - shortcut: views without a resource object (constant buffers, memory
- `crates/victory-gpu/src/descriptor.rs:756` - shortcut: a placement is recognised by its size matching the count,

### `crates/victory-gpu/src/descriptor/host.rs`

- `crates/victory-gpu/src/descriptor/host.rs:156` - shortcut: a host that caps its heaps below `HEAP_VIEWS` (NVIDIA)
- `crates/victory-gpu/src/descriptor/host.rs:161` - shortcut: descriptors past the host's limit are left
- `crates/victory-gpu/src/descriptor/host.rs:430` - shortcut: a buffer view holds at most 2^27 elements (512 MiB raw).
- `crates/victory-gpu/src/descriptor/host.rs:629` - shortcut: a buffer view holds at most 2^27 elements (512 MiB raw);
- `crates/victory-gpu/src/descriptor/host.rs:787` - shortcut: views of textures the title placed itself are made
- `crates/victory-gpu/src/descriptor/host.rs:1092` - shortcut: the address, format and tiling fields read 0 (no translated
- `crates/victory-gpu/src/descriptor/host.rs:1277` - shortcut: what shaders write stays in the host copy; title memory (and
- `crates/victory-gpu/src/descriptor/host.rs:1345` - shortcut: such a view (sRGB of UNORM, say, on a texture the host made
- `crates/victory-gpu/src/descriptor/host.rs:1422` - shortcut: such views are null; spell the description out (dimension,
- `crates/victory-gpu/src/descriptor/host.rs:1801` - shortcut: slots with no view at all (kind unknown) get this null 2D

### `crates/victory-gpu/src/device.rs`

- `crates/victory-gpu/src/device.rs:162` - shortcut: the creation interface is the only sign of the newer layouts
- `crates/victory-gpu/src/device.rs:315` - shortcut: every use of every format; answer from the host

### `crates/victory-gpu/src/draw.rs`

- `crates/victory-gpu/src/draw.rs:207` - shortcut: a root view has no size; constant buffers are taken as 64 KiB,
- `crates/victory-gpu/src/draw.rs:920` - shortcut: what shaders write through a root UAV goes back to title

### `crates/victory-gpu/src/dxgi.rs`

- `crates/victory-gpu/src/dxgi.rs:152` - shortcut: memory sizes are the title's split (GPU-optimal, standard),
- `crates/victory-gpu/src/dxgi.rs:232` - shortcut: fixed mode, not the host display's; report the host's rates if

### `crates/victory-gpu/src/fence.rs`

- `crates/victory-gpu/src/fence.rs:102` - shortcut: a null event means "block until reached"; nothing can

### `crates/victory-gpu/src/frame.rs`

- `crates/victory-gpu/src/frame.rs:9` - shortcut: event offsets are ignored (every event is at the tick); tie

### `crates/victory-gpu/src/hold.rs`

- `crates/victory-gpu/src/hold.rs:230` - shortcut: a safety valve against waits nothing will satisfy (a write

### `crates/victory-gpu/src/host.rs`

- `crates/victory-gpu/src/host.rs:4` - shortcut: every submission is waited for, so CPU and GPU never overlap;
- `crates/victory-gpu/src/host.rs:2164` - shortcut: one buffer per copy; pool them once uploads are frequent.

### `crates/victory-gpu/src/indirect.rs`

- `crates/victory-gpu/src/indirect.rs:14` - shortcut: the bounds (2^24 vertices, 2^26 vertex instances, 65,535
- `crates/victory-gpu/src/indirect.rs:201` - shortcut: a buffer a shader writes through an unordered view and the

### `crates/victory-gpu/src/list.rs`

- `crates/victory-gpu/src/list.rs:509` - shortcut: whole resources, from the state the list tracked
- `crates/victory-gpu/src/list.rs:846` - shortcut: depth and stencil planes copy out in typeless formats of
- `crates/victory-gpu/src/list.rs:1132` - shortcut: no query runs on the host GPU; results are plausible values,
- `crates/victory-gpu/src/list.rs:1519` - shortcut: dispatches whose group count is in GPU memory (indirect)
- `crates/victory-gpu/src/list.rs:1586` - shortcut: dropped until their shaders translate (decision 0007).
- `crates/victory-gpu/src/list.rs:1599` - shortcut: signatures that change root arguments are dropped (logged
- `crates/victory-gpu/src/list.rs:1825` - shortcut: 0x470's purpose is unknown; ignored until a title shows it

### `crates/victory-gpu/src/memory.rs`

- `crates/victory-gpu/src/memory.rs:30` - shortcut: a page two copies share without either holding all of it is
- `crates/victory-gpu/src/memory.rs:32` - shortcut: a table names copies in at most [`CAPACITY`] 256 MiB regions;
- `crates/victory-gpu/src/memory.rs:85` - shortcut: a marked page partly in such a range loses what shaders

### `crates/victory-gpu/src/pipeline.rs`

- `crates/victory-gpu/src/pipeline.rs:12` - shortcut: only vertex plus pixel pipelines from streams and compute
- `crates/victory-gpu/src/pipeline.rs:445` - shortcut: the extensions (console shader settings) are ignored, as is
- `crates/victory-gpu/src/pipeline.rs:673` - shortcut: a pipeline the library refused to store is listed

### `crates/victory-gpu/src/pipeline/make.rs`

- `crates/victory-gpu/src/pipeline/make.rs:17` - shortcut: the budget (half a second of waiting per title frame) is a

### `crates/victory-gpu/src/primitive.rs`

- `crates/victory-gpu/src/primitive.rs:7` - shortcut: the fourth vertex is computed from vertex inputs, not from

### `crates/victory-gpu/src/queue.rs`

- `crates/victory-gpu/src/queue.rs:187` - shortcut: a safety valve against a wait nothing satisfies (a write we do
- `crates/victory-gpu/src/queue.rs:402` - shortcut: the fence is not kept alive while the queue waits for it;
- `crates/victory-gpu/src/queue.rs:490` - shortcut: PresentX does not wait behind work the queue still holds (it
- `crates/victory-gpu/src/queue.rs:647` - shortcut: done ahead of work the queue still holds (counted in the frame

### `crates/victory-gpu/src/resource.rs`

- `crates/victory-gpu/src/resource.rs:234` - shortcut: a depth bias on such a target counts in float steps, not
- `crates/victory-gpu/src/resource.rs:630` - shortcut: the console runtime's own choice of page size and memory type
- `crates/victory-gpu/src/resource.rs:1246` - shortcut: title textures of a replaced host texture keep it (placed
- `crates/victory-gpu/src/resource.rs:1683` - shortcut: a use racing this (a title thread recording) may view the
- `crates/victory-gpu/src/resource.rs:1789` - shortcut: same start only; overlapping ranges, other mips or slices of
- `crates/victory-gpu/src/resource.rs:2019` - shortcut: never released (one host texture per distinct address and
- `crates/victory-gpu/src/resource.rs:2050` - shortcut: the old one is kept (view heap slots made from it view its

### `crates/victory-gpu/src/shadow.rs`

- `crates/victory-gpu/src/shadow.rs:201` - shortcut: a page the copy holds only in part (at its ends) has no
- `crates/victory-gpu/src/shadow.rs:303` - shortcut: what shaders wrote is found by keeping the range's bytes
- `crates/victory-gpu/src/shadow.rs:488` - shortcut: a page the title wrote that was not armed (a write through
- `crates/victory-gpu/src/shadow.rs:897` - shortcut: marks do not tell lists apart: a page that a withheld list and
- `crates/victory-gpu/src/shadow.rs:1056` - shortcut: copies of windows are never freed; key them to the title's

### `crates/victory-gpu/src/texture.rs`

- `crates/victory-gpu/src/texture.rs:13` - shortcut: a change detiles the whole texture, not the pages written.
- `crates/victory-gpu/src/texture.rs:347` - shortcut: a title copy into texture memory recorded in the same
- `crates/victory-gpu/src/texture.rs:350` - shortcut: a texture the host copied into keeps the host's contents

### `crates/victory-gpu/src/twin.rs`

- `crates/victory-gpu/src/twin.rs:16` - shortcut: uses the lists do not name (shaders indexing the heap

### `crates/victory-gpu/src/watch.rs`

- `crates/victory-gpu/src/watch.rs:33` - shortcut: a system call writing into a guarded block fails; the known
- `crates/victory-gpu/src/watch.rs:379` - shortcut: memory free or reserved when the range is tracked counts as the

### `crates/victory-hle/src/modules/apu.rs`

- `crates/victory-hle/src/modules/apu.rs:163` - shortcut: rate conversion (decode-convert, PCM convert) is not

### `crates/victory-hle/src/modules/audio.rs`

- `crates/victory-hle/src/modules/audio.rs:169` - shortcut: the cached and non-cached sizes are not enforced as limits;
- `crates/victory-hle/src/modules/audio.rs:347` - shortcut: each enabled context decodes from a cold state and the overlap
- `crates/victory-hle/src/modules/audio.rs:893` - shortcut: context updates and the other SHAPE units' commands are

### `crates/victory-hle/src/modules/display.rs`

- `crates/victory-hle/src/modules/display.rs:61` - shortcut: HDR output is not done (see the presentation items in
- `crates/victory-hle/src/modules/display.rs:130` - shortcut: screenshots fail as if capture were unavailable. Taking one needs

### `crates/victory-hle/src/modules/dsp.rs`

- `crates/victory-hle/src/modules/dsp.rs:15` - shortcut: no convolution or block transform; outputs are zeros. Needs the

### `crates/victory-hle/src/modules/gameinput.rs`

- `crates/victory-hle/src/modules/gameinput.rs:787` - shortcut: reading callbacks never run; Grand Theft Auto VI registers none
- `crates/victory-hle/src/modules/gameinput.rs:983` - shortcut: trigger motors are dropped (XInput has none). The host API that
- `crates/victory-hle/src/modules/gameinput.rs:1212` - shortcut: no characters (code point 0); Grand Theft

### `crates/victory-hle/src/modules/net.rs`

- `crates/victory-hle/src/modules/net.rs:514` - shortcut: `[::]` becomes `[::1]`, so a dual-stack listener no longer hears
- `crates/victory-hle/src/modules/net.rs:640` - shortcut: local name spaces are refused too; pass those through if a title

### `crates/victory-hle/src/modules/speech.rs`

- `crates/victory-hle/src/modules/speech.rs:4` - shortcut: silent speech. The host's engine (SAPI through COM, or WinRT

### `crates/victory-hle/src/modules/xg.rs`

- `crates/victory-hle/src/modules/xg.rs:54` - shortcut: the rule of the library's older optimal-mode function (any

### `crates/victory-hle/src/modules/xmem.rs`

- `crates/victory-hle/src/modules/xmem.rs:110` - shortcut: committing part of such a reservation later would fail
- `crates/victory-hle/src/modules/xmem.rs:198` - shortcut: every committed page reports graphics memory and no tag, since
- `crates/victory-hle/src/modules/xmem.rs:312` - shortcut: the host heap's addresses lie above 2^40, unlike the
- `crates/victory-hle/src/modules/xmem.rs:1020` - shortcut: all fields are filled whatever the flags ask for, and use is

### `crates/victory-hle/src/modules/xpackage.rs`

- `crates/victory-hle/src/modules/xpackage.rs:150` - shortcut: every selector counts as installed, including ones the
- `crates/victory-hle/src/modules/xpackage.rs:222` - shortcut: accepted but nothing is removed (the package folder is the
- `crates/victory-hle/src/modules/xpackage.rs:268` - shortcut: the game kind (0) also lists nothing, not this installation;

### `crates/victory-loader/src/tls.rs`

- `crates/victory-loader/src/tls.rs:85` - shortcut: a byte match, not a decode. In Grand Theft Auto VI all

### `crates/victory-shader/src/emit/lower.rs`

- `crates/victory-shader/src/emit/lower.rs:354` - shortcut: a genuine loop of more iterations than this is cut short too.
- `crates/victory-shader/src/emit/lower.rs:733` - shortcut: an index outside the table reads its last dword (the
- `crates/victory-shader/src/emit/lower.rs:874` - shortcut: the field is taken not to cross a dword.
- `crates/victory-shader/src/emit/lower.rs:1176` - shortcut: the f16 sum is taken in f32 and rounded once; the
- `crates/victory-shader/src/emit/lower.rs:1506` - shortcut: only the wave index (bits 24-27) is set; the
- `crates/victory-shader/src/emit/lower.rs:1582` - shortcut: 16-bit vertex inputs (the fetch routine's) are not
- `crates/victory-shader/src/emit/lower.rs:2220` - shortcut: rounding toward +-infinity, or a mode not known at this
- `crates/victory-shader/src/emit/lower.rs:2470` - shortcut: below 2^-12 turns the hardware scales by a slightly
- `crates/victory-shader/src/emit/lower.rs:2589` - shortcut: "some lane of the quad is set" is taken as true: the
- `crates/victory-shader/src/emit/lower.rs:2896` - shortcut: no wave size for mesh shaders in SM 6.6, so host
- `crates/victory-shader/src/emit/lower.rs:2957` - shortcut: the counts are the declared maxima and each lane first culls
- `crates/victory-shader/src/emit/lower.rs:3284` - shortcut: LDS addresses are taken as aligned to what they access.
- `crates/victory-shader/src/emit/lower.rs:3399` - shortcut: lanes writing the same part in one instruction
- `crates/victory-shader/src/emit/lower.rs:3589` - shortcut: the float MODE register is not modelled; s_swappc_b64 is the
- `crates/victory-shader/src/emit/lower.rs:4257` - shortcut: the base level reads 0 (the host view starts at its own
- `crates/victory-shader/src/emit/lower.rs:4656` - shortcut: one lookup per access, so its bytes are taken to lie in one
- `crates/victory-shader/src/emit/lower.rs:4810` - shortcut: dword 1 holds the address bits only (stride and swizzle read
- `crates/victory-shader/src/emit/lower.rs:4911` - shortcut: the descriptor's channel swizzle is taken as the identity
- `crates/victory-shader/src/emit/lower.rs:6253` - shortcut: DXIL has no fused f32 multiply-add; FMad lets the driver fuse.
- `crates/victory-shader/src/emit/lower.rs:6310` - shortcut: the hardware reciprocal is approximate (1 ULP); this divides.
- `crates/victory-shader/src/emit/lower.rs:7038` - shortcut: fused f16 multiply-adds as FMad in f32 (the
- `crates/victory-shader/src/emit/lower.rs:7282` - shortcut: no constant operand seen; its high half is not known.
- `crates/victory-shader/src/emit/lower.rs:7345` - shortcut: FMad in f32, as v_fma_f16.
- `crates/victory-shader/src/emit/lower.rs:8590` - shortcut: stores are taken to write all four channels of a
- `crates/victory-shader/src/emit/lower.rs:8850` - shortcut: sampled at that level, so a sampler's
- `crates/victory-shader/src/emit/lower.rs:9027` - shortcut: d16 results are taken as floats (rounded to
- `crates/victory-shader/src/emit/lower.rs:9060` - shortcut: the view is taken as a cube, not a cube array (whose face
- `crates/victory-shader/src/emit/lower.rs:9483` - shortcut: the console's CBV descriptor is taken to have a 16-byte
- `crates/victory-shader/src/emit/lower.rs:9599` - shortcut: the field is taken not to cross a dword.
- `crates/victory-shader/src/emit/lower.rs:9673` - shortcut: typed stores are taken to write all four channels
- `crates/victory-shader/src/emit/lower.rs:9749` - shortcut: the field is taken not to cross a dword.
- `crates/victory-shader/src/emit/lower.rs:9844` - shortcut: an integer view is read and written through the
- `crates/victory-shader/src/emit/lower.rs:9998` - shortcut: without the misc vector (point size, render

### `crates/victory-shader/src/emit/lower/bvh.rs`

- `crates/victory-shader/src/emit/lower/bvh.rs:127` - shortcut: the root address is taken to be 256-byte aligned

### `crates/victory-shader/src/emit/mod.rs`

- `crates/victory-shader/src/emit/mod.rs:477` - shortcut: older containers fall back to the group's lane count, right

### `crates/victory-shader/src/emit/sampler.rs`

- `crates/victory-shader/src/emit/sampler.rs:79` - shortcut: a custom border colour (type 3, a table entry) becomes

### `crates/victory-shader/src/emit/shader.rs`

- `crates/victory-shader/src/emit/shader.rs:790` - shortcut: every output is declared to depend on every input; the

### `crates/victory-shader/src/resource.rs`

- `crates/victory-shader/src/resource.rs:936` - shortcut: with a computed term, a high half that adds anything to the
- `crates/victory-shader/src/resource.rs:1814` - shortcut: what a lane holds after some passes is not traced.
- `crates/victory-shader/src/resource.rs:1896` - shortcut: only the accesses are proven. Any other per-lane work in the
- `crates/victory-shader/src/resource.rs:2165` - shortcut: only the one-operand ways the compiler makes a constant
- `crates/victory-shader/src/resource.rs:2169` - shortcut: a literal below 2^31, whose high dword is 0 whether the

### `xtask/src/policy.rs`

- `xtask/src/policy.rs:194` - shortcut: decimal arrays (`[77, 90, ...]`) still pass, add a literal count if one shows up.

## Asset-policy shortcut

- `xtask/src/policy.rs:194` - shortcut: decimal arrays (`[77, 90, ...]`) still pass, add a literal count if one shows up.
  - Follow-up: #2246 (`xtask: reject decimal byte arrays in the asset policy`),
    currently open. This index points at it instead of restating the gap.

## Keeping this index current

There is no generator yet. Adding `cargo xtask shortcuts` would also touch
`xtask/src/main.rs` and `xtask/src/policy.rs`, which other pull requests are
editing, so the table is hand-maintained for now.
