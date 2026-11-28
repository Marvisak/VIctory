"""Summarize exercised shader and texture paths in an offline VIctory run.

Usage: python scripts/analyze-diagnostic-run.py target/test-runs/RUN
Only local logs are read; coverage.json is written beside them.
"""

from __future__ import annotations

import argparse
import collections
import json
import re
import statistics
import sys
from datetime import datetime
from pathlib import Path


SHADER = re.compile(r'\bstage="([^"]+)"\s+(?:size=\d+\s+)?hash=([0-9a-f]{16})')
FIRST_BIND = re.compile(r'\bfirst bind pipeline=\S+ frame=(\d+)')
MESH_FALLBACK = re.compile(r'host mesh compute-to-vertex fallback shaders=\[mesh ([0-9a-f]{16})')
DROP = re.compile(r'dropped ([\d.]+)/frame without a host pipeline: (.+)$')
DROP_ACCOUNTING = re.compile(
    r'drop accounting: regular_failed=(\d+) regular_ray_disabled=(\d+) '
    r'indirect_failed=(\d+) indirect_ray_disabled=(\d+) reason_events=(\d+) '
    r'reason_categories=(\d+) reason_categories_omitted=(\d+)'
)
FPS = re.compile(r'\b([\d.]+) fps, ([\d.]+) ms/frame .*?, ([\d.]+) draws/frame, dropped ([\d.]+)')
INDIRECT_COMMANDS = re.compile(r'\band ([\d.]+) indirect;')
PERF = re.compile(r'\(([\d.]+) preparing: copies ([\d.]+) ms [\d.]+ MB, textures [\d.]+ ms [\d.]+ MB; ([\d.]+) on the GPU; ([\d.]+) copying back')
WORLD = re.compile(r"the world's first frame .*?frame=(\d+)")
FIELD = re.compile(r'(\w+)=("[^"]*"|\S+)')
USED_NULL_MARKER = "actual shader-use sites with null host buffer descriptors"
USED_NULL_FIELDS = (
    "table_budget", "table_invalid_or_empty", "table_other",
    "observed_heap_budget", "observed_heap_invalid_or_empty", "observed_heap_other",
)
USED_TEXTURE_NULL_MARKER = "shader-used texture slots with null host views"
USED_TEXTURE_NULL_FIELDS = (
    "table_host_copy_unavailable", "table_placed_texture_unavailable",
    "table_format_incompatible", "table_other",
    "observed_heap_host_copy_unavailable", "observed_heap_placed_texture_unavailable",
    "observed_heap_format_incompatible", "observed_heap_other",
)
USED_TEXTURE_INVALID_FIELDS = ("table_invalid_or_empty", "observed_heap_invalid_or_empty")


def parse_timestamp(value: str) -> datetime:
    # PowerShell's round-trip UTC format has seven fractional digits. Python
    # builds that accept only microseconds need the seventh trimmed.
    value = value.replace("Z", "+00:00")
    value = re.sub(r"(\.\d{6})\d+(?=[+-]\d{2}:\d{2}$)", r"\1", value)
    return datetime.fromisoformat(value)


