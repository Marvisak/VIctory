"""Regression checks for world-only draw-drop classification."""

import importlib.util
import tempfile
import unittest
from pathlib import Path


SPEC = importlib.util.spec_from_file_location(
    "analyze_diagnostic_run", Path(__file__).with_name("analyze-diagnostic-run.py")
)
assert SPEC and SPEC.loader
ANALYZER = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(ANALYZER)


class DrawDropReportTests(unittest.TestCase):
    def test_world_only_and_suppressed_ray_dispatches(self):
        with tempfile.TemporaryDirectory() as directory:
            run = Path(directory)
            (run / "stderr.log").write_text(
                "\n".join([
                    "2026-11-28T09:00:00Z INFO victory_gpu::host: dropped 9.0/frame without a host pipeline: loading-only",
                    "2026-11-28T09:00:01Z INFO victory_gpu::capture: the world's first frame frame=200 draws=600",
                    "2026-11-28T09:00:02Z INFO victory_gpu::host: dropped 4.0/frame without a host pipeline: indirect kind 2: ray traversal disabled",
                    "2026-11-28T09:00:03Z INFO victory_gpu::host: dropped 2.0/frame without a host pipeline: indexed: index buffer unavailable",
                    "2026-11-28T09:00:04Z INFO victory_gpu::host: dropped 1.0/frame without a host pipeline: indexed: index buffer unavailable",
                    "2026-11-28T09:00:05Z INFO victory_gpu::host: 2.0 fps, 500.0 ms/frame (100.0 preparing: copies 10.0 ms 10.0 MB, textures 1.0 ms 1.0 MB; 20.0 on the GPU; 5.0 copying back 0.0 MB), 1000 draws/frame, dropped 2 without a host pipeline (0 being made) and 4 indirect; 20 root-view slots/frame",
                ]) + "\n", encoding="utf-8"
            )
            (run / "result.txt").write_text("reason=world timing window complete\n", encoding="utf-8")
            report = ANALYZER.analyze(run)
            self.assertEqual(report["world_first_frame"], 200)
            self.assertTrue(report["world_completed"])
            self.assertEqual(report["dropped_reason_peak_per_frame"]["loading-only"], 9.0)
            self.assertNotIn("loading-only", report["world_dropped_reason_peak_per_frame"])
            self.assertEqual(
                report["world_intentionally_suppressed_reason_peak_per_frame"],
                {"indirect kind 2: ray traversal disabled": 4.0},
            )
            self.assertEqual(
                report["world_unexpected_dropped_reason_peak_per_frame"],
                {"indexed: index buffer unavailable": 2.0},
            )
            self.assertEqual(report["world_fps"]["regular_drop_peak"], 2.0)
            self.assertEqual(report["world_fps"]["indirect_commands_per_frame_peak"], 4.0)
            self.assertEqual(report["world_drop_accounting"]["mode"], "legacy_rounded")
            self.assertTrue(ANALYZER.has_unexpected_drops(report))
            report["world_unexpected_dropped_reason_peak_per_frame"] = {}
            self.assertTrue(ANALYZER.has_unexpected_drops(report),
                            "reasons from another interval cannot certify a regular drop")
            report["gpu_removed"] = True
            self.assertTrue(ANALYZER.has_unexpected_drops(report),
                            "a removed GPU cannot certify a drop-free world window")

    def test_legacy_reasons_must_match_each_interval_and_indirect_calls_are_not_drops(self):
        with tempfile.TemporaryDirectory() as directory:
            run = Path(directory)
            (run / "stderr.log").write_text("\n".join([
                "2026-11-28T09:00:00Z INFO victory_gpu::capture: the world's first frame frame=200 draws=600",
                "2026-11-28T09:00:01Z INFO victory_gpu::host: 2.0 fps, 500.0 ms/frame (100.0 preparing: copies 10.0 ms 10.0 MB, textures 1.0 ms 1.0 MB; 20.0 on the GPU; 5.0 copying back 0.0 MB), 1000 draws/frame, dropped 1 without a host pipeline (0 being made) and 4 indirect; 20 root-view slots/frame",
                "2026-11-28T09:00:02Z INFO victory_gpu::host: dropped 1.0/frame without a host pipeline: dispatch: ray traversal disabled [compute 1234567890abcdef]",
                "2026-11-28T09:00:03Z INFO victory_gpu::host: dropped 4.0/frame without a host pipeline: indirect kind 2: ray traversal disabled",
                "2026-11-28T09:00:04Z INFO victory_gpu::host: 2.0 fps, 500.0 ms/frame (100.0 preparing: copies 10.0 ms 10.0 MB, textures 1.0 ms 1.0 MB; 20.0 on the GPU; 5.0 copying back 0.0 MB), 1000 draws/frame, dropped 0 without a host pipeline (0 being made) and 50 indirect; 20 root-view slots/frame",
            ]) + "\n", encoding="utf-8")
            (run / "result.txt").write_text("reason=world timing window complete\n", encoding="utf-8")
            report = ANALYZER.analyze(run)
            self.assertEqual(report["world_drop_accounting"]["mode"], "legacy_rounded")
            self.assertEqual(report["world_fps"]["indirect_commands_per_frame_peak"], 50.0)
            self.assertFalse(ANALYZER.has_unexpected_drops(report))
            with (run / "stderr.log").open("a", encoding="utf-8") as log:
                log.write("2026-11-28T09:00:05Z INFO victory_gpu::host: 2.0 fps, 500.0 ms/frame (100.0 preparing: copies 10.0 ms 10.0 MB, textures 1.0 ms 1.0 MB; 20.0 on the GPU; 5.0 copying back 0.0 MB), 1000 draws/frame, dropped 1 without a host pipeline (0 being made) and 50 indirect; 20 root-view slots/frame\n")
            report = ANALYZER.analyze(run)
            self.assertEqual(len(report["world_drop_accounting"]["unclassified_intervals"]), 1)
            self.assertTrue(ANALYZER.has_unexpected_drops(report))

    def test_exact_raw_counts_catch_hidden_regular_and_indirect_failures(self):
        fps = "2026-11-28T09:00:01Z INFO victory_gpu::host: 2.0 fps, 500.0 ms/frame (100.0 preparing: copies 10.0 ms 10.0 MB, textures 1.0 ms 1.0 MB; 20.0 on the GPU; 5.0 copying back 0.0 MB), 1000 draws/frame, dropped 1 without a host pipeline (0 being made) and 50 indirect; 20 root-view slots/frame"
        for counts, expected in (
            ((5, 5, 250, 250, 0), False),
            ((6, 5, 250, 250, 0), True),
            ((5, 5, 251, 250, 0), True),
            ((5, 5, 250, 250, 1), True),
        ):
            with self.subTest(counts=counts), tempfile.TemporaryDirectory() as directory:
                run = Path(directory)
                regular, ray, indirect, indirect_ray, omitted = counts
                (run / "stderr.log").write_text("\n".join([
                    "2026-11-28T09:00:00Z INFO victory_gpu::capture: the world's first frame frame=200 draws=600",
                    fps,
                    f"2026-11-28T09:00:02Z INFO victory_gpu::host: drop accounting: regular_failed={regular} regular_ray_disabled={ray} indirect_failed={indirect} indirect_ray_disabled={indirect_ray} reason_events=255 reason_categories=2 reason_categories_omitted={omitted}",
                    "2026-11-28T09:00:03Z INFO victory_gpu::host: dropped 1.0/frame without a host pipeline: dispatch: ray traversal disabled [compute 1234567890abcdef]",
                    "2026-11-28T09:00:04Z INFO victory_gpu::host: dropped 50.0/frame without a host pipeline: indirect kind 2: ray traversal disabled",
                ]) + "\n", encoding="utf-8")
                (run / "result.txt").write_text("reason=world timing window complete\n", encoding="utf-8")
                report = ANALYZER.analyze(run)
                self.assertEqual(report["world_drop_accounting"]["mode"], "exact")
                self.assertEqual(ANALYZER.has_unexpected_drops(report), expected)

    def test_new_format_missing_an_interval_fails_closed(self):
        with tempfile.TemporaryDirectory() as directory:
            run = Path(directory)
            fps = "2.0 fps, 500.0 ms/frame (100.0 preparing: copies 10.0 ms 10.0 MB, textures 1.0 ms 1.0 MB; 20.0 on the GPU; 5.0 copying back 0.0 MB), 1000 draws/frame, dropped 0 without a host pipeline (0 being made) and 50 indirect; 20 root-view slots/frame"
            (run / "stderr.log").write_text("\n".join([
                "2026-11-28T09:00:00Z INFO victory_gpu::capture: the world's first frame frame=200 draws=600",
                "2026-11-28T09:00:01Z INFO victory_gpu::host: " + fps,
                "2026-11-28T09:00:02Z INFO victory_gpu::host: drop accounting: regular_failed=0 regular_ray_disabled=0 indirect_failed=0 indirect_ray_disabled=0 reason_events=0 reason_categories=0 reason_categories_omitted=0",
                "2026-11-28T09:00:03Z INFO victory_gpu::host: " + fps,
            ]) + "\n", encoding="utf-8")
            (run / "result.txt").write_text("reason=world timing window complete\n", encoding="utf-8")
            report = ANALYZER.analyze(run)
            self.assertEqual(report["world_drop_accounting"]["mode"], "incomplete")
            self.assertEqual(report["world_drop_accounting"]["raw_intervals"], 1)
            self.assertTrue(ANALYZER.has_unexpected_drops(report))


