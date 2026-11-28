"""Inventory indexed SixNG TTPF stores without decoding image pixels.

The CSV is metadata only. Material roles require separate evidence.
"""

from __future__ import annotations

import argparse
import csv
import json
import subprocess
from collections import Counter
from pathlib import Path


def census(sixng: Path, game: Path, work: Path, stores: list[str],
           output: Path, timeout: int) -> dict:
    output.parent.mkdir(parents=True, exist_ok=True)
    count = 0
    errors = []
    formats: Counter[int] = Counter()
    maximum_edge: Counter[int] = Counter()
    per_store = []
    with output.open("w", newline="", encoding="utf-8") as stream:
        writer = csv.DictWriter(stream, fieldnames=["store", "hash", "width",
            "height", "dxgi_format", "size", "role", "tier"])
        writer.writeheader()
        for store in stores:
            try:
                result = subprocess.run([str(sixng), "--game", str(game),
                    "--work", str(work), "ttpf", store, "--quiet"],
                    check=True, capture_output=True, text=True, timeout=timeout)
                payload = json.loads(result.stdout)
                if payload.get("store") != store or not isinstance(payload.get("textures"), list):
                    raise ValueError("store identity or texture list mismatch")
                textures = payload["textures"]
                rows = []
                for texture in textures:
                    width, height = int(texture["width"]), int(texture["height"])
                    if width <= 0 or height <= 0:
                        raise ValueError("nonpositive texture dimensions")
                    fmt = int(texture["dxgi_format"])
                    rows.append({"store": store, "hash": f"{int(texture['hash']):016x}",
                        "width": width, "height": height, "dxgi_format": fmt,
                        "size": int(texture["size"]), "role": "", "tier": ""})
                for row in rows:
                    writer.writerow(row)
                    width, height, fmt = row["width"], row["height"], row["dxgi_format"]
                    formats[fmt] += 1
                    maximum_edge[max(width, height)] += 1
                count += len(textures)
                per_store.append({"store": store, "textures": len(textures)})
            except (subprocess.CalledProcessError, subprocess.TimeoutExpired,
                    json.JSONDecodeError, KeyError, TypeError, ValueError) as error:
                errors.append({"store": store, "error": str(error)[:1000]})
    summary = {"stores_requested": len(stores), "stores_read": len(per_store),
        "textures": count, "per_store": per_store,
        "edge_2048_or_more": sum(n for edge, n in maximum_edge.items() if edge >= 2048),
        "edge_4096_or_more": sum(n for edge, n in maximum_edge.items() if edge >= 4096),
        "maximum_edge": max(maximum_edge, default=0),
        "formats": dict(sorted(formats.items())), "errors": errors}
    output.with_suffix(".summary.json").write_text(json.dumps(summary, indent=2) + "\n",
        encoding="utf-8")
    return summary


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--sixng", type=Path, required=True)
    parser.add_argument("--game", type=Path, required=True)
    parser.add_argument("--work", type=Path, required=True)
    parser.add_argument("--store", action="append", required=True,
                        help="repeat for each fully qualified TTPF store id")
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--timeout", type=int, default=120)
    args = parser.parse_args()
    if args.timeout < 1:
        parser.error("timeout must be positive")
    result = census(args.sixng, args.game, args.work, args.store,
                    args.output, args.timeout)
    print(json.dumps(result, indent=2))
    if result["errors"]:
        raise SystemExit(1)


if __name__ == "__main__":
    main()
