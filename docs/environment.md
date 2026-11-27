# `VICTORY_*` environment variables

An index of the environment variables read by the workspace. It records where
each variable is read and whether `README.md` documents it. It does not change
the README and does not guess the meaning of an undocumented variable.

- Snapshot: `main` `63bcca43c6a00a758510398fa43096ce7e4f7df0`.
- 226 read sites, 59 distinct variables, 28 documented in `README.md`,
  31 with no in-repo documentation.
- Undocumented variables are marked `TBD`, not filled in from their names.

## Variables

| Variable | Sites | In README | Notes |
| --- | --- | --- | --- |
| `VICTORY_AUDIO_CAPTURE` | 6 | yes | - |
| `VICTORY_AUDIO_CAPTURE_OF` | 1 | no | `TBD` |
| `VICTORY_BCPACK_CASES` | 3 | no | `TBD` |
| `VICTORY_BCPACK_TABLES` | 3 | no | `TBD` |
| `VICTORY_CACHE_DIR` | 3 | no | `TBD` |
| `VICTORY_CAPTURE` | 4 | yes | - |
| `VICTORY_CAPTURE_FRAMES` | 4 | yes | - |
| `VICTORY_CAPTURE_WORLD` | 4 | yes | - |
| `VICTORY_CONFIG` | 7 | no | `TBD` |
| `VICTORY_CONSOLE_DXC` | 5 | no | `TBD` |
| `VICTORY_COPIES_IN_VIDEO_MEMORY` | 2 | no | `TBD` |
| `VICTORY_COPY_MODE` | 2 | no | `TBD` |
| `VICTORY_CRASH_BREAKPOINTS` | 6 | yes | - |
| `VICTORY_CRASH_SAVE` | 3 | yes | - |
| `VICTORY_D3D12_DEBUG` | 6 | yes | - |
| `VICTORY_D3D12_DRED` | 8 | yes | - |
| `VICTORY_D3D12_GBV` | 3 | yes | - |
| `VICTORY_D3D12_WARP` | 5 | yes | - |
| `VICTORY_DATA_DIR` | 5 | no | `TBD` |
| `VICTORY_DIFF_ONLY` | 4 | no | `TBD` |
| `VICTORY_DUMP_TARGETS` | 4 | yes | - |
| `VICTORY_DUMP_WRITERS` | 4 | yes | - |
| `VICTORY_DXIL` | 5 | yes | - |
| `VICTORY_EMIT_DUMP` | 11 | no | `TBD` |
| `VICTORY_EXIT_CHILD` | 2 | no | `TBD`; inferred from its name to be part of an internal subprocess protocol (unconfirmed) |
| `VICTORY_FIDELITYFX_DIR` | 10 | yes | - |
| `VICTORY_GAME_DIR` | 5 | no | `TBD` |
| `VICTORY_GPU_TIMING` | 7 | yes | - |
| `VICTORY_HOST_LOCK_CHILD` | 1 | no | `TBD`; inferred from its name to be part of an internal subprocess protocol (unconfirmed) |
| `VICTORY_KEYS` | 6 | yes | - |
| `VICTORY_LOG` | 6 | yes | - |
| `VICTORY_PAIRS` | 2 | no | `TBD` |
| `VICTORY_PC_DXC` | 4 | no | `TBD` |
| `VICTORY_PEEK` | 3 | yes | - |
| `VICTORY_PIPELINE_LOG` | 4 | yes | - |
| `VICTORY_POKE` | 3 | yes | - |
| `VICTORY_PREBUILD_KEYS` | 4 | yes | - |
| `VICTORY_REF_PS` | 1 | no | `TBD` |
| `VICTORY_REF_VS` | 1 | no | `TBD` |
| `VICTORY_RELAUNCHED_FROM` | 1 | no | `TBD`; inferred from its name to be part of an internal subprocess protocol (unconfirmed) |
| `VICTORY_SAVE_CHILD` | 2 | no | `TBD`; inferred from its name to be part of an internal subprocess protocol (unconfirmed) |
| `VICTORY_SAVE_OP` | 2 | no | `TBD`; inferred from its name to be part of an internal subprocess protocol (unconfirmed) |
| `VICTORY_SAVE_STOP` | 3 | no | `TBD`; inferred from its name to be part of an internal subprocess protocol (unconfirmed) |
| `VICTORY_SEMANTICS_REPORT` | 1 | no | `TBD` |
| `VICTORY_SHADER_LOOP_LIMIT` | 5 | yes | - |
| `VICTORY_SHADER_SOURCE_HASH` | 3 | no | `TBD` |
| `VICTORY_SKIP_CPU_CHECK` | 1 | no | `TBD` |
| `VICTORY_SKIP_PIPELINES` | 3 | yes | - |
| `VICTORY_STACKS` | 5 | yes | - |
| `VICTORY_TEST_CRASH_LOG` | 1 | no | `TBD` |
| `VICTORY_TEST_TITLES` | 4 | no | `TBD` |
| `VICTORY_TRACE_SHADERS` | 4 | yes | - |
| `VICTORY_TRAP_LOG` | 3 | no | `TBD` |
| `VICTORY_WARP` | 6 | no | `TBD` |
| `VICTORY_WATCH` | 3 | yes | - |
| `VICTORY_WINDOW_MAX` | 4 | yes | - |
| `VICTORY_XMA_DUMP` | 3 | yes | - |
| `VICTORY_XMA_REFERENCE` | 2 | no | `TBD` |
| `VICTORY_XMA_STREAMS` | 3 | no | `TBD` |

