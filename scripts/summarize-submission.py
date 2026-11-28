"""Summarize diagnostic command notes for one host submission."""

from collections import Counter, defaultdict
from pathlib import Path
import re
import sys


def main() -> None:
    lines = Path(sys.argv[1]).read_text(encoding="utf-8", errors="replace").splitlines()
    wanted = int(sys.argv[2])
    first_list = int(sys.argv[3]) if len(sys.argv) > 3 else 0
    last_list = int(sys.argv[4]) if len(sys.argv) > 4 else 1_000_000
    brief = "--brief" in sys.argv[5:]
    commands: dict[int, list[tuple[int, str]]] = defaultdict(list)
    pattern = re.compile(rf"s{wanted} l(\d+)/(\d+) #(\d+): (.*)")
    for line in lines:
        match = pattern.search(line)
        if match:
            commands[int(match.group(1))].append((int(match.group(3)), match.group(4).split(" | ", 1)[0]))
    for index, ops in sorted(commands.items()):
        if not first_list <= index <= last_list:
            continue
        kinds = Counter(op.split(" ", 1)[0] for _, op in ops)
        print(f"list={index} commands={len(ops)} kinds={dict(kinds)}")
        for number, op in ops:
            if not brief and op.startswith(("copy ", "dispatch ", "draw ", "indexed ", "DROPPED")):
                print(f"  #{number} {op[:180]}")


if __name__ == "__main__":
    main()
