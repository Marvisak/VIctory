# Shader translation audit (2026-11-25)

The local `victory-input-run5` log recorded 288 distinct shader hashes that did
not translate. We matched 253 of them to DXBC programs in `shaders.rpf` and
replayed those exact programs through `victory translate --validate`. The other
35 hashes were not found in that archive; their source still needs locating.

## Result after the fixes

Six of the 253 programs now emit DXIL and pass the Windows SDK validator. Each
also creates a D3D12 compute pipeline on the test machine. The remaining 247
matched programs stop with a reported unsupported operation. None failed DXIL
validation in the final replay.

| First remaining error | Programs | Finding |
| --- | ---: | --- |
| Lane mask holding lanes elsewhere read as data | 79 | A mask can be joined across divergent blocks with bits for lanes outside the active block. A ballot at its use site loses those bits. The numeric mask needs to be carried through the join. |
| Descriptor read only as data | 69 | Some shaders read Xbox resource-heap descriptor words as numbers without a tracked image or buffer use. The host descriptor heap does not expose equivalent Xbox words. |
| Structured buffer of unknown stride | 32 | An indexed buffer can come from a dynamic resource-heap slot. Its stride is not in the static used-resource table, while the emitted DXIL structured-buffer handle requires a fixed stride. |
| Scalar memory read as data | 17 | The load is neither a tracked descriptor nor a supported scalar-buffer read. Trace its address and data use before assigning a host resource type. |
| Address not from a root parameter | 17 | Address tracing does not reach a recognized root parameter. |
| Address from a phi | 8 | Different incoming addresses meet at a control-flow join; `Address` currently represents one base and one computed term. |
| Descriptor address or format read as data | 7 | The shader uses Xbox descriptor fields for which the host view has no equivalent value. |
| `image_gather4_l` | 4 | DXIL's ordinary gather has no explicit LOD argument. A correct fallback must preserve texel selection and sampler addressing. |

Other first errors account for 14 programs. Three involve SGPR inputs that are
uninitialized on at least one path in the inspected example. Two read pixel
barycentrics directly. The remaining cases include untracked descriptor
construction and other lane-mask or LDS operations.

## Changes made

- Preserve scalar masks used as lane conditions through selects, carry inputs,
  and control-flow joins without treating their data words as ballots.
- Translate 8-bit and 16-bit LDS reads and writes. Partial-word writes use an
  atomic compare-exchange loop so concurrent writes to different parts of a
  dword do not overwrite each other.
- Emit valid Shader Model 6.6 compute sampling with implicit derivatives for
  compatible thread groups. The 81 encountered compute programs used
  `(32,1,1)`, `(64,1,1)`, or `(8,8,1)` groups. This needs validator 1.8
  metadata and PSV runtime info 3. It lets two additional programs complete;
  the others reach later unsupported operations.
- Report an unknown SGPR input as uninitialized or unrecognized, rather than
  assuming it is a hardware input.

The remaining descriptor and mask cases need representations that preserve
the Xbox values. Substituting a guessed stride, descriptor field, or ballot
would let some shaders compile while producing incorrect game output.

## Runtime priorities from the same run

The archive replay counts distinct programs; it does not measure how often the
game needs each one. Near the end of `victory-input-run5`, the diagnostics log
reported 0.5 fps, 2,202.8 ms per frame and 963 operations per frame dropped
without a host pipeline. No pipelines were still being made in that sample.
The largest reported reasons were:

| Dropped operations per frame | Reason | Next investigation |
| ---: | --- | --- |
| 259 | Compute `e30970f1727063fd` | Dynamic resource-heap buffer with unknown stride; trace its actual descriptor and preserve its stride. |
| 256 | Indirect draw kind 0, `prepare failed` | The host pipeline exists and is neither compute nor mesh. Rectangle topology is the likely refusal path, unless this pipeline was explicitly skipped; log the bound topology to confirm, then design GPU-side rectangle expansion for indirect arguments. |
| 225 | Mesh `910b4be059bfb28f` with pixel `bff8660bf8e29c26` | Host feature check reported `mesh=false` on this machine's RX 5700 XT. Mesh work needs a mesh-capable adapter or a substantial non-mesh fallback. |
| 195 | Compute `f6e47efa50226beb` | This shader was not found in `shaders.rpf`; locate its source, then trace the SGPR address that does not resolve to a root parameter. |

The current machine's missing mesh support is independent of shader translation.
The installed WARP adapter was probed too: it reports mesh tier 0 and shader
model 6.2, so software D3D12 cannot substitute for the missing mesh stage on
this Windows build. A functional fallback here requires translating mesh work
to compute output buffers and a conventional vertex/geometry draw path.
The direct mesh-dispatch fallback now emits a compute producer with contiguous
index and vertex UAVs, then a vertex shader that reads the generated vertices.
On the RX 5700 XT, one sampled mesh program (`d4111a6e6ea49d91`) and its
pixel partner validated, both D3D12 pipeline states were created, and the
renderer created the corresponding runtime pipeline pair. The generated draw
was also executed with a synthetic compute producer: two groups wrote generated
indices and vertices, and the indexed draw produced the expected center and
corner pixels on the RX 5700 XT. A second dispatch on the same command list
also passed while reusing the output buffers. The title's mesh producers now
execute during live tests. The direct path initializes unused index slots
before each producer dispatch, because an indexed draw covers the declared
maximum even when the producer writes fewer primitives. The indirect path
bounds GPU-written group counts to 32 and uses the clamped count for both
its producer and indexed draw. However, the
`mesh-producer-marker-20261125-063129Z` run faulted inside an indirect mesh
producer (`c5ffa556ab926155`) after the player prompt and title art. The
fault was a GPU page access at `0x435b000`; DRED identified the indirect
dispatch but did not prove which shader input caused the access. The indirect
fallback is disabled by default. Set `VICTORY_INDIRECT_MESH_FALLBACK=1` only
for focused diagnostics.

The same machine remained around 0.2 to 0.5 fps in the black 3D scene. The
`mesh-zero-20261125-062139Z` log measured 663 to 1,235 dropped operations
per frame after the scene transition, with copy preparation and copyback
often taking more time than host GPU execution. It did not show a playable
world. Both live runs had verified inbound and outbound firewall blocks for
the exact executable and zero TCP/UDP endpoints in process checks.
The default-off `safe-mesh-20261125-063832Z` run stayed alive for nearly
three minutes after entering that black 3D scene before it was stopped to
release memory. A Computer Use screenshot confirmed the window was fully
black. Enabling the indirect fallback while skipping `c5ffa556ab926155`
passed that shader's earlier fault, but later faulted at `0x423b000` in a
different title compute dispatch (`a1f6cdf8874960a9`). A single shader
blacklist is therefore not a sufficient safety rule.

## Live shader containers captured locally

The `shader-dump-20261125-065456Z` run saved 228 title-supplied containers
that failed translation in `target/test-runs/shader-dump-20261125-065456Z/failed-shaders/`
(7.2 MB). The run stopped after the two high-impact compute programs appeared.
Both firewall directions remained blocked for the exact executable, and the
process had zero TCP and UDP endpoints in each check. The offline replay is
in `failed-shader-replay.txt` beside that folder.

| First failure in this live set | Programs |
| --- | ---: |
| Lane mask held across control flow read as data | 41 |
| SGPR address not recognized as a root parameter | 34 |
| Structured buffer with unknown stride | 29 |
| Descriptor read only as data | 26 |
| No DXIL part (older shader format) | 20 |
| Scalar memory read as data | 19 |

The exact `f6e47efa50226beb` container begins by loading 16 dwords from
`s[2:3] + 0x40`. Its `s[2:3]` value comes from entry registers `s[0:1]`,
while its declared root parameters start at user-data dword 16. This is
consistent with a spilled root-parameter block. The translator now maps
the observed `s[0:1]` spill loads to declared root slots in resource tracing
and DXIL emission. This moves the program's first failure from `0x40` to a
later dynamic GPU address at `0x13ec`; the shader still does not translate.
The exact `e30970f1727063fd` container fails at a
`buffer_store_dwordx4` using a descriptor selected dynamically; its stride
cannot be taken from the static used-resource table. A raw-buffer path with
correct byte addressing is a possible approach, but has not been implemented
or validated for this program.

The two sampled high-impact mesh programs (`910b4be059bfb28f`
and `321d411b20a35ef9`) still fail descriptor tracing before the fallback
can be built. Indirect mesh dispatches are dropped with a diagnostic unless
the experimental fallback is enabled explicitly.
These rates are from the run **before** the six translation fixes, so a new run
must measure their effect. Once missing work is reduced, profile the approximately
1,160 ms GPU time, 361 ms CPU copy preparation, and 356 ms copyback in that
sample separately. Optimizing shader translation or pipeline creation is unlikely
to improve steady-state frames in this sample, where no pipelines were pending.

## Root UAV and subword work (later live capture)

The `black-targets-live-20261125-072832Z` run reached the player prompt,
accepted diagnostic Enter, and displayed two loading-art frames. Its third
black transition was at frame 6630, when 3D mesh and compute programs began
binding. A Computer Use screenshot after that transition was still black at
roughly 0.4 to 0.6 fps. The run was ended after saving 354 distinct failed
shader containers and while it still had zero TCP/UDP endpoints. Inbound and
outbound firewall blocks remained active for the exact release executable.
The capture and log are under `target/test-runs/black-targets-live-20261125-072832Z/`.

Among the highest-frequency dropped compute programs, `3b90165a77d95cdf`
(up to 1,221 drops/frame), `caa78b8ca62bc3d6` (1,091), and
`28918b7150a7aad5` (613) initially failed on raw root UAV support or a
later subword instruction. The translator and host now declare and bind raw
root UAVs. For a root structured view with a known stride, indexed access
is converted to a byte offset into that raw view. The host tracks GPU writes
for copyback to title memory. Unaligned 8-bit and 16-bit buffer loads are
assembled from adjacent aligned dwords, with sign extension where needed.
The subword store in `caa78b8ca62bc3d6` remains unsupported.

Replaying all 354 captured containers after these changes produced 12
validated compute shaders, up from zero in the pre-change replay. Two
high-frequency programs (`3b90165a77d95cdf` and `28918b7150a7aad5`) and
the structured root program `5552dac6a2b7a1fd` each passed DXIL validation
and host pipeline creation. The remaining leading first-error categories are
81 lane masks held across control flow, 73 descriptor words read as data,
and 32 structured buffers of unknown stride.

The first `root-uav-live-20261125-075132Z` run reached the third black
transition at frame 6643, but the GPU was removed in the first subsequent
submission. DRED reported a page fault at `0x556ed3000` in a list whose
first recorded operation was a dispatch using `5552dac6a2b7a1fd`. That
list also contained other new root UAV programs, so the record does not
prove which access was invalid. A validated DXIL shader and a created
pipeline are insufficient evidence of safe guest memory addressing.
Runtime root UAV execution now requires explicit `VICTORY_ROOT_UAV=1`;
the default remains the prior safe path. A follow-up run can skip the
suspected shader to isolate the next cause.

That follow-up, `root-uav-skip-5552-20261125-075835Z`, used the opt-in
root UAV path with `5552dac6a2b7a1fd` skipped. It reached the 3D transition
at frame 5795, then DRED stopped at command 1549 in a dispatch of
`28918b7150a7aad5`, with a page fault at `0x5d1e16000`. The first shader
was therefore not the only unsafe access. The host's copied root buffers
were much smaller than the distance to the reported fault, but DRED does
not establish which root or index produced that address. Correct bounded
root-buffer access and the missing upstream compute work need investigation
before this path can be enabled by default.

The bounded descriptor experiment, `bounded-root-uav-20261125-080923Z`,
advanced to frame 6263, then the D3D12 debug layer reported an invalid raw
buffer view. Its `FirstElement` was 12635, which is not a multiple of four;
raw buffer views require a 16-byte-aligned start. The following descriptor
table therefore held an SRV where the shader's root signature required a
UAV, and the host removed the GPU device with `DXGI_ERROR_INVALID_CALL`.
An isolated `aligned-root-uav-20261125-081559Z` follow-up bound a null UAV
for such an address. It avoided GPU removal but the title crashed just after
the third black transition, with an access violation reading
`0xffffffffffffffff` at title image RVA `0x13abcec`. The title's crash
reporter attempted an HTTPS upload to `submit.backtrace.io`, which VIctory's
network HLE blocked; the process had no TCP or UDP endpoint. A null UAV is
not a correct substitute for this output.

The current experimental path instead aligns the host raw UAV view down to
16 bytes and supplies the lost low byte offset through a one-dword root
constant. The translator adds that offset to every raw root UAV buffer
access. Non-dword-aligned addresses still bind a null view and produce a
local warning. Three leading programs still passed DXIL validation and
D3D12 pipeline creation after the change; both shader and GPU unit suites
passed. The live `shifted-root-uav-20261125-082448Z` run again reached the
third black transition (frame 6073) without a D3D12 descriptor warning or
GPU removal. It then crashed in title code at RVA `0x9d7e2e`, reading
`0xffffffffffffffff` through a title object pointer. The earlier null-view
run crashed at a different RVA. The exact source of the invalid title
pointer remains unproven. The title's blocked crash-upload attempt and zero
TCP/UDP endpoints were verified again.

At the 3D handoff, many compute, vertex, and pixel shaders still fail to
translate; the renderer drops their commands. The log also records a call
to unimplemented command-list slot `0x1a8` (the usual D3D12 UAV clear
position), a call to unimplemented pipeline-state slot `0x58`, and a
texture-to-buffer copy the host does not implement. These are concrete gaps
to trace and fix before claiming a world frame. The selected render target
and alternate 3D targets at the handoff were black; the loading artwork
survived only on a separate target.

The first UAV-clear implementation was exercised by
`clear-uav-live-20261125-083645Z` at frame 7362. The debug layer rejected
its CPU descriptor handle because it came from a shader-visible heap, which
is CPU-write-only for this D3D12 operation. That test was stopped before
any GPU removal. The host now creates a separate CPU-only view heap for
these clears, makes matching UAV descriptors in it and the bound GPU heap,
and checks that both name the same host resource. The next live run tests
this correction. A dropped clear is logged locally when its view or heap
cannot be represented.

