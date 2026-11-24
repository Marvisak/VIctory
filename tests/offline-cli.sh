#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)

python3 -E - "${1:?Path to victory binary required}" "$repo_root" <<'PY'
import hashlib
import os
import struct
import subprocess
import sys
import tempfile
import zlib
from pathlib import Path

binary = Path(sys.argv[1]).resolve()
repo = Path(sys.argv[2]).resolve()

# Keep generated artifacts outside the checkout.
temp_dir = Path(tempfile.gettempdir()).resolve()

if temp_dir == repo or repo in temp_dir.parents:
    if os.name == "posix":
        temp_dir = Path("/tmp").resolve()

if not temp_dir.is_dir() or temp_dir == repo or repo in temp_dir.parents:
    raise SystemExit("Set TMPDIR outside the checkout")

def fixture(words, lds=0, threads=1):
    payload = bytearray(0x68)
    struct.pack_into("<Q", payload, 0, 6)

    for slot in (8, 16, 24, 32, 40, 48, 72, 96):
        struct.pack_into(
            "<Q", payload, slot,
            0xDEADBEEFDEADBEEF
        )

    record = len(payload)
    struct.pack_into("<Q", payload, 0x60, record)

    code = struct.pack("<" + "I" * len(words), *words)

    stage = bytearray(0x34)
    struct.pack_into("<I", stage, 0x24, lds)
    struct.pack_into(
        "<III", stage, 0x28, threads, 1, 1
    )

    header = bytearray(0x54)

    struct.pack_into(
        "<QQ", header, 0,
        record + len(header) + len(stage),
        len(code)
    )

    struct.pack_into("<I", header, 0x14, 1 << 5)
    struct.pack_into("<I", header, 0x50, 28)

    payload += header + stage + code

    fssm = (
        b"FSSM"
        + struct.pack("<IQ", 0x101, len(payload))
        + payload
    )

    compressed = zlib.compress(fssm)

    xbch = (
        b"XBCH"
        + struct.pack(
            "<IQQ", 3, len(compressed), len(fssm)
        )
        + compressed
    )

    dxil = (
        bytes(0x18)
        + b"XDAT"
        + struct.pack("<II", 1, 0x24)
        + struct.pack("<III", 0x0D, 0x30, len(xbch))
        + xbch
    )

    return (
        b"DXBC"
        + bytes(16)
        + struct.pack(
            "<HHIII", 1, 0, 44 + len(dxil), 1, 36
        )
        + b"DXIL"
        + struct.pack("<I", len(dxil))
        + dxil
    )

def verify_dxil(path):
    data = path.read_bytes()

    assert data[:4] == b"DXBC", path

    total, count = struct.unpack_from("<II", data, 24)

    assert total == len(data)
    assert 32 + count * 4 <= total

    parts = {}
    ranges = []

    for i in range(count):
        offset = struct.unpack_from(
            "<I", data, 32 + 4 * i
        )[0]

        assert offset >= 32 + count * 4
        assert offset + 8 <= total

        kind, size = struct.unpack_from(
            "<4sI", data, offset
        )

        assert offset + 8 + size <= total

        assert kind not in parts, (path, kind)
        ranges.append((offset, offset + 8 + size))

        parts[kind] = data[
            offset + 8:offset + 8 + size
        ]

    ranges.sort()

    for (_, end), (start, _) in zip(ranges, ranges[1:]):
        assert end <= start, ("overlapping DXBC parts", path)

    assert b"DXIL" in parts
    assert b"SFI0" in parts

    program = parts[b"DXIL"]
    assert len(program) >= 24

    version, dwords = struct.unpack_from("<II", program)
    assert version >> 16 == 5, ("expected compute shader", path)

    size = dwords * 4
    assert 24 <= size <= len(program)
    assert program[8:12] == b"DXIL"

    offset, length = struct.unpack_from("<II", program, 16)
    start = 8 + offset

    assert start >= 24
    assert length >= 4
    assert start + length <= size

    assert program[start:start + 4] == bytes([
        0x42, 0x43, 0xC0, 0xDE
    ]), ("invalid LLVM bitcode magic", path)

    print(
        "Valid container structure:",
        path.name, len(data), "bytes"
    )

