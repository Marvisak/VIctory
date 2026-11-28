"""Compare VIctory's bounded BC1/BC3 mip chains and render them as PNGs.

Usage: py -3 scripts/analyze-lighting-bc.py target/test-runs/<run>/lighting-bc
Uses only the Python standard library. The source detiler below is independent
of the Rust layout and serves as a check on the uploaded linear BC bytes.
"""

import argparse
import struct
import zlib
from pathlib import Path


CASES = {0: (2048, "BC1"), 1: (1024, "BC3"), 2: (512, "BC3"), 3: (256, "BC1")}
MIPS = {0: 12, 1: 11, 2: 10, 3: 1}


def tiled_offset(x: int, y: int, blocks: int, bc: str) -> int:
    """GFX10 SW_4KB_S tile order and RB+ element bits for 64/128 bpp."""
    tile_w = 32 if bc == "BC1" else 16
    tile = (y // 16) * (blocks // tile_w) + x // tile_w
    x %= tile_w
    y %= 16
    bit = lambda n, source, target: ((n >> source) & 1) << target
    if bc == "BC1":
        within = (bit(x, 0, 3) | bit(y, 0, 4) | bit(y, 1, 5)
                  | bit(x, 1, 6) | bit(x, 2, 7) | bit(y, 2, 8)
                  | bit(x, 3, 9) | bit(y, 3, 10) | bit(x, 4, 11))
    else:
        within = (bit(y, 0, 4) | bit(y, 1, 5) | bit(x, 0, 6)
                  | bit(x, 1, 7) | bit(y, 2, 8) | bit(x, 2, 9)
                  | bit(y, 3, 10) | bit(x, 3, 11))
    return tile * 4096 + within


def detile_reference(source: bytes, width: int, bc: str) -> bytes:
    blocks = width // 4
    bpe = 8 if bc == "BC1" else 16
    output = bytearray(len(source))
    for y in range(blocks):
        for x in range(blocks):
            start = tiled_offset(x, y, blocks, bc)
            end = (y * blocks + x) * bpe
            output[end:end + bpe] = source[start:start + bpe]
    return bytes(output)


def rgb565(value: int) -> tuple[int, int, int]:
    r, g, b = (value >> 11) & 31, (value >> 5) & 63, value & 31
    return ((r * 255 + 15) // 31, (g * 255 + 31) // 63, (b * 255 + 15) // 31)


def color_palette(block: bytes, bc3: bool) -> list[tuple[int, int, int, int]]:
    c0, c1 = struct.unpack_from("<HH", block)
    a, b = rgb565(c0), rgb565(c1)
    colors = [(*a, 255), (*b, 255)]
    if bc3 or c0 > c1:
        colors += [tuple((2 * a[i] + b[i]) // 3 for i in range(3)) + (255,),
                   tuple((a[i] + 2 * b[i]) // 3 for i in range(3)) + (255,)]
    else:
        colors += [tuple((a[i] + b[i]) // 2 for i in range(3)) + (255,), (0, 0, 0, 0)]
    return colors


def alpha_palette(block: bytes) -> list[int]:
    a0, a1 = block[:2]
    if a0 > a1:
        return [a0, a1] + [((7 - i) * a0 + i * a1) // 7 for i in range(1, 7)]
    return [a0, a1] + [((5 - i) * a0 + i * a1) // 5 for i in range(1, 5)] + [0, 255]


def decode_bc(linear: bytes, width: int, bc: str) -> bytearray:
    blocks = width // 4
    bpe = 8 if bc == "BC1" else 16
    pixels = bytearray(width * width * 4)
    for by in range(blocks):
        for bx in range(blocks):
            start = (by * blocks + bx) * bpe
            block = linear[start:start + bpe]
            color = block[8:] if bc == "BC3" else block
            palette = color_palette(color[:4], bc == "BC3")
            indices = int.from_bytes(color[4:8], "little")
            alphas = alpha_palette(block) if bc == "BC3" else None
            alpha_indices = int.from_bytes(block[2:8], "little") if alphas else 0
            for p in range(16):
                rgba = palette[(indices >> (2 * p)) & 3]
                if alphas is not None:
                    rgba = (*rgba[:3], alphas[(alpha_indices >> (3 * p)) & 7])
                x, y = bx * 4 + p % 4, by * 4 + p // 4
                pixels[(y * width + x) * 4:(y * width + x + 1) * 4] = bytes(rgba)
    return pixels


def write_png(path: Path, pixels: bytes, width: int) -> None:
    def chunk(tag: bytes, data: bytes) -> bytes:
        return struct.pack(">I", len(data)) + tag + data + struct.pack(">I", zlib.crc32(tag + data))

    stride = width * 4
    rows = b"".join(b"\0" + pixels[y * stride:(y + 1) * stride] for y in range(width))
    png = (b"\x89PNG\r\n\x1a\n"
           + chunk(b"IHDR", struct.pack(">IIBBBBB", width, width, 8, 6, 0, 0, 0))
           + chunk(b"IDAT", zlib.compress(rows, level=1))
           + chunk(b"IEND", b""))
    path.write_bytes(png)


def seam_stats(pixels: bytes, width: int, x_tile: int, y_tile: int) -> dict[str, tuple[float, float, float, int] | None]:
    def difference(x1: int, y1: int, x2: int, y2: int) -> int:
        a = (y1 * width + x1) * 4
        b = (y2 * width + x2) * 4
        return sum(abs(pixels[a + k] - pixels[b + k]) for k in range(3))

    # A tile edge is also a 4-pixel BC block edge. Compare like with like:
    # the four nearby BC block edges at -8, -4, +4 and +8 pixels. Comparing
    # against adjacent pixel pairs inside a BC block exaggerates tile seams.
    controls = (-8, -4, 4, 8)
    results = {}
    for axis, tile in (("vertical", x_tile), ("horizontal", y_tile)):
        # A single edge between only two tiles is easily confounded by scene
        # content. Require two independent tile edges in this direction.
        if width < 3 * tile:
            results[axis] = None
            continue
        edge = nearby = count = 0
        for at in range(tile, width, tile):
            for other in range(width):
                if axis == "vertical":
                    edge += difference(at - 1, other, at, other)
                    nearby += sum(difference(at + offset - 1, other, at + offset, other)
                                  for offset in controls)
                else:
                    edge += difference(other, at - 1, other, at)
                    nearby += sum(difference(other, at + offset - 1, other, at + offset)
                                  for offset in controls)
                count += 1
        edge_mean = edge / (count * 3) if count else 0.0
        nearby_mean = nearby / (count * len(controls) * 3) if count else 0.0
        results[axis] = (edge_mean, nearby_mean, edge_mean / nearby_mean if nearby_mean else float("inf"), count)
    return results


def print_seams(pixels: bytes, width: int, bc: str, indent: str = "    ") -> None:
    # Each 4 KiB tile remains 32x16 BC1 blocks or 16x16 BC3 blocks at
    # every non-tail mip; only the mip's number of tiles decreases.
    x_tile, y_tile = (128, 64) if bc == "BC1" else (64, 64)
    for axis, stats in seam_stats(pixels, width, x_tile, y_tile).items():
        if stats is None:
            print(f"{indent}{axis} tile edge: skipped (fewer than 3 full tiles)")
            continue
        edge, nearby, ratio, count = stats
        print(f"{indent}{axis} tile edge RGB mean={edge:.3f}, nearby BC block edges={nearby:.3f}, ratio={ratio:.3f}, pairs={count}")


def analyze(manifest: Path) -> None:
    details = dict(line.split("=", 1) for line in manifest.read_text().splitlines() if "=" in line)
    index, width, bc = int(details["index"]), int(details["width"]), details["format"]
    if CASES.get(index) != (width, bc):
        raise ValueError(f"unexpected lighting source in {manifest}: {index}, {width}, {bc}")
    stem = manifest.with_suffix("")
    source = Path(f"{stem}-tiled.bc").read_bytes()
    linear = Path(f"{stem}-linear.bc").read_bytes()
    expected_size = width * width // 16 * (8 if bc == "BC1" else 16)
    if len(source) != expected_size or len(linear) != expected_size:
        raise ValueError(f"unexpected mip-0 size in {manifest}: {len(source)}, {len(linear)}")
    reference = detile_reference(source, width, bc)
    mismatches = sum(a != b for a, b in zip(reference, linear))
    print(f"{manifest.name}: independent detile mismatched bytes={mismatches}/{expected_size}")
    gpu_path = Path(f"{stem}-gpu.bc")
    if gpu_path.is_file():
        gpu = gpu_path.read_bytes()
        if len(gpu) != expected_size:
            raise ValueError(f"unexpected GPU mip-0 size in {gpu_path}: {len(gpu)}")
        gpu_mismatches = sum(a != b for a, b in zip(gpu, linear))
        print(f"  GPU readback mismatched staged bytes={gpu_mismatches}/{expected_size}")
        if gpu_mismatches:
            output = Path(f"{stem}-gpu.png")
            gpu_pixels = decode_bc(gpu, width, bc)
            write_png(output, gpu_pixels, width)
            print(f"  GPU: {output}")
            print_seams(gpu_pixels, width, bc, "    GPU ")
    for name, bc_bytes in (("linear", linear), ("source-reference", reference)):
        if name == "source-reference" and not mismatches:
            continue
        pixels = decode_bc(bc_bytes, width, bc)
        output = Path(f"{stem}-{name}.png")
        write_png(output, pixels, width)
        print(f"  {name}: {output}")
        print_seams(pixels, width, bc)
    mip_base = stem.name.rsplit("mip", 1)[0]
    if not Path(f"{stem.with_name(mip_base + 'mip1')}-linear.bc").is_file():
        return  # Older mip-0-only diagnostic run.
    for mip in range(1, MIPS[index]):
        mip_stem = stem.with_name(mip_base + f"mip{mip}")
        linear_path = Path(f"{mip_stem}-linear.bc")
        gpu_path = Path(f"{mip_stem}-gpu.bc")
        if not linear_path.is_file() or not gpu_path.is_file():
            print(f"  mip {mip}: staged or GPU bytes missing")
            continue
        edge = max(1, width >> mip)
        blocks = max(1, (edge + 3) // 4)
        expected_size = blocks * blocks * (8 if bc == "BC1" else 16)
        staged, gpu = linear_path.read_bytes(), gpu_path.read_bytes()
        if len(staged) != expected_size or len(gpu) != expected_size:
            raise ValueError(f"unexpected mip {mip} size: staged={len(staged)}, GPU={len(gpu)}, expected={expected_size}")
        mismatch = sum(a != b for a, b in zip(staged, gpu))
        print(f"  mip {mip} ({edge}x{edge}): GPU mismatched staged bytes={mismatch}/{expected_size}")
        if edge >= 4 and (mip <= 4 or mismatch):
            output = Path(f"{mip_stem}-linear.png")
            staged_pixels = decode_bc(staged, edge, bc)
            write_png(output, staged_pixels, edge)
            print(f"    staged PNG: {output}")
            if mip <= 4:
                print_seams(staged_pixels, edge, bc)
            if mismatch:
                output = Path(f"{mip_stem}-gpu.png")
                gpu_pixels = decode_bc(gpu, edge, bc)
                write_png(output, gpu_pixels, edge)
                print(f"    GPU PNG: {output}")
                if mip <= 4:
                    print_seams(gpu_pixels, edge, bc, "    GPU ")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("directory", type=Path, help="VICTORY_DUMP_LIGHTING_BC_DIR output directory")
    args = parser.parse_args()
    directory = args.directory
    if not list(directory.glob("lighting-bc-*-mip0.txt")) and (directory / "lighting-bc").is_dir():
        directory /= "lighting-bc"
    manifests = sorted(directory.glob("lighting-bc-*-mip0.txt"))
    if not manifests:
        parser.error(f"no bounded lighting BC dumps found in {directory}")
    for manifest in manifests:
        analyze(manifest)


if __name__ == "__main__":
    main()