class TextureCoverageReportTests(unittest.TestCase):
    def test_disabled_unknown_and_measured_zero_are_distinct(self):
        for marker, expected_state, expected_views in (
            ("disabled", "disabled", None),
            ("enabled", "enabled", []),
            (None, "unknown", None),
        ):
            with self.subTest(marker=marker), tempfile.TemporaryDirectory() as directory:
                run = Path(directory)
                (run / "stderr.log").write_text("", encoding="utf-8")
                if marker is not None:
                    (run / "coverage-mode.txt").write_text(marker + "\n", encoding="utf-8")
                report = ANALYZER.analyze(run)
                self.assertEqual(report["texture_coverage_state"], expected_state)
                self.assertEqual(report["texture_views"], expected_views)
                self.assertEqual(report["texture_view_totals"], {} if marker == "enabled" else None)
                self.assertEqual(report["texture_view_measurement"], "descriptor_materialization_attempts")

    def test_legacy_log_rows_prove_collection_and_preserve_counts(self):
        with tempfile.TemporaryDirectory() as directory:
            run = Path(directory)
            (run / "stderr.log").write_text(
                "2026-11-28T09:00:00Z INFO victory_gpu::host: "
                'kind="srv" title=28 requested=28 host=28 outcome="made" count=3 texture view coverage\n',
                encoding="utf-8",
            )
            report = ANALYZER.analyze(run)
            self.assertEqual(report["texture_coverage_state"], "enabled")
            self.assertEqual(report["texture_view_totals"], {"made": 3})
            self.assertEqual(report["texture_views"][0]["count"], 3)


