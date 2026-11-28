"""Copy supplied PE images and redirect their static network imports.

The output must be outside this repository. Game assets are not copied.
Requires the third-party `pefile` package.
"""

from __future__ import annotations

import argparse
import os
import shutil
import struct
from pathlib import Path

import pefile


ALIASES = {
    "ws2_32.dll": ("VS2_32.dll", "WS2_32"),
    "winhttp.dll": ("VINHTTP.dll", "WINHTTP"),
    "api-ms-win-core-windowserrorreporting-l1-1-0.dll": (
        "vpi-ms-win-core-windowserrorreporting-l1-1-0.dll",
        "api-ms-win-core-windowserrorreporting-l1-1-0",
    ),
}
GUARDS = {
    "ws2_32.dll": {
        "connect": "VictoryGuardConnect",
        "bind": "VictoryGuardBind",
        "sendto": "VictoryGuardSendTo",
        "getaddrinfo": "VictoryGuardGetAddrInfo",
        "gethostbyname": "VictoryGuardGetHostByName",
    },
    "winhttp.dll": {
        "WinHttpConnect": "VictoryGuardWinHttpConnect",
        "WinHttpOpen": "VictoryGuardWinHttpOpen",
        "WinHttpSetOption": "VictoryGuardWinHttpSetOption",
    },
    "api-ms-win-core-windowserrorreporting-l1-1-0.dll": {
        "WerRegisterFile": "VictoryWerRegisterFile",
    },
}

# The title's GDK stub loads xgameruntime.dll with SYSTEM32-only search on an
# unpackaged PC. Add APPLICATION_DIR to that one call so our local offline
# runtime can satisfy it. The signature and pointed-to name must both match.
RUNTIME_LOAD_SIGNATURE = bytes.fromhex("41 b8 00 08 00 00 48 8b 0d")
RUNTIME_NAME = "xgameruntime.dll".encode("utf-16le") + b"\0\0"


def within(path: Path, parent: Path) -> bool:
    return path == parent or parent in path.parents


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("game_dir", type=Path)
    parser.add_argument("output_dir", type=Path)
    args = parser.parse_args()
    game = args.game_dir.resolve(strict=True)
    output = args.output_dir.resolve()
    repo = Path(__file__).resolve().parent.parent
    if within(output, repo) or within(output, game) or within(game, output):
        parser.error("output must be outside the repository and game folder")

    imports: dict[str, dict[str, int | None]] = {name: {} for name in ALIASES}
    host_ordinals: dict[str, dict[int, str]] = {}
    for original in ALIASES:
        host_path = Path(os.environ["SystemRoot"]) / "System32" / original
        if host_path.is_file():
            host_pe = pefile.PE(str(host_path), fast_load=True)
            host_pe.parse_data_directories(
                directories=[pefile.DIRECTORY_ENTRY["IMAGE_DIRECTORY_ENTRY_EXPORT"]]
            )
            host_ordinals[original] = {
                export.ordinal: export.name.decode("ascii")
                for export in host_pe.DIRECTORY_ENTRY_EXPORT.symbols
                if export.name is not None
            }
        else:
            host_ordinals[original] = {}
    patched = 0
    runtime_patched = 0
    copied = 0
    signed_patched: list[str] = []
    for source in game.rglob("*"):
        if not source.is_file() or source.suffix.lower() not in {".exe", ".dll"}:
            continue
        relative = source.relative_to(game)
        destination = output / relative
        destination.parent.mkdir(parents=True, exist_ok=True)
        pe = pefile.PE(str(source), fast_load=True)
        pe.parse_data_directories(
            directories=[
                pefile.DIRECTORY_ENTRY["IMAGE_DIRECTORY_ENTRY_IMPORT"],
                pefile.DIRECTORY_ENTRY["IMAGE_DIRECTORY_ENTRY_DELAY_IMPORT"],
            ]
        )
        for delayed in getattr(pe, "DIRECTORY_ENTRY_DELAY_IMPORT", []):
            if delayed.dll.decode("ascii").lower() in ALIASES:
                raise RuntimeError(f"network delay import needs handling: {relative}")
        changed = False
        for entry in getattr(pe, "DIRECTORY_ENTRY_IMPORT", []):
            name = entry.dll.decode("ascii").lower()
            if name not in ALIASES:
                continue
            alias, _host = ALIASES[name]
            original = entry.dll
            replacement = alias.encode("ascii")
            if len(original) != len(replacement):
                raise RuntimeError(f"import alias length mismatch: {name}")
            for symbol in entry.imports:
                if symbol.name is None:
                    imported = host_ordinals[name].get(symbol.ordinal)
                    if imported is None:
                        raise RuntimeError(f"unknown network ordinal: {relative}")
                else:
                    imported = symbol.name.decode("ascii")
                ordinal = symbol.ordinal
                previous = imports[name].get(imported)
                if previous is not None and ordinal is not None and previous != ordinal:
                    raise RuntimeError(f"conflicting import ordinals: {imported}")
                imports[name][imported] = ordinal if ordinal is not None else previous
            offset = pe.get_offset_from_rva(entry.struct.Name)
            pe.set_bytes_at_offset(offset, replacement)
            changed = True
        if relative.name.lower() == "game_scarlett_master.exe":
            offset = pe.__data__.find(RUNTIME_LOAD_SIGNATURE)
            if offset < 0 or pe.__data__.find(RUNTIME_LOAD_SIGNATURE, offset + 1) >= 0:
                raise RuntimeError("GDK runtime load signature missing or ambiguous")
            instruction_rva = pe.get_rva_from_offset(offset)
            displacement = struct.unpack_from("<i", pe.__data__, offset + 9)[0]
            pointer_rva = instruction_rva + 13 + displacement
            runtime_va = struct.unpack("<Q", pe.get_data(pointer_rva, 8))[0]
            runtime_rva = runtime_va - pe.OPTIONAL_HEADER.ImageBase
            if pe.get_data(runtime_rva, len(RUNTIME_NAME)).lower() != RUNTIME_NAME:
                raise RuntimeError("GDK runtime load target is not xgameruntime.dll")
            pe.set_bytes_at_offset(offset + 3, b"\x0a")  # 0x800 -> 0xa00
            changed = True
            runtime_patched += 1
        if changed:
            certificate = pe.OPTIONAL_HEADER.DATA_DIRECTORY[
                pefile.DIRECTORY_ENTRY["IMAGE_DIRECTORY_ENTRY_SECURITY"]
            ]
            if certificate.Size:
                signed_patched.append(str(relative))
            pe.write(filename=str(destination))
            shutil.copystat(source, destination)
            patched += 1
        else:
            shutil.copy2(source, destination)
        copied += 1

    for original, (alias, host) in ALIASES.items():
        lines = [f"LIBRARY {alias}", "EXPORTS"]
        for symbol, ordinal in sorted(imports[original].items()):
            target = GUARDS[original].get(symbol)
            forward = (
                f"victory_native_runtime.{target}" if target else f"{host}.{symbol}"
            )
            suffix = f" @{ordinal}" if ordinal is not None else ""
            lines.append(f"{symbol}={forward}{suffix}")
        (output / f"{Path(alias).stem}.def").write_text(
            "\n".join(lines) + "\n", encoding="ascii"
        )
    print(f"copied {copied} PE images, patched {patched} PE import tables")
    print(f"redirected {runtime_patched} GDK runtime load to application directory")
    if signed_patched:
        print("signatures invalidated in staged copies: " + ", ".join(signed_patched))
    for original, symbols in imports.items():
        print(f"{ALIASES[original][0]}: {len(symbols)} imports")


if __name__ == "__main__":
    main()