A small group of names (`VICTORY_*_CHILD`, `VICTORY_RELAUNCHED_FROM`,
`VICTORY_SAVE_OP`, `VICTORY_SAVE_STOP`) is *inferred* to be an internal
subprocess protocol. That classification is unconfirmed and needs a maintainer
to confirm or correct it; it is not used to fill in any behaviour above.

## All read sites

One line per read site, grouped by file. Line numbers are from the snapshot above.

### `README.md`

- `README.md:37` - `VICTORY_D3D12_WARP`
- `README.md:54` - `VICTORY_FIDELITYFX_DIR`
- `README.md:118` - `VICTORY_FIDELITYFX_DIR`
- `README.md:127` - `VICTORY_WINDOW_MAX`
- `README.md:163` - `VICTORY_LOG`
- `README.md:175` - `VICTORY_DXIL`
- `README.md:196` - `VICTORY_LOG`
- `README.md:197` - `VICTORY_STACKS`
- `README.md:198` - `VICTORY_WATCH`
- `README.md:199` - `VICTORY_POKE`
- `README.md:200` - `VICTORY_KEYS`
- `README.md:201` - `VICTORY_CRASH_BREAKPOINTS`
- `README.md:202` - `VICTORY_CRASH_SAVE`
- `README.md:203` - `VICTORY_CAPTURE`
- `README.md:203` - `VICTORY_CAPTURE_FRAMES`
- `README.md:204` - `VICTORY_DUMP_TARGETS`
- `README.md:205` - `VICTORY_CAPTURE`
- `README.md:205` - `VICTORY_CAPTURE_WORLD`
- `README.md:205` - `VICTORY_DUMP_TARGETS`
- `README.md:206` - `VICTORY_DUMP_WRITERS`
- `README.md:207` - `VICTORY_PEEK`
- `README.md:208` - `VICTORY_D3D12_DEBUG`
- `README.md:209` - `VICTORY_D3D12_GBV`
- `README.md:210` - `VICTORY_D3D12_DRED`
- `README.md:211` - `VICTORY_SKIP_PIPELINES`
- `README.md:212` - `VICTORY_GPU_TIMING`
- `README.md:213` - `VICTORY_D3D12_DRED`
- `README.md:213` - `VICTORY_TRACE_SHADERS`
- `README.md:214` - `VICTORY_D3D12_WARP`
- `README.md:215` - `VICTORY_PIPELINE_LOG`
- `README.md:216` - `VICTORY_PREBUILD_KEYS`
- `README.md:217` - `VICTORY_SHADER_LOOP_LIMIT`
- `README.md:218` - `VICTORY_AUDIO_CAPTURE`
- `README.md:219` - `VICTORY_XMA_DUMP`
- `README.md:225` - `VICTORY_CRASH_BREAKPOINTS`
- `README.md:236` - `VICTORY_D3D12_WARP`

### `apps/victory/src/main.rs`

- `apps/victory/src/main.rs:82` - `VICTORY_DXIL`
- `apps/victory/src/main.rs:147` - `VICTORY_AUDIO_CAPTURE`