class UsedNullBufferReportTests(unittest.TestCase):
    def test_world_budget_and_other_gate_ignores_boot_and_invalid_optional_slots(self):
        fps = ("2.0 fps, 500.0 ms/frame (100.0 preparing: copies 10.0 ms 10.0 MB, "
               "textures 1.0 ms 1.0 MB; 20.0 on the GPU; 5.0 copying back 0.0 MB), "
               "1000 draws/frame, dropped 0 without a host pipeline (0 being made) "
               "and 50 indirect; 20 root-view slots/frame")

        def null_row(stamp, budget=0, other=0, invalid=0):
            return (f"{stamp} INFO victory_gpu::descriptor::host: "
                    "actual shader-use sites with null host buffer descriptors "
                    "(events, not unique slots) "
                    f"table_budget={budget} table_invalid_or_empty={invalid} table_other={other} "
                    "observed_heap_budget=0 observed_heap_invalid_or_empty=0 observed_heap_other=0")

        for budget, other, second_report, expected_state, expected_failure in (
            (0, 0, True, "complete", False),
            (2, 0, True, "complete", True),
            (0, 1, True, "complete", True),
            (0, 0, False, "incomplete", True),
        ):
            with self.subTest(budget=budget, other=other, second_report=second_report), tempfile.TemporaryDirectory() as directory:
                run = Path(directory)
                lines = [
                    "2026-11-28T08:59:59Z INFO victory_gpu::host: " + fps,
                    null_row("2026-11-28T08:59:59Z", budget=100),
                    "2026-11-28T09:00:00Z INFO victory_gpu::capture: the world's first frame frame=200 draws=600",
                    "2026-11-28T09:00:01Z INFO victory_gpu::host: " + fps,
                    null_row("2026-11-28T09:00:02Z", budget=budget, other=other, invalid=3),
                    "2026-11-28T09:00:03Z INFO victory_gpu::host: " + fps,
                ]
                if second_report:
                    lines.append(null_row("2026-11-28T09:00:04Z", invalid=4))
                (run / "stderr.log").write_text("\n".join(lines) + "\n", encoding="utf-8")
                (run / "result.txt").write_text("reason=world timing window complete\n", encoding="utf-8")
                report = ANALYZER.analyze(run)
                nulls = report["world_used_null_buffer_descriptors"]
                self.assertEqual(nulls["coverage_state"], expected_state)
                self.assertEqual(nulls["budget_total"], budget)
                self.assertEqual(nulls["other_total"], other)
                self.assertEqual(nulls["invalid_or_empty_total"], 7 if second_report else 3)
                self.assertEqual(ANALYZER.has_used_budget_nulls(report), expected_failure)

    def test_no_probe_rows_cannot_certify_zero_used_budget_nulls(self):
        with tempfile.TemporaryDirectory() as directory:
            run = Path(directory)
            (run / "stderr.log").write_text("\n".join([
                "2026-11-28T09:00:00Z INFO victory_gpu::capture: the world's first frame frame=200 draws=600",
                "2026-11-28T09:00:01Z INFO victory_gpu::host: 2.0 fps, 500.0 ms/frame (100.0 preparing: copies 10.0 ms 10.0 MB, textures 1.0 ms 1.0 MB; 20.0 on the GPU; 5.0 copying back 0.0 MB), 1000 draws/frame, dropped 0 without a host pipeline (0 being made) and 50 indirect; 20 root-view slots/frame",
            ]) + "\n", encoding="utf-8")
            (run / "result.txt").write_text("reason=world timing window complete\n", encoding="utf-8")
            report = ANALYZER.analyze(run)
            self.assertEqual(report["world_used_null_buffer_descriptors"]["coverage_state"], "unavailable")
            self.assertTrue(ANALYZER.has_used_budget_nulls(report))


