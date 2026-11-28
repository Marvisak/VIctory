"""Cook explicitly classified, extracted 2D textures for 1080p and 720p.

Input is a JSON manifest with ``assets`` entries. This does not edit RPF9
archives or alter the title's runtime resources. Requires Pillow and NumPy.
Optional ``--texconv`` converts the cooked PNGs into BC-compressed DDS files.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import shutil
import struct
import subprocess
import tempfile
from pathlib import Path

import numpy as np
from PIL import Image


TARGETS = {
    "1080p": {"hero": 2048, "environment": 2048, "prop": 1024, "ui": 2048},
    "720p": {"hero": 1024, "environment": 1024, "prop": 512, "ui": 1024},
}
KINDS = {"albedo", "normal", "roughness", "metalness", "mask", "ui"}


def srgb_to_linear(value: np.ndarray) -> np.ndarray:
    value = np.asarray(value, dtype=np.float32)
    return np.where(value <= 0.04045, value / 12.92,
                    ((value + 0.055) / 1.055) ** 2.4)


def linear_to_srgb(value: np.ndarray) -> np.ndarray:
    value = np.clip(value, 0.0, 1.0)
    return np.where(value <= 0.0031308, value * 12.92,
                    1.055 * value ** (1.0 / 2.4) - 0.055)


def resize_float(values: np.ndarray, size: tuple[int, int]) -> np.ndarray:
    if values.ndim == 2:
        values = values[..., None]
    channels = [np.asarray(Image.fromarray(values[..., channel].astype(np.float32), "F")
                           .resize(size, Image.Resampling.LANCZOS), dtype=np.float32)
                for channel in range(values.shape[2])]
    return np.stack(channels, axis=2)


def to_u8(values: np.ndarray) -> np.ndarray:
    return np.rint(np.clip(values, 0.0, 1.0) * 255).astype(np.uint8)


def alpha_coverage(alpha: np.ndarray, cutoff: float) -> float:
    return float(np.count_nonzero(alpha >= cutoff)) / alpha.size


def preserve_coverage(alpha: np.ndarray, source_coverage: float,
                      cutoff: float) -> np.ndarray:
    if source_coverage <= 0:
        return np.zeros_like(alpha)
    if source_coverage >= 1:
        return np.ones_like(alpha)
    lo, hi = 0.0, 16.0
    for _ in range(32):
        mid = (lo + hi) / 2
        if alpha_coverage(np.clip(alpha * mid, 0, 1), cutoff) < source_coverage:
            lo = mid
        else:
            hi = mid
    # The discrete target grid may be unable to match exactly. Choose the
    # closer side, with no larger than one-pixel error when feasible.
    choices = (np.clip(alpha * lo, 0, 1), np.clip(alpha * hi, 0, 1))
    return min(choices, key=lambda item: abs(alpha_coverage(item, cutoff) - source_coverage))


def filter_normal(image: Image.Image, size: tuple[int, int],
                  encoding: str = "xyz") -> tuple[Image.Image, np.ndarray]:
    if encoding not in {"xyz", "xy", "xy_ag"}:
        raise ValueError("normal encoding must be xyz, xy, or xy_ag")
    rgb = np.asarray(image.convert("RGB"), dtype=np.float32) / 255
    normal = rgb * 2 - 1
    if encoding == "xy_ag":
        if "A" not in image.getbands():
            raise ValueError("xy_ag normal needs an alpha channel")
        normal[..., 0] = np.asarray(image.getchannel("A"), dtype=np.float32) / 255 * 2 - 1
    if encoding in {"xy", "xy_ag"}:
        normal[..., 2] = np.sqrt(np.maximum(0.0,
            1.0 - normal[..., 0] ** 2 - normal[..., 1] ** 2))
    source_length = np.linalg.norm(normal, axis=2)
    normal /= np.maximum(source_length[..., None], 1e-8)
    normal[source_length < 1e-8] = (0, 0, 1)
    filtered = resize_float(normal, size)
    length = np.linalg.norm(filtered, axis=2)
    safe = np.maximum(length, 1e-8)
    filtered /= safe[..., None]
    filtered[length < 1e-8] = (0, 0, 1)
    return Image.fromarray(to_u8((filtered + 1) * 0.5), "RGB"), np.clip(length, 0, 1)


def scalar_channel(image: Image.Image, channel: str | None) -> np.ndarray:
    if image.mode in {"1", "L"} and channel is None:
        return np.asarray(image.convert("L"), dtype=np.float32) / 255
    if channel not in {"r", "g", "b", "a", "luma"}:
        raise ValueError("packed scalar map needs explicit r/g/b/a/luma channel")
    if channel == "a" and "A" not in image.getbands():
        raise ValueError("alpha channel requested but source has no alpha")
    if channel == "luma":
        return np.asarray(image.convert("L"), dtype=np.float32) / 255
    components = np.asarray(image.convert("RGBA" if channel == "a" else "RGB"),
                            dtype=np.float32)
    return components[..., {"r": 0, "g": 1, "b": 2, "a": 3}[channel]] / 255


def filter_roughness(image: Image.Image, size: tuple[int, int],
                     normal_image: Image.Image | None,
                     channel: str | None = None,
                     normal_encoding: str = "xyz") -> Image.Image:
    roughness = scalar_channel(image, channel)
    filtered = np.clip(resize_float(roughness, size)[..., 0], 0, 1)
    if normal_image is not None:
        _, strength = filter_normal(normal_image, size, normal_encoding)
        # Toksvig exponent correction: broadens the lobe when filtering a
        # varying normal field, then converts the exponent back to roughness.
        exponent = 2 / np.maximum(filtered, 0.04) ** 2 - 2
        adjusted = strength * exponent / np.maximum(
            strength + exponent * (1 - strength), 1e-8)
        filtered = np.sqrt(2 / (adjusted + 2))
    return Image.fromarray(to_u8(filtered), "L")


def filter_image(image: Image.Image, kind: str, size: tuple[int, int],
                 normal_image: Image.Image | None = None,
                 cutoff: float = 0.5, channel: str | None = None,
                 normal_encoding: str = "xyz",
                 color_space: str = "srgb",
                 preserve_alpha_coverage: bool = False) -> Image.Image:
    if kind == "normal":
        return filter_normal(image, size, normal_encoding)[0]
    if kind == "roughness":
        return filter_roughness(image, size, normal_image, channel, normal_encoding)
    if kind == "metalness":
        scalar = scalar_channel(image, channel)
        return Image.fromarray(to_u8(resize_float(scalar, size)[..., 0]), "L")
    if kind == "mask":
        alpha = scalar_channel(image, channel)
        if size == image.size:
            return Image.fromarray(to_u8(alpha), "L")
        filtered = np.clip(resize_float(alpha, size)[..., 0], 0, 1)
        return Image.fromarray(to_u8(preserve_coverage(
            filtered, alpha_coverage(alpha, cutoff), cutoff)), "L")
    if kind in {"ui", "albedo"}:
        if color_space not in {"srgb", "linear"}:
            raise ValueError("color space must be srgb or linear")
        rgba = np.asarray(image.convert("RGBA"), dtype=np.float32) / 255
        alpha = rgba[..., 3:4]
        linear = (srgb_to_linear(rgba[..., :3]) if color_space == "srgb"
                  else rgba[..., :3])
        premul = resize_float(linear * alpha, size)
        alpha_filtered = np.clip(resize_float(alpha, size), 0, 1)
        straight = premul / np.maximum(alpha_filtered, 1e-8)
        encoded = linear_to_srgb(straight) if color_space == "srgb" else straight
        if preserve_alpha_coverage and kind == "albedo" and size != image.size:
            alpha_filtered = preserve_coverage(alpha_filtered[..., 0],
                alpha_coverage(alpha[..., 0], cutoff), cutoff)[..., None]
        return Image.fromarray(np.dstack((to_u8(encoded),
                                         to_u8(alpha_filtered))), "RGBA")
    raise ValueError(f"unknown kind: {kind}")


def target_size(width: int, height: int, limit: int) -> tuple[int, int]:
    scale = min(1.0, limit / max(width, height))
    return max(1, round(width * scale)), max(1, round(height * scale))


def filter_atlas(image: Image.Image, kind: str, size: tuple[int, int],
                 rects: list[list[int]], gutter: int,
                 normal_image: Image.Image | None, cutoff: float,
                 channel: str | None = None,
                 normal_encoding: str = "xyz",
                 color_space: str = "srgb",
                 preserve_alpha_coverage: bool = False) -> Image.Image:
    if not isinstance(gutter, int) or not 0 <= gutter <= 16:
        raise ValueError("atlas gutter must be 0..16 pixels")
    if not isinstance(rects, list) or not rects:
        raise ValueError("atlas needs UV island rectangles")
    mode = "RGBA" if kind in {"albedo", "ui"} else "RGB" if kind == "normal" else "L"
    canvas = Image.new(mode, size)
    occupied = np.zeros((size[1], size[0]), dtype=bool)
    scale_x, scale_y = size[0] / image.width, size[1] / image.height
    islands = []
    for rect in rects:
        if not isinstance(rect, list) or len(rect) != 4 or not all(
            isinstance(value, int) for value in rect
        ):
            raise ValueError("atlas rectangles must be [x,y,width,height] integers")
        x, y, w, h = rect
        if x < 0 or y < 0 or w < 1 or h < 1 or x + w > image.width or y + h > image.height:
            raise ValueError(f"atlas rectangle outside image: {rect}")
        left, top = round(x * scale_x), round(y * scale_y)
        right, bottom = round((x + w) * scale_x), round((y + h) * scale_y)
        if right <= left or bottom <= top or occupied[top:bottom, left:right].any():
            raise ValueError(f"atlas rectangles overlap or collapsed: {rect}")
        occupied[top:bottom, left:right] = True
        crop = image.crop((x, y, x + w, y + h))
        paired = normal_image.crop((x, y, x + w, y + h)) if normal_image else None
        cooked = filter_image(crop, kind, (right - left, bottom - top),
                              paired, cutoff, channel, normal_encoding,
                              color_space, preserve_alpha_coverage)
        canvas.paste(cooked, (left, top))
        islands.append((left, top, right, bottom, cooked))
    if gutter:
        nearest = np.full((size[1], size[0]), np.inf, dtype=np.float32)
        for left, top, right, bottom, cooked in islands:
            # Give each unoccupied pixel the closest island's edge. This
            # avoids manifest order determining the color where gutters meet.
            x0, y0 = max(0, left - gutter), max(0, top - gutter)
            x1, y1 = min(size[0], right + gutter), min(size[1], bottom + gutter)
            for yy in range(y0, y1):
                for xx in range(x0, x1):
                    distance = (max(left - xx, 0, xx - right + 1) ** 2
                                + max(top - yy, 0, yy - bottom + 1) ** 2)
                    if not occupied[yy, xx] and distance < nearest[yy, xx]:
                        nearest[yy, xx] = distance
                        canvas.putpixel((xx, yy), cooked.getpixel((
                            min(max(xx - left, 0), cooked.width - 1),
                            min(max(yy - top, 0), cooked.height - 1))))
    return canvas


def checked_source(root: Path, relative: str) -> Path:
    source = (root / relative).resolve(strict=True)
    if not source.is_relative_to(root) or not source.is_file():
        raise ValueError(f"source escapes input directory: {relative}")
    if source.suffix.lower() not in {".png", ".tif", ".tiff"}:
        raise ValueError(f"decode source to PNG/TIFF first: {relative}")
    return source


def texconv_format(kind: str, image: Image.Image,
                   color_space: str = "srgb",
                   normal_encoding: str = "xyz") -> str:
    if kind == "normal":
        return "BC7_UNORM" if normal_encoding == "xyz" else "BC5_UNORM"
    if kind in {"roughness", "metalness", "mask"}:
        return "BC4_UNORM"
    if kind == "albedo":
        opaque = image.mode != "RGBA" or image.getchannel("A").getextrema() == (255, 255)
        return ("BC1_UNORM_SRGB" if color_space == "srgb" else "BC1_UNORM") if opaque else (
            "BC7_UNORM_SRGB" if color_space == "srgb" else "BC7_UNORM")
    return "BC7_UNORM_SRGB" if color_space == "srgb" else "BC7_UNORM"


def validate_dds(path: Path, size: tuple[int, int], fmt: str,
                 mip_levels: int = 1) -> None:
    # texconv is an external process; verify it emitted the expected
    # DX10 BC texture before recording an apparently successful cook.
    expected = {"BC1_UNORM": (71, 8), "BC1_UNORM_SRGB": (72, 8),
                "BC4_UNORM": (80, 8), "BC5_UNORM": (83, 16),
                "BC7_UNORM": (98, 16), "BC7_UNORM_SRGB": (99, 16)}
    data = path.read_bytes()
    if len(data) < 148 or data[:4] != b"DDS " or data[84:88] != b"DX10":
        raise ValueError(f"invalid DDS/DX10 header: {path}")
    height, width = struct.unpack_from("<II", data, 12)
    mips = struct.unpack_from("<I", data, 28)[0]
    format_id = struct.unpack_from("<I", data, 128)[0]
    block_bytes = expected[fmt][1]
    data_bytes = sum(
        ((max(1, width >> level) + 3) // 4)
        * ((max(1, height >> level) + 3) // 4) * block_bytes
        for level in range(mip_levels)
    )
    if ((width, height) != size or mips != mip_levels
            or format_id != expected[fmt][0] or len(data) != 148 + data_bytes):
        raise ValueError(f"DDS size, format, mip count, or payload mismatch: {path}")


def cook(manifest: Path, input_root: Path, output_root: Path,
         texconv: Path | None, texassemble: Path | None = None) -> list[dict]:
    input_root = input_root.resolve(strict=True)
    output_root = output_root.resolve()
    if output_root == input_root or output_root.is_relative_to(input_root):
        raise ValueError("output must not overwrite input assets")
    if texconv is not None and texassemble is None:
        found = shutil.which("texassemble")
        if found is None:
            raise ValueError("full DDS mips require texassemble; pass --texassemble")
        texassemble = Path(found)
    entries = json.loads(manifest.read_text(encoding="utf-8"))["assets"]
    if not isinstance(entries, list) or not entries:
        raise ValueError("manifest needs a nonempty assets list")
    seen: set[str] = set()
    for entry in entries:
        relative = entry["source"]
        output_relative = entry.get("output", relative)
        output_key = str(Path(output_relative)).casefold()
        if output_key in seen:
            raise ValueError(f"duplicate output path: {output_relative}")
        seen.add(output_key)
        if (Path(output_relative).drive or Path(output_relative).is_absolute()
                or ".." in Path(output_relative).parts
                or Path(output_relative).suffix.lower() != ".png"):
            raise ValueError(f"output must be a relative PNG path: {output_relative}")
        if entry["kind"] not in KINDS or entry["tier"] not in TARGETS["1080p"]:
            raise ValueError(f"classify kind and tier explicitly: {relative}")
        if entry.get("normal_encoding", "xyz") not in {"xyz", "xy", "xy_ag"}:
            raise ValueError(f"unsupported normal encoding: {relative}")
        if entry.get("color_space", "srgb") not in {"srgb", "linear"}:
            raise ValueError(f"unsupported color space: {relative}")
        if entry["kind"] not in {"albedo", "ui"} and "color_space" in entry:
            raise ValueError(f"scalar and normal maps are linear: {relative}")
        if entry.get("atlas") and not entry.get("rects"):
            raise ValueError(f"atlas needs UV island rectangles: {relative}")
        checked_source(input_root, relative)
        if entry.get("normal_source"):
            checked_source(input_root, entry["normal_source"])
    report: list[dict] = []
    for entry in entries:
        output_relative = entry.get("output", entry["source"])
        source = checked_source(input_root, entry["source"])
        source_sha256 = hashlib.sha256(source.read_bytes()).hexdigest()
        image = Image.open(source)
        image.load()
        if image.mode not in {"L", "RGB", "RGBA"}:
            raise ValueError(f"source must be 8-bit L/RGB/RGBA: {source} ({image.mode})")
        normal = None
        normal_source_sha256 = None
        if entry.get("normal_source"):
            normal_path = checked_source(input_root, entry["normal_source"])
            normal_source_sha256 = hashlib.sha256(normal_path.read_bytes()).hexdigest()
            normal = Image.open(normal_path)
            normal.load()
            if normal.mode not in {"RGB", "RGBA"}:
                raise ValueError(f"paired normal must be 8-bit RGB/RGBA: {normal.mode}")
            if normal.size != image.size or entry["kind"] != "roughness":
                raise ValueError(f"roughness/normal pair size or kind mismatch: {source}")
        kind = entry["kind"]
        cutoff = float(entry.get("alpha_cutoff", 0.5))
        channel = entry.get("channel")
        normal_encoding = entry.get("normal_encoding", "xyz")
        color_space = entry.get("color_space", "srgb")
        if not 0 < cutoff < 1:
            raise ValueError(f"alpha cutoff outside (0,1): {source}")
        for profile, tiers in TARGETS.items():
            limit = tiers[entry["tier"]]
            if kind in {"roughness", "metalness"}:
                limit = min(limit, 1024 if profile == "1080p" else 512)
            size = target_size(*image.size, limit)
            if texconv is not None:
                # D3D BC top levels must have 4x4-aligned dimensions. Round
                # inward, never beyond the original image's dimensions.
                if min(size) < 4:
                    raise ValueError(f"BC texture is smaller than 4x4: {source}")
                size = tuple(value - value % 4 for value in size)
            destination = output_root / f"assets_{profile}" / output_relative
            if not destination.resolve().is_relative_to(output_root):
                raise ValueError(f"output escapes destination directory: {output_relative}")
            destination.parent.mkdir(parents=True, exist_ok=True)
            mip_paths: list[Path] = []
            mip_stop_reason = None
            level_size = size
            while True:
                try:
                    if entry.get("atlas"):
                        cooked = filter_atlas(image, kind, level_size, entry["rects"],
                                              entry.get("gutter", 0), normal, cutoff,
                                              channel, normal_encoding, color_space,
                                              "alpha_cutoff" in entry and kind == "albedo")
                    else:
                        cooked = filter_image(image, kind, level_size, normal, cutoff,
                                              channel, normal_encoding, color_space,
                                              "alpha_cutoff" in entry and kind == "albedo")
                except ValueError as error:
                    if not entry.get("atlas") or not mip_paths:
                        raise
                    mip_stop_reason = str(error)
                    break
                level = len(mip_paths)
                mip_path = (destination if level == 0 else
                            output_root / f"mips_{profile}" /
                            Path(output_relative).with_suffix("") / f"mip{level:02}.png")
                mip_path.parent.mkdir(parents=True, exist_ok=True)
                cooked.save(mip_path)
                mip_paths.append(mip_path)
                # texassemble's from-mips command accepts levels only until
                # the shorter dimension reaches 1 (2048x1024 => 11 levels).
                if min(level_size) == 1:
                    break
                level_size = (max(1, level_size[0] // 2),
                              max(1, level_size[1] // 2))
            output = destination
            fmt = None
            if texconv is not None:
                with Image.open(destination) as top:
                    fmt = texconv_format(kind, top, color_space, normal_encoding)
                with tempfile.TemporaryDirectory(prefix="victory-cook-") as scratch:
                    scratch = Path(scratch)
                    assembled = scratch / "assembled.dds"
                    process = subprocess.run(
                        [str(texassemble), "from-mips", "-y", "-dx10", "-o",
                         str(assembled), *map(str, mip_paths)],
                        capture_output=True, text=True)
                    if process.returncode != 0:
                        raise RuntimeError(f"texassemble failed for {destination}: {process.stderr or process.stdout}")
                    compressed = scratch / "compressed"
                    compressed.mkdir()
                    command = [str(texconv), "-y", "-dx10", "-nogpu", "-f", fmt,
                               "-o", str(compressed)]
                    if kind in {"albedo", "ui"} and color_space == "srgb":
                        # PNG stores sRGB code values; without this, texconv
                        # interprets them as linear and gamma-encodes twice.
                        command.append("-srgb")
                    command.append(str(assembled))
                    process = subprocess.run(command, capture_output=True, text=True)
                    if process.returncode != 0:
                        raise RuntimeError(f"texconv failed for {destination}: {process.stderr or process.stdout}")
                    produced = compressed / "assembled.dds"
                    if not produced.is_file():
                        raise RuntimeError(f"texconv produced no DDS: {produced}")
                    output = destination.with_suffix(".dds")
                    shutil.copyfile(produced, output)
                validate_dds(output, size, fmt, len(mip_paths))
            report.append({"source": entry["source"], "profile": profile,
                           "source_sha256": source_sha256,
                           "normal_source_sha256": normal_source_sha256,
                           "output_name": output_relative,
                           "kind": kind, "tier": entry["tier"],
                           "color_space": color_space if kind in {"albedo", "ui"} else "linear",
                           "normal_encoding": normal_encoding if kind == "normal" or normal is not None else None,
                           "channel": channel,
                           "alpha_cutoff": cutoff if kind == "mask" or "alpha_cutoff" in entry else None,
                           "atlas_rects": entry.get("rects") if entry.get("atlas") else None,
                           "gutter": entry.get("gutter", 0) if entry.get("atlas") else None,
                           "input_size": list(image.size), "output_size": list(size),
                           "mip_levels": len(mip_paths),
                           "mip_stop_reason": mip_stop_reason,
                           "format": fmt or "PNG",
                           "output_bytes": output.stat().st_size,
                           "output": str(output.relative_to(output_root)),
                           "sha256": hashlib.sha256(output.read_bytes()).hexdigest()})
    output_root.mkdir(parents=True, exist_ok=True)
    (output_root / "cook-report.json").write_text(
        json.dumps({"assets": report}, indent=2) + "\n", encoding="utf-8")
    return report


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--manifest", type=Path, required=True)
    parser.add_argument("--input", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--texconv", type=Path)
    parser.add_argument("--texassemble", type=Path)
    args = parser.parse_args()
    if args.texconv is not None and not args.texconv.is_file():
        parser.error("texconv executable does not exist")
    if args.texassemble is not None and not args.texassemble.is_file():
        parser.error("texassemble executable does not exist")
    result = cook(args.manifest, args.input, args.output,
                  args.texconv, args.texassemble)
    print(f"Cooked {len(result)} profile assets; report: {args.output / 'cook-report.json'}")


if __name__ == "__main__":
    main()