### `apps/victory/src/prebuild.rs`

- `apps/victory/src/prebuild.rs:175` - `VICTORY_PREBUILD_KEYS`
- `apps/victory/src/prebuild.rs:177` - `VICTORY_PIPELINE_LOG`
- `apps/victory/src/prebuild.rs:179` - `VICTORY_PREBUILD_KEYS`

### `apps/victory/src/run.rs`

- `apps/victory/src/run.rs:8` - `VICTORY_SKIP_CPU_CHECK`
- `apps/victory/src/run.rs:233` - `VICTORY_CRASH_SAVE`
- `apps/victory/src/run.rs:236` - `VICTORY_CRASH_SAVE`
- `apps/victory/src/run.rs:252` - `VICTORY_WATCH`
- `apps/victory/src/run.rs:256` - `VICTORY_WATCH`
- `apps/victory/src/run.rs:280` - `VICTORY_POKE`
- `apps/victory/src/run.rs:287` - `VICTORY_POKE`

### `apps/victory/src/translate.rs`

- `apps/victory/src/translate.rs:148` - `VICTORY_PAIRS`
- `apps/victory/src/translate.rs:163` - `VICTORY_EMIT_DUMP`
- `apps/victory/src/translate.rs:179` - `VICTORY_PAIRS`
- `apps/victory/src/translate.rs:334` - `VICTORY_PREBUILD_KEYS`
- `apps/victory/src/translate.rs:606` - `VICTORY_EMIT_DUMP`
- `apps/victory/src/translate.rs:608` - `VICTORY_EMIT_DUMP`
- `apps/victory/src/translate.rs:655` - `VICTORY_EMIT_DUMP`
- `apps/victory/src/translate.rs:861` - `VICTORY_EMIT_DUMP`
- `apps/victory/src/translate.rs:865` - `VICTORY_EMIT_DUMP`

### `crates/victory-core/Cargo.toml`

- `crates/victory-core/Cargo.toml:21` - `VICTORY_LOG`

### `crates/victory-core/src/config.rs`

- `crates/victory-core/src/config.rs:4` - `VICTORY_CONFIG`
- `crates/victory-core/src/config.rs:63` - `VICTORY_CONFIG`
- `crates/victory-core/src/config.rs:86` - `VICTORY_TEST_TITLES`
- `crates/victory-core/src/config.rs:92` - `VICTORY_CACHE_DIR`
- `crates/victory-core/src/config.rs:95` - `VICTORY_DATA_DIR`
- `crates/victory-core/src/config.rs:99` - `VICTORY_GAME_DIR`
- `crates/victory-core/src/config.rs:103` - `VICTORY_FIDELITYFX_DIR`
- `crates/victory-core/src/config.rs:111` - `VICTORY_GAME_DIR`
- `crates/victory-core/src/config.rs:128` - `VICTORY_CONFIG`
- `crates/victory-core/src/config.rs:130` - `VICTORY_CONFIG`
- `crates/victory-core/src/config.rs:148` - `VICTORY_CACHE_DIR`
- `crates/victory-core/src/config.rs:148` - `VICTORY_DATA_DIR`
- `crates/victory-core/src/config.rs:164` - `VICTORY_GAME_DIR`
- `crates/victory-core/src/config.rs:176` - `VICTORY_GAME_DIR`
- `crates/victory-core/src/config.rs:187` - `VICTORY_TEST_TITLES`
- `crates/victory-core/src/config.rs:196` - `VICTORY_TEST_TITLES`
- `crates/victory-core/src/config.rs:206` - `VICTORY_CONFIG`
- `crates/victory-core/src/config.rs:208` - `VICTORY_CONFIG`

### `crates/victory-core/src/logging.rs`

- `crates/victory-core/src/logging.rs:3` - `VICTORY_LOG`
- `crates/victory-core/src/logging.rs:4` - `VICTORY_LOG`
- `crates/victory-core/src/logging.rs:21` - `VICTORY_LOG`

### `crates/victory-formats/src/bcpack.rs`

- `crates/victory-formats/src/bcpack.rs:1587` - `VICTORY_BCPACK_CASES`
- `crates/victory-formats/src/bcpack.rs:1587` - `VICTORY_BCPACK_TABLES`
- `crates/victory-formats/src/bcpack.rs:1592` - `VICTORY_BCPACK_TABLES`
- `crates/victory-formats/src/bcpack.rs:1592` - `VICTORY_BCPACK_TABLES`
- `crates/victory-formats/src/bcpack.rs:1593` - `VICTORY_BCPACK_CASES`
- `crates/victory-formats/src/bcpack.rs:1593` - `VICTORY_BCPACK_CASES`

