# Representative heavy-frame graphics trace

Run this from the repository root to capture the first sky writer after a
high-draw scene appears:

```powershell
pwsh -NoProfile -ExecutionPolicy Bypass -File scripts/test-rx5700-offline.ps1 `
  -Title 'C:\path\to\title.exe' -FidelityFX 'C:\path\to\FidelityFX\signedbin' `
  -Minutes 9 -Resolution 720p -PresentMax 1280x720 `
  -StreamMaxEdge 512 -BoundPriorityMips -RootSystemMB 4750 `
  -RootReuseOnly -RootPressureTrimMB 768 -FfxExposure 16 `
  -Ambient 1 -AmbientSky off -AmbientShade '0,0.6,0.1,0.45' `
  -WorldSeconds 60 -WorldMinDraws 5000 `
  -WorldCaptureOffsets '0,1,2,3,4,5,30,60' `
  -Capture348EOutput -Capture348EInputs -Capture348EOffset 1 -Coverage
```

The harness builds offline, verifies inbound and outbound blocks for the exact
loader executable, and sends Enter during startup. It monitors for one minute
after the first presented frame with at least 5,000 host draws. The output directory
under `target/test-runs` contains `frames/frame-*.png`, the immediate `348e-pre`
and `348e-post` format-10 scene-color readbacks, the three small bound sky and
depth input images, `stderr.log`, `monitor.csv`, `coverage.json`, and
`result.txt`.

`-Capture348EOffset 1` starts looking for shader 348e on the first frame **after** the
draw-count marker. The marker is recognized when its frame is presented, so
that frame's earlier dispatch cannot be captured by a marker-relative probe.
Offsets 2 through 120 start looking on a later frame. The first matching 348e
dispatch at or after the requested offset is captured once; the log records
its actual frame and offset. If none runs before the harness stops, no pre/post
snapshot is written and `result.txt` reports
`lighting_snapshot_captured=False`. Without `-Capture348EOffset`, the existing
first-348e-dispatch behavior is retained.

Compare `348e-pre` and `348e-post` from the same frame to bracket the first
green or black region. The pre image is the prior scene-color stage immediately
before this sky pass. A defect already present in `pre` comes from an earlier
writer or input; a defect first appearing in `post` implicates this sky pass
or its inputs. If the pre image is already corrupt, record the target resource
and its preceding writer sequence for a narrower follow-up capture. The saved
HDR preview PNGs are scaled separately to their own
99th-percentile brightness, so compare geometry and seams in the images and
use the logged raw `target values` for brightness. The high draw threshold
reduces intro false positives but does not prove gameplay has started. At a
slow frame rate later requested presented frames may be absent; capture
readbacks add overhead, so this run is not a performance benchmark.
