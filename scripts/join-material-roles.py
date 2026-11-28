"""Join SixNG material-slot evidence to a TTPF texture census.

This is a review CSV, not a cook manifest. Unknown packing, atlas layout,
screen coverage, and alpha semantics must be checked before conversion.
"""

from __future__ import annotations

import argparse
import csv
import gzip
import json
from collections import Counter, defaultdict
from pathlib import Path


def slot_evidence(users: Path) -> dict[str, Counter[str]]:
    evidence: dict[str, Counter[str]] = defaultdict(Counter)
    with gzip.open(users, "rt", encoding="utf-8") as stream:
        header = json.loads(next(stream))
        if header.get("texture_users") != 1:
            raise ValueError("unsupported SixNG texture-users index")
        for line in stream:
            row = json.loads(line)
            if "m" not in row:
                continue
            for slot, hash64 in row["m"][1]:
                evidence[hash64.lower()][slot.lower()] += 1
    return evidence


def joined(census: Path, users: Path, output: Path) -> dict:
    if output.resolve() in {census.resolve(), users.resolve()}:
        raise ValueError("output must differ from the census and material index")
    evidence = slot_evidence(users)
    counts: Counter[str] = Counter()
    output.parent.mkdir(parents=True, exist_ok=True)
    with census.open(newline="", encoding="utf-8") as source, output.open(
            "w", newline="", encoding="utf-8") as destination:
        reader = csv.DictReader(source)
        fields = ["store", "hash", "width", "height", "dxgi_format", "size",
                  "role_candidate", "material_slots", "material_references",
                  "review_needed"]
        writer = csv.DictWriter(destination, fieldnames=fields)
        writer.writeheader()
        for row in reader:
            slots = evidence.get(row["hash"].lower(), Counter())
            kinds = set()
            if {"diffusetex", "diffusetex2"} & slots.keys():
                kinds.add("albedo")
            if {"bumptex", "bumptexture", "bumptexture2"} & slots.keys():
                kinds.add("normal")
            role = kinds.pop() if len(kinds) == 1 else "conflict" if kinds else "unknown"
            counts[role] += 1
            writer.writerow({key: row[key] for key in fields[:6]} | {
                "role_candidate": role,
                "material_slots": ";".join(sorted(slots)),
                "material_references": sum(slots.values()),
                "review_needed": "normal encoding and tier" if role == "normal" else
                    "atlas, alpha, color space and tier" if role == "albedo" else
                    "classify material role and tier",
            })
    summary = {"rows": sum(counts.values()), "candidate_counts": dict(counts),
               "indexed_texture_hashes": len(evidence)}
    output.with_suffix(".summary.json").write_text(
        json.dumps(summary, indent=2) + "\n", encoding="utf-8")
    return summary


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--census", type=Path, required=True)
    parser.add_argument("--users", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    print(json.dumps(joined(args.census, args.users, args.output), indent=2))


if __name__ == "__main__":
    main()