### `crates/victory-formats/src/xma.rs`

- `crates/victory-formats/src/xma.rs:1736` - `VICTORY_XMA_STREAMS`
- `crates/victory-formats/src/xma.rs:1743` - `VICTORY_XMA_STREAMS`
- `crates/victory-formats/src/xma.rs:1777` - `VICTORY_XMA_STREAMS`
- `crates/victory-formats/src/xma.rs:1782` - `VICTORY_XMA_REFERENCE`
- `crates/victory-formats/src/xma.rs:1788` - `VICTORY_XMA_REFERENCE`

### `crates/victory-gpu/src/capture.rs`

- `crates/victory-gpu/src/capture.rs:2` - `VICTORY_CAPTURE`
- `crates/victory-gpu/src/capture.rs:15` - `VICTORY_CAPTURE`
- `crates/victory-gpu/src/capture.rs:19` - `VICTORY_CAPTURE_FRAMES`
- `crates/victory-gpu/src/capture.rs:20` - `VICTORY_CAPTURE_WORLD`
- `crates/victory-gpu/src/capture.rs:25` - `VICTORY_CAPTURE_FRAMES`
- `crates/victory-gpu/src/capture.rs:40` - `VICTORY_CAPTURE_WORLD`
- `crates/victory-gpu/src/capture.rs:54` - `VICTORY_CAPTURE_WORLD`
- `crates/victory-gpu/src/capture.rs:227` - `VICTORY_DUMP_TARGETS`
- `crates/victory-gpu/src/capture.rs:228` - `VICTORY_CAPTURE_FRAMES`
- `crates/victory-gpu/src/capture.rs:236` - `VICTORY_DUMP_WRITERS`
- `crates/victory-gpu/src/capture.rs:257` - `VICTORY_DUMP_TARGETS`
- `crates/victory-gpu/src/capture.rs:260` - `VICTORY_DUMP_WRITERS`
- `crates/victory-gpu/src/capture.rs:282` - `VICTORY_DUMP_WRITERS`
- `crates/victory-gpu/src/capture.rs:394` - `VICTORY_PEEK`
- `crates/victory-gpu/src/capture.rs:400` - `VICTORY_PEEK`

### `crates/victory-gpu/src/copystat.rs`

- `crates/victory-gpu/src/copystat.rs:3` - `VICTORY_COPY_MODE`
- `crates/victory-gpu/src/copystat.rs:118` - `VICTORY_COPY_MODE`

### `crates/victory-gpu/src/host.rs`

- `crates/victory-gpu/src/host.rs:61` - `VICTORY_D3D12_DEBUG`
- `crates/victory-gpu/src/host.rs:106` - `VICTORY_D3D12_DEBUG`
- `crates/victory-gpu/src/host.rs:118` - `VICTORY_D3D12_DEBUG`
- `crates/victory-gpu/src/host.rs:148` - `VICTORY_D3D12_DEBUG`
- `crates/victory-gpu/src/host.rs:155` - `VICTORY_D3D12_GBV`
- `crates/victory-gpu/src/host.rs:159` - `VICTORY_D3D12_GBV`
- `crates/victory-gpu/src/host.rs:188` - `VICTORY_D3D12_WARP`
- `crates/victory-gpu/src/host.rs:308` - `VICTORY_GPU_TIMING`
- `crates/victory-gpu/src/host.rs:836` - `VICTORY_GPU_TIMING`
- `crates/victory-gpu/src/host.rs:867` - `VICTORY_D3D12_DRED`
- `crates/victory-gpu/src/host.rs:871` - `VICTORY_D3D12_DRED`
- `crates/victory-gpu/src/host.rs:875` - `VICTORY_GPU_TIMING`
- `crates/victory-gpu/src/host.rs:876` - `VICTORY_D3D12_DRED`
- `crates/victory-gpu/src/host.rs:880` - `VICTORY_GPU_TIMING`
- `crates/victory-gpu/src/host.rs:1219` - `VICTORY_D3D12_DRED`
- `crates/victory-gpu/src/host.rs:1255` - `VICTORY_GPU_TIMING`
- `crates/victory-gpu/src/host.rs:1449` - `VICTORY_D3D12_DRED`
- `crates/victory-gpu/src/host.rs:3588` - `VICTORY_COPIES_IN_VIDEO_MEMORY`
- `crates/victory-gpu/src/host.rs:3597` - `VICTORY_COPIES_IN_VIDEO_MEMORY`
- `crates/victory-gpu/src/host.rs:3712` - `VICTORY_TRACE_SHADERS`
- `crates/victory-gpu/src/host.rs:3726` - `VICTORY_TRACE_SHADERS`
- `crates/victory-gpu/src/host.rs:3732` - `VICTORY_TRACE_SHADERS`
- `crates/victory-gpu/src/host.rs:3743` - `VICTORY_GPU_TIMING`
- `crates/victory-gpu/src/host.rs:3761` - `VICTORY_D3D12_DRED`

