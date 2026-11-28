"""Checks that material-role joins keep conflicts and unknowns for review."""

import csv
import gzip
import importlib.util
import json
import tempfile
import unittest
from pathlib import Path

spec = importlib.util.spec_from_file_location(
    "join_material_roles", Path(__file__).with_name("join-material-roles.py"))
joiner = importlib.util.module_from_spec(spec)
spec.loader.exec_module(joiner)


class JoinTests(unittest.TestCase):
    def test_slots_and_conflicts(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            users = root / "users.jsonl.gz"
            with gzip.open(users, "wt", encoding="utf-8") as stream:
                for row in [
                    {"texture_users": 1},
                    {"m": ["shader", [["diffusetex", "0000000000000001"],
                                        ["bumptex", "0000000000000002"]]]},
                    {"m": ["shader2", [["bumptex", "0000000000000001"]]]},
                    {"m": ["shader3", [["diffusetex2", "0000000000000004"],
                                         ["bumptexture2", "0000000000000005"]]]},
                    {"id": "model", "mats": [0, 1, 2]},
                ]:
                    stream.write(json.dumps(row) + "\n")
            census = root / "census.csv"
            with census.open("w", newline="", encoding="utf-8") as stream:
                writer = csv.writer(stream)
                writer.writerow(["store", "hash", "width", "height", "dxgi_format",
                                 "size", "role", "tier"])
                for hash64 in range(1, 6):
                    writer.writerow(["store", f"{hash64:016x}", 1024, 1024,
                                     71, 100, "", ""])
            output = root / "joined.csv"
            with self.assertRaisesRegex(ValueError, "output must differ"):
                joiner.joined(census, users, census)
            summary = joiner.joined(census, users, output)
            self.assertEqual(summary["candidate_counts"],
                             {"conflict": 1, "normal": 2, "unknown": 1,
                              "albedo": 1})
            with output.open(newline="", encoding="utf-8") as stream:
                rows = list(csv.DictReader(stream))
            self.assertEqual(rows[0]["material_slots"], "bumptex;diffusetex")
            self.assertEqual(rows[1]["role_candidate"], "normal")


if __name__ == "__main__":
    unittest.main()
