# Reconstruct BC5 records on D3D12

This Shader Model 6.6 compute example implements the reconstruction stage of
BCPack BC5. It consumes the symbols produced by entropy decoding. The normal
CPU decoder remains the entry point for compressed streams.

Bind a raw SRV at descriptor heap index 0 and a separate raw UAV at index 1
with the root signature flag `CBV_SRV_UAV_HEAP_DIRECTLY_INDEXED`. Dispatch one group per invocation.
One thread walks the stream in record order because predictors can refer to
earlier output. All integers are little-endian.

| Input offset in bytes | Contents |
| --- | --- |
| 0 | Block count, one unsigned 32-bit word |
| 4 | 64-byte rank table |
| 68 | 255 packed 16-byte BC5 dictionary entries |
| 4148 | 24 decoded symbols per block |

Each record starts with four paired reference symbols followed by two endpoint
deltas per channel. The remaining sixteen symbols are eight pixel pairs for
channel 0, then eight for channel 1. The low six bits select a rank-table
entry. Bits 0-2 and 4-6 of that entry update the even and odd pixels.
At block i, distance zero selects logical zero (packed endpoints 0x80);
distances 1 through i read earlier output, and distance i + k + 1 selects
dictionary entry k.

Validate input and output lengths before dispatch. Allocate 16 output bytes
per block. Limit each dispatch to a stream size appropriate for the device.
The example has been tested on synthetic inputs and is intended for checking
format implementations.

On Windows, put a current Windows SDK x64 `dxc.exe` on PATH. Run from the
repository root with an SM 6.6 adapter and WARP available:

    cargo test --locked -p victory-shader --features host --lib bc5_record_shader -- --ignored --nocapture

To compile the shader on its own:

    dxc -T cs_6_6 -E main -O3 -Fo "$env:TEMP/bc5.dxil" crates/victory-formats/examples/bc5/bc5-records.hlsl