### `crates/victory-gpu/src/pipeline.rs`

- `crates/victory-gpu/src/pipeline.rs:283` - `VICTORY_SKIP_PIPELINES`
- `crates/victory-gpu/src/pipeline.rs:295` - `VICTORY_SKIP_PIPELINES`

### `crates/victory-gpu/src/pipeline/make.rs`

- `crates/victory-gpu/src/pipeline/make.rs:153` - `VICTORY_PIPELINE_LOG`
- `crates/victory-gpu/src/pipeline/make.rs:164` - `VICTORY_PIPELINE_LOG`

### `crates/victory-gpu/src/shader_cache.rs`

- `crates/victory-gpu/src/shader_cache.rs:64` - `VICTORY_SHADER_LOOP_LIMIT`

### `crates/victory-gpu/src/switch.rs`

- `crates/victory-gpu/src/switch.rs:1` - `VICTORY_D3D12_DEBUG`
- `crates/victory-gpu/src/switch.rs:7` - `VICTORY_D3D12_WARP`

### `crates/victory-hle/src/host.rs`

- `crates/victory-hle/src/host.rs:158` - `VICTORY_HOST_LOCK_CHILD`

### `crates/victory-hle/src/modules/audio.rs`

- `crates/victory-hle/src/modules/audio.rs:681` - `VICTORY_XMA_DUMP`
- `crates/victory-hle/src/modules/audio.rs:687` - `VICTORY_XMA_DUMP`

### `crates/victory-hle/src/modules/audio/capture.rs`

- `crates/victory-hle/src/modules/audio/capture.rs:1` - `VICTORY_AUDIO_CAPTURE`
- `crates/victory-hle/src/modules/audio/capture.rs:49` - `VICTORY_AUDIO_CAPTURE_OF`
- `crates/victory-hle/src/modules/audio/capture.rs:51` - `VICTORY_AUDIO_CAPTURE`
- `crates/victory-hle/src/modules/audio/capture.rs:69` - `VICTORY_AUDIO_CAPTURE`
- `crates/victory-hle/src/modules/audio/capture.rs:85` - `VICTORY_AUDIO_CAPTURE`

### `crates/victory-hle/src/modules/fidelityfx.rs`

- `crates/victory-hle/src/modules/fidelityfx.rs:3` - `VICTORY_FIDELITYFX_DIR`
- `crates/victory-hle/src/modules/fidelityfx.rs:150` - `VICTORY_FIDELITYFX_DIR`
- `crates/victory-hle/src/modules/fidelityfx.rs:156` - `VICTORY_FIDELITYFX_DIR`

### `crates/victory-hle/src/modules/gameinput.rs`

- `crates/victory-hle/src/modules/gameinput.rs:114` - `VICTORY_KEYS`
- `crates/victory-hle/src/modules/gameinput.rs:145` - `VICTORY_KEYS`
- `crates/victory-hle/src/modules/gameinput.rs:159` - `VICTORY_KEYS`
- `crates/victory-hle/src/modules/gameinput.rs:1273` - `VICTORY_KEYS`
- `crates/victory-hle/src/modules/gameinput.rs:1280` - `VICTORY_KEYS`

### `crates/victory-hle/src/modules/lifecycle.rs`

