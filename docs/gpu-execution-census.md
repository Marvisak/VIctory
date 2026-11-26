# GPU execution census

Build with `--features diagnostics` and run with `VICTORY_GPU_CENSUS=1` and `VICTORY_LOG=info`.
`victory_gpu::census` reports pipeline and shader hashes, call and timestamp counts, work sizes, and refusal counts.
GPU spans include median, maximum, and accumulated milliseconds. CPU pipeline build time is reported separately.
Counted indirect calls show argument limits. GPU spans can overlap. Measure frame time separately.