with tempfile.TemporaryDirectory(
    prefix="victory-offline-", dir=temp_dir
) as tmp:
    root = Path(tmp)

    inputs = root / "inputs"
    outputs = root / "outputs"

    inputs.mkdir()
    outputs.mkdir()

    empty = fixture([0xBF810000])

    perm = fixture([
        0x7E0202FF,
        0x817F34AB,
        0x7E0402FF,
        0xFE0156CD,
        0xD7440003,
        257 | (258 << 9) | (256 << 18),
        0x34080082,
        0xD8340000,
        0x00000304,
        0xBF810000,
    ], lds=1 << 15, threads=64)

    (inputs / "empty.dxbc").write_bytes(empty)
    (inputs / "duplicate.dxbc").write_bytes(empty)
    (inputs / "perm.dxbc").write_bytes(perm)
    (inputs / "noise.txt").write_text(
        "synthetic unrelated input"
    )

    env = os.environ.copy()
    env.update(TMPDIR=str(root), TMP=str(root), TEMP=str(root))

    env.pop("VICTORY_PAIRS", None)
    env.pop("VICTORY_EMIT_DUMP", None)

    def run(*args, success=True):
        result = subprocess.run(
            [str(binary), *map(str, args)],
            cwd=root,
            env=env,
            capture_output=True,
            text=True,
            timeout=60,
        )

        print(result.stdout)
        print(result.stderr)

        assert (result.returncode == 0) == success

        return result.stdout + result.stderr

    info = run("shader-info", inputs)

    assert "3 (in 3 files)" in info

    env["VICTORY_EMIT_DUMP"] = str(outputs)

    translated = run(
        "translate", inputs,
        "--jobs", "2", "--list"
    )

    assert "3 (1 duplicates skipped)" in translated
    assert "2 translated of 2" in translated
    assert "2 emitted of 2 compute programs" in translated

    emitted = list(outputs.glob("*.dxil"))

    assert len(emitted) == 2, emitted

    for path in emitted:
        verify_dxil(path)

    # Multiple containers in ONE file are required to exercise
    # translate's per-file worker pool. Duplicate labels can vary
    # with scheduling, so compare a multiset of content hashes.
    parallel_input = root / "parallel.dxbc"

    parallel_input.write_bytes(
        empty + bytes(16)
        + perm + bytes(16)
        + empty
    )

    baseline = None
    runs = 0

    for jobs in (1, 2, 4, 64):
        for repeat in range(3):
            dest = root / f"jobs-{jobs}-{repeat}"
            dest.mkdir()

            env["VICTORY_EMIT_DUMP"] = str(dest)

            result = run(
                "translate", parallel_input,
                "--jobs", jobs, "--list"
            )

            assert "3 (1 duplicates skipped)" in result
            assert "2 translated of 2" in result
            assert "2 emitted of 2 compute programs" in result

            files = sorted(dest.glob("*.dxil"))
            assert len(files) == 2, (jobs, repeat, files)

            for path in files:
                verify_dxil(path)

            hashes = sorted(
                hashlib.sha256(path.read_bytes()).hexdigest()
                for path in files
            )

            if baseline is None:
                baseline = hashes

            assert hashes == baseline, (
                "non-deterministic DXIL", jobs, repeat,
                baseline, hashes
            )

            runs += 1

            print(
                f"DETERMINISM PASS: jobs={jobs} "
                f"repeat={repeat + 1} shaders={len(files)}"
            )

    env.pop("VICTORY_EMIT_DUMP", None)

    print(f"DETERMINISM RUNS PASSED: {runs}")

    if sys.platform != "win32":
        for option in ("--validate", "--pipelines"):
            error = run(
                "translate", inputs,
                option, success=False
            )

            assert "needs Windows" in error

    print("OFFLINE CLI SMOKE TEST: PASS")

    summary = os.environ.get("GITHUB_STEP_SUMMARY")

    if summary:
        with open(summary, "a", encoding="utf-8") as f:
            f.write("\n## Offline shader regressions\n\n")
            f.write("| Check | Result |\n")
            f.write("|---|---|\n")
            f.write("| Discovery and deduplication | PASS |\n")
            f.write("| DXBC and DXIL structure | PASS |\n")
            f.write("| LLVM bitcode header | PASS |\n")
            f.write(f"| Determinism runs | {runs}/{runs} |\n")
            f.write("| Worker counts | 1, 2, 4, 64 |\n")
            f.write("| Synthetic fixtures only | Yes |\n")

PY
