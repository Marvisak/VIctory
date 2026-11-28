"""Checks RIP reference discovery, including stores with trailing immediates."""

import importlib.util
import struct
import unittest
from pathlib import Path

import capstone


spec = importlib.util.spec_from_file_location(
    "find_pe_rip_refs", Path(__file__).with_name("find-pe-rip-refs.py"))
finder = importlib.util.module_from_spec(spec)
spec.loader.exec_module(finder)


class RipReferenceTests(unittest.TestCase):
    def test_reads_and_immediate_stores(self):
        section_va = 0x1000
        target = 0x2000
        code = bytearray()

        def append(opcode, immediate=b""):
            address = section_va + len(code)
            size = len(opcode) + 4 + len(immediate)
            code.extend(opcode)
            code.extend(struct.pack("<i", target - address - size))
            code.extend(immediate)
            return address

        read = append(b"\x8b\x05")
        store_byte = append(b"\xc6\x05", b"\x01")
        store_dword = append(b"\xc7\x05", b"\x01\x00\x00\x00")
        code.extend(b"\x90" * 16)

        decoder = capstone.Cs(capstone.CS_ARCH_X86, capstone.CS_MODE_64)
        decoder.detail = True
        found = finder.references_in_code(bytes(code), section_va, [target], decoder)
        self.assertEqual({address for address, _, _ in found},
                         {read, store_byte, store_dword})
        self.assertEqual(finder.references_in_code(bytes(code), section_va,
                                                   [target + 4], decoder), set())


if __name__ == "__main__":
    unittest.main()
