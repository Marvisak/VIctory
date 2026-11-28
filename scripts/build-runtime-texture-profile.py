#!/usr/bin/env python3
"""Build or validate opt-in runtime manifests from verified cooked BC assets.

The identity input supplies hashes of ORIGINAL DETILED title mips. The cooker
report alone cannot establish a title resource's identity or role. A generated
manifest lists only these explicitly verified assets; it never modifies RPFs.
Requires Python 3.10+, blake3, and tomli (or Python 3.11+ tomllib).
"""

from __future__ import annotations

import argparse
import ctypes
import hashlib
import json
import os
from pathlib import Path
import stat
import struct
import sys
import tempfile

import blake3

try:
    import tomllib
except ModuleNotFoundError:
    import tomli as tomllib


FORMATS = {
    "BC1_UNORM": (71, 8), "BC1_UNORM_SRGB": (72, 8),
    "BC2_UNORM": (74, 16), "BC2_UNORM_SRGB": (75, 16),
    "BC3_UNORM": (77, 16), "BC3_UNORM_SRGB": (78, 16),
    "BC4_UNORM": (80, 8), "BC4_SNORM": (81, 8),
    "BC5_UNORM": (83, 16), "BC5_SNORM": (84, 16),
    "BC7_UNORM": (98, 16), "BC7_UNORM_SRGB": (99, 16),
}
BLOCK_BYTES = {number: block for number, block in FORMATS.values()}
MAX_ENTRIES = 16_384
MAX_DDS = 64 << 20


def fixed_local_file(path: Path) -> Path:
    """Match the runtime's drive-qualified, no-reparse input policy."""
    if os.name != "nt" or not path.is_absolute() or not path.drive \
            or path.drive.startswith("\\\\") or ".." in path.parts:
        raise ValueError(f"expected a fixed local Windows-drive path: {path}")
    drive = path.drive + "\\"
    if ctypes.windll.kernel32.GetDriveTypeW(ctypes.c_wchar_p(drive)) != 3:
        raise ValueError(f"not a fixed local drive: {drive}")
    current = Path(drive)
    for part in path.parts[1:]:
        current = current / part
        if os.lstat(current).st_file_attributes & stat.FILE_ATTRIBUTE_REPARSE_POINT:
            raise ValueError(f"reparse point in profile path: {current}")
    if not path.is_file():
        raise ValueError(f"missing profile file: {path}")
    return path


def relative_dds(root: Path, name: str) -> Path:
    rel = Path(name)
    if rel.is_absolute() or rel.drive or not rel.parts or ".." in rel.parts \
            or rel.suffix.lower() != ".dds":
        raise ValueError(f"invalid relative DDS path: {name}")
    return fixed_local_file(root / rel)


def digest(hex_value: str) -> str:
    if not isinstance(hex_value, str) or len(hex_value) != 64:
        raise ValueError("BLAKE3 hash must have 64 hex digits")
    try:
        bytes.fromhex(hex_value)
    except ValueError as error:
        raise ValueError("invalid BLAKE3 hash") from error
    return hex_value.lower()