In `clear-uav-cpu-live-20261125-084359Z`, both observed integer UAV clears
used matching CPU-only and GPU-visible descriptors. The debug layer reported
no clear or descriptor error. The title still crashed immediately after the
third black transition, this time at RVA `0x4e9d281` while reading
`0xffffffffffffffff`. Three runs using the experimental root UAV path have
therefore failed in title code at different addresses after the handoff,
even though the host GPU was healthy. A run with root UAV execution off is
needed to isolate whether its shader writes are corrupting CPU-visible title
memory. No world frame has been observed.

The `root-off-clear-on-20261125-085514Z` comparison disabled experimental
root UAV execution but retained both corrected UAV clears. It survived the
third black transition and the two clears without a title exception for
more than a minute, while the selected output remained black. It was
stopped to free memory. This points to new root UAV shader execution or
its CPU-visible writeback as the source of the immediate title crashes.

Reviewing `Shadow::copy_back` found a concrete concurrent-write bug: when
both the GPU copy and title CPU changed a byte after the snapshot, it copied
the GPU value into title memory anyway. A comment in that function already
described a title fence list being read as `-1` near the latest crash RVA.
The merge now copies a GPU value only if the title byte still equals the
snapshot; otherwise it preserves the CPU value and updates the host copy.
A regression test covers GPU-only, CPU-only, and conflicting writes. The
first conflict is logged locally. A new root UAV live run is needed to see
whether this prevents the title exception.

The follow-up `safe-merge-root-uav-20261125-090245Z` still crashed during the
third transition, but recorded 30 title CPU/GPU write conflicts near
`0x69cf80000`. A controlled `root-off-clear-on-20261125-085514Z` run had
already survived the same two UAV clears. CDB then separated many handled
first-chance allocation faults at image RVA `0x18849c` from a later deliberate
null-write abort at RVA `0x23cf683`; the latter is a title abort instruction,
not a D3D12 device fault. CDB changed timing enough that this run alone did
not identify the triggering write.

`VICTORY_TRACE_WRITEBACK=1` exposed a concrete performance bug in the first
safe-merge implementation: striped GPU results changed one byte every 32
bytes, yielding 2,048 `WriteProcessMemory` calls per 64 KiB piece and about
one second per piece. Dense writes now batch by 4 KiB page, rereading the
title page immediately before overlaying GPU-changed bytes. Sparse writes
remain exact runs. A regression test covers a late title write within and
between striped GPU results. The same dense pieces then took about 10 ms,
but full root UAV writeback still crashed in title code after the clears.

Three controlled variants isolate the remaining failure. With root UAV
shader execution on and root CPU writeback off,
`page-batch-no-copyback-20261125-093459Z` passed the two clears and remained
alive more than three minutes afterward; it was stopped at 12 GB RAM. With
the full 1 MiB root writeback on, `page-batch-root-uav-*` crashed soon after
the clears, reading address `0xf` at title RVA `0xf6349`. Static disassembly
shows an internal virtual lookup returned null and the caller read byte
`+0xf` without checking. A diagnostics-only 4 KiB copyback cap reached frame
6,000 but still crashed before frame 7,000, reading
`0xffffffffffffffff` at title RVA `0x4f28df1`.

The `root-origin-4k-*` trace tagged GPU writeback by its declaration source.
It recorded 233 pieces overlapping root UAV declarations. Root-marked runs
wrote thousands of `0xff` bytes into title memory and, immediately before a
`0xffffffffffffffff` read fault, changed adjacent fields to the bit patterns
for positive and negative floating-point infinity. This establishes that
root UAV CPU writeback changes title memory in the failing run; it does not
yet identify which shader generated the values or whether some overlapping
non-root UAV operation also wrote that host copy. Root UAVs currently declare
up to 1 MiB from each bound address, often with thousands of tightly packed
bindings, so the copied-back range is broader than a proven shader output.
Root UAV execution remains opt-in, and its CPU writeback now requires a
second explicit `VICTORY_ROOT_UAV_COPYBACK=1` opt-in. No 3D world frame has
been observed; the title's local crash report from an earlier run recorded
`Game Running` simulation state while the selected output stayed black.

Further root-only tracing separated root UAV writeback from ordinary UAV
writeback on the same host shadow. The `root-shader-origin-4k-*` run recorded
2,048 root-only changed runs and no root/ordinary overlap; the values near
the subsequent invalid-pointer fault included `0xffffffff` and both infinity
bit patterns. The native compute shader `28918b7150a7aad5` contains those
values and writes 128-byte output records. Its root slot 1 reads and slot 2
writes the same title address in the observed bindings. The slot mapping
through the loader is distinct and follows the title's supplied addresses.
Static disassembly shows the values are deliberate empty-result sentinels,
so their appearance alone does not establish a shader translation error.

The 64-byte copyback cap run `root-copyback-64b-*` survived more than five
minutes and reached frame 6,000 before it was stopped for RAM pressure. A
4 KiB run with `28918b7150a7aad5` skipped survived about two minutes after
the UAV clears, also stopped for RAM pressure. The reciprocal run, skipping
`4ddd6f9702fab2a7` but executing `28918b7150a7aad5`, crashed after about
six minutes. It recorded root-only infinity writes and a title access
violation at image RVA `0x4f3b0ac`. Disassembly identifies a malformed
tile header: its packed-data byte offset at `tile+0xc` was `0xff800000`
and its dimension at `tile+0x18` was zero. No GPU writeback directly to
that tile header was logged, and the shorter skip-289 control cannot yet
prove the shader caused this later fault.

The `exclude-289-copyback-4k-*` run executed `28918b7150a7aad5` but
excluded its root UAV CPU writeback. It reached frame 6,000 without an
exception and without root-only infinity or `0xffffffff` writeback runs;
it was stopped when its working set reached 14 GiB. This narrows the
immediate root-writeback corruption, but a longer comparison is required
for the separate tile-header fault. A diagnostics-only exclusion knob and a
selected-shader dump were added to keep these comparisons reproducible.

The shader's native output address is root constant zero plus 8,192 bytes
per dispatch X group. The host currently caps every root UAV descriptor at
1 MiB, and root SRV refresh has the same arbitrary limit. A focused
dispatch trace now logs the actual group count, root constant, and mapped
extent to test whether a title dispatch exceeds the host view. Enlarging
all views to the whole mapped resource would snapshot very large buffers
and can still truncate unregistered loose memory; the result of that trace
should guide a bounded correction.

In `trace-289-footprint-64b-*` at 1280×720, 52 selected dispatches were
logged before RAM pressure ended the run at frame 6,000. Three had a
potential output end beyond the host view. One had an estimated end of
1,778,304 bytes, greater than both the 1 MiB view and the 1,441,536 bytes
remaining in the registered buffer. The other two likewise exceeded their
registered buffer ends. Because the shader conditionally skips stores,
these are upper bounds, not proof of out-of-range accesses or a cause of
the crash. The trace now labels the view and mapped-buffer limits
separately. The run did not produce an exception before it was stopped.

Buffer resource aliasing barriers that the title requested were previously
dropped because the barrier translator only handled texture resources.
They are now forwarded to D3D12 for buffer aliases too, with a focused
regression test. The reciprocal root-shader run included this fix and still
crashed, so the missing barrier was not sufficient to resolve that failure.

A longer 1280×720 `long-exclude-289-4k-*` comparison executed the shader
while excluding only its CPU copyback. It reached frame 6,000, then the
title deliberately wrote to null at image RVA `0x23cf683`, a different
failure from the invalid tile-header read seen with `4ddd...` skipped.
The title's own local crash context reported `Game Running`, a live player
position, an intro mission, and populated world object pools. The selected
frame remained loading artwork; a rendered 3D scene has still not been
observed. The title attempted to upload its minidump, and the local network
wrapper blocked the HTTP connection. The isolation test also observed zero
TCP or UDP endpoints in the title process.

## Debugger trace of the later null write

CDB breakpoints in `watchdog-cdb2-exclude-289-*` established that
`0x23cf683` is the title's own deliberate null write after a watchdog
detects 30 one-second checks without a heartbeat. The alternate watchdog
path at `0x2215c0c` did not execute in that run. This exception is a
secondary symptom; it does not identify the original stalled operation.

Thread snapshots at 120, 180, 220, and 260 seconds showed the title main
thread entering `0x812676` and then waiting in `0x23cf6a0` for a title
semaphore. A debugger breakpoint at `0x2258de7` showed the title main
thread clearing its own request byte during state-3 initialization.
The nearby state byte at object offset `0x1b3` was zero at the later
`0x812676` branch. A hardware write watch on that byte ran from about
8 seconds through frame 3,800 without a hit; samples at 40 and 190
seconds also read zero. Thus no late write to that field was observed.
The later `0x812620` call is state-3 update, not a state-3 exit callback.
There was no `window closed` log in these runs. The reason the update
enters cleanup is still under investigation.

The title's semaphore imports pass through to real Windows APIs; Victory
does not replace `CreateSemaphoreExW`, `ReleaseSemaphore`, or
`WaitForSingleObject`. Static tracing found a title worker that signals
the main thread's semaphore on several paths. Thread snapshots confirm
the worker was alive, blocked in another title wait at 120–220 seconds
and at a different wait at 260 seconds. The producer for the earlier
wait and the reason it took over 100 seconds remain under investigation.

The follow-up CDB capture in `semaphore-handle-20261125-114349Z` identified
the worker's blocked handle as a real Windows semaphore `0x9b8`, slot 6
of a 126-slot title job semaphore array at image RVA `0x71c0f10`. Its
slot state byte was 1. Title code at `0x62d0` signals those slots after
an atomic wait-word update; one producer path for the worker's wait word
at RVA `0x75e0e78` is `0x5caba60`. This wait is separate from the
main thread's later cleanup semaphore. The worker's same job-wait stack
was present at 120, 180, and 220 seconds. The live run was stopped when
free RAM fell to about 4.3 GiB, before a further result could be seen.

Stack symbolization also found all 24 DirectStorage workers inside
host `FileExt::seek_read` during part of the long load, with all idle
at later snapshots. The game files reside on an 8 TB SATA hard disk.
Concurrent random I/O is a plausible source of delay, but the
relationship to the title job semaphore remains to be measured.

The shader translation cache did hit in repeated runs of an unchanged
binary. The separate D3D12 host pipeline library rejected its first
`StorePipeline` with `0x8000ffff`, leaving host library hits at zero.
A focused adapter test also found `ID3D12PipelineState::GetCachedBlob`
returns `0x8000ffff`, so a per-pipeline blob fallback cannot help on this
driver. The burst of world pipeline creation occurred after the title
main thread entered the wait in the observed run, so its contribution
to the later stall remains unproven.

The `dstorage4-handoff-20261125-115936Z` run used the same title with a
four-worker cap and the new DirectStorage latency counters. It reached
frame 6,129, then its **first** access violation was a title read at image
RVA `0x550cfcb` from address `0xa6b01000` around 12:04:01 UTC. The
watchdog's null write at `0x23cf683` began about 30 seconds later, so
the earlier read fault is the proximate failure in this run. At the
fault, `r13=0xa6ad5ca8` and `rdi=0xacd6` give exactly the read address
for the instruction's `[r13+rdi*4]` operand. The loop's `esi` value was
`0xa08a1248`, implausibly large for a record count. The source of that
value is not yet known. CDB observed the title's GPU job worker repeatedly
returning tokens to the same atomic completion word the waiting worker
used, so a lost semaphore signal is not supported by this run.

With four storage readers, one read from `anim.rpf` took 10.7 seconds;
later five-second windows had p95 upper bounds of 64–128 ms and queues
sometimes exceeding 1,000 requests. Frame 4,096 arrived about 28 seconds
later than in the older 24-worker run, although the new binary also had
a cold shader cache. The worker cap remains optional and the default is
unchanged. The new texture-to-buffer readback path recorded its first
full-row copy at 12:04:08, **after** the first title fault; it cannot
explain that fault. It passed a real D3D12 row-padding roundtrip test,
but this run cannot establish whether it improves the 3D handoff.

The follow-up `dstorage24-record-20261125-121047Z` run used the normal 24
storage workers and passed frame 6,000 without the title's `0x550cfcb`
fault. A breakpoint after the title routine's scratch-array `memset`
confirmed record 4,914 started with count zero; a write watch and a
count-greater-than-eight breakpoint saw no anomaly in this run. The first
reported access violation was instead in `D3D12Core.dll+0x7e79b` during
`Host::shared_buffer`'s `CreateCommittedResource` call at frame 7,459,
followed by the title watchdog. Neither run supports treating the
watchdog as the initial cause, and the two runs do not share a proven
primary fault.

Both runs reported `Adler32Mismatch` for DirectStorage zlib requests
before their respective faults. The six failing file chunks in the
24-worker run, and the first failing `story000.rpf` chunk in the
four-worker run, were independently read from disk and decompressed with
Python zlib; each produced exactly the logged destination size and a
matching Adler checksum. An isolated Rust probe using the same
`miniz_oxide` call successfully decoded the `story000.rpf` chunk, and a
24-thread shared-handle `FileExt::seek_read` probe found no wrong-offset
reads in 24,576 cached requests. These checks rule out those on-disk
chunks and the isolated decoder as explanations, but do not prove which
live memory operation caused the mismatches. The loader had inflated
directly into title/GPU-visible memory; `miniz_oxide` rereads that output
to compute Adler32, so a concurrent write could produce this exact
failure. Queued file requests also lacked an owned file reference.
Private-output decompression, queued-file ownership, and per-request
source/destination diagnostics were added and passed 18 focused
DirectStorage tests (including two review hardening cases). The first
release diagnostic build preceded the last two small test-only/invalid
request hardening edits, which will be included in the next build.