class UsedNullTextureReportTests(unittest.TestCase):
    def test_world_texture_null_uses_are_separate_from_materialization_attempts(self):
        fps = ("2.0 fps, 500.0 ms/frame (100.0 preparing: copies 10.0 ms 10.0 MB, "
               "textures 1.0 ms 1.0 MB; 20.0 on the GPU; 5.0 copying back 0.0 MB), "
               "1000 draws/frame, dropped 0 without a host pipeline (0 being made) "
               "and 50 indirect; 20 root-view slots/frame")

        def row(stamp, table=0, heap=0, invalid=0):
            return (f"{stamp} INFO victory_gpu::descriptor::host: "
                    f"table_host_copy_unavailable={table} table_placed_texture_unavailable=0 "
                    "table_format_incompatible=0 table_other=0 table_invalid_or_empty=0 "
                    f"observed_heap_host_copy_unavailable={heap} "
                    "observed_heap_placed_texture_unavailable=0 "
                    "observed_heap_format_incompatible=0 observed_heap_other=0 "
                    f"observed_heap_invalid_or_empty={invalid} "
                    "shader-used texture slots with null host views (events, not unique slots)")

        with tempfile.TemporaryDirectory() as directory:
            run = Path(directory)
            lines = [
                "2026-11-28T08:59:59Z INFO victory_gpu::host: " + fps,
                row("2026-11-28T08:59:59Z", table=100),
                "2026-11-28T09:00:00Z INFO victory_gpu::capture: the world's first frame frame=200 draws=600",
                "2026-11-28T09:00:01Z INFO victory_gpu::host: " + fps,
                row("2026-11-28T09:00:02Z", table=2, heap=3),
                "2026-11-28T09:00:03Z INFO victory_gpu::host: " + fps,
                row("2026-11-28T09:00:04Z"),
            ]
            (run / "stderr.log").write_text("\n".join(lines) + "\n", encoding="utf-8")
            report = ANALYZER.analyze(run)
            nulls = report["world_used_null_texture_descriptors"]
            self.assertEqual(nulls["coverage_state"], "complete")
            self.assertEqual(nulls["reports"], 2)
            self.assertEqual(nulls["total"], 5)
            self.assertEqual(nulls["classification_state"], "complete")
            self.assertEqual(nulls["valid_missing_total"], 5)
            self.assertEqual(nulls["invalid_or_empty_total"], 0)
            self.assertEqual(nulls["table_host_copy_unavailable"], 2)
            self.assertEqual(nulls["observed_heap_host_copy_unavailable"], 3)
            (run / "stderr.log").write_text("\n".join(lines[:-1]) + "\n", encoding="utf-8")
            self.assertEqual(ANALYZER.analyze(run)["world_used_null_texture_descriptors"]["coverage_state"], "incomplete")

    def test_zero_address_texture_nulls_are_separate_from_valid_misses(self):
        fps = ("2.0 fps, 500.0 ms/frame (100.0 preparing: copies 10.0 ms 10.0 MB, "
               "textures 1.0 ms 1.0 MB; 20.0 on the GPU; 5.0 copying back 0.0 MB), "
               "1000 draws/frame, dropped 0 without a host pipeline (0 being made) "
               "and 50 indirect; 20 root-view slots/frame")
        with tempfile.TemporaryDirectory() as directory:
            run = Path(directory)
            prefix = [
                "2026-11-28T09:00:00Z INFO victory_gpu::capture: the world's first frame frame=200 draws=600",
                "2026-11-28T09:00:01Z INFO victory_gpu::host: " + fps,
            ]
            tail = "shader-used texture slots with null host views (events, not unique slots)"
            classified = ("2026-11-28T09:00:02Z WARN victory_gpu::descriptor::host: "
                          "table_host_copy_unavailable=0 table_placed_texture_unavailable=0 "
                          "table_format_incompatible=0 table_other=0 table_invalid_or_empty=0 "
                          "observed_heap_host_copy_unavailable=0 observed_heap_placed_texture_unavailable=0 "
                          "observed_heap_format_incompatible=0 observed_heap_other=0 "
                          "observed_heap_invalid_or_empty=3 " + tail)
            (run / "stderr.log").write_text("\n".join(prefix + [classified]) + "\n", encoding="utf-8")
            nulls = ANALYZER.analyze(run)["world_used_null_texture_descriptors"]
            self.assertEqual(nulls["classification_state"], "complete")
            self.assertEqual(nulls["total"], 3)
            self.assertEqual(nulls["invalid_or_empty_total"], 3)
            self.assertEqual(nulls["valid_missing_total"], 0)

            legacy = classified.replace("table_invalid_or_empty=0 ", "").replace(
                "observed_heap_invalid_or_empty=3 ", "observed_heap_placed_texture_unavailable=3 ")
            legacy = legacy.replace("observed_heap_placed_texture_unavailable=0 ", "")
            (run / "stderr.log").write_text("\n".join(prefix + [legacy]) + "\n", encoding="utf-8")
            nulls = ANALYZER.analyze(run)["world_used_null_texture_descriptors"]
            self.assertEqual(nulls["classification_state"], "legacy_unclassified")
            self.assertIsNone(nulls["valid_missing_total"])


if __name__ == "__main__":
    unittest.main()
