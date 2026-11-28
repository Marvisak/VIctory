"""Small synthetic checks for the offline semantic texture cooker."""

from __future__ import annotations

import importlib.util
import json
import shutil
import struct
import subprocess
import tempfile
import unittest
from pathlib import Path

import numpy as np
from PIL import Image


spec = importlib.util.spec_from_file_location(
    "cook_textures", Path(__file__).with_name("cook-textures.py"))
cooker = importlib.util.module_from_spec(spec)
spec.loader.exec_module(cooker)


class CookingTests(unittest.TestCase):
    def test_srgb_roundtrip_and_linear_average(self):
        samples = np.linspace(0, 1, 256, dtype=np.float32)
        np.testing.assert_allclose(
            cooker.linear_to_srgb(cooker.srgb_to_linear(samples)), samples,
            atol=2e-7)
        image = Image.fromarray(np.array([[[0, 0, 0], [255, 255, 255]]],
                                           dtype=np.uint8), "RGB")
        cooked = cooker.filter_image(image, "albedo", (1, 1))
        self.assertTrue(185 <= int(np.asarray(cooked)[0, 0, 0]) <= 190)
        linear = cooker.filter_image(image, "ui", (1, 1), color_space="linear")
        self.assertTrue(125 <= int(np.asarray(linear)[0, 0, 0]) <= 130)
        self.assertEqual(cooker.texconv_format("ui", linear, "linear"), "BC7_UNORM")

    def test_normals_are_unit_length_and_variance_broadens_roughness(self):
        vectors = np.array([[[255, 128, 128], [0, 128, 128]]], dtype=np.uint8)
        normals = Image.fromarray(vectors, "RGB")
        cooked, strength = cooker.filter_normal(normals, (1, 1))
        v = np.asarray(cooked, dtype=np.float32)[0, 0] / 255 * 2 - 1
        self.assertAlmostEqual(float(np.linalg.norm(v)), 1, places=2)
        self.assertLess(float(strength[0, 0]), 0.1)
        roughness = Image.fromarray(np.full((1, 2), 64, dtype=np.uint8), "L")
        plain = int(np.asarray(cooker.filter_roughness(roughness, (1, 1), None))[0, 0])
        broadened = int(np.asarray(cooker.filter_roughness(roughness, (1, 1), normals))[0, 0])
        self.assertGreater(broadened, plain)

    def test_cutout_coverage(self):
        rng = np.random.default_rng(42)
        source = (rng.random((256, 256)) * 255).astype(np.uint8)
        mask = Image.fromarray(source, "L")
        cooked = np.asarray(cooker.filter_image(mask, "mask", (64, 64))) / 255
        before = cooker.alpha_coverage(source / 255, 0.5)
        after = cooker.alpha_coverage(cooked, 0.5)
        self.assertLess(abs(before - after), 0.02)

    def test_albedo_alpha_preserves_cutout_coverage(self):
        rng = np.random.default_rng(7)
        pixels = np.zeros((128, 128, 4), dtype=np.uint8)
        pixels[..., :3] = (80, 150, 20)
        pixels[..., 3] = rng.integers(0, 256, (128, 128), dtype=np.uint8)
        source = Image.fromarray(pixels, "RGBA")
        cooked = cooker.filter_image(source, "albedo", (64, 64),
                                     preserve_alpha_coverage=True)
        before = cooker.alpha_coverage(pixels[..., 3] / 255, 0.5)
        after = cooker.alpha_coverage(np.asarray(cooked)[..., 3] / 255, 0.5)
        self.assertLess(abs(before - after), 0.02)

    def test_packed_alpha_channel_and_xy_normals(self):
        rgba = np.zeros((2, 2, 4), dtype=np.uint8)
        rgba[..., :3] = 255
        rgba[0, 0, 3] = 255
        mask = cooker.filter_image(Image.fromarray(rgba, "RGBA"), "mask",
                                   (2, 2), channel="a")
        self.assertEqual(int(np.asarray(mask)[0, 0]), 255)
        self.assertEqual(int(np.asarray(mask)[1, 1]), 0)
        with self.assertRaisesRegex(ValueError, "explicit"):
            cooker.filter_image(Image.fromarray(rgba, "RGBA"), "mask", (2, 2))
        xy = Image.fromarray(np.array([[[128, 128, 0]]], dtype=np.uint8), "RGB")
        normal, strength = cooker.filter_normal(xy, (1, 1), "xy")
        self.assertGreater(int(np.asarray(normal)[0, 0, 2]), 250)
        self.assertGreater(float(strength[0, 0]), 0.99)
        ag = Image.fromarray(np.array([[[0, 128, 0, 255]]], dtype=np.uint8), "RGBA")
        ag_normal, _ = cooker.filter_normal(ag, (1, 1), "xy_ag")
        self.assertGreater(int(np.asarray(ag_normal)[0, 0, 0]), 250)

    def test_manifest_cooks_both_profiles_and_rejects_unknown_semantics(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / "source"
            source.mkdir()
            Image.new("RGB", (4096, 2048), (120, 80, 40)).save(source / "wood.png")
            manifest = root / "manifest.json"
            manifest.write_text(json.dumps({"assets": [{
                "source": "wood.png", "kind": "albedo", "tier": "prop"}]}))
            report = cooker.cook(manifest, source, root / "cooked", None)
            self.assertEqual([tuple(x["output_size"]) for x in report],
                             [(1024, 512), (512, 256)])
            self.assertTrue((root / "cooked" / "cook-report.json").is_file())
            manifest.write_text(json.dumps({"assets": [{
                "source": "wood.png", "kind": "guess", "tier": "prop"}]}))
            with self.assertRaises(ValueError):
                cooker.cook(manifest, source, root / "bad", None)

    def test_unmapped_atlas_fails_closed(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / "source"
            source.mkdir()
            Image.new("RGB", (2048, 2048), "red").save(source / "atlas.png")
            manifest = root / "manifest.json"
            manifest.write_text(json.dumps({"assets": [{
                "source": "atlas.png", "kind": "albedo", "tier": "prop",
                "atlas": True}]}))
            with self.assertRaisesRegex(ValueError, "UV island rectangles"):
                cooker.cook(manifest, source, root / "output", None)

    def test_atlas_islands_filter_independently_and_dilate_gutters(self):
        source = Image.new("RGB", (1024, 512), "black")
        source.paste((255, 0, 0), (0, 0, 400, 512))
        source.paste((0, 255, 0), (624, 0, 1024, 512))
        cooked = cooker.filter_atlas(source, "albedo", (256, 128),
                                     [[0, 0, 400, 512], [624, 0, 400, 512]],
                                     2, None, 0.5)
        pixels = np.asarray(cooked)
        self.assertGreater(int(pixels[64, 99, 0]), 245)
        self.assertLess(int(pixels[64, 99, 1]), 10)
        self.assertGreater(int(pixels[64, 100, 0]), 245)  # red gutter
        self.assertGreater(int(pixels[64, 155, 1]), 245)  # green gutter
        self.assertLess(int(pixels[64, 155, 0]), 10)
        tight = Image.new("RGB", (16, 8), "black")
        tight.paste((255, 0, 0), (0, 0, 6, 8))
        tight.paste((0, 255, 0), (10, 0, 16, 8))
        close = cooker.filter_atlas(tight, "albedo", (16, 8),
            [[0, 0, 6, 8], [10, 0, 6, 8]], 4, None, 0.5)
        self.assertEqual(close.getpixel((6, 4))[:3], (255, 0, 0))
        self.assertEqual(close.getpixel((9, 4))[:3], (0, 255, 0))

    def test_ui_atlas_preserves_color_and_alpha(self):
        ui = Image.new("RGBA", (16, 8), (255, 20, 40, 128))
        cooked = cooker.filter_atlas(ui, "ui", (8, 4), [[0, 0, 16, 8]],
                                     0, None, 0.5)
        self.assertEqual(cooked.mode, "RGBA")
        self.assertEqual(cooked.getpixel((4, 2)), (255, 20, 40, 128))

    def test_packed_map_can_produce_two_distinct_outputs(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / "source"
            source.mkdir()
            packed = np.zeros((16, 16, 3), dtype=np.uint8)
            packed[..., 1] = 64
            packed[..., 2] = 255
            Image.fromarray(packed, "RGB").save(source / "orm.png")
            manifest = root / "manifest.json"
            manifest.write_text(json.dumps({"assets": [
                {"source": "orm.png", "output": "roughness.png", "kind": "roughness",
                 "tier": "prop", "channel": "g"},
                {"source": "orm.png", "output": "metalness.png", "kind": "metalness",
                 "tier": "prop", "channel": "b"}]}))
            cooker.cook(manifest, source, root / "out", None)
            rough = Image.open(root / "out" / "assets_720p" / "roughness.png")
            metal = Image.open(root / "out" / "assets_720p" / "metalness.png")
            self.assertEqual(rough.getpixel((0, 0)), 64)
            self.assertEqual(metal.getpixel((0, 0)), 255)

    def test_dds_validation_rejects_wrong_dimensions_and_mips(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "test.dds"
            data = bytearray(148 + 8)
            data[:4] = b"DDS "
            struct.pack_into("<I", data, 4, 124)
            struct.pack_into("<II", data, 12, 4, 4)
            struct.pack_into("<I", data, 28, 1)
            data[84:88] = b"DX10"
            struct.pack_into("<I", data, 128, 72)
            path.write_bytes(data)
            cooker.validate_dds(path, (4, 4), "BC1_UNORM_SRGB")
            with self.assertRaisesRegex(ValueError, "mismatch"):
                cooker.validate_dds(path, (8, 4), "BC1_UNORM_SRGB")
            struct.pack_into("<I", data, 28, 2)
            path.write_bytes(data)
            with self.assertRaisesRegex(ValueError, "mismatch"):
                cooker.validate_dds(path, (4, 4), "BC1_UNORM_SRGB")

    @unittest.skipUnless(shutil.which("texconv") and shutil.which("texassemble"),
                         "DirectXTex command-line tools unavailable")
    def test_bc_roundtrip_keeps_srgb_code_values_and_custom_mips(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / "source"
            source.mkdir()
            Image.new("RGB", (16, 16), (128, 128, 128)).save(source / "gray.png")
            manifest = root / "manifest.json"
            manifest.write_text(json.dumps({"assets": [{
                "source": "gray.png", "kind": "albedo", "tier": "prop"}]}))
            report = cooker.cook(manifest, source, root / "out",
                                 Path(shutil.which("texconv")),
                                 Path(shutil.which("texassemble")))
            self.assertEqual(report[1]["mip_levels"], 5)
            compressed = root / "out" / "assets_720p" / "gray.dds"
            decoder_out = root / "decode"
            decoder_out.mkdir()
            subprocess.run([shutil.which("texconv"), "-y", "-dx10", "-f",
                            "R8G8B8A8_UNORM_SRGB", "-o", str(decoder_out),
                            str(compressed)], check=True, capture_output=True)
            pixel = (decoder_out / "gray.dds").read_bytes()[148:151]
            self.assertTrue(all(abs(value - 128) <= 6 for value in pixel), pixel)

    @unittest.skipUnless(shutil.which("texconv") and shutil.which("texassemble"),
                         "DirectXTex command-line tools unavailable")
    def test_xyz_preserves_z_in_bc7_and_xy_uses_bc5(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / "source"
            source.mkdir()
            Image.new("RGB", (16, 16), (153, 128, 250)).save(source / "normal.png")
            manifest = root / "manifest.json"
            manifest.write_text(json.dumps({"assets": [{
                "source": "normal.png", "output": "xyz.png", "kind": "normal",
                "tier": "prop", "normal_encoding": "xyz"}, {
                "source": "normal.png", "output": "xy.png", "kind": "normal",
                "tier": "prop", "normal_encoding": "xy"}]}))
            report = cooker.cook(manifest, source, root / "out",
                                 Path(shutil.which("texconv")),
                                 Path(shutil.which("texassemble")))
            self.assertEqual(report[1]["mip_levels"], 5)
            xyz = (root / "out" / "assets_720p" / "xyz.dds").read_bytes()
            xy = (root / "out" / "assets_720p" / "xy.dds").read_bytes()
            self.assertEqual(struct.unpack_from("<I", xyz, 128)[0], 98)  # BC7_UNORM
            self.assertEqual(struct.unpack_from("<I", xy, 128)[0], 83)  # BC5_UNORM


if __name__ == "__main__":
    unittest.main()