The `dstorage24-private-20261125-122835Z` run used that diagnostic
build with the exact inbound/outbound firewall blocks still active.
It passed the earlier frame 6,129 fault and entered the world pipeline
burst around frames 6,215–6,218. No Adler mismatch, zlib failure,
`0x550cfcb` count anomaly, or D3D12Core/AMD access violation was
observed. All 2,078 logged shared-buffer `CreateCommittedResource`
attempts returned success. The maximum logged GPU frame was 6,220.
The title watchdog at `0x23cf683` first fired between 12:33:02 and
12:33:07 UTC, after frame throughput slowed sharply. Timed stacks and
a later CDB snapshot show the GPU submission thread progressed from
normal frame pacing to GPU execution and then twice
sampled inside `VCRUNTIME140!memmove` → `Shadow::writing` while title
workers waited. `Shadow::writing` expands its old-byte snapshot across
overlapping views of the 174 MB world heap by allocating and copying
the whole prior snapshot on each expansion. The samples make this a
concrete performance suspect, not proof it alone caused the watchdog.
The run was stopped before free physical RAM dropped below 3 GB.
The frame log measured 24.6 seconds for the first world-transition
frame, then 61.0 seconds for the next; a later frame still took about
10 seconds. In those frames, copyback spent 5.9–18.7 seconds to commit
roughly 5–7 MB, while shadow refresh examined 4.4–8.6 GB per frame and
one interval reported about 3 million page faults. These timings show
the slowdown is in the host GPU memory path; snapshot growth is one
candidate within it.

Among first-bound world shaders, compute hash `caa78b8ca62bc3d6`
was dropped 536 times per frame because `buffer_store_short` is not
translated. The 16-bit store path needs a real subword UAV operation;
a 32-bit read-modify-write would race when lanes address adjacent
halfwords. Compute `e30970f1727063fd` (unknown structured stride)
and `f6e47efa50226beb` (address loaded from memory), and mesh
`30332cf87c4e0eae` (host pipeline refusal), were also dropped often.
The next targeted shader work is `buffer_store_short`, alongside
reducing repeated shadow snapshot copies.

Those targeted changes are now implemented in the next diagnostic
build. `buffer_store_short` emits a native DXIL `rawBufferStore.i16`
with alignment 2 and a real 16-bit value; an adapter test verified
independent writes to adjacent halfwords. The exact cached
`caa78b8ca62bc3d6` title shader then translated, passed `dxil.dll`
validation, and created a D3D12 pipeline offline. The mesh fallback
also accepts a pipeline with no pixel stage; a focused D3D12 test
created that form of graphics PSO on this adapter. GPU write snapshots
now store ordered segments and capture each newly covered byte once,
including the gaps the old contiguous extent had captured. A
deterministic test compared 256 declarations with the old algorithm.
The next live run must still verify frame rate and whether the title
continues beyond the world transition.

The combined `combined24-20261125-124417Z` run used that build,
verified exact-path inbound/outbound firewall blocks, and consumed
Enter at the player prompt. It held about 30 fps on loading artwork
through frame 8,192 and reached a maximum logged frame 9,145, past
the previous 6,220-frame ceiling. The exact `caa78b8ca62bc3d6`
shader translated and created a host pipeline, and the mesh-only
`30332cf87c4e0eae` fallback also created a pipeline at frame 9,145;
the frame was not presented, so dispatch success is unproven. There
were no zlib failures, earlier access violations, anomalous record
counts, or D3D resource creation failures. One new snapshot cost report
showed 52 MB captured versus 52 MB the old algorithm would copy in
348 ms, so this run did not exercise the quadratic growth case before
its first fault.

At about 12:49:47 UTC, multiple title workers began repeatedly
faulting at RVA `0x8f92a` while reading through the value
`0xfc007c007c007c00`. The routine obtains it from
`[[rdi+8]+0x10]`; it checks only for zero before dereferencing
`[rax+0x18]`. The value encodes three FP16 positive infinities and
one negative infinity, consistent with an empty bounds sentinel but
invalid as a pointer. The same value appeared in several distinct
objects, and no logged GPU writeback overlapped their `rdi` addresses.
The source of the bad object field is not established. The process was
stopped at 4.57 GB free RAM before a menu/world frame appeared. Next
debugger work must capture the intermediate pointer, surrounding
object data, and first writer of its `+0x10` field rather than assuming
which subsystem placed it.

The `badptr24-20261125-130540Z` replay added an opt-in crash-chain dump
for a first-chance fault at `0x8f92a`, and a GPU copyback trace for the
exact eight-byte infinity pattern. It consumed the automated Enter key,
passed the player prompt, and captured loading artwork. No `0x8f92a`
fault occurred. Many accepted root-UAV copybacks contained the pattern,
often every 12 bytes across empty-bounds-looking records, but the
faulting holder field was not observed in this run. Root shader 289's
copyback exclusion cannot identify the producer: another shader (notably
4ddd) binds the same root ranges, and the runtime merges write
declarations across lists before copying GPU results back.

This replay was slowed by a debugger software breakpoint on a hot title
instruction for over a minute; it was removed before world entry. The
last presented frame was approximately 6,924; later world frames spent
roughly 3–24 seconds in copyback for about 4–7 MB written, while
read-watch checks covered several GB. The title's deliberate watchdog
null write at `0x23cf683` first appeared at 13:14:41 UTC, after those
long frames; no earlier bad-pointer or zlib fault was logged. The process
was stopped with over 4 GB physical RAM free. Because the breakpoint
changed timing, this run does not establish a regression relative to
the previous 9,145-frame run. Stage timings are needed to locate the
copyback cost before changing memory behavior.

The clean `copyprofile24-20261125-132821Z` Series X run enabled
`VICTORY_PROFILE_COPYBACK=1` and automated the player-menu Enter input
through `VICTORY_KEYS_FILE`. It passed the prompt and continued through
loading artwork, but one range at frame 6,871 took 17.9 seconds to copy
about 14 KB of changed bytes from a 5 MB shadow. The title then took its
deliberate watchdog null-write path at RVA `0x23cf683`. No `0x8f92a`
bad-pointer fault occurred in this run. The first version of range
profiling did not yet separate the writeback stages, so it could not
attribute the 17.9 seconds.

The follow-up Series S run `copyprofile-series-s-20261125-133717Z`
used the same automated Enter input, root UAV copyback, bad-pointer
trace, and profiling, with `--console series-s` and a requested
1280x720 output. It passed the prompt and reached frame 6,836 before a
title access violation. The actual selected present plane was still
3840x2160 at frame 2,048, so the requested output size cannot be
treated as proof that all title rendering was lower resolution. The
profile located the scene-transition bottleneck in the process memory
write calls: one frame spent 5.49 seconds copying 4.1 MB of accepted
changes, including 5.25 seconds in 3,166 writes, versus 19 ms scanning
and 19 ms merging. A 47 MB shadow range at frame 6,836 spent 3.35 of
its 3.56 seconds in 476 writes. It recorded no fallback or failed final
writes. The title was running around 0.1–0.6 fps during this stretch.

At 13:42:17 UTC that run faulted at title RVA `0xe48dbb`. Static
disassembly shows the first body instruction of the function is
`movzx ebx, word ptr [rdx+0x10]`; `rdx` held the same
`0xfc007c007c007c00` sentinel seen at `0x8f92a`. Its caller loaded
`rdx` from `[rdi+0x90]` after a lookup, and the faulting `rdi` was
`0x2aacd0000`. The stack is `0xe48dbb` ← `0xe48aff` ← `0x51c92c2`.
The title's crash reporter tried to submit
a minidump to `submit.backtrace.io:443`; the HLE network layer logged
that it blocked the connection. Exact-path inbound and outbound
Windows Firewall block rules were also verified before both runs.

The same log shows root-UAV copyback introducing that sentinel. The
first hit, at frame 6,821 guest `0x6a94c0128`, changed
`00 a2 00 00 00 00 00 00` into `00 7c 00 7c 00 7c 00 fc`. Only four of
those eight bytes were accepted GPU stores: the three `0x7c` halves
and the final `0xfc`. The `0x00` low bytes were already zero, so the
qword is four FP16 values (`+inf +inf +inf -inf`), an empty bounds
record, not an 8-byte pointer store. Later hits in `0x6a94cxxxx` and
`0x6a9166xxx` repeat that pattern, all `root=true`. None of the logged
writebacks cover `0x2aacd0000` or `0x2aacd0090`. The GPU path is a
producer of the bit pattern. It is not the writer of this fault's
object field. `VICTORY_TRACE_BAD_POINTER` only dumped the `0x8f92a`
chain, so this run recorded no object bytes. The diagnostics build now
dumps `[rdi+0x80, rdi+0xc0)` for `0xe48dbb` as well.

Copyback time in that run is the kernel write, not the compare. Frame
6,825 wrote 33 KB of a 1 MB shadow as 374 dense 4 KB calls and spent
1,006 ms in those calls; scan plus merge was under 3 ms. Frame 6,836's
47 MB shadow spent 3,347 ms in 476 writes. A separate check on ordinary
committed memory runs hundreds of 4 KB `WriteProcessMemory` calls in a
few milliseconds, so the live cost is per call against the title heap.
Dense publish now uses one write per 64 KB piece, which is the range
already compared, instead of one write per 4 KB page. A sparse page
still ends the batch early, so a piece that is not uniformly dense
becomes more than one write.

Series S run `copybatch-series-s-20261126` is the diagnostics build
with root-UAV copyback, copyback profiling, and the bad-pointer trace.
Output was requested at 1280x720; the title still selected a 3840x2160
present plane. Timed Enter presses were armed at 45, 80, and 120
seconds. From 03:16:32Z the menus held about 30 fps. The world
copyback window, logged at 03:20:38Z, averaged 248.25 dense pages,
33.0 dense batches, 49.25 sparse runs, and 82.25 write calls per
presented frame. Write calls equal dense batches plus sparse runs, and
the batch count matches the 34 changed 64 KB pieces, so dense pages
were published once per piece rather than once per 4 KB page. That
window averaged 120 ms in the writes and 169 ms in the whole copyback.
No range reached the one-second slow-copyback line. The previous run's
frames spent 1.0–3.3 seconds in the 4 KB writes alone.

The same window's last present still took 2,116 ms (0.5 fps). Of that
bucket, the host meter attributes 157 ms to preparing submissions,
47 ms to the GPU, and 197 ms to copyback. The rest of the 2.1 seconds
is outside those three buckets. Batching removed the write storm. It
did not make the world frame interactive. Dropped draws remained about
1,066 per frame, mostly pipelines still being made and the disabled
indirect-mesh fallback.

This run never executed `0xe48dbb`, so the new object dump did not
fire. Root-UAV copyback still introduced the FP16 sentinel: 72
writeback warnings from 03:20:41Z to 03:20:46Z. The title then faulted
at RVA `0x108557`, a read of address `0x8` with `rcx` equal to 0.
Eight first-chance and eight unhandled exceptions share that site.
One captured context is `rdi=0x9016b0b00`, `r13=0`, and the stack
`0x108557` ← `0x1f6dcf6` ← `0x258b7af` ← `0x9d9444` ← `0x257035e` ←
`0x221c29c` ← `0x2215eca`. The last log line is 03:21:08Z.

## RX 5700 XT world-render diagnosis (2026-11-27)

The Series X 720p/low run `rx5700-seriesx-low720-20261127-155507`
reached the world. Its 2560×1440 G-buffer color, normals, and depth
targets contain scene geometry without regular row seams. The lit color
target (`frame-6170-50`, format 10) is dark with broad horizontal bands.
In its half-size PNG, mean row-luma jumps at each 32-row boundary are
13.79 versus 1.27 on other rows: the discontinuities repeat every 64
source pixels. The downstream color target and final frame retain them.
The same target was 99.9% black and seam-free after submission 18718,
then banded after submission 18829. That latter list runs compute
`348e1e8c414c7eca` and pixel shaders `3ae25b11290cadde` and
`d7b97830ede61511` against the target. The PNG's `-by-compute` label
names its first UAV writer, not its final writer; it does not isolate
which of the three passes introduced the bands. Native disassembly of
the compute pass uses `group_id * 8 + local_id` for its 8×8 thread
coordinates, loads and stores the same pixel, and has no explicit
64-pixel tile addressing. Its first depth load gates work to depth-zero
pixels, so it may account for the sky while opaque lighting is missing.

At the previous general stream cap, four BC1/BC3 textures read by those
passes had null SRVs. The bounded `VICTORY_STREAM_PRIORITY_SOURCES`
experiment admits only their exact addresses/shapes, at most 8 MiB in
total and with a 256 MiB DXGI budget margin. The first guard accidentally
tested the console's `0x300000` texture flags as host flags; it now tests
the normalized host description. In
`rx5700-priority4-writer-20261127`, all four host copies and SRVs were
created (1 MiB total), but the captured lit target still had 32-row
seams and a dark lower half. This proves missing descriptors were real;
it does not prove their uploaded texels were correct or that they alone
caused the image defect.

The same old run dropped compute `3b90165a77d95cdf` about 244 times
per frame and `f6e47efa50226beb` about 194 times per frame because
root constants dword 4 was mistaken for a root descriptor offset.
`draw.rs` now checks both the root type and the dword. In the later
Series S/host-FSR run `rx5700-series-s-fsr-fixed-20261127`, no false
root-offset warnings appeared through frame 5,460. Real mesh root views
were still dropped when the loose 256 KiB copy was refused above the
3,500 MiB system-memory cap; that is a separate budget issue.

The Series S run stopped before a world capture: its first format-67
(`R9G9B9E5_SHAREDEXP`) render-target view was made on the R32_UINT
writer resource, and D3D12 debug ID 38 rejected it and removed the
device at frame 5,460. The current two-resource alias makes format 67
readable as an SRV, but that alias is read-only on this PC. The immediate
safe path skips format-67 RTV creation and clears, and drops graphics
draws with that target bound. A scoped packed integer output can handle
opaque overwrite passes. Blended or partial writes will need a separate
ordered blend path, likely a float render twin and pack pass.
The next diagnostic run, `rx5700-series-s-fsr-rtvskip-20261127`, did
proceed past that view. It reached the first world frame at 5,506 and
continued through frame 5,613 without the earlier device removal. The
captured world has recognizable geometry but is dark and noisy; the lower
half of frame 5,506 was nearly black, and later frames retained repeated
32-row jumps in the half-size image. This run used Series S, 720p/low,
host FSR 3.1.6, a 512-pixel stream edge cap, D3D12 debug/DRED, and
scripted Enter presses. Enabled exact-path Windows Firewall rules blocked
inbound and outbound traffic; monitoring found no TCP or UDP endpoints.

