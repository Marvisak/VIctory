"""Inventory decoded RPF9 texture metadata via a separately built SixNG CLI.

This is read-only and never exports image pixels. Roles are intentionally left
unclassified: BC format and file name are insufficient to choose a safe cook.
"""

from __future__ import annotations

import argparse
import concurrent.futures
import csv
import json
import subprocess
from pathlib import Path


def run(sixng: Path, game: Path, work: Path, *args: str,
        timeout: int = 120) -> str:
    result = subprocess.run(
        [str(sixng), "--game", str(game), "--work", str(work), *args],
        check=True, capture_output=True, text=True, timeout=timeout)
    return result.stdout


def census(sixng: Path, game: Path, work: Path, archive: str,
           output: Path, jobs: int, limit: int = 0,
           timeout: int = 120) -> tuple[list[dict], list[dict]]:
    listed = run(sixng, game, work, "list", archive, "--rtype", "txd", "--ndjson")
    entries = [json.loads(line) for line in listed.splitlines() if line.strip()]
    if limit:
        entries = entries[:limit]

    def one(entry: dict) -> tuple[list[dict], dict | None]:
        try:
            inspected = json.loads(run(sixng, game, work, "inspect", entry["id"],
                                       timeout=timeout))["entry"]
        except (subprocess.CalledProcessError, subprocess.TimeoutExpired,
                json.JSONDecodeError, KeyError) as error:
            return [], {"archive_id": entry["id"], "error": str(error)[:1000]}
        return [{"archive_id": entry["id"], "dictionary": entry.get("name") or "",
                 "name": texture.get("name") or "", "width": texture.get("width"),
                 "height": texture.get("height"), "mips": texture.get("mips"),
                 "dimension": texture.get("dimension"),
                 "depth": texture.get("depth"), "format": texture.get("format"),
                 "role": "", "tier": ""}
                for texture in inspected.get("textures", [])], None

    with concurrent.futures.ThreadPoolExecutor(max_workers=jobs) as executor:
        results = list(executor.map(one, entries))
    textures = [row for group, _ in results for row in group]
    errors = [error for _, error in results if error is not None]
    output.parent.mkdir(parents=True, exist_ok=True)
    with output.open("w", encoding="utf-8", newline="") as stream:
        writer = csv.DictWriter(stream, fieldnames=["archive_id", "dictionary", "name",
                                                  "width", "height", "mips", "dimension",
                                                  "depth", "format", "role", "tier"])
        writer.writeheader()
        writer.writerows(textures)
    output.with_suffix(".summary.json").write_text(
        json.dumps({"archive": archive, "dictionaries": len(entries),
                    "inspected": len(entries) - len(errors),
                    "errors": errors, "textures": len(textures)}, indent=2) + "\n",
        encoding="utf-8")
    return textures, errors


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--sixng", type=Path, required=True)
    parser.add_argument("--game", type=Path, required=True)
    parser.add_argument("--work", type=Path, required=True)
    parser.add_argument("--archive", default="textures")
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--jobs", type=int, default=4)
    parser.add_argument("--limit", type=int, default=0,
                        help="inspect only the first N dictionaries (0 = all)")
    parser.add_argument("--timeout", type=int, default=120,
                        help="seconds allowed for each dictionary")
    args = parser.parse_args()
    if not 1 <= args.jobs <= 16:
        parser.error("jobs must be 1..16")
    if args.limit < 0:
        parser.error("limit must be nonnegative")
    if args.timeout < 1:
        parser.error("timeout must be positive")
    textures, errors = census(args.sixng, args.game, args.work, args.archive,
                              args.output, args.jobs, args.limit, args.timeout)
    print(f"Inventoried {len(textures)} textures in {args.output}; "
          f"{len(errors)} dictionary errors in {args.output.with_suffix('.summary.json')}")
    if errors:
        raise SystemExit(1)


if __name__ == "__main__":
    main()
