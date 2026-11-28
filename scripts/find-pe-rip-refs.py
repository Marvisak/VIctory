"""Find candidate x64 RIP-relative references without full disassembly.

Usage: python scripts/find-pe-rip-refs.py TITLE.exe 0x14a9438ec 0x14a9438f0
The hit address is a candidate instruction start. Confirm hits in a debugger or
from a known function boundary; x86 variable-length code can overlap.
"""

from __future__ import annotations

import argparse
from pathlib import Path

import capstone
import numpy as np
import pefile


def references(path: Path, targets: list[int]) -> list[tuple[int, int, str]]:
    pe = pefile.PE(str(path), fast_load=True)
    image_base = pe.OPTIONAL_HEADER.ImageBase
    decoder = capstone.Cs(capstone.CS_ARCH_X86, capstone.CS_MODE_64)
    decoder.detail = True
    found: set[tuple[int, int, str]] = set()
    for section in pe.sections:
        if not section.Characteristics & 0x20000000:  # IMAGE_SCN_MEM_EXECUTE
            continue
        found.update(references_in_code(section.get_data(), image_base + section.VirtualAddress,
                                        targets, decoder))
    return sorted(found)


def references_in_code(data: bytes, section_va: int, targets: list[int],
                       decoder: capstone.Cs) -> set[tuple[int, int, str]]:
    """Find RIP operands, including stores with an immediate after the displacement."""
    found: set[tuple[int, int, str]] = set()
    # The displacement occupies four consecutive bytes, but is not always
    # the instruction's final field: `mov [rip+disp32], imm8/imm32` writes
    # exactly the globals a resolution investigation needs to find.
    for start in range(0, max(0, len(data) - 3), 1_000_000):
        count = min(1_000_000, len(data) - 3 - start)
        bytes4 = np.frombuffer(data, dtype=np.uint8, count=count + 3, offset=start)
        word = (bytes4[:count].astype(np.uint32)
                | (bytes4[1:count + 1].astype(np.uint32) << 8)
                | (bytes4[2:count + 2].astype(np.uint32) << 16)
                | (bytes4[3:count + 3].astype(np.uint32) << 24))
        offsets = np.arange(start, start + count, dtype=np.uint32)
        for target in targets:
            for after in (0, 1, 2, 4, 8):
                expected = np.uint32((target - section_va - 4 - after) & 0xffffffff)
                for match in np.flatnonzero(word + offsets == expected):
                    displacement_at = start + int(match)
                    # Candidate data may be an immediate or arbitrary bytes.
                    # Decode nearby starts and keep actual RIP memory operands.
                    candidates = []
                    for instruction_at in range(max(0, displacement_at - 15), displacement_at):
                        for insn in decoder.disasm(data[instruction_at:displacement_at + 20],
                                                  section_va + instruction_at, count=1):
                            if instruction_at + insn.disp_offset != displacement_at:
                                continue
                            if insn.disp_size != 4 or insn.size - insn.disp_offset - 4 != after:
                                continue
                            if not any(operand.type == capstone.x86.X86_OP_MEM
                                       and operand.mem.base == capstone.x86.X86_REG_RIP
                                       for operand in insn.operands):
                                continue
                            if insn.address + insn.size + insn.disp != target:
                                continue
                            candidates.append((insn.address, target,
                                               f"{insn.mnemonic} {insn.op_str}"))
                    if candidates:
                        found.add(min(candidates))
    return found


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("pe", type=Path)
    parser.add_argument("targets", type=lambda value: int(value, 0), nargs="+")
    args = parser.parse_args()
    for address, target, instruction in references(args.pe, args.targets):
        print(f"{address:#x} -> {target:#x}: {instruction}")


if __name__ == "__main__":
    main()