Two format-67 pixel programs were skipped in this world segment. Both
write only render target 0 without blending or alpha-to-coverage and have
a full write mask. `f79e8a2e8c379374` exports RGB; `813a7ead7f09ebd7`
exports RGBA and depth. They are candidates for a scoped R32_UINT pixel
output variant that packs RGB as R9G9B9E5, preserving the depth export.
Format-67 clears still need a packed uint clear path; passing the packed
word through the float-based `ClearRenderTargetView` API loses integer
precision. The current safe skip prevents device removal but cannot make
these passes visually correct.

The world ran at roughly 0.4–3.2 fps. Local video memory rose to about
5.16 GB against a 6.49 GB current budget, nonlocal to 4.68 GB, and the
process working set to about 14 GB. Root-view misses reached a cumulative
32,768 because new loose windows were refused over the bounded 3.75 GB
nonlocal limit. Persistent buffer descriptors now retain their shadow
copy, and the existing trim path rechecks current-frame use before
removal; a new pressure trim has not been enabled. A diagnostics-only
frame phase timer was added to locate the roughly 0.7 seconds per frame
outside the existing submission-preparation, GPU-wait, and copy-back
timers.

D3D12 debug also reported alias state mismatch ID 527 on a separate
2048×120 read-only format-67 alias: a COMMON-to-SRV transition was
recorded while the prior state was already SRV. The placed alias now
starts in SRV state and its tracker is seeded to the same state; a live
debug run must verify that the warning is gone.

The next bounded image A/B is a fresh Series X low720 run with the
root-constants fix, capturing only the lit target with
`VICTORY_DUMP_TARGETS=1` and
`VICTORY_DUMP_WRITERS=348e1e8c414c7eca`. A second run can skip both
following pixel hashes with `VICTORY_SKIP_PIPELINES` and capture the
compute-only target. If the seam remains, inspect the compute pass's
sampled source textures and translation; if it disappears, test the two
pixel passes separately. These captures avoid the all-target per-list
dump that previously stalled the title watchdog.

The current source also has a scoped packed format-67 output path. Only
the two inspected opaque, single-target pixel shaders above qualify;
their translated output signature becomes one `uint SV_Target0`, while
the second shader's depth export remains. Their host PSOs use an
`R32_UINT` render target and red-channel write mask, and the draw path
binds the R32_UINT primary of the existing shared-memory alias. All
other format-67 draws keep the safe skip. Both extracted title shaders
passed DXIL validation and R32_UINT PSO creation on the RX 5700 XT with
the D3D12 debug layer enabled. A separate 201,331-case float32 comparison
of the pack math against DirectXMath's scalar `XMStoreFloat3SE` algorithm
found no mismatched words, including NaN, infinities and boundary cases.

The first integrated packed run (`rx5700-series-s-packed-20261127`)
closed its window before either format-67 writer bound, so it supplies
no live evidence about this path. A subsequent close probe confirmed a
posted `WM_CLOSE` reached the visible stand-in window; the bounded test
now has an opt-in guard that logs and ignores that message until its time
or RAM stop limit. Format-67 clears are still skipped;
they need a uint clear or compute fill that writes the packed word.
The packed variant also cannot preserve blending or partial writes;
those remain excluded by the eligibility gate. A depth-bounds stream
PSO branch now shares the same target-format and write-mask override
as the ordinary PSO path, with a focused test.

The guarded integrated run (`rx5700-series-s-packed-keepopen-20261127`)
reached the world at frame 6,185. Both inspected pixel shaders translated
to packed output and ran through R32_UINT PSOs; the second retained depth.
The captured scene is still very dark and speckled. The D3D12 debug layer
identified a concrete new state error: ID 527 reports a COMMON-before
transition on the R32_UINT primary even though its actual state is UAV,
and ID 538 then reports packed RTV draws using that primary in UAV state.
The first error arises when a list's first use is the read-only alias:
its `Tracked.primary_at` is captured while recording, but can be stale
after earlier lists execute. `open_move` prepares only the alias, so a
later in-list alias-to-primary transition uses the stale before-state.
The alias prelude must normalize the primary to that recorded state before
the list's first alias use whenever the list later uses the primary.
This state error must be fixed and retested before judging the image.

No format-67 clear was logged in that world segment. Heavy frames took
about 1.2–1.35 seconds, with 445–540 ms in host execution and a further
600–720 ms between presents; lighter frames reached roughly 2–3.6 fps.
The run ended at 06:34:22 UTC with `DXGI_ERROR_DEVICE_RESET` and a driver
"kernel memory failure" warning after nonlocal memory reached about
4.84 GB. DRED had no completed breadcrumb that isolates the command
responsible; the invalid R32_UINT state and memory growth are both real
observations, but neither is established as the reset's sole cause.

### Capped Series S world run, 27 November 2026

`target/test-runs/rx5700-series-s-budget8-priority4-20261127` ran with an
8,000 MB stream cap, Series S low at 720p, D3D12 debug, scripted Enter, and
the verified inbound/outbound WFP and firewall blocks. It reached world
frame 6,298 and ran to the harness's 5.5-minute limit without a device
reset. The monitor recorded zero TCP and UDP endpoints. The captured world
remained dark, speckled, and marked by large rectangular bands; this is not
a visually correct or playable result. Late frames ranged from roughly
0.6 to 1.8 fps and copied about 289–412 MB per frame. Root-view admission
was still denied after nonlocal usage exceeded the 3,750 MB copy threshold,
dropping many mesh and indirect draws.

The D3D12 debug layer reported two ID 527 transition state mismatches and
19 ID 615 draws with a null depth view but a depth-enabled PSO. The alias
state fix now separately tracks the primary's immutable initial state and
its final state; the former determines the first in-list alias-to-primary
barrier. A depth-disabled PSO variant with `DSVFormat=UNKNOWN` is selected
when the 1280×720 depth view is omitted beside a 1920×1080 color target.
Both fixes passed focused tests but require a new integrated debug run.

The proposed four-texture full-mip test was not valid: addresses from the
previous run identified different resources in this run, including a
256×256 BC1 texture and later an array texture. The abort was recorded in
`target/test-runs/rx5700-series-s-budget8-fullmips-20261127/manual-stop.txt`.
The four inputs can instead be selected by their shader hash, descriptor
slot, format, and dimensions at binding time; that opt-in diagnostic path
and per-input upload probes are being implemented before the next A/B.

An opt-in `VICTORY_ROOT_PRESSURE_TRIM_MB` uses the existing safe loose-copy
trim at the start of submission, at most once per second after nonlocal
usage reaches the current 3,750 MB admission threshold. It is disabled by
default. Testing at 750 MB may determine whether this frees enough room for
root views without destabilizing the title.

The next 8,000 MB control (`rx5700-series-s-control-statefix-20261127`)
reached the world and ran to its 5.5-minute time limit with no GPU reset,
no error-level D3D12 messages, and zero TCP/UDP endpoints. It exercised the
new color-only PSO. Its world was still dark and patterned, and late
frames slowed to 0.2–0.6 fps while root views were denied at the nonlocal
limit. The new dropped-mip detile indexing fix was included in this run.

The matching bound-source full-mip run
(`rx5700-series-s-bound-fullmips-20261127`) selected all four expected
shader-bound BC inputs. It admitted 2048² BC1, 1024² BC3, 512² BC3, and
256² BC1 with zero dropped mips. Detile probes found zero unreadable and
zero all-zero 4 KiB pages in each source, and uploads recorded every host
subresource. It also ran to the time limit with no GPU reset, D3D12 error,
or network endpoint. The world remained visibly grid-like and dark; full
mips alone did not repair the image.

Three 1280×720 format-67 SRVs in the first world pass were null under the
stream cap, despite overlapping format-42 R32_UINT UAV writers at the same
title addresses. The read views' default layout resolves to a different
swizzle, so the memory-key lookup does not reuse the live writer. A
diagnostics-only `VICTORY_R9_SAME_MEMORY_ALIAS=1` path now accepts only an
exact same-address, size, mip, sample, and physical-footprint match and
reuses the writer's existing read-only R9 alias. A focused test confirmed
one shared host `Arc`; a separate live run is needed to judge its visual
effect and D3D12 barriers.

Control traces also located a CPU bottleneck: the world performed roughly
350–445 MB of title-memory copies per frame and thousands of soft guard
faults, while hard faults remained zero. A worst interval spent about
3.1 seconds per frame in the guard handler and hundreds of milliseconds
inside protection calls. `watch::open_guarded` now batches contiguous page
opens into one `VirtualProtect` call with a per-page fallback for sparse
regions; all 17 watch tests pass. Its frame-time impact remains unmeasured.

The separate same-memory alias run
(`rx5700-series-s-r9-same-memory-20261127`) matched four 1280×720
R32_UINT writer/R9G9B9E5 reader pairs. The previously null R9 shader slots
were made from the shared alias. Captured world frames still had the dark
grid, so those missing SRVs were not the sole visual cause. The run ended
after about five minutes with `DXGI_ERROR_DEVICE_RESET` and three ID 834
kernel-memory warnings, no other D3D12 error IDs, and no useful completed
DRED breadcrumb. Just before reset the driver reported about 4.67 GB local
and 4.66 GB nonlocal usage, while the 24 write-back heaps were nearly full.
The alias reuses the writer's heap and is only a few small resources, but
the run alone cannot isolate the reset's cause.

The pressure trim was refined to skip held oldest windows and continue to
younger releasable ones, with an eight-pass cap only in the opt-in mode.
Focused tests pass. A 750 MiB loose-copy target could reclaim at most
105–147 MB at the first root misses seen in control/full-mip logs, not
enough for the later nonlocal excess. A 256 MiB target could theoretically
reclaim about 623–665 MB if old copies are releasable. The next bounded
run tests this while keeping the R9 alias and 8,000 MB stream cap.

The 256 MiB pressure-trim run
(`rx5700-series-s-r9-roottrim256-20261127`) reached world frame 7,836
and stopped at the 6.5-minute harness limit without a reported device
reset. Its monitor recorded no TCP or UDP endpoints. The image was still
dark and heavily blocked; the last measured frames ranged from about
0.1 to 1.2 fps. The trim did release younger copies after temporary
"no releasable" intervals, but 25 write-back heaps remained allocated
with no fully empty heap. Nonlocal usage rose to about 4.56 GiB while
free space inside those heaps grew to about 357 MiB; the 3,750 MiB root
admission gate continued to reject requests. A separate opt-in
`VICTORY_ROOT_REUSE_ONLY=1` path now allows an over-cap root window only
from an existing free heap range, without creating another heap or a
committed fallback. It has a focused allocator test but no live A/B yet.

The lit compute writer `348e1e8c414c7eca` was examined by skipping
the two following pixel shaders and dumping its 1280x720 format-10 target
(`rx5700-series-s-lit-compute-only-20261127`, frame 6,568). The block
pattern and black lower region were present after that compute dispatch,
before those pixel passes. The compute shader reads and adds to the
existing target, so this capture alone cannot establish when the pattern
first appeared. This run stopped when the target was
captured, without a reset or observed network endpoint. Native disassembly
shows the compute shader reads the 1280x720 format-19 depth SRV at table
slot 64 first and retains lanes where depth equals zero. On a reversed-depth
path with zero clear, this can be a sky-only pass; the black geometry region
in this isolated target may be expected. The block pattern in the sky still
requires a pre-dispatch comparison.
Static inspection found the format-19 typeless depth allocation, typed
format-20 DSV, and typed format-21 SRV in the expected family; no format
or twin-routing defect was demonstrated. The next capture will verify the
actual bound SRV and its contents at the first dispatch.

An independent block-address oracle for the exact four shader-bound
BC1/BC3 sizes, mips, and GFX10 4KB_S layout found no detile or tracked-span
misses. A second compute-only run with bound full mips
(`rx5700-series-s-lit-bc-20261127`, frame 6,955) dumped the first 2 MiB
BC1 and 1 MiB BC3 mip-0 inputs before upload. An offline decoder independently
detiled them and matched every staged compressed byte; decoded images are
continuous cloud and star-field textures. The compute output remained
blocked. This excludes an obvious CPU detile or missing-page failure in
those two inputs, but does not yet prove their GPU SRV contents or the
depth input are correct. The run also stopped on target capture with no
device reset and no TCP/UDP endpoints.
Comparing tile boundaries with neighboring *BC block* boundaries, rather
than within-block pixels, gives edge ratios of 1.005 and 0.995 for the BC1
input and 0.992 and 1.047 for BC3 (vertical/horizontal). These sources
show no meaningful 4 KiB tile seam. The captured compute target, by
contrast, has dominant seams at 128x64 game texels, exactly the BC1
tile shape. This makes GPU BC1 readback the next discriminating test.

The compute shader uses implicit texture samples. Its emitted DXIL was
version 1.6 but incorrectly declared validator version 1.6; the installed
validator requires 1.8 for those compute derivatives. A narrow fix sets
validator version 1.8 and supplies the corresponding PSV0 runtime-info
layout while retaining DXIL 1.6 and Shader Model 6.6. A focused shader
validation test passes. The second compute capture was built before the
companion PSV0 fix, so it is an input diagnostic, not evidence that the
metadata fix changes rendering. A rebuilt live A/B is still required.

The rebuilt `rx5700-series-s-lit-gpuread-20261127` run reached the first
world lighting dispatch at frame 6,900 and stopped on target capture without
a device reset or network endpoint. The exact 2 MiB BC1 and 1 MiB BC3 mip-0
GPU readbacks matched the staged linear bytes with **zero differences**.
The first `348e` input snapshot also captured the other three texture SRVs:
slot 40 format-67 is a smooth 192x108 sky-like gradient, slot 48 format-67
is black, and slot 64 format-19 has coherent depth silhouettes. The depth
capture supports the sky-only gate interpretation. The compute target still
contains sharp 128x64-texel rectangles. This shifts attention to an earlier
target writer, shader sampling/translation, or a more specific binding/state
error; it does not implicate mip-0 BC detiling, upload, or missing depth
geometry. Lower mips still need GPU comparison. The run used the rebuilt
validator/PSV metadata variant, so that fix alone did not remove the
post-dispatch grid. The native shader loads the existing format-10 target
before adding its sky contribution; a pre-dispatch snapshot is required
to localize the first writer.

The first opt-in root-range reuse run
(`rx5700-series-s-root-reuse-20261127`) stopped at its 6.5-minute limit
without a device reset or network endpoint, but remained at the account
picker. Its capture at frame 8,192 shows the player option; the scripted
Enter sequence had ended at 300 seconds. No root reuse was exercised, so
this is not a performance A/B. The offline harness now schedules Enter
through 480 seconds and allows a longer bounded visual run.

