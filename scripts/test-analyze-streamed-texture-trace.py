"""Synthetic checks for zero and recovery accounting in the trace analyzer."""

import importlib.util
from pathlib import Path
import unittest

import blake3


SOURCE = Path(__file__).with_name("analyze-streamed-texture-trace.py")
SPEC = importlib.util.spec_from_file_location("analyze_streamed_texture_trace", SOURCE)
analyzer = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(analyzer)


def detile(hash_value: str, *, zero: bool, world: bool) -> str:
    return ("INFO large sampled texture detiled for upload address=0x1000 "
            "title_width=2048 title_height=2048 title_format=71 title_mips=10 "
            "host_width=8 host_height=8 host_format=71 host_mips=9 "
            f"dropped=1 source_mip=1 block_bytes=8 hash={hash_value} "
            f"all_zero={str(zero).lower()} world_started={str(world).lower()}")


class AnalyzeTraceTests(unittest.TestCase):
    def test_zero_then_populated_with_copy_recording(self) -> None:
        empty = blake3.blake3(bytes(32)).hexdigest()
        populated = blake3.blake3(bytes([1] * 32)).hexdigest()
        lines = [
            detile(empty, zero=True, world=False),
            "INFO sampled texture upload recorded address=0x1000 "
            f"hash={empty} all_zero=true copy_confirmed=true",
            detile(populated, zero=False, world=True),
            "INFO sampled texture upload recorded address=0x1000 "
            f"hash={populated} all_zero=false copy_confirmed=true",
            "INFO zero-copied sampled texture source audit address=0x1000 "
            "source_now_zero=true later_copies=1 stale_candidate=false",
        ]
        summary = analyzer.summarize(lines)
        self.assertEqual(summary["detile_entries"], 2)
        self.assertEqual(summary["world_zero_entries"], 0)
        self.assertEqual(summary["same_shape_address_zero_then_nonzero"], 1)
        self.assertEqual(summary["zero_copy_recorded"], 1)
        self.assertEqual(summary["nonzero_copy_recorded"], 1)
        self.assertEqual(summary["zero_source_audits"], 1)
        self.assertEqual(summary["audit_zero_after_later_copy"], 1)
        self.assertEqual(summary["audit_stale_candidates"], 0)

    def test_incorrect_zero_label_is_rejected(self) -> None:
        empty = blake3.blake3(bytes(32)).hexdigest()
        with self.assertRaises(ValueError):
            analyzer.summarize([detile(empty, zero=False, world=True)])


if __name__ == "__main__":
    unittest.main()