- `crates/victory-hle/src/modules/lifecycle.rs:198` - `VICTORY_WINDOW_MAX`
- `crates/victory-hle/src/modules/lifecycle.rs:226` - `VICTORY_WINDOW_MAX`
- `crates/victory-hle/src/modules/lifecycle.rs:231` - `VICTORY_WINDOW_MAX`
- `crates/victory-hle/src/modules/lifecycle.rs:531` - `VICTORY_RELAUNCHED_FROM`

### `crates/victory-hle/src/modules/xgamesave.rs`

- `crates/victory-hle/src/modules/xgamesave.rs:996` - `VICTORY_SAVE_STOP`
- `crates/victory-hle/src/modules/xgamesave.rs:998` - `VICTORY_SAVE_STOP`
- `crates/victory-hle/src/modules/xgamesave.rs:1050` - `VICTORY_SAVE_CHILD`
- `crates/victory-hle/src/modules/xgamesave.rs:1050` - `VICTORY_SAVE_OP`
- `crates/victory-hle/src/modules/xgamesave.rs:1078` - `VICTORY_SAVE_CHILD`
- `crates/victory-hle/src/modules/xgamesave.rs:1079` - `VICTORY_SAVE_OP`
- `crates/victory-hle/src/modules/xgamesave.rs:1080` - `VICTORY_SAVE_STOP`

### `crates/victory-hle/tests/fidelityfx.rs`

- `crates/victory-hle/tests/fidelityfx.rs:4` - `VICTORY_FIDELITYFX_DIR`
- `crates/victory-hle/tests/fidelityfx.rs:160` - `VICTORY_FIDELITYFX_DIR`
- `crates/victory-hle/tests/fidelityfx.rs:161` - `VICTORY_FIDELITYFX_DIR`

### `crates/victory-loader/src/crash.rs`

- `crates/victory-loader/src/crash.rs:12` - `VICTORY_CRASH_BREAKPOINTS`
- `crates/victory-loader/src/crash.rs:189` - `VICTORY_CRASH_BREAKPOINTS`

### `crates/victory-loader/src/stacks.rs`

- `crates/victory-loader/src/stacks.rs:1` - `VICTORY_STACKS`
- `crates/victory-loader/src/stacks.rs:24` - `VICTORY_STACKS`
- `crates/victory-loader/src/stacks.rs:27` - `VICTORY_STACKS`
- `crates/victory-loader/src/stacks.rs:40` - `VICTORY_STACKS`

### `crates/victory-loader/src/threads.rs`

- `crates/victory-loader/src/threads.rs:407` - `VICTORY_EXIT_CHILD`
- `crates/victory-loader/src/threads.rs:430` - `VICTORY_EXIT_CHILD`

### `crates/victory-loader/src/thunks.rs`

- `crates/victory-loader/src/thunks.rs:157` - `VICTORY_TRAP_LOG`
- `crates/victory-loader/src/thunks.rs:160` - `VICTORY_TRAP_LOG`
- `crates/victory-loader/src/thunks.rs:179` - `VICTORY_TRAP_LOG`

### `crates/victory-loader/tests/crash_log.rs`

- `crates/victory-loader/tests/crash_log.rs:12` - `VICTORY_TEST_CRASH_LOG`
- `crates/victory-loader/tests/crash_log.rs:46` - `VICTORY_CRASH_BREAKPOINTS`
- `crates/victory-loader/tests/crash_log.rs:48` - `VICTORY_CRASH_BREAKPOINTS`

### `crates/victory-shader/build.rs`

- `crates/victory-shader/build.rs:1` - `VICTORY_SHADER_SOURCE_HASH`
- `crates/victory-shader/build.rs:37` - `VICTORY_SHADER_SOURCE_HASH`

### `crates/victory-shader/src/emit/diff_tests.rs`