The next diagnostic run
(`rx5700-series-s-lit-allmips-prepost-20261127`) reached the first world
lighting dispatch at frame 6,929 and stopped on capture without a reset
or observed socket endpoint. It compared *every mip* of the bound BC1
(12 levels) and BC3 (11 levels), including the packed tails: GPU readback
equaled staged bytes at every level. The actual SRVs are BC1_UNORM and
BC3_UNORM Texture2D views with first mip zero, all levels, default
component mapping, and zero minimum LOD. These checks rule out a missing
or corrupt upload in either sampled BC chain.

The same command list copied the exact format-10 target immediately before
and immediately after `348e`'s first dispatch. The pre image is mostly
black with coherent, unbanded scene details. The post image adds sky with
sharp rectangular discontinuities. Both following pixel shaders were
skipped. This establishes `348e` as the origin of the sky grid in that
frame, while its depth-zero mask explains the unlit geometry area in this
isolated pass. Native BC1 and BC3 sample instructions use implicit LOD;
their coordinate order, resource-table slots, immediate sampler filter,
wrap modes, LOD bias, and emitted DXIL opcodes match on static review.
The remaining testable distinction is implicit derivative/LOD behavior
versus an input value within the shader. An exact-shader diagnostics-only
LOD-zero variant is being prepared for a bounded A/B.
The independent seam analyzer also compared eligible lower mip tile
boundaries to neighboring BC block boundaries: ratios were 0.821–1.105
across BC1/BC3 mips 1–3, with no strong source seam. Smaller mips have
too few full tiles for that statistic.

The exact-shader LOD-zero run
(`rx5700-series-s-lit-lod0-20261128`, frame 8,564) forced the three
implicit `348e` image samples to `SampleLevel` at mip zero. Its pre/post
snapshots still show the same sharp sky boundary. In the half-resolution
capture, the mean RGB jump across x=269 in a sky region is 19.018 in the
normal all-mip run and 18.976 with LOD zero; the adjacent control boundary
at x=256 is 0.218 versus 0.212. This rules out implicit derivative or mip
choice as the dominant cause of this grid. The shader variant was logged
as selected, the run stopped on target capture, and no reset, D3D severity-1
message, or TCP/UDP endpoint was observed.

A second exact-shader diagnostic
(`rx5700-series-s-lit-zero-bc1-20261128`, frame 6,489) replaced the
`image_sample` result at native offset `0xbcc` with zero. The sky boundary
persisted: the mean RGB jump at x=269 was 19.170 versus 19.018 in the
normal run, and the two sky regions differed by a mean absolute 2.167
RGB levels. The sample feeds the final RGB calculation, but is not the
source of this boundary. The 192x108 R9G9B9E5 inputs at table slot 1,
offsets 40 and 48, each shared its actual host copy with a same-address
R32_UINT writer of matching footprint and swizzle. This rules out a stale
separate title-memory upload for these two views. The offset-40 input had
nonzero values only in columns 0–127 of 192; the offset-48 input was black.
Whether this partial fill is expected needs a writer and shader-use trace.
The run stopped on target capture without a reset or observed TCP/UDP
endpoint.

Replacing all eight `348e` samples from the two packed 192x108 sky inputs
with a nonzero constant (`rx5700-series-s-lit-constant-luts-20261128`,
frame 6,711) removed the x=269 sky discontinuity (mean RGB jump 0.0;
normal run 19.018). The isolated sky becomes nearly uniform white, so this
test localizes the visible color change to these samples but does not yet
prove why their original values are discontinuous. A first-writer trace
identified compute shader `26b8bbb2e7ae4dc6` as the R32_UINT UAV producer
of both same-memory packed inputs. Its native workgroup is 32x2 threads;
the observed 128-column fill is exactly four horizontal workgroups. The
next bounded test logs its actual dispatch and, only if it is 4x54x1,
tries six horizontal groups to cover the full 192x108 resource. The
constant-input run stopped on target capture without reset or TCP/UDP
endpoint.

The coverage check (`rx5700-series-s-lit-fullwidth-20261128`, frame 6,527)
logged the *actual* title dispatch as 6x54x1. With native 32x2 groups,
it already covers all 192x108 texels. The conditional expansion therefore
did nothing and was removed; the source and sky seam remained unchanged.
The right-third cutoff cannot be explained by too few dispatched groups.
The producer and consumer are named `sky_scattering:CS_SkyView` and
`sky_dome:CS_SkyMain` in `shaders.rpf`. An older R32_UINT writer-target PNG
shows nonzero integer values to its right edge, unlike the R9 read-view
PNG, but those PNGs normalize different formats and were not an exact
same-frame byte comparison. The next diagnostic saves bounded raw readback
of both views and a final presented world frame alongside the intermediate
lighting target. The coverage run stopped on target capture without a GPU
reset or observed TCP/UDP endpoint.

The synchronized raw-byte run (`rx5700-series-s-raw-timeline-20261128`)
captured the 192x108 R32_UINT output immediately after `CS_SkyView` in
submission 11,772 and the paired R9G9B9E5 read view before `CS_SkyMain`
in submission 11,783. At the same title address, all 20,736 texels of the
nonblack R32 output had valid packed values across the full width. Every
texel of the R9 view differed: its first 128 columns contained altered
values and its last 64 columns were zero. The R32 bytes at frame presentation
were bit-for-bit identical to those after the producer, and the R9 bytes
remained bit-for-bit identical across subsequent captures. The other
writer/read pair was all zero in both formats. These comparisons isolate the
error to the cross-format placed-resource alias, independent of dispatch
coverage, producer output, mip choice, or later writes. The same run saved
the final backbuffer, which shows the heavy grain and blocks reported by the
user despite a much cleaner intermediate sky target. A diagnostics-only
GPU buffer repack from R32 to R9 is under live A/B test. No GPU reset or
TCP/UDP endpoint was observed in the raw-byte run.

The first repack run (`rx5700-series-s-repacked-r9-20261128`) kept the
R32_UINT and R9G9B9E5 resources overlapping in one heap. It filled the
rightmost 64 columns of the R9 read view, but 11,264 texels in the left
128 columns still differed from the producer. The lighting target retained
the abrupt sky boundary and the presented image retained heavy grain.

In `rx5700-series-s-separate-r9-20261128`, the bounded 192x108 R9 readers
instead had separate committed allocations. Each GPU buffer repack copied
the producer's logical rows into that allocation before the sky shader
sampled it; the two resources use independent state transitions without
alias barriers. The synchronized raw output and input were bit-for-bit
equal for all 20,736 texels of the nonblack pair (and all zero in the other
pair). The post-`CS_SkyMain` lighting capture has a continuous blue sky
without the previous sharp rectangular seam. The presented 1920x1080
frame still has dense colored grain and blocks, so that is a distinct
downstream fault. This run had no GPU reset or observed TCP/UDP endpoint.
An opt-in first-dispatch FidelityFX input/output capture is being tested
to locate where that noise begins.

The first FidelityFX stage run (`rx5700-series-s-ffx-stage-20261128`)
logged a successful host FSR 3.1.6 dispatch. Its input color is 1280x720
R16G16B16A16_FLOAT (Ffx surface format 4, DXGI 10); depth is DXGI 19;
motion vectors are DXGI 34. Its output is declared to FidelityFX as
1920x1080 R9G9B9E5_SHAREDEXP (Ffx surface format 29) but is actually a
DXGI 42 R32_UINT host resource. The SDK returned success and wrote an
image with a coherent scene silhouette. The final presented image still
contains the heavy colored noise. A first-output raw capture and a
same-address R9 reader capture will separate an invalid FSR output from
a further cross-format placed-resource alias failure. AMD's API expects
the resource description to reflect the supplied Direct3D resource's
format (`ffxApiGetResourceDX12` constructs both together).

The raw FSR run (`rx5700-series-s-ffx-raw-20261128`, frame 5,771)
captured the exact 1920x1080 R32_UINT output after the first FSR dispatch
and the same host texture's R9G9B9E5 read view two submissions later.
2,066,166 of 2,073,600 packed words differed. More decisively, the
R32_UINT words form a spatially smooth single-channel float image:
interpreting their bits as float32 gives mean neighboring 8-bit pixel
change 2.17, whereas interpreting them as R9G9B9E5 gives 39.78 and
dense colored noise. The values are often around 0x3b000000, a plausible
small positive float but nonsensical packed color for the visible scene.
The host FSR dispatch was given a packed-RGB description over an R32_UINT
UAV, so its single-channel output and the subsequent incompatible placed
R9 alias are both proven faults. A GPU conversion path now gives FSR a
matching R16G16B16A16_FLOAT UAV, then packs its RGB into the title's
R32_UINT words, and uses a separate R9 resource with a row-major copy.
The combined path is opt-in pending the live A/B world test.

The combined live run (`rx5700-series-s-ffx-corrected-20261128`, frame
5,858) passed. FSR wrote to a matching 1920x1080 R16G16B16A16_FLOAT
temporary; a GPU compute pass packed the RGB channels into the title's
R32_UINT output. A row-major copy populated a separately allocated R9
read texture. The raw R32 writer and R9 reader matched at all 2,073,600
texels (SHA-256 prefix `9671dcf066428f80` for each). The presented frame
lost the dense colored static and shows the scene's buildings, trees,
sky, and character silhouette. It remains very dark and has broad
rectangular blocks and missing detail. Its captured R9 output already
has these defects, so the remaining work is upstream of that read view
or in FSR itself. A retained first FSR color-input capture and later
world-frame captures are next. There was no GPU reset or observed socket
endpoint in the combined run.

The longer corrected run (`rx5700-series-s-ffx-later-20261128`) captured the
world's first frame and five later frames. Its clean post-`348e` sky target
became a heavily tiled FSR color input within the same world frame. It then
hit D3D12 debug error 28: a BC5_UNORM SRV was made on a BC7_UNORM host
resource, followed by device removal. Equal 16-byte compressed blocks are
not a sufficient condition for a cross-family D3D12 view, even with both
formats in the castable list. The castable list now keeps BC formats within
their own DXGI family; incompatible aliases use separate textures and the
existing row-major buffer copy. The rerun
(`rx5700-series-s-bc-cast-fixed-20261128`) presented 30 world frames without
a device reset and with zero observed TCP/UDP endpoints. It captured the
exact FSR input: its 1280x720 format-10 color texture already contains the
rectangular corruption. The post-`348e` snapshot of that same host texture
is substantially cleaner. FSR preserves rather than originates this
remaining pattern. The first world frames are dark but recognizable; the
30th has prominent purple rectangles. A bounded write-by-write capture is
underway to locate which intervening draw changes the color texture.

The 30-frame run's local `coverage.json` reports 1,747 translated paths of
1,748 shader hashes bound at least once. The one bound translation failure
is compute hash `90a0692af4320129` (lane mask read as data at `0x14dc`);
the related `82a2ad6d07417d36` failed during preparation but was not
bound in this run. Mesh compute-to-vertex fallbacks are counted as
translated. The diagnostic run's world frame-rate median was 0.7 fps with
full D3D12 debug, many readback captures, and complete draw tracing; this
is not a production performance measurement. Root views and indirect mesh
work were still dropped when the 3,750 MB nonlocal admission limit filled.

The packed sky R9 reader, the same-memory 720p R9 placed-view normalization,
the 1080p FSR output conversion, and the R32-to-R9 row-major repacks are now
enabled in ordinary builds after the bounded live comparison. The
`rx5700-lit-path2-coverage-20261128` run is checking that promoted path and
counting texture view outcomes by title, requested, and host format with a
failure reason.

The first same-resource FSR pre-capture (`rx5700-ffx-pre-stage-20261128`)
showed a coherent post-lighting 1280x720 image and an already corrupted
image before the SDK call. The host copy, D3D12 resource, and title memory
address matched across those captures. List checkpoints in actual GPU
submission order (`rx5700-lit-stage-checkpoints-20261128`, first world frame
6,466) isolated the first corruption to original host list 9, immediately
after list 7's clean sky. That list has one direct dispatch of compute shader
`577248728544d74b`, a barrier, and an indirect dispatch whose UAVs do not
include the 1280x720 lighting color. The 577 dispatch writes that color at
`s1+48`. Earlier command tracing missed it because CPU threads recorded the
lists out of GPU submission order. The readback checkpoint run hit the
title's known watchdog null-write at `victory-image.dll+0x23cf683` under its
24 extra readbacks; its first two checkpoints completed, and no GPU reset or
socket endpoint was observed.

Skipping 577 removed the large orange rectangular pattern in
`rx5700-skip-577-dump-20261128`, confirming it is the first corruptor, but
the final image remained too dark to use. This is a diagnostic-only skip.
Forcing level-zero sampling in 577 (`rx5700-577-lod0-20261128`) left the
pattern essentially unchanged, excluding implicit mip choice as the main
cause. The exact 4,812-byte shader container was saved for instruction-level
work. Three of its four selected color source captures in
`rx5700-577-inputs-20261128` did not have the output's rectangular pattern.
The fourth source is a placed R9G9B9E5 view whose host copy has an R32_UINT
primary; that first diagnostic attempted to read the primary. The capture
now selects the actual R9 shader resource after alias preparation so the
next run can test whether the remaining pattern is in the source or appears
inside 577's translation. Diagnostic capture and forced LOD are opt-in.

The corrected R9 input capture (`rx5700-577-r9-input-20261128`) showed the
orange blocks already in the 480x272 R9 view sampled at `s1+16`, while its
other selected color inputs did not share the pattern. Its R9 reader and
R32_UINT primary occupied the same host allocation. The existing separate
reader and row-major repack was extended to 480x272. In
`rx5700-480-r9-repacked-20261128`, five such readers were made and the GPU
performed bounded 557,056-byte copies. The sampled R9 texture became
coherent, and the first FSR input and final world frame lost the large
orange rectangles. The world character, buildings, trees, and sky are now
recognizable. The ground is still severely underexposed; a later frame can
still be nearly black. This run translated all 1,657 bound shader paths,
showed no GPU removal, and observed zero TCP/UDP endpoints. Its 1 fps world
sample included diagnostic capture and is not a normal performance result.
The FSR input's median nonzero RGB value was around 0.0000068, while its
99th percentile was around 0.0041. A configurable display exposure in the
existing FSR packing shader is being compared under the offline harness.

