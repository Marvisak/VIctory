//! Synthetic CPU benchmark; no GPU or external assets required.
//! Run: cargo run --release --locked -p victory-formats --example linear_copy_bench
use std::{hint::black_box, time::Instant};
use victory_formats::texture::{Desc, Dimension, Layout, Swizzle};

fn main() {
    const SAMPLES: usize = 9;
    const BYTES_PER_SAMPLE: usize = 256 << 20;
    let base = Desc {
        dimension: Dimension::Tex2d,
        width: 256,
        height: 256,
        depth: 1,
        mips: 1,
        element_bytes: 4,
        block: 1,
        samples: 1,
        swizzle: Swizzle::LINEAR,
    };
    let volume = Desc {
        dimension: Dimension::Tex3d,
        height: 64,
        depth: 16,
        ..base
    };
    for (name, desc, row_pad, depth_pad) in [
        ("dense 256x256", base, 0, 0),
        (
            "dense 1920x1080",
            Desc {
                width: 1920,
                height: 1080,
                ..base
            },
            0,
            0,
        ),
        ("dense volume", volume, 0, 0),
        ("mip-chain volume", Desc { mips: 4, ..volume }, 0, 0),
        ("padded rows", base, 13, 0),
        ("padded depth", volume, 0, 17),
        ("unaligned width", Desc { width: 255, ..base }, 0, 0),
        ("matching padded pitch", Desc { width: 255, ..base }, 4, 0),
        (
            "BC7 linear",
            Desc {
                width: 512,
                height: 512,
                element_bytes: 16,
                block: 4,
                ..base
            },
            0,
            0,
        ),
        (
            "linear 1D",
            Desc {
                dimension: Dimension::Tex1d,
                height: 1,
                ..base
            },
            0,
            0,
        ),
        (
            "tiled 2D control",
            Desc {
                swizzle: Swizzle::SW_64KB_D,
                ..base
            },
            0,
            0,
        ),
        (
            "tiled 3D control",
            Desc {
                swizzle: Swizzle::SW_4KB_S,
                ..volume
            },
            0,
            0,
        ),
    ] {
        let layout = Layout::new(&desc).unwrap();
        let m = layout.mips[0];
        let active = m.width as usize * desc.element_bytes as usize;
        let row = active + row_pad;
        let plane = row * m.height as usize + depth_pad;
        let bytes = active * m.height as usize * m.depth as usize;
        let mut tiled = vec![0; layout.size as usize];
        let mut linear: Vec<_> = (0..plane * m.depth as usize)
            .map(|i| (i ^ (i >> 8) ^ (i >> 16)) as u8)
            .collect();
        layout.tile(&mut tiled, 0, 0, &linear, row, plane).unwrap();
        let iterations = BYTES_PER_SAMPLE.div_ceil(bytes);
        for detile in [true, false] {
            let mut copy = || {
                if detile {
                    layout.detile(black_box(&tiled), 0, 0, black_box(&mut linear), row, plane)
                } else {
                    layout.tile(black_box(&mut tiled), 0, 0, black_box(&linear), row, plane)
                }
                .unwrap();
            };
            copy(); // Warm buffers, tables and code outside the timed samples.
            let mut samples = [0.0; SAMPLES];
            for sample in &mut samples {
                let start = Instant::now();
                for _ in 0..iterations {
                    copy();
                }
                *sample = start.elapsed().as_secs_f64() * 1e9 / iterations as f64;
            }
            samples.sort_by(f64::total_cmp);
            let ns = samples[SAMPLES / 2];
            println!(
                "{name:24} {:6} {ns:12.2} ns/copy {:10.1} MB/s",
                if detile { "detile" } else { "tile" },
                bytes as f64 * 1e3 / ns
            );
        }
    }
}
