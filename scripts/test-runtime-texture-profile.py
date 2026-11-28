"""Synthetic checks for the explicit cooked-runtime profile builder."""

from __future__ import annotations

import argparse
import hashlib
import importlib.util
import json
from pathlib import Path
import struct
import tempfile

import blake3


SCRIPT = Path(__file__).with_name("build-runtime-texture-profile.py")
SPEC = importlib.util.spec_from_file_location("runtime_profile", SCRIPT)
assert SPEC and SPEC.loader
profile = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(profile)


def dds() -> bytes:
    data = bytearray(148 + 32 + 8)
    data[:4] = b"DDS "
    struct.pack_into("<I", data, 4, 124)
    struct.pack_into("<III", data, 12, 8, 8, 0)
    struct.pack_into("<I", data, 28, 2)
    data[84:88] = b"DX10"
    struct.pack_into("<III", data, 128, 72, 3, 0)
    struct.pack_into("<I", data, 140, 1)
    data[148:] = b"\x12" * 32 + b"\x34" * 8
    return bytes(data)


def write_fixture(root: Path) -> tuple[Path, Path, Path, dict]:
    marker = root / ".victory-synthetic-profile-fixture"
    if root.exists() and any(root.iterdir()) and not marker.is_file():
        raise ValueError(f"refusing to overwrite non-fixture folder: {root}")
    root.mkdir(parents=True, exist_ok=True)
    marker.write_text("Synthetic test files only; contains no game assets.\n",
                      encoding="utf-8")
    (root / "assets_720p").mkdir(exist_ok=True)
    cooked = root / "assets_720p" / "wall.dds"
    cooked.write_bytes(dds())
    report = {"assets": [{"profile": "720p", "output_name": "wall.png",
                          "format": "BC1_UNORM_SRGB", "output": "assets_720p/wall.dds",
                          "sha256": hashlib.sha256(dds()).hexdigest()}]}
    (root / "cook-report.json").write_text(json.dumps(report), encoding="utf-8")
    identities = root / "identities.json"
    good = {"assets": [{"profile": "720p", "output_name": "wall.png",
                        "title_width": 8, "title_height": 8, "title_mips": 2,
                        "title_format": 72, "dropped": 0,
                        "source_blake3": [blake3.blake3(b"first").hexdigest(),
                                          blake3.blake3(b"second").hexdigest()]}]}
    identities.write_text(json.dumps(good), encoding="utf-8")
    [manifest] = profile.build(root, identities)
    return manifest, cooked, identities, good


def run() -> None:
    with tempfile.TemporaryDirectory(prefix="victory-profile-test-") as temporary:
        root = Path(temporary)
        manifest, cooked, identities, good = write_fixture(root)
        assert manifest.name == "runtime_720p.toml"
        assert profile.validate_manifest(manifest, "720p") == 2
        try:
            profile.validate_manifest(manifest, "1080p")
        except ValueError:
            pass
        else:
            raise AssertionError("wrong profile name accepted")
        assert manifest.read_text(encoding="utf-8").count("[[texture]]") == 2
        bad = json.loads(json.dumps(good))
        bad["assets"][0]["title_format"] = 99
        identities.write_text(json.dumps(bad), encoding="utf-8")
        try:
            profile.build(root, identities)
        except ValueError:
            pass
        else:
            raise AssertionError("format-changing identity accepted")
        cooked.write_bytes(dds()[:-1])
        try:
            profile.validate_manifest(manifest, "720p")
        except ValueError:
            pass
        else:
            raise AssertionError("truncated cooked DDS accepted")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output-root", type=Path,
                        help="leave a valid synthetic profile in this folder")
    args = parser.parse_args()
    if args.output_root:
        manifest, _, _, _ = write_fixture(args.output_root.absolute())
        print(f"synthetic runtime profile ready: {manifest}")
    else:
        run()
        print("runtime texture profile synthetic checks passed")
