"""Summarize bounded title texture detile/upload traces from a local game log.

Install the Python ``blake3`` package. No texture bytes are read or exported.
"""

from __future__ import annotations

import argparse
from collections import defaultdict
import json
from pathlib import Path
import re

try:
    import blake3
except ImportError as exc:
    raise SystemExit("Install blake3 for all-zero texture hash checks") from exc


FIELD = re.compile(r"([a-z_]+)=([^\s]+)")
DETILE = "large sampled texture detiled for upload"
UPLOAD = "sampled texture upload recorded"
AUDIT = "zero-copied sampled texture source audit"


def fields(line: str) -> dict[str, str]:
    return dict(FIELD.findall(line))


def zero_hash(length: int, cache: dict[int, str]) -> str:
    if length not in cache:
        h = blake3.blake3()
        block = bytes(min(length, 1 << 20))
        for _ in range(length // len(block)):
            h.update(block)
        h.update(block[:length % len(block)])
        cache[length] = h.hexdigest()
    return cache[length]


def summarize(lines: list[str]) -> dict:
    entries: list[dict] = []
    uploads: list[dict] = []
    audits: list[dict] = []
    zeros: dict[int, str] = {}
    for line in lines:
        if DETILE in line:
            f = fields(line)
            try:
                width, height, block = (int(f[k]) for k in ("host_width", "host_height", "block_bytes"))
                length = ((width + 3) // 4) * ((height + 3) // 4) * block
                if not (0 < length <= 64 << 20 and block in (8, 16)):
                    continue
                is_zero = f["hash"] == zero_hash(length, zeros)
                # A new runtime trace prints this itself; verify it against
                # the computed digest instead of trusting a stale label.
                if "all_zero" in f and (f["all_zero"] == "true") != is_zero:
                    raise ValueError(f"all_zero label disagrees with digest: {line.strip()}")
                entries.append({
                    "address": f["address"], "hash": f["hash"], "zero": is_zero,
                    "world": f.get("world_started") == "true",
                    "title_width": int(f["title_width"]),
                    "title_height": int(f["title_height"]),
                    "title_format": int(f["title_format"]),
                    "title_mips": int(f["title_mips"]),
                    "host_width": width, "host_height": height,
                    "host_format": int(f["host_format"]),
                    "host_mips": int(f["host_mips"]),
                    "source_mip": int(f["source_mip"]),
                })
            except (KeyError, ValueError) as exc:
                raise ValueError(f"invalid texture trace: {line.strip()}") from exc
        elif UPLOAD in line:
            f = fields(line)
            uploads.append({
                "address": f.get("address"), "hash": f.get("hash"),
                "zero": f.get("all_zero") == "true",
                "confirmed": f.get("copy_confirmed") == "true",
            })
        elif AUDIT in line:
            f = fields(line)
            audits.append({
                "address": f.get("address"),
                "source_now_zero": f.get("source_now_zero") == "true" if "source_now_zero" in f else None,
                "later_copies": int(f.get("later_copies", "0")),
                "stale_candidate": f.get("stale_candidate") == "true",
            })

    by_resource: dict[tuple, list[dict]] = defaultdict(list)
    for e in entries:
        by_resource[(e["address"], e["title_width"], e["title_height"],
                     e["title_format"], e["title_mips"])].append(e)
    recovered = [
        {"address": group[0]["address"], "title_width": group[0]["title_width"],
         "title_height": group[0]["title_height"], "first_nonzero_hash": next(
             e["hash"] for e in group[first_zero + 1:] if not e["zero"])}
        for group in by_resource.values()
        if (first_zero := next((i for i, e in enumerate(group) if e["zero"]), None)) is not None
        and any(not e["zero"] for e in group[first_zero + 1:])
    ]
    world = [e for e in entries if e["world"]]
    nonzero_world = [e for e in world if not e["zero"]]
    return {
        "detile_entries": len(entries),
        "preworld_entries": len(entries) - len(world),
        "world_entries": len(world),
        "zero_entries": sum(e["zero"] for e in entries),
        "world_zero_entries": sum(e["zero"] for e in world),
        "distinct_addresses": len({e["address"] for e in entries}),
        "same_shape_address_zero_then_nonzero": len(recovered),
        "recoveries": recovered[:16],
        "world_nonzero_candidates": nonzero_world[:20],
        "upload_records": len(uploads),
        "zero_copy_recorded": sum(u["zero"] and u["confirmed"] for u in uploads),
        "nonzero_copy_recorded": sum(not u["zero"] and u["confirmed"] for u in uploads),
        "upload_confirmation_available": bool(uploads),
        "zero_source_audits": len(audits),
        "audit_source_still_zero": sum(a["source_now_zero"] is True for a in audits),
        "audit_source_nonzero": sum(a["source_now_zero"] is False for a in audits),
        "audit_unavailable": sum(a["source_now_zero"] is None for a in audits),
        "audit_stale_candidates": sum(a["stale_candidate"] for a in audits),
        "audit_zero_after_later_copy": sum(a["source_now_zero"] is True and a["later_copies"] > 0 for a in audits),
    }


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("log", type=Path)
    args = parser.parse_args()
    lines = args.log.read_text(encoding="utf-8", errors="replace").splitlines()
    print(json.dumps(summarize(lines), indent=2))


if __name__ == "__main__":
    main()