def analyze(run: Path) -> dict:
    shaders: dict[str, dict] = {}
    dropped: collections.Counter[str] = collections.Counter()
    world_dropped: collections.Counter[str] = collections.Counter()
    views: collections.Counter[tuple] = collections.Counter()
    d3d_errors: collections.Counter[str] = collections.Counter()
    warnings: collections.Counter[str] = collections.Counter()
    frame_rates: list[dict] = []
    raw_drop_reports = 0
    used_null_rows = 0
    used_null_malformed_world = 0
    used_texture_null_rows = 0
    used_texture_null_malformed_world = 0
    world_frame = None
    gpu_removed = False
    world_start = None
    coverage_mode_file = run / "coverage-mode.txt"
    if coverage_mode_file.exists():
        coverage_mode = coverage_mode_file.read_text(encoding="utf-8-sig").strip().lower()
        if coverage_mode not in ("enabled", "disabled"):
            coverage_mode = "unknown"
            warnings["invalid texture coverage mode"] += 1
    else:
        # Older runs did not record whether -Coverage was requested.
        coverage_mode = "unknown"
    timing_file = run / "world-timing.txt"
    if timing_file.exists():
        match = re.search(r'World timing window started at (\S+)', timing_file.read_text(encoding="utf-8"))
        if match:
            world_start = parse_timestamp(match[1])

    with (run / "stderr.log").open(encoding="utf-8", errors="replace") as log:
        for line in log:
            shader = SHADER.search(line)
            if shader:
                stage, digest = shader.groups()
                row = shaders.setdefault(digest, {"stage": stage, "translated": False, "failure": None, "first_bound_frame": None})
                if "shader translated" in line:
                    row["translated"] = True
                if "shader does not translate" in line:
                    row["failure"] = line.split(" error=", 1)[-1].split(" cached=", 1)[0].strip()
                if "first bind pipeline=" in line:
                    bound = FIRST_BIND.search(line)
                    if bound:
                        row["first_bound_frame"] = int(bound[1])
            fallback = MESH_FALLBACK.search(line)
            if fallback:
                row = shaders.setdefault(fallback[1], {"stage": "mesh", "translated": False, "failure": None, "first_bound_frame": None})
                row["translated"] = True

            if "texture view coverage" in line:
                fields = dict(FIELD.findall(line))
                key = tuple(fields.get(k, "?").strip('"') for k in ("kind", "title", "requested", "host", "outcome"))
                try:
                    views[key] += int(fields["count"])
                except (KeyError, ValueError):
                    warnings["malformed texture coverage row"] += 1

            if "without a host pipeline:" in line:
                match = DROP.search(line)
                if match:
                    # Periodic numbers are per-frame rates. Keep the peak for
                    # each reason instead of summing overlapping intervals.
                    dropped[match[2]] = max(dropped[match[2]], float(match[1]))
                    stamp = parse_timestamp(line.split(" ", 1)[0])
                    if world_frame is not None or (world_start is not None and stamp >= world_start):
                        world_dropped[match[2]] = max(world_dropped[match[2]], float(match[1]))
                    if frame_rates:
                        frame_rates[-1]["drop_reasons"].append((match[2], float(match[1])))

            if "drop accounting:" in line:
                match = DROP_ACCOUNTING.search(line)
                if match:
                    raw_drop_reports += 1
                    if frame_rates:
                        frame_rates[-1]["drop_accounting"] = dict(zip(
                            ("regular_failed", "regular_ray_disabled", "indirect_failed",
                             "indirect_ray_disabled", "reason_events", "reason_categories",
                             "reason_categories_omitted"),
                            map(int, match.groups()),
                        ))
                else:
                    warnings["malformed drop accounting row"] += 1

            if USED_NULL_MARKER in line:
                fields = dict(FIELD.findall(line))
                try:
                    counts = {name: int(fields[name]) for name in USED_NULL_FIELDS}
                    if any(count < 0 for count in counts.values()):
                        raise ValueError("negative count")
                except (KeyError, ValueError):
                    warnings["malformed used-null row"] += 1
                    if frame_rates and frame_rates[-1]["world"]:
                        used_null_malformed_world += 1
                else:
                    used_null_rows += 1
                    if frame_rates and frame_rates[-1]["used_null"] is None:
                        frame_rates[-1]["used_null"] = counts
                    else:
                        warnings["orphan or duplicate used-null row"] += 1
                        if frame_rates and frame_rates[-1]["world"]:
                            used_null_malformed_world += 1

            if USED_TEXTURE_NULL_MARKER in line:
                fields = dict(FIELD.findall(line))
                try:
                    counts = {name: int(fields[name]) for name in USED_TEXTURE_NULL_FIELDS}
                    # The two invalid-address fields were added after the
                    # original probe. Keep older runs readable, but mark
                    # their valid-miss classification as unavailable below.
                    classified = all(name in fields for name in USED_TEXTURE_INVALID_FIELDS)
                    counts.update({name: int(fields.get(name, "0")) for name in USED_TEXTURE_INVALID_FIELDS})
                    if any(count < 0 for count in counts.values()):
                        raise ValueError("negative count")
                except (KeyError, ValueError):
                    warnings["malformed used-texture-null row"] += 1
                    if frame_rates and frame_rates[-1]["world"]:
                        used_texture_null_malformed_world += 1
                else:
                    used_texture_null_rows += 1
                    if frame_rates and frame_rates[-1]["used_null_texture"] is None:
                        frame_rates[-1]["used_null_texture"] = counts
                        frame_rates[-1]["used_null_texture_classified"] = classified
                    else:
                        warnings["orphan or duplicate used-texture-null row"] += 1
                        if frame_rates and frame_rates[-1]["world"]:
                            used_texture_null_malformed_world += 1

            if "D3D12:" in line and "severity=1" in line:
                text = line.split("D3D12: ", 1)[-1]
                key = re.search(r'\bid=(\d+)', text)
                d3d_errors[key[1] if key else "unknown"] += 1
            if "host GPU device removed" in line:
                gpu_removed = True
            if "the world's first frame" in line:
                match = WORLD.search(line)
                if match:
                    world_frame = int(match[1])
            if " fps, " in line and " draws/frame" in line:
                match = FPS.search(line)
                if match:
                    fps, ms, draws, lost = map(float, match.groups())
                    stamp = parse_timestamp(line.split(" ", 1)[0])
                    performance = PERF.search(line)
                    indirect = INDIRECT_COMMANDS.search(line)
                    frame_rates.append({
                        "stamp": line.split(" ", 1)[0],
                        "fps": fps, "ms": ms, "draws": draws, "dropped": lost,
                        "indirect_commands": float(indirect[1]) if indirect else None,
                        "world": world_frame is not None or (world_start is not None and stamp >= world_start),
                        "drop_reasons": [], "drop_accounting": None, "used_null": None,
                        "used_null_texture": None,
                        "used_null_texture_classified": False,
                        "prepare_ms": float(performance[1]) if performance else None,
                        "copy_ms": float(performance[2]) if performance else None,
                        "gpu_ms": float(performance[3]) if performance else None,
                        "copyback_ms": float(performance[4]) if performance else None,
                    })
            if "WARN " in line:
                for phrase in ("root view unavailable", "view format of another family", "host format incompatible", "a texture not placed", "FidelityFX target has no host copy"):
                    if phrase in line:
                        warnings[phrase] += 1

    network = {"samples": 0, "max_tcp": 0, "max_udp": 0}
    monitor = run / "monitor.csv"
    if monitor.exists():
        for line in monitor.read_text(encoding="utf-8-sig", errors="replace").splitlines()[1:]:
            parts = line.split(",")
            if len(parts) >= 6:
                try:
                    network["samples"] += 1
                    network["max_tcp"] = max(network["max_tcp"], int(parts[4]))
                    network["max_udp"] = max(network["max_udp"], int(parts[5]))
                except ValueError:
                    warnings["malformed network sample"] += 1

    bound = {h: v for h, v in shaders.items() if v["first_bound_frame"] is not None}
    failed_bound = {h: v for h, v in bound.items() if v["failure"]}
    unknown_bound = {h: v for h, v in bound.items() if not v["failure"] and not v["translated"]}
    world_rows = [r for r in frame_rates if r["world"]]
    world_samples = [r for r in world_rows if r["draws"] >= 500]
    world_rates = [r["fps"] for r in world_samples]
    world_null_reports = sum(r["used_null"] is not None for r in world_rows)
    world_null_totals = {
        name: sum(r["used_null"][name] for r in world_rows if r["used_null"] is not None)
        for name in USED_NULL_FIELDS
    }
    world_texture_null_reports = sum(r["used_null_texture"] is not None for r in world_rows)
    world_texture_null_totals = {
        name: sum(r["used_null_texture"][name] for r in world_rows if r["used_null_texture"] is not None)
        for name in (*USED_TEXTURE_NULL_FIELDS, *USED_TEXTURE_INVALID_FIELDS)
    }
    classified_texture_null_reports = sum(r["used_null_texture_classified"] for r in world_rows)
    invalid_texture_null_total = sum(world_texture_null_totals[name] for name in USED_TEXTURE_INVALID_FIELDS)
    if not world_rows:
        used_texture_null_coverage = "missing_world"
    elif world_texture_null_reports == len(world_rows) and used_texture_null_malformed_world == 0:
        used_texture_null_coverage = "complete"
    elif used_texture_null_rows:
        used_texture_null_coverage = "incomplete"
    else:
        used_texture_null_coverage = "unavailable"
    if not world_rows:
        used_null_coverage = "missing_world"
    elif world_null_reports == len(world_rows) and used_null_malformed_world == 0:
        used_null_coverage = "complete"
    elif used_null_rows:
        used_null_coverage = "incomplete"
    else:
        used_null_coverage = "unavailable"
    if not world_rows:
        drop_accounting_mode = "missing"
    elif raw_drop_reports or warnings["malformed drop accounting row"]:
        drop_accounting_mode = "exact" if all(r["drop_accounting"] is not None for r in world_rows) else "incomplete"
    else:
        drop_accounting_mode = "legacy_rounded"
    unclassified_drop_intervals = []
    for index, row in enumerate(world_rows):
        raw = row["drop_accounting"]
        failures = []
        if drop_accounting_mode in ("exact", "incomplete"):
            if raw is None:
                failures.append("missing raw drop accounting")
            else:
                if raw["regular_failed"] != raw["regular_ray_disabled"]:
                    failures.append("regular failures exceed intentional ray suppression")
                if raw["indirect_failed"] != raw["indirect_ray_disabled"]:
                    failures.append("indirect failures exceed intentional ray suppression")
                if raw["reason_categories_omitted"]:
                    failures.append("drop reasons omitted from log")
        elif drop_accounting_mode == "legacy_rounded":
            # Older binaries only reported integer-rounded regular failures
            # and one-decimal reason rates. Associate reasons with their FPS
            # interval; a ray reason in another interval cannot certify it.
            direct_ray = sum(rate for reason, rate in row["drop_reasons"]
                             if not reason.startswith("indirect kind ")
                             and "ray traversal disabled" in reason)
            if row["dropped"] > direct_ray + 0.55:
                failures.append("regular failures lack an interval ray reason")
        if failures:
            unclassified_drop_intervals.append({"index": index, "stamp": row["stamp"], "reasons": failures})
    # The ray-traversal switch deliberately suppresses those dispatches.
    # Keep them visible, but do not mix them with buffer/pipeline failures.
    suppressed = {reason: peak for reason, peak in world_dropped.items() if "ray traversal disabled" in reason}
    unexpected = {reason: peak for reason, peak in world_dropped.items() if reason not in suppressed}
    def median_field(name: str) -> float | None:
        values = [r[name] for r in world_samples if r[name] is not None]
        return round(statistics.median(values), 2) if values else None
    texture_rows = [
        {"kind": k[0], "title_format": k[1], "requested_format": k[2], "host_format": k[3], "outcome": k[4], "count": n}
        for k, n in sorted(views.items(), key=lambda item: (-item[1], item[0]))
    ]
    if views:
        # Rows prove collection happened, including on a legacy run without
        # a marker. A contradictory marker is worth surfacing.
        if coverage_mode == "disabled":
            warnings["texture coverage marker contradicted by log"] += 1
        coverage_mode = "enabled"
    texture_totals = dict(collections.Counter({
        outcome: sum(v for k, v in views.items() if k[4] == outcome)
        for outcome in {k[4] for k in views}
    }))
    result = (run / "result.txt").read_text(encoding="utf-8", errors="replace") if (run / "result.txt").exists() else ""
    return {
        "run": str(run.resolve()),
        "world_first_frame": world_frame,
        "world_completed": "reason=world timing window complete" in result,
        "shader": {
            "seen": len(shaders), "bound": len(bound),
            "translated_bound": sum(v["translated"] for v in bound.values()),
            "failed_bound": failed_bound, "unknown_bound": unknown_bound,
            "all_failures": {h: v for h, v in shaders.items() if v["failure"]},
        },
        "texture_coverage_state": coverage_mode,
        "texture_view_measurement": "descriptor_materialization_attempts",
        "texture_views": texture_rows if coverage_mode == "enabled" else None,
        "texture_view_totals": texture_totals if coverage_mode == "enabled" else None,
        "world_fps": {
            "samples": len(world_rates),
            "median": round(statistics.median(world_rates), 2) if world_rates else None,
            "minimum": min(world_rates) if world_rates else None,
            "maximum": max(world_rates) if world_rates else None,
            "prepare_ms_median": median_field("prepare_ms"),
            "copy_ms_median": median_field("copy_ms"),
            "gpu_ms_median": median_field("gpu_ms"),
            "copyback_ms_median": median_field("copyback_ms"),
            "draws_median": median_field("draws"),
            "dropped_median": median_field("dropped"),
            "regular_drop_peak": max((r["dropped"] for r in world_rows), default=None),
            "indirect_commands_per_frame_peak": max((r["indirect_commands"] for r in world_rows if r["indirect_commands"] is not None), default=None),
        },
        "world_drop_accounting": {
            "mode": drop_accounting_mode,
            "intervals": len(world_rows),
            "raw_intervals": sum(r["drop_accounting"] is not None for r in world_rows),
            "unclassified_intervals": unclassified_drop_intervals,
        },
        "world_used_null_buffer_descriptors": {
            "coverage_state": used_null_coverage,
            "reports": world_null_reports,
            "expected_reports": len(world_rows),
            "malformed_world_rows": used_null_malformed_world,
            **world_null_totals,
            "budget_total": world_null_totals["table_budget"] + world_null_totals["observed_heap_budget"],
            "other_total": world_null_totals["table_other"] + world_null_totals["observed_heap_other"],
            "invalid_or_empty_total": (
                world_null_totals["table_invalid_or_empty"]
                + world_null_totals["observed_heap_invalid_or_empty"]
            ),
        },
        "world_used_null_texture_descriptors": {
            "coverage_state": used_texture_null_coverage,
            "reports": world_texture_null_reports,
            "expected_reports": len(world_rows),
            "malformed_world_rows": used_texture_null_malformed_world,
            "classification_state": (
                "complete" if used_texture_null_coverage == "complete"
                and classified_texture_null_reports == world_texture_null_reports
                else "legacy_unclassified" if used_texture_null_coverage == "complete"
                else "incomplete" if world_texture_null_reports else "unavailable"
            ),
            **world_texture_null_totals,
            "total": sum(world_texture_null_totals.values()),
            "invalid_or_empty_total": invalid_texture_null_total,
            "valid_missing_total": (
                sum(world_texture_null_totals.values()) - invalid_texture_null_total
                if used_texture_null_coverage == "complete"
                and classified_texture_null_reports == world_texture_null_reports else None
            ),
        },
        "dropped_reason_peak_per_frame": dict(dropped.most_common()),
        "world_dropped_reason_peak_per_frame": dict(world_dropped.most_common()),
        "world_intentionally_suppressed_reason_peak_per_frame": suppressed,
        "world_unexpected_dropped_reason_peak_per_frame": unexpected,
        "d3d12_severity_1_ids": dict(d3d_errors),
        "gpu_removed": gpu_removed,
        "network": network,
        "warnings": dict(warnings),
        "result": result.strip().splitlines(),
    }