The 32x display exposure comparison (`rx5700-exposure32-20261128`) revealed
terrain, vegetation, buildings, and the player's body in the second world
frame without changing FSR's temporal input. The first world frame's sky
was blown out, so 32x is a diagnostic setting rather than an accepted
default. Five frames later the sky showed a separate coarse rectangular
pattern, while lit ground lights remained clear. The run translated all
1,582 bound shader paths, had no device removal, and observed no TCP/UDP
endpoints. Targeted `VICTORY_CAPTURE_577_OFFSET` and
`VICTORY_CAPTURE_FFX_OFFSET` controls now select a later world frame so the
next run can capture the exact source and FSR input at the onset of that
pattern. Exposure defaults to 1x and is limited to 0.25x–64x.

## RX 5700 XT local memory budget (28 November 2026)

Longer diagnostic runs still do not demonstrate an interactive game world or
30 fps. The 720p `rx5700-live-world-20261128` run translated all 1,730
shader paths it bound, but lost the GPU after local and nonlocal use climbed
near 5.0 GB and 4.6 GB. Its draw-count based `world_first_frame` marker can
also fire during the opening cinematic, so it is only a heavy-scene timing
marker. A 960x540 output request lowered the presentation size but did not
change the title's 1280x720 internal targets. Shader and root-view coverage
still need work; lower display resolution alone does not cure the scene.

The texture streaming admission and eviction paths both had a fixed 4,800 MB
local-memory threshold. They now share `VICTORY_STREAM_LOCAL_PERCENT`, default
85% of the *current* DXGI local budget, with eviction aiming five percentage
points lower. The combined local/nonlocal streaming cap remains separate at
9,000 MB by default. Windows began the tests with roughly 7,741 MB available
to this process, then reduced that budget to about 6,300 MB in the heavy
scene. Thus 85% there means about 5,350 MB, not 6.8 GB of the card's physical
8 GB. Small eligible sampled textures can also occupy 4 KB aligned ranges
within placement heaps; the live log confirmed that path for 1x1, 16x4,
128x128, and other textures.

The first 85% test changed admission only and exposed the missed 4,800 MB
eviction threshold. It ended in GPU removal with 145,440 missing texture
views. After aligning eviction, `rx5700-local85-consistent-20261128` ran its
90-second heavy-scene window and was stopped by the harness without GPU
removal. It translated 1,884/1,884 bound shader paths, made 671,497 texture
views, and had 20,197 misses, chiefly 17,008 format-10 placed SRVs whose
host copy was unavailable. Peak sampled local use was 5,129 MB of a 6,298 MB
budget (81.4%); peak combined local/nonlocal use was 9,629 MB. The 9,000 MB
combined cap halted further streaming before the 85% local limit. The
draw-count based heavy-scene sample was only 1.6 fps median, with many root
views and indirect draws still missing. The network monitor observed no TCP
or UDP endpoints in 55 samples. The two runs reached different scene frames,
so their view-miss counts are diagnostic rather than a controlled A/B result.

Raising only the combined streaming cap to 9,500 MB
(`rx5700-local85-total9500-20261128`) admitted more textures but ended in
GPU removal after 417.6 seconds, before the timed window completed. Peak
sampled local use reached 5,444 MB (86.5% of the sampled live budget) and
combined use reached 10,210 MB. The 85% check stops *new streamed textures*;
GPU-written resources, buffers, and already allocated textures can still
push current use past it. This run translated 2,088/2,088 bound shader paths
but had 65,814 missing texture views and a 1.45 fps heavy-scene median. The
memory increase and removal are correlated, but the diagnostic run did not
enable D3D12 debug breadcrumbs, so the exact cause of removal is unproven.
The 9,000 MB combined default remains; more streaming did not improve this
run's reliability or performance.

## External renderer comparison (28 November 2026)