def dds_info(path: Path) -> dict:
    data = path.read_bytes()
    if len(data) < 148 or len(data) > MAX_DDS or data[:4] != b"DDS " \
            or struct.unpack_from("<I", data, 4)[0] != 124 or data[84:88] != b"DX10":
        raise ValueError(f"invalid or oversized DX10 DDS: {path}")
    height, width, mips = (struct.unpack_from("<I", data, at)[0] for at in (12, 16, 28))
    fmt, dimension, misc, array = (struct.unpack_from("<I", data, at)[0]
                                   for at in (128, 132, 136, 140))
    if fmt not in BLOCK_BYTES or dimension != 3 or misc or array != 1 \
            or struct.unpack_from("<I", data, 112)[0] & 0x0000_fe00 \
            or not 0 < width <= 16384 or not 0 < height <= 16384 \
            or not 0 < mips <= 15 or mips > max(width, height).bit_length():
        raise ValueError(f"unsupported DDS shape or format: {path}")
    offsets = []
    at = 148
    for level in range(mips):
        w, h = max(1, width >> level), max(1, height >> level)
        size = ((w + 3) // 4) * ((h + 3) // 4) * BLOCK_BYTES[fmt]
        offsets.append((w, h, at, size))
        at += size
    if at != len(data):
        raise ValueError(f"truncated or extra DDS payload: {path}")
    return {"width": width, "height": height, "mips": offsets,
            "format": fmt, "blake3": blake3.blake3(data).hexdigest(),
            "sha256": hashlib.sha256(data).hexdigest()}


def checked_entry(entry: dict, root: Path, profile: str,
                  known_report: dict | None = None,
                  dds_cache: dict[Path, dict] | None = None) -> dict:
    required = {"source_blake3", "title_width", "title_height", "title_mips",
                "title_format", "dropped", "host_mips", "host_format",
                "source_mip", "dds_mip", "dds", "dds_blake3"}
    if set(entry) != required:
        raise ValueError(f"profile entry fields differ: {set(entry) ^ required}")
    source = digest(entry["source_blake3"])
    expected_dds = digest(entry["dds_blake3"])
    ints = {name: entry[name] for name in required - {"source_blake3", "dds", "dds_blake3"}}
    if any(type(value) is not int for value in ints.values()):
        raise ValueError("profile dimensions, formats, and mips must be integers")
    title_width, title_height = entry["title_width"], entry["title_height"]
    count, dropped = entry["title_mips"], entry["dropped"]
    source_mip, dds_mip = entry["source_mip"], entry["dds_mip"]
    fmt = entry["title_format"]
    if fmt not in BLOCK_BYTES or entry["host_format"] != fmt \
            or not 0 < title_width <= 16384 or not 0 < title_height <= 16384 \
            or not 0 < count <= 15 or not 0 <= dropped <= source_mip < count \
            or entry["host_mips"] != count - dropped or dds_mip != source_mip - dropped:
        raise ValueError("invalid title/host shape or format mapping")
    path = relative_dds(root, entry["dds"])
    if dds_cache is None:
        dds = dds_info(path)
    else:
        if path not in dds_cache:
            dds_cache[path] = dds_info(path)
        dds = dds_cache[path]
    if dds["blake3"] != expected_dds or dds["format"] != fmt \
            or dds_mip >= len(dds["mips"]) \
            or dds["mips"][dds_mip][:2] != (
                max(1, title_width >> source_mip),
                max(1, title_height >> source_mip)):
        raise ValueError(f"cooked DDS hash, format, or dimensions mismatch: {path}")
    if known_report is not None and (
            known_report["profile"] != profile
            or known_report["sha256"].lower() != dds["sha256"]):
        raise ValueError(f"DDS differs from cooker report: {path}")
    return {**entry, "source_blake3": source, "dds_blake3": expected_dds}


def validate_manifest(path: Path, expected_profile: str | None = None) -> int:
    path = fixed_local_file(path)
    if not 0 < path.stat().st_size <= 4 << 20:
        raise ValueError("manifest is empty or too large")
    with path.open("rb") as handle:
        manifest = tomllib.load(handle)
    if set(manifest) != {"version", "profile", "texture"} \
            or type(manifest["version"]) is not int or manifest["version"] != 1 \
            or manifest["profile"] not in {"720p", "1080p"} \
            or expected_profile is not None and manifest["profile"] != expected_profile \
            or not isinstance(manifest["texture"], list) \
            or not 0 < len(manifest["texture"]) <= MAX_ENTRIES:
        raise ValueError("invalid profile manifest header or entry count")
    seen = set()
    dds_cache: dict[Path, dict] = {}
    for entry in manifest["texture"]:
        checked = checked_entry(entry, path.parent, manifest["profile"],
                                dds_cache=dds_cache)
        shape = tuple(checked[name] for name in (
            "title_width", "title_height", "title_mips", "title_format",
            "dropped", "host_mips", "source_mip", "source_blake3"))
        if shape in seen:
            raise ValueError("duplicate source/shape profile entry")
        seen.add(shape)
    return len(seen)


def build(root: Path, identities: Path) -> list[Path]:
    root = root.absolute()
    report_path = fixed_local_file(root / "cook-report.json")
    identity_path = fixed_local_file(identities.absolute())
    report = json.loads(report_path.read_text(encoding="utf-8"))["assets"]
    identity_records = json.loads(identity_path.read_text(encoding="utf-8"))["assets"]
    if not isinstance(report, list) or not isinstance(identity_records, list) \
            or not identity_records:
        raise ValueError("cook report and identity assets must be nonempty arrays")
    index = {(asset["profile"], asset["output_name"]): asset for asset in report}
    generated = {"720p": [], "1080p": []}
    dds_cache: dict[Path, dict] = {}
    for identity in identity_records:
        profile = identity["profile"]
        if profile not in generated:
            raise ValueError(f"unsupported profile: {profile}")
        report_entry = index.get((profile, identity["output_name"]))
        if report_entry is None or report_entry["format"] not in FORMATS:
            raise ValueError(f"identity has no BC cooked output: {identity['output_name']}")
        fmt = FORMATS[report_entry["format"]][0]
        if fmt != identity["title_format"]:
            raise ValueError("cooker changed the title texture's format")
        count, dropped = identity["title_mips"], identity["dropped"]
        source_hashes = identity["source_blake3"]
        if type(count) is not int or type(dropped) is not int \
                or not 0 <= dropped < count <= 15 \
                or not isinstance(source_hashes, list) or len(source_hashes) != count - dropped:
            raise ValueError("identity must have one verified hash per retained title mip")
        dds_path = relative_dds(root, report_entry["output"])
        if dds_path not in dds_cache:
            dds_cache[dds_path] = dds_info(dds_path)
        dds = dds_cache[dds_path]
        if dds["format"] != fmt or dds["sha256"] != report_entry["sha256"].lower() \
                or (dds["width"], dds["height"]) != (
                    max(1, identity["title_width"] >> dropped),
                    max(1, identity["title_height"] >> dropped)) \
                or len(dds["mips"]) < len(source_hashes):
            raise ValueError(f"cooked output does not fit title retained mips: {dds_path}")
        for dds_mip, source in enumerate(source_hashes):
            entry = {
                "source_blake3": digest(source),
                "title_width": identity["title_width"],
                "title_height": identity["title_height"],
                "title_mips": count,
                "title_format": fmt,
                "dropped": dropped,
                "host_mips": count - dropped,
                "host_format": fmt,
                "source_mip": dropped + dds_mip,
                "dds_mip": dds_mip,
                "dds": str(dds_path.relative_to(root)).replace("\\", "/"),
                "dds_blake3": dds["blake3"],
            }
            generated[profile].append(checked_entry(entry, root, profile, report_entry,
                                                    dds_cache))
    output = []
    for profile, entries in generated.items():
        if not entries:
            continue
        if len(entries) > MAX_ENTRIES:
            raise ValueError("profile has too many verified mip entries")
        lines = ["version = 1", f"profile = {json.dumps(profile)}", ""]
        for entry in entries:
            lines.append("[[texture]]")
            for key, value in entry.items():
                lines.append(f"{key} = {json.dumps(value)}")
            lines.append("")
        manifest_path = root / f"runtime_{profile}.toml"
        with tempfile.NamedTemporaryFile(mode="w", encoding="utf-8", dir=root,
                                         prefix=f"runtime_{profile}_", suffix=".tmp",
                                         delete=False) as handle:
            handle.write("\n".join(lines))
            temporary = Path(handle.name)
        try:
            validate_manifest(temporary, profile)
            temporary.replace(manifest_path)
        finally:
            temporary.unlink(missing_ok=True)
        output.append(manifest_path)
    return output


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    mode = parser.add_mutually_exclusive_group(required=True)
    mode.add_argument("--cooked-root", type=Path)
    mode.add_argument("--validate-manifest", type=Path)
    parser.add_argument("--identities", type=Path,
                        help="explicit title shape and detiled-mip hash JSON")
    parser.add_argument("--profile", choices=("720p", "1080p"),
                        help="expected profile for validation")
    args = parser.parse_args()
    try:
        if args.validate_manifest is not None:
            count = validate_manifest(args.validate_manifest, args.profile)
            print(f"Valid {args.profile or 'selected'} cooked profile: {count} verified mips")
        else:
            if args.identities is None:
                parser.error("--cooked-root requires --identities")
            output = build(args.cooked_root, args.identities)
            print("Built verified cooked profiles: " + ", ".join(str(path) for path in output))
    except (ValueError, KeyError, OSError, TypeError) as error:
        print(f"runtime profile error: {error}", file=sys.stderr)
        raise SystemExit(1) from None


if __name__ == "__main__":
    main()
