"""Print lighting snapshots in completed host submission order, with float ranges."""

import re
import sys
from pathlib import Path


def main() -> None:
    lines = Path(sys.argv[1]).read_text(encoding="utf-8", errors="replace").splitlines()
    values = ""
    for line in lines:
        if "target values format=10 " in line:
            values = line.split("target values format=10 ", 1)[1]
        if "lighting output snapshot saved phase=" not in line:
            continue
        phase = re.search(r'phase="([^"]+)"', line)
        frame = re.search(r"frame=(\d+)", line)
        submission = re.search(r"submission=(\d+)", line)
        list_index = re.search(r"list_index=(\d+)", line)
        print(
            line[:19],
            f"frame={frame.group(1) if frame else '?'}",
            f"sub={submission.group(1) if submission else '?'}",
            f"list={list_index.group(1) if list_index else '?'}",
            f"phase={phase.group(1) if phase else '?'}",
            values,
        )


if __name__ == "__main__":
    main()