- `crates/victory-shader/src/emit/diff_tests.rs:5` - `VICTORY_CONSOLE_DXC`
- `crates/victory-shader/src/emit/diff_tests.rs:6` - `VICTORY_PC_DXC`
- `crates/victory-shader/src/emit/diff_tests.rs:7` - `VICTORY_WARP`
- `crates/victory-shader/src/emit/diff_tests.rs:486` - `VICTORY_WARP`
- `crates/victory-shader/src/emit/diff_tests.rs:752` - `VICTORY_CONSOLE_DXC`
- `crates/victory-shader/src/emit/diff_tests.rs:753` - `VICTORY_PC_DXC`
- `crates/victory-shader/src/emit/diff_tests.rs:779` - `VICTORY_CONSOLE_DXC`
- `crates/victory-shader/src/emit/diff_tests.rs:881` - `VICTORY_DIFF_ONLY`
- `crates/victory-shader/src/emit/diff_tests.rs:896` - `VICTORY_EMIT_DUMP`
- `crates/victory-shader/src/emit/diff_tests.rs:1001` - `VICTORY_DIFF_ONLY`
- `crates/victory-shader/src/emit/diff_tests.rs:1015` - `VICTORY_EMIT_DUMP`
- `crates/victory-shader/src/emit/diff_tests.rs:1444` - `VICTORY_DIFF_ONLY`
- `crates/victory-shader/src/emit/diff_tests.rs:1445` - `VICTORY_WARP`
- `crates/victory-shader/src/emit/diff_tests.rs:1452` - `VICTORY_EMIT_DUMP`
- `crates/victory-shader/src/emit/diff_tests.rs:1463` - `VICTORY_EMIT_DUMP`
- `crates/victory-shader/src/emit/diff_tests.rs:1476` - `VICTORY_WARP`

### `crates/victory-shader/src/emit/flow_tests.rs`

- `crates/victory-shader/src/emit/flow_tests.rs:11` - `VICTORY_CONSOLE_DXC`
- `crates/victory-shader/src/emit/flow_tests.rs:23` - `VICTORY_CONSOLE_DXC`
- `crates/victory-shader/src/emit/flow_tests.rs:24` - `VICTORY_PC_DXC`
- `crates/victory-shader/src/emit/flow_tests.rs:384` - `VICTORY_DIFF_ONLY`

### `crates/victory-shader/src/emit/host.rs`

- `crates/victory-shader/src/emit/host.rs:167` - `VICTORY_WARP`
- `crates/victory-shader/src/emit/host.rs:171` - `VICTORY_WARP`

### `crates/victory-shader/src/emit/mod.rs`

- `crates/victory-shader/src/emit/mod.rs:250` - `VICTORY_SHADER_LOOP_LIMIT`
- `crates/victory-shader/src/emit/mod.rs:260` - `VICTORY_SHADER_LOOP_LIMIT`
- `crates/victory-shader/src/emit/mod.rs:267` - `VICTORY_SHADER_LOOP_LIMIT`
- `crates/victory-shader/src/emit/mod.rs:1092` - `VICTORY_REF_PS`
- `crates/victory-shader/src/emit/mod.rs:1093` - `VICTORY_REF_VS`
- `crates/victory-shader/src/emit/mod.rs:1626` - `VICTORY_PC_DXC`

### `crates/victory-shader/src/emit/semantics_tests.rs`

- `crates/victory-shader/src/emit/semantics_tests.rs:556` - `VICTORY_SEMANTICS_REPORT`

### `crates/victory-shader/src/emit/validator.rs`

- `crates/victory-shader/src/emit/validator.rs:5` - `VICTORY_DXIL`
- `crates/victory-shader/src/emit/validator.rs:122` - `VICTORY_DXIL`

### `crates/victory-shader/src/emit/valu_tests.rs`

- `crates/victory-shader/src/emit/valu_tests.rs:380` - `VICTORY_EMIT_DUMP`
- `crates/victory-shader/src/emit/valu_tests.rs:387` - `VICTORY_DXIL`

### `crates/victory-shader/src/lib.rs`

- `crates/victory-shader/src/lib.rs:30` - `VICTORY_SHADER_SOURCE_HASH`

### `docs/architecture.md`

- `docs/architecture.md:72` - `VICTORY_DATA_DIR`
- `docs/architecture.md:78` - `VICTORY_DATA_DIR`

### `victory.example.toml`

- `victory.example.toml:4` - `VICTORY_CONFIG`
- `victory.example.toml:7` - `VICTORY_GAME_DIR`
- `victory.example.toml:10` - `VICTORY_CACHE_DIR`
- `victory.example.toml:14` - `VICTORY_DATA_DIR`
- `victory.example.toml:19` - `VICTORY_FIDELITYFX_DIR`
- `victory.example.toml:23` - `VICTORY_TEST_TITLES`

## Scope

This file is an index only. Several open pull requests already edit the README's
Debugging table and Getting started section to describe individual variables;
this index deliberately does not compete with them.