The upstream VIctory `main` remains at local commit `63bcca4` after a fresh
fetch. Its open [ambient fallback PR #3414](https://github.com/Marvisak/VIctory/pull/3414)
and [wave/lanes PR #2633](https://github.com/Marvisak/VIctory/pull/2633)
are relevant but not merged. The latter is a large translator change; the
open [permlane PR #3587](https://github.com/Marvisak/VIctory/pull/3587)
addresses one instruction seen in our late bound-shader failures. These
were inspected rather than merged wholesale into our modified translator.

[ScarlettPort](https://github.com/ZigZagBoi1/ScarlettPort) is another
GPL-3.0-or-later fork of VIctory. Its documented lit scene relies on a
screen-probe ambient stand-in because the title's ray-traced probe data is
absent, and on texture/shader fixes beyond our checkout. Its published
requirements start at RX 6000 for hardware mesh shaders; the RX 5700 XT
needs our separate compute-to-vertex fallback, so its executable is not a
drop-in replacement for this machine.

The fork's [BC7 rotation fix](https://github.com/ZigZagBoi1/ScarlettPort/pull/108)
exposed a concrete bug in our BCPack decoder: only modes 4 and 5 carry
rotation bits. Reading those bits in other modes can swap alpha with a colour
channel. The decoder and a focused test now use mode-aware rotation. The
opt-in ambient pass from upstream PR #3414 has also been ported, without its
unrelated Series S plane compositor. It compiles on this RX 5700 XT and is
off unless `VICTORY_AMBIENT` is set. The first diagnostic run confirmed the
pass wrote the 1280x720 indirect-light target, then the title hit its known
`0x23cf683` watchdog abort about 30 seconds after the first heavy frame.
The title's attempted Breakpad HTTP upload was blocked by the local network
layer, and no TCP or UDP endpoint was observed. That run used D3D12 debug;
the abort alone does not establish whether the ambient pass caused a stall.

The subsequent no-debug ambient-on run
(`rx5700-ambient-bc7-later-20261128`) stopped on its 30th captured
heavy-scene frame without device removal. It translated 1,321/1,321 bound
shader paths, made 158,495 views without a reported miss, and observed no
network endpoint. Its last image showed the character, ground, trees, and
buildings, but strong black holes and square sky blocks remained. A run with
the same BC7 fix and exposure but ambient off
(`rx5700-bc7-ambientoff-control-20261128`) also reached the 30th frame
without removal or an observed endpoint; it translated 1,357/1,357 bound
paths and made 125,040 views without a reported miss. Its frames still had
large square blocks. The two runs entered the heavy scene on different
presented frames and drew different content, so their 1.8 versus 2.4 fps
diagnostic samples and later images are not a controlled lighting or speed
A/B. The matching early tiled frames establish only that ambient light does
not by itself remove the grid.

ScarlettPort also records a separate case of blocky patches from truncating
indirect dispatches at 65,535 groups. Our clamp has that limit as well, but
the current 1280x720 internal color target uses smaller tile lists; no live
evidence yet links that limit to these sky blocks. Its reported rectangle-list
fix is already present in this checkout. A later `577`/FidelityFX capture
(`rx5700-grid-source-offset5-20261128`) found the four sampled `577` inputs
and FFX color input coherent at one targeted frame; the packed output in the
following frame again showed sky squares. The draw-count world marker can
fire in the intro, and the resolve may skip a presented frame. Diagnostic
offsets now follow the first actual `577` resolve and capture the first
matching pass at or after the requested offset. FFX packed-read capture is
consumed once, preventing repeated large readbacks. These changes affect
diagnostic builds only.

The final paired readback (`rx5700-577-sameframe-ffx-20261128`) caught the
first `577` lighting write on the same 1280x720 host texture, at submission
17,794/list 8. Its immediate pre image has a smooth blue sky; the immediate
post image has a smooth orange cloud sky. The targeted FFX input at later
submission 17,819/list 31 has coarse square sky patches, which the packed
FFX output retains. All four `577` source snapshots were saved as well.
Because 25 submissions and a present separate the post and FFX snapshots,
this pair **does not** prove the selected `577` dispatch introduced the
grid. It proves that the grid appeared after that smooth post image and by
the later FFX handoff on the same resource. The next diagnostic should list
the writes to that exact title-memory range in actual submission order
between those two points, then read back immediately after the first suspect
writer. This run translated 1,598/1,598 bound shader paths, reported no
missing views or device removal, and observed no TCP/UDP endpoints. It
stopped on capture before a meaningful world-fps sample.

### First tiled world composite, 28 November 2026

The writer-order diagnostic (`rx5700-lit-writer-order-20261128`) observed a
smooth `577` post image on the 1280x720 format-10 color resource, followed
by a tiled FFX input on that same host resource. Command-list checkpoints
(`rx5700-lit-gap-checkpoints-20261128`) placed the first tiled image after
original list 27 in the FFX submission; the preceding checkpoint was clean.
The list binds the color target from compute shader `731a39171b568298`
(`rx5700-lit-target-binding-20261128`).

The targeted pre/post readback around that shader's **indirect** dispatch
(`rx5700-731-indirect-20261128`) confirms it: `731-pre` is smooth and
`731-post` has the first coarse sky blocks. Its argument buffer contains one
record, `[14580, 1, 1]`, below the 65,535-record clamp. The console shader
declares an 8x8 group. 14,580 groups are 933,120 threads, or 1296x720 if
laid out in 720 rows, whereas the target and FFX render input are 1280x720
(921,600 pixels). That arithmetic initially suggested a width/padding
problem; the tile-list capture below shows the dispatch does not map groups
linearly onto pixels. Its ISA also has a 128-pixel indexed read around
offset `0x0760`, which may be intended sampling logic.

The same run translated 1,644/1,644 bound shader paths, had no device
removal, and sampled zero TCP/UDP endpoints. It reported 121 unavailable
placed texture views, mainly BC1/BC7, which need a separate memory-pressure
and residency audit. Its short diagnostic world sample had median 2.0 fps;
the capture overhead makes this unsuitable as a performance baseline.

An opt-in control omitted shader `731a39171b568298` at pipeline creation.
The first FFX input then kept a smooth sky without the large tiles in two
runs (`rx5700-731-skip-control-20261128` and
`rx5700-731-fallback-creation-20261128`). Green edge noise and a very dark
ground remain. The release-capable switch
`VICTORY_DISABLE_731_COMPOSITE=1` now takes the same pipeline-creation path.
A first attempt to skip only at dispatch time produced a near-black FFX
input (`rx5700-731-fallback-20261128`), so that implementation was removed.
The successful fallback run had no missing views, device removal, or observed
TCP/UDP endpoints; it stopped on the first FFX capture before a useful fps
sample.

The native ISA explains why the 14,580-group count must not be treated as
933,120 linearly addressed pixels. At `0x00e0` shader `731` loads a packed
tile coordinate from its slot-1, offset-0 buffer SRV, decodes two 16-bit
coordinates, and adds the 8x8 lane position at `0x0110`/`0x0114`. The
indirect count is `3,645 * 4`: four 8x8 groups per 16x16 tile. The captured
constant buffer contains `3,645` and `3` at offset `0xd0`, which selects
one of four tile-list partitions before adding the group index divided by
four. `3,645 = 81 * 45`, including a guard column against an 80x45 grid
for 1280x720.

Two isolated runs (`rx5700-731-tilelist-20261128` and
`rx5700-731-tilelist-after-20261128`) captured the 58,320-byte tile SRV.
The exact host view was all zero before the command list, then contained
2,150 distinct nonzero coordinates and 1,495 zero entries after it. All
nonzero coordinates were aligned to 16 pixels; 27 lay in the guard column
at x=1280. The nonzero tile list is sparse, covering sky and scene regions
that the pass actually changes. In the 1280x720 pre/post image comparison,
905 of 2,124 listed in-bounds tiles changed by more than five 8-bit levels
per channel on average, while **none** of 1,476 unlisted tiles did. The
average difference of unlisted tiles was 0.001 level. The repeated zero
entries therefore do not imply a broken tile producer; one zero entry may
represent the valid origin tile at (0,0). The dispatch count and guard
column are not supported as the cause of the large sky blocks. The next
diagnosis should inspect shader `731`'s sampled color/depth/noise inputs
and calculations inside a listed tile. Both runs translated every bound
shader path and observed no GPU removal or TCP/UDP endpoints. The harness
stopped at the first FFX capture, so their fps measurements are not a world
performance baseline.

The same run also exercised the explicit RT-off route: the live title
`settings.xml` had both `rtLODLightShadows` and `rtDynamicShadows` set to
`false`, and the GPU log recorded suppressed `DispatchX` ray-tracing builder
calls. This verifies those two controls in a real run, though it does not
by itself enumerate every possible shader-based ray path.

The full source capture (`rx5700-731-all-sources-20261128`) resolved the
placed texture views as well as ordinary resource views. The 640x360
R9G9B9E5 texture bound at slot 1 offset 64 was already tiled *before* 731
ran. Its captured host copy used a same-memory R32_UINT/R9 alias
(`shared_exp_separate=false`). At the saved PNG's 320-pixel preview width,
the mean RGB change across 16-pixel grid seams in its upper 160 rows was
20.42 levels, versus 1.83 at the halfway control positions. The nearby
slot-1 offset-24 texture was substantially smoother. No GPU reset or
TCP/UDP endpoint was observed.

The existing separate committed R9 reader and row-major repack now also
cover 640x360. A matched run (`rx5700-640-r9-separated-20261128`) recorded
`shared_exp_separate=true` for that same slot-1 offset-64 source. Its
captured sky has no visible tile grid; the 16-pixel seam/control means were
2.46/2.23 (ratio 1.10, down from 11.15). The immediate 731 post image and
the presented frame likewise have a continuous sky. The scene is still
severely underexposed, and the run's short, capture-heavy sample is not a
useful frame-rate measurement. It translated every bound shader path,
reported no GPU removal, and observed zero TCP/UDP endpoints. The launcher
now leaves 731 enabled by default; its skip switch remains for diagnosis.

An offline follow-up with the local screen-probe ambient stand-in at level
0.6 (`rx5700-ambient-640-r9-20261128`) reached the world marker and saved
frame 6,352. The stand-in ran and the ray-tracing builder was suppressed,
but the image remained nearly black, so ambient stays off by default. The
short world sample was 5.9 fps on the RX 5700 XT, far below the 30 fps
goal. All 1,684 shader paths bound in this run translated, with no reported
GPU removal or observed TCP/UDP endpoints. The unlit world and performance
remain separate issues from the corrected sky alias.

The next coverage run (`rx5700-world-views-20261128`) captured a later
character scene five frames after its world marker. It showed recognizable
terrain, foliage, houses, and the player without the earlier sky grid.
71,082 texture views were made and 15 placed SRVs were unavailable. The
dominant missing work was instead root views: live title-memory windows
were refused when nonlocal use exceeded the port's 3,750 MB admission cap,
dropping mesh, indirect, and ordinary draws. The short two-sample 0.8 fps
median included full coverage and capture overhead.

`rx5700-world-rootreuse-20261128` paired the existing 512 MB pressure trim
with reuse of free write-back heap ranges. It logged at least 128 reused
root windows and had no sampled root-view warnings or dropped-work reasons,
but 622 placed SRVs became unavailable versus 15 in the baseline. Its
fifth world frame was also darker. Since the two runs entered different
frames, these are a tradeoff signal, not a controlled speed or lighting
comparison; root reuse remains off. Both runs translated all shader paths
they bound, reported no GPU removal, and observed no TCP/UDP endpoints.

A trim-only run (`rx5700-world-trim512-20261128`) exposed a diagnostic
marker problem: the first 866-draw intro frame satisfied the old 500-draw
"world" threshold, so its capture stopped before gameplay and cannot
compare with the two character scenes. The capture threshold is now
configurable through `VICTORY_CAPTURE_WORLD_MIN_DRAWS` and the offline
harness's `-WorldMinDraws` parameter. The 3,000-draw trim-only rerun
(`rx5700-world-trim512-gameplay-20261128`) did not reach that marker: as
nonlocal use climbed to roughly 4.5 GB, root-view failures exceeded 32,768
and 26,331 texture views were unavailable, mostly format-10 and format-34
placed SRVs. The GPU was removed after about six minutes. Trimming had
released several hundred old loose copies, but did not prevent this
longer-run failure. No TCP/UDP endpoint was observed. A separate run at a
256-pixel texture edge limit and no root trim (`rx5700-edge256-20261128`)
also failed before the 3,000-draw world marker: more than 65,536 root views
were unavailable, 67,806 texture views were unavailable, and the GPU was
removed after about seven minutes. At the end, write-back heaps held roughly
1.8 GB across about 8,000 live or pending placed resources; those counts
alone do not establish a leak. This establishes that lowering texture mips
alone does not resolve the pressure. Neither failure proves the trim caused
removal, since the earlier no-trim control ended after five world frames.

The RT-off audit found a gap in the first implementation. Both title RT
shadow settings were false, and `DispatchX` was suppressed, yet a regular
compute program (`32ed072b9f845048`) containing native `image_bvh`
ray-traversal instructions was still submitted. The translator now exposes
a cache-independent native instruction classifier. With RT off, the host
rejects pipelines containing those instructions before resource binding;
`DispatchX` remains suppressed separately. A six-minute offline run with
the initial compute-only guard (`rx5700-rt-off-bvh-20261128`) logged the
BVH shader's dispatches as suppressed and captured a recognizable scene at
world frame 7,674; all 1,632 bound shader paths translated, no GPU removal
or TCP/UDP endpoint was observed. Its two coverage-heavy world samples had
a 0.45 fps median; that is not a sustained performance baseline. The
guard was then moved to shared pipeline binding to cover any shader stage.
The second six-minute offline run (`rx5700-rt-off-complete-20261128`)
reached world frame 6,385, found 13 native ray-traversal shaders and logged
over 2,048 suppressed uses of them, as well as over 4,096 suppressed
`DispatchX` calls. All 1,866 bound shader paths translated; no GPU removal
or TCP/UDP endpoint was observed. This run stopped after the next world
frame, so it has no sustained fps sample. It also had 27,690 unavailable
texture views; RT-off alone does not solve memory pressure or rendering
quality. The classifier initially warned on port-generated DXIL, which has
no native program; those generated shaders are now marked explicitly and
are excluded from native classification.

Lowering the combined streaming admission limit from 9,000 to 8,000 MB
(`rx5700-stream8000-20261128`) did not reserve enough nonlocal memory for
root views. When nonlocal use crossed roughly 3.9 GB, root-view denials
began; they exceeded 65,536, and the GPU was removed before the 3,000-draw
world marker. The run translated all 1,933 bound shader paths and observed
no TCP/UDP endpoints. The texture-view miss count was 232,049, mostly
repeated attempts at deferred host copies, not necessarily that many
distinct textures. The launcher default remains 9,000 MB.

The loose-copy cache previously keyed copies by their exact start and
length. A root view asking for the first 256 KiB of a larger copy could
allocate another buffer containing the same title bytes. Lookup now reuses
the smallest covering copy with that start, and logs reuse counts. Root
denial diagnostics also check beyond the ordinary 64-entry registered
buffer search to distinguish a hidden registered copy from the nonlocal
admission limit. The 9 GB offline run with this fix
(`rx5700-loose-cover-20261128`) logged more than 2,048 covering-copy
reuses and reached the fifth world frame with a recognizable scene. It
translated all 1,605 bound shader paths, reported no GPU removal and no
TCP/UDP endpoint. Root-view denials still reached at least 512; sampled
failures had `beyond_64=None` and were denied by the nonlocal cap. The
50 unavailable texture-view events and two 0.45 fps median samples are
from this capture-heavy run, not evidence of a sustained speed gain.

On 2026-11-28 the machine had 31.9 GB installed physical RAM and 23.7 GB
available with the game stopped; system commit was about 19.7 GB of a
55.8 GB limit. The port's root-copy stop at 3,750 MB is an observed DXGI
nonlocal-use safety limit, not a cap derived from physical RAM. Prior GPU
removals occurred while DXGI nonlocal use was only about 4.5–5.4 GB,
despite ample free RAM. An opt-in `VICTORY_ROOT_SYSTEM_MB` override
(3,750–5,000 MB, default 3,750) and the offline harness's `-RootSystemMB`
parameter now allow a bounded test of extra root-copy headroom without
changing normal launches.

The eight-minute offline test with the root-copy stop raised to 4,250 MB
(`rx5700-root4250-20261128`) reached the world and captured its fifth
frame without GPU removal or a TCP/UDP endpoint. It translated all 1,997
bound shader paths. This did not clear the memory-related rendering gaps:
nonlocal DXGI use climbed past 5 GB, root-view denials reached at least
32,768, and the run recorded 4,013 unavailable texture-view events. The
two world fps samples (median 0.15) were taken with heavy capture enabled
and differ in scene workload from the 3,750 MB run, so they cannot establish
a speed comparison. The normal root-copy stop remains 3,750 MB.

The 2026-11-28 offline 720p capture run at the default 3,750 MB limit
(`rx5700-offline-20261128-082955Z`) reached frame 8,014, captured a
recognizable but dark world scene, and measured 1.35 fps median over six
world samples. All 1,686 bound shader paths translated. Root-view misses
reached at least 8,192 and were diagnosed as nonlocal copy-budget denials;
there was no reported GPU removal or TCP/UDP endpoint. A separate bounded
20-second world run at 4,750 MB (`rx5700-offline-20261128-083706Z`)
translated all 1,609 bound paths, logged no root-view denials in that
window, measured 2.3 fps median over four samples, and reported no GPU
removal or TCP/UDP endpoint. These runs use different capture settings and
scene windows, so their fps medians are not a controlled speed comparison.
The higher limit remains opt-in until a longer matched run establishes
stability and image improvement.

CPU sampling during loading showed about 1.6 occupied logical cores; a
six-second world sample showed about 1.1. That is a process total on the
24-logical-processor Ryzen 3900X, not proof that more workers would improve
the frame rate. World telemetry showed 100–400 ms preparing submissions,
roughly 90–200 ms waiting for GPU completion, and substantial title-memory
watch and guard contention. Pipeline creation already uses up to eight
workers, and changed texture refreshes already use up to four. The offline
harness now records cumulative CPU seconds and interval-equivalent CPU cores
in `monitor.csv` for future runs. Prioritize memory tracking, copy-back, and
GPU waits before increasing worker counts.

The no-capture 3,750 MB baseline (`rx5700-cpu-baseline-20261128`)
reached its 3,000-draw world marker and showed a 1.95 fps median over the
world samples. This is a more useful speed observation than captured-frame
samples, although the scene workload varied. At the last measured frame it
spent 213 ms preparing submissions (119 ms on copy refresh), 191 ms waiting
for GPU completion, and 58 ms in copyback. The five-second process samples
around gameplay used about 0.2–0.9 equivalent logical cores, with CPU
bursts above three during loading. Roughly 9.6 GB of physical RAM remained
free when the GPU was removed at 4,650 MB nonlocal and 5,098 MB local DXGI
use. Root-view denials had passed 65,536. The title exited with
`0x887A0005`; this run did not enable DRED, so its "no breadcrumbs" message
did not establish whether the GPU recorded a fault. Device-removal logs now
say explicitly when DRED was disabled. No TCP/UDP endpoint was observed.
More physical RAM or a higher generic worker count would not
address this failure mode. The world-timing harness also had a bug: it
looked for a 3,000 *average* draws/frame rate even though the marker is
based on one frame, so this run stopped on the GPU failure. The timer now
uses the marker directly.

A follow-up with forced DRED (`rx5700-dred-world-20261128`) reached the
3,000-draw world marker. The corrected timer stopped it after 65 seconds
of world time without a reported device removal or socket endpoint. It
still showed root-view denials as nonlocal use passed roughly 4.2 GB. DRED
recording and its very large diagnostic trace slowed the world markedly,
so this run does not disprove the uncaptured reset or establish performance.
No fault address was available because no reset occurred in its window.

The uncaptured 256 MB pressure-trim test (`rx5700-trim256-world-20261128`)
reached its world marker and survived the 80-second timing window, versus
the earlier baseline reset after roughly 40 seconds in its world. Its
12 world fps samples had a 1.6 median, and root-view denials still passed
65,536. Nonlocal use reached 4.8 GB while the local budget fell from 7.7
to 6.9 GB. The scenes and run conditions were not identical, so survival
alone does not prove a trim speed or stability gain; it did not restore
the missing draws. The later captured frame remained very dark. Pressure
trim stays opt in.

Loose-copy lookup now also reuses a nearby copy starting in an earlier
256 KiB window when it covers the whole requested range. Previously only
same-start covering copies were found, so a root view of the second half
of a two-window buffer view could allocate duplicate write-back memory.
Lookup checks same-start copies first, then at most 32 preceding cache
entries; a focused coverage test passes. The bounded offline live run
(`rx5700-crossstart-cover-20261128`) logged more than 131,072 earlier-start
reuse *calls* and reached a recognizable player/yard world frame. All
1,915 bound shader paths translated, no GPU removal was reported during its
45-second world window, and no TCP/UDP endpoint appeared. Its six world
fps samples had a 2.2 median. This does not establish a speed gain over
the prior 1.95 fps baseline because the frames and load differed. The
host still denied over 16,384 root views as nonlocal use reached 4.6 GB;
the cache lookup fixes duplicate copies but does not make the world fully
rendered or stable for extended play.

The requested 720p launch writes 1280x720 into the title's two saved
resolution fields and has 1280x720 internal resources, but the presented
frame can still be 1920x1080 after the game's output path. A direct
640x360 launch (`rx5700-360p-world-20261128`) wrote 640x360 and fitted
the title window to that size. It nevertheless still made observed
1280x720 render textures and 1920x1080 presented captures. It never
reached the 3,000-draw world marker: root-view denials passed 65,536 and
the GPU was removed at about 4.64 GB nonlocal use. The title translated
all 1,898 bound shader paths and the monitor saw zero socket endpoints.
This launch setting controls the requested display/window size but has
not been shown to reduce the title's internal render workload below 720p;
the 640x360 option should not be represented as a proven fps optimization.
The mechanism is explicit in `resource.rs`: `shrink_streamed_to` removes
existing top mips only from ordinary sampled two-dimensional textures
with `flags == 0`. It leaves render targets, depth resources, unordered
resources and single-mip textures at the dimensions the title requested.
The 360p run still had four 3840x2160 render-target textures (about 134 MB
together) and a 3840x2160 depth texture (about 43 MB), as well as observed
1280x720 targets. Lowering shader *source size* would not reduce per-frame
work; the useful controls are which passes execute, their output sizes,
dispatch counts and sample counts. Arbitrarily shrinking a GPU-written
target would make the title's viewports, integer texture coordinates and
compute dispatch dimensions disagree with the actual allocation.
At the last 360p census, GPU-written host textures occupied about 2,553 MB
(1,287 MB used by lists, 1,266 MB only visible through heap slots), while
textures sourced solely from title memory occupied about 268 MB. Thus
roughly 90% of the counted host-texture bytes were in the class that the
existing safe mip reducer cannot shrink. This explains why more aggressive
sampled-texture limits alone have not cleared nonlocal pressure. A useful
lower-render-resolution path must change the title's render-target sizes
and the associated shader viewport/dispatch constants together.
### 2026-11-28: why smaller window and streamed textures did not cut the main load

The offline `rx5700-360p-shape-probe-20261128` run requested 640x360 and
confirmed zero TCP/UDP endpoints. The title still set a **3840x2160 viewport
on frame 1**. Two 3840x2160 colour/UAV textures (67 MB) and one 3840x2160
depth texture (43 MB) were named by command lists and stamped within the last
30 frames. Four other 3840x2160 render targets (134 MB) were allocated later,
although this one-minute probe did not observe a recent stamp on those four.
The output setting changes the window and settings file, but the title still
selects this internal 4K path. The new five-second diagnostic groups the
largest live GPU-written textures by shape and reports list use and recent
stamps separately, so later world runs can show which allocations are active.

The FidelityFX context in both the 360p probe and the earlier world run says
`render=[1280,720]`, `output=[1920,1080]`. The world run's first dispatch
confirms 1280x720 scene colour/depth/motion inputs and a 1920x1080 output.
Therefore the 4K viewport is **not evidence that the whole scene renders at
4K**; the title uses multiple render/present paths, including a likely 4K
UI/overlay path. First-PresentX plane logging was added to identify each
actual presented plane rather than infer its role solely from dimensions.

`VICTORY_STREAM_MAX_EDGE` only drops existing leading mips on ordinary sampled
textures. It cannot reduce a GPU-written render target or a single-mip
texture without also changing the title's viewport, shaders' pixel-coordinate
constants, compute dispatches and dependent views/copies. The 360p world run
had about 2.55 GB of GPU-written host textures versus about 0.27 GB from
title memory in the counted reachable categories. Thus lowering the streamed
texture cap can save little of the present VRAM footprint. A smaller shader
binary alone also does not reduce the instructions executed per pixel; a
working low-cost shader variant must actually do less work at runtime.

The last world sample before this probe was about 2.2 fps, while the 30 fps
target permits 33.3 ms total per frame. Its CPU preparation and copy time,
and its GPU time, each already exceed that budget. Reducing internal render
size is useful, but it must be a coordinated title-facing change and will not
by itself solve CPU-side copy and preparation costs.

The optional `VICTORY_PRESENT_MAX=1280x720` host-only cap now scales the
actual PresentX planes (1920x1080 scene and 3840x2160 overlay) into a 720p
swap chain. The offline `rx5700-present720-world-20261128` run reached the
world at frame 6376, captured recognizable geometry at frame 6381, translated
all 1970 bound shader paths, and completed a 45-second world timing window
without a device reset or TCP/UDP endpoints. The median was 2.3 fps. This
matches the earlier uncapped range rather than proving a world speed gain;
the scenes and moments differ. The host cap saves final composite pixels and
swap-chain memory but leaves the title's 720p scene and 4K overlay rendering,
GPU-written textures, and large per-frame CPU copies intact. The launcher
offers the cap as a separate control and defaults it to 720p for new profiles.

### 2026-11-28: 4K target provenance and active-world accounting

The `rx5700-offline-20261128-082955Z` world census, around frames
8,014–8,074, distinguishes active targets from allocations left behind:
two 3840×2160 RGBA8 render/UAV targets (67 MB together) and one 4K depth
target (43 MB) were named by command lists and stamped within 30 frames.
Four 4K `format=87` render targets (134 MB) and another 4K depth target
(43 MB) were live but had no recent list stamp. Thus 110 MB of counted
4K targets were active; 177 MB were stale in that window. First PresentX
reported 1920×1080 and 3840×2160 planes, whereas FidelityFX reported
1280×720 render and 1920×1080 output. The active 4K path is consistent
with an overlay, but its exact producer has not been proven.

Static disassembly of the title executable with SHA-256
`e95020b2fa4a721db3df59fdecf75621730ac559d8835252f6c3b658d5a08993`
found a 4K target creation call
returning at title VA `0x14193e1ba`, with a resource description built from
the title object at `rsi+0x20`. Another 4K creation path reaches
`0x14193de10` via `0x14131a28d`. A separate constructor at
`0x145eb2614`/`0x145eb261b` reads width/height from globals
`0x14a9438f0`/`0x14a9438ec`; the function at `0x141f77fb7` selects
those dimensions or `0x14a9438cc`/`0x14a9438d0` for inverse-size math
according to flag `0x14a9438f4`. Static references alone do not prove
that changing that flag or pair resizes every active 4K target. They also
do not establish what sets the flag. A focused run can read the five
globals with `VICTORY_WATCH=0xa9438cc,0xa9438d0,0xa9438ec,0xa9438f0,0xa9438f4`
or use a debugger write watchpoint, outside an fps measurement. Only a
title-facing change that also updates viewports, shader coordinates,
dispatches and copies should be used to reduce GPU-written targets.

The bounded two-minute offline watch run
`rx5700-watch4k-20261128` requested 1280×720 and observed the main pair
`0xa9438cc/0xa9438d0` change from zero to 1920×1080. The secondary
pair `0xa9438ec/0xa9438f0` changed from zero to 3840×2160 and flag
`0xa9438f4` changed from zero to one before the first two 4K RGBA8
render/UAV resources were created. All five values stayed fixed for the
remainder of the run. The first PresentX planes were 1920×1080 and
3840×2160, the first viewport was 3840×2160, and the initial 4K color
and depth targets were stamped by lists. This establishes two distinct
title-side dimensions and an enabled secondary path in this configuration;
it does not identify the flag's setter or prove that toggling it would
resize every 4K resource. The run timed out before the world, had no
reported GPU removal, and its 22 network samples saw zero TCP/UDP
endpoints under verified inbound/outbound WFP blocks.

The saved `settings.xml` used by this watch run had both `uiResolution`
and `resolutionFullscreen` set to 1280×720; the launch log also reported
`graphics 1280x720 (flag) low`. Its `uiOverlay` value was still `true`.
The 4K secondary pair therefore is not explained by a stale 4K XML
resolution. The title also contains hard-coded 3840×2160 values at
`0x14231eb56`, `0x14314e78e` (both used in display-mode capability
checks), and `0x145b1337b` (used in a resource lookup). These show 4K
paths are built into the title, but do not identify which path set the
observed secondary pair.

The RIP-reference scanner now checks instructions whose displacement is
followed by an immediate value, including `mov [rip+disp32], imm8/imm32`.
A full scan of this title for `0x14a9438ec`, `0x14a9438f0`, and
`0x14a9438f4` found reads/comparisons and address-taking, but no direct
RIP-relative writes. The setter may use an indirect pointer, an aggregate
copy, or code outside the scanned executable; the scan cannot distinguish
these. A bounded A/B run could first change only the existing
`<uiOverlay value="true" />` setting to `false`, watch the five globals and
4K resource counts, and check that the menu stays usable. That is a
reversible settings test, not a known fix. If the flag still becomes one,
an early debugger write watchpoint on the relocated title image's
`+0xa9438ec`, `+0xa9438f0`, and `+0xa9438f4` will identify the actual
writer before an opt-in patch is designed.

### 2026-11-28: shader-used missing texture identities

The strict offline RX 5700 XT run `rx5700-offline-20261128-142321Z` reached
world frame 8,877 and completed a two-minute world window. Its exact loader
WFP inbound/outbound IPv4/IPv6 blocks were verified; 0 TCP and 0 UDP
endpoints were observed; the GPU was not reported removed. All 1,771 bound
shader paths translated. The 15 world FPS reports had a 1.7 fps median, so
this is not a performance acceptance result. The diagnostic gate failed:
7,554 shader-used Budget-null buffer events and 1,395 shader-used null
texture events were observed in 14 classified intervals. One final FPS row
was emitted after the last drop and descriptor report, so this run's exact
drop and texture coverage is marked incomplete. The strict harness now waits
for reports through its latest FPS timestamp, including the texture-use
report when coverage is enabled.

The new bounded identity census recorded at most 366 distinct missing
textures in any five-second interval, with no identity overflow in this run.
The missing texture views were almost entirely `host copy unavailable`, not
shader translation or format-compatibility failures. A sampled 1x1 format-41
texture at title address `0x9bda40000` was used 184 times in one interval;
four 16x8–16x13 format-10 table textures were each used 4–8 times in nearby
intervals. Their host copies were explicitly deferred while local plus
nonlocal DXGI use exceeded the 9,000 MB stream cap. During the first world
minute, the already allocated regular texture heaps often had no reusable
range, despite their untouched tails being reserved. Later evictions exposed
reusable ranges and admitted more sampled textures without making a new
heap. The next instrumented run separates unplaceable descriptions,
missing reusable ranges and driver rejection before changing the cap or
admission policy; an earlier 9,500 MB cap test removed the GPU.

The first follow-up with placement-failure counters
(`rx5700-offline-20261128-143702Z`) reached world frame 6,093 but ended
before the memory cap shut. It had zero missing texture views and no GPU
removal in its one classified world interval; it cannot test the intended
placement question. The title's previously identified watchdog wrote to
null at image RVA `0x23cf683` roughly 31 seconds after the first heavy
frame, and the process exited with `0xc0000005`. The proximate stalled
operation was not established. The title attempted a Breakpad upload to
`submit.backtrace.io`; the HLE network layer blocked it and the WFP-backed
monitor saw zero TCP and UDP endpoints. A repeat without full descriptor
coverage is needed to distinguish a repeatable rendering stall from
instrumentation overhead.

That repeat, `rx5700-offline-20261128-144255Z`, omitted the full view and
pipeline coverage census while keeping the same 720p, 9,000 MB combined cap,
85% local limit, 512-pixel streamed edge, and offline WFP rules. It reached
world frame 6,217 and completed a 90-second world window without GPU removal
or the watchdog abort. All 1,724 bound shader paths translated. Ten of ten
world intervals had exact drop and used-null classifications. The median was
1.55 fps, with 477 shader-used Budget-null buffer events and 61 valid
shader-used texture-null events. WFP verified the loader process's blocks and
60 network samples observed zero TCP and UDP endpoints. Different world
frames and the coverage switch mean the two runs do not isolate the cause of
the first abort or the difference in texture misses.

The placement-failure counters do isolate this run's closed-cap texture
failure mode: 1,078 reuse-only texture attempts, 897 admitted and 181
deferred. Across the small-alignment attempt and its ordinary-alignment
fallback, 189 placement attempts found no eligible range; zero descriptions
were unplaceable and zero placed-resource creations were rejected by D3D12.
At the busiest interval the regular pool held 467.6 of its 469.8 MB of
allocated heaps, with only 2.2 MB free across 14 ranges. This is a capacity
and fragmentation problem under the safe combined cap, not a shader-format
or placed-resource API failure. Raising the cap without reclaiming other
memory remains unsafe given the earlier device-removal result.

The subsequent eviction review found a bounded-reclaim edge case: after
selecting one old texture, `choose` stopped at the first candidate larger
than the remaining per-frame allowance. Smaller older candidates later in
the list were never considered, even when their ranges could be reused.
It now skips an oversized candidate and continues through the sorted list,
still honoring the same byte allowance and recent/held protections. Unit
tests cover both idle and budget-driven selection. Its effect on the RX
5700 XT's closed-cap missing views is not yet measured in a world run.
The existing heap census distinguishes reusable from pristine bytes; at
the busiest measured interval, all 2.20 MB free was reusable, but the
largest contiguous range was 1.31 MB. Diagnostics now also log the first
24 distinct failed host-texture shapes, including their requested D3D12
allocation size and alignment, so the next closed-cap run can match each
miss to the available contiguous ranges rather than infer it from totals.

### 2026-11-28: `uiOverlay` A/B for the secondary 4K path

The paired offline `rx5700-ui-overlay-ab-20261128-145503Z` test changed
only the saved `<uiOverlay>` value from `true` to `false` between two
two-minute 720p launches. Both arms saw the main title dimensions become
1920×1080, the secondary dimensions become 3840×2160, and the secondary
flag become one. Both first PresentX calls listed 1920×1080 and
3840×2160 planes, and each arm logged 17 title-created 4K textures in its
startup window. The loader binary hash matched across arms, the settings
were not rewritten during either arm, and the original settings bytes were
restored with a matching SHA-256. The WFP preflight passed and 22 network
samples per arm saw zero TCP and UDP endpoints. Neither arm reached the
world, so this is evidence about the 4K setup path, not an FPS comparison.
Disabling `uiOverlay` alone does not turn off the secondary 4K path. The
writer of the secondary dimensions/flag remains to be identified before a
safe title-facing resolution change is attempted.

### 2026-11-28: secondary 4K state write watchpoint

The offline CDB run `rx5700-offline-20261128-150644Z` installed a hardware
write watchpoint on the relocated prepared title image's
`+0xa9438f4` before title initialization. The first write was zero during
CRT initialization. The next write made the flag one while the title was
copying a 0x1e8-byte display-state structure with `VCRUNTIME140!memmove`.
The test harness now accepts a local `-DebuggerCommands` file alongside
`-TraceExit` for focused CDB watchpoints while retaining the exact-loader
offline firewall preflight and bounded run timer.
At that instant, the destination at `+0xa9438ec` held 3840, the next
dword held 2160, and the flag held one. The title call site returning at
`+0x19f4e15` copies from an object passed by `+0x5804379` into globals
starting at `+0xa9437b8`; the higher callers were `+0x43a490c` and
`+0x2240706`. This explains why a scan for direct RIP-relative writes to
the individual globals did not find the setter. The copy is an observer
of state assembled earlier in the object, so the original source of the
4K dimensions is still unproven. Changing the global flag after this copy
would leave the object's source state and other 4K render targets intact.
The debugger quit at the watched write, before gameplay. WFP preflight
passed and the monitor recorded no TCP or UDP endpoints. This run is not
an FPS or picture-quality test.

A second bounded offline watch run, `rx5700-offline-20261128-152039Z`,
watched the source display object's `+0x164` width field from immediately
after its constructor. It began at zero and later became 3840 during a
0x1e8-byte `memmove` from a stack temporary at title return RVA
`+0x43a48de`. The adjacent source fields then held 2160 and flag one.
The call path was `+0x43a47a8` via `+0x224110f` and `+0x81b253`.
Earlier snapshots of one initialization pass were zero after its
constructor, settings decode, and callback, so the later invocation
builds the 4K temporary. This narrows the producer to the temporary's
construction/decode/callback path; it does not prove which operation
selects 4K. The run quit at the source write, saw no network endpoints
under WFP, and is not a world or performance test.

The subsequent offline CDB watch `rx5700-offline-20261128-152510Z`
stopped at the first write of 3840 into the stack temporary's width.
The writer is title RVA `+0x1e0fb58`: a callback obtains the width from
`+0x5b943c0`, then the height from `+0x5b94380`. Static disassembly of
the cached relocated title image shows both wrappers call `+0x7b4e10`.
That function reads primary width/height from the display object at
`+0x204/+0x208`, applies a ratio selected by its `+0x144` field to a
separate output pair, and selects the alternate width/height at
`+0x1ec/+0x1f0` when its `+0x1e8` flag is set. The observed callback
returns 3840 and 2160 and sets that flag to one. This establishes that
the secondary dimensions are chosen in the title's display object before
the global copy; clamping the global fields alone would leave the source
object and its users inconsistent. It remains unproven which earlier
assignment sets the object's alternate pair and flag. A focused write
watchpoint on those three object fields, before the callback, is the next
safe diagnostic step. The debugger quit before gameplay; the WFP preflight
passed, and the run is not an FPS or visual-quality test.