def has_unexpected_drops(report: dict) -> bool:
    if (report["world_first_frame"] is None or not report["world_completed"]
            or report["gpu_removed"] or report["world_unexpected_dropped_reason_peak_per_frame"]):
        return True
    accounting = report["world_drop_accounting"]
    return accounting["mode"] in ("missing", "incomplete") or bool(accounting["unclassified_intervals"])


def has_used_budget_nulls(report: dict) -> bool:
    """Fail closed unless every reported world interval has no used Budget/Other nulls."""
    if report["world_first_frame"] is None or not report["world_completed"] or report["gpu_removed"]:
        return True
    nulls = report["world_used_null_buffer_descriptors"]
    return (nulls["coverage_state"] != "complete"
            or nulls["budget_total"] != 0 or nulls["other_total"] != 0)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("run", type=Path)
    parser.add_argument("--require-no-unexpected-drops", action="store_true",
                        help="require a completed world timing window without GPU removal or unexpected drops")
    parser.add_argument("--require-no-used-budget-nulls", action="store_true",
                        help="require complete world null-use coverage and no shader-used Budget/Other null buffer descriptors")
    args = parser.parse_args()
    report = analyze(args.run)
    output = args.run / "coverage.json"
    output.write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")
    shader = report["shader"]
    print(f"Shader paths: {shader['translated_bound']}/{shader['bound']} bound translated; "
          f"{len(shader['failed_bound'])} failed, {len(shader['unknown_bound'])} unknown")
    views = report["texture_view_totals"]
    if views is None:
        print(f"Texture view materialization: not collected ({report['texture_coverage_state']})")
    else:
        missing = sum(count for outcome, count in views.items() if outcome != "made")
        print(f"Texture view materialization: {views.get('made', 0)} made, {missing} missing "
              f"across {len(report['texture_views'])} format/outcome paths")
    print(f"World fps (sample median): {report['world_fps']['median']}; "
          f"GPU removed: {report['gpu_removed']}; TCP/UDP max: "
          f"{report['network']['max_tcp']}/{report['network']['max_udp']}")
    print(f"World drop reasons: {len(report['world_unexpected_dropped_reason_peak_per_frame'])} unexpected, "
          f"{len(report['world_intentionally_suppressed_reason_peak_per_frame'])} ray-traversal suppressed")
    accounting = report["world_drop_accounting"]
    print(f"World drop accounting: {accounting['mode']}, "
          f"{accounting['raw_intervals']}/{accounting['intervals']} intervals with raw counts, "
          f"{len(accounting['unclassified_intervals'])} unclassified")
    nulls = report["world_used_null_buffer_descriptors"]
    print(f"World used null buffer descriptors: Budget {nulls['budget_total']}, "
          f"Other {nulls['other_total']}, invalid/empty {nulls['invalid_or_empty_total']}; "
          f"coverage {nulls['coverage_state']} ({nulls['reports']}/{nulls['expected_reports']} reports)")
    texture_nulls = report["world_used_null_texture_descriptors"]
    print(f"World used null texture descriptors: {texture_nulls['total']} "
          f"(valid missing {texture_nulls['valid_missing_total']}, "
          f"invalid/empty {texture_nulls['invalid_or_empty_total']}); "
          f"coverage {texture_nulls['coverage_state']} "
          f"({texture_nulls['reports']}/{texture_nulls['expected_reports']} reports), "
          f"classification {texture_nulls['classification_state']}")
    print(output)
    if args.require_no_unexpected_drops and has_unexpected_drops(report):
        sys.exit(1)
    if args.require_no_used_budget_nulls and has_used_budget_nulls(report):
        sys.exit(1)


if __name__ == "__main__":
    main()
