//! Immediate samplers: the console's four sampler words (the GPU's public
//! `SQ_IMG_SAMP_WORD0-3` layout) as a D3D12 static sampler.
//!
//! Word 0: address modes x, y, z (bits 0-8, 3 each), log2 of the maximum
//! anisotropy (9-11), comparison function (12-14), reduction (29-30: 0
//! filtering, 1 minimum, 2 maximum). Word 1: minimum and maximum LOD
//! (0-11, 12-23, unsigned 4.8; a maximum below the minimum is raised to it,
//! since a host sampler needs `MaxLOD >= MinLOD`). Word 2: LOD bias (0-13, signed 5.8; D3D12
//! takes -16 to 15.99, so the rest of the field's range is clamped),
//! magnification and minification filters (20-21, 22-23: point, linear,
//! anisotropic point, anisotropic linear), mip filter (26-27: none, point,
//! linear). Word 3: border colour (30-31: transparent black, opaque black,
//! opaque white, custom).
//!
//! Comparison is the sampling instruction's, not the sampler's: the console
//! encodes a comparison function in every sampler (D3D's default, less or
//! equal, when unused).

/// `D3D12_MIP_LOD_BIAS_MIN` and `D3D12_MIP_LOD_BIAS_MAX`: a root signature
/// with a static sampler outside them is refused.
const MIP_LOD_BIAS: (f32, f32) = (-16.0, 15.99);

/// A static sampler in D3D12 terms; the integers are the `D3D12_FILTER`,
/// `D3D12_TEXTURE_ADDRESS_MODE`, `D3D12_COMPARISON_FUNC` and
/// `D3D12_STATIC_BORDER_COLOR` values.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StaticSampler {
    pub filter: u32,
    /// U, V, W.
    pub address: [u32; 3],
    pub mip_lod_bias: f32,
    pub max_anisotropy: u32,
    pub comparison: u32,
    pub border: u32,
    pub min_lod: f32,
    pub max_lod: f32,
}

// The floats come from fixed-point fields: never NaN.
impl Eq for StaticSampler {}

impl StaticSampler {
    /// Decodes the four sampler words.
    pub fn from_words(w: [u32; 4]) -> Self {
        let bits = |word: u32, at: u32, n: u32| word >> at & ((1 << n) - 1);
        // wrap, mirror, clamp, mirror once; the half-border modes have no
        // D3D form and become their nearest.
        let address = |m: u32| [1, 2, 3, 5, 3, 5, 4, 5][m as usize];
        let (mag, min, mip) = (bits(w[2], 20, 2), bits(w[2], 22, 2), bits(w[2], 26, 2));
        let reduction = match bits(w[0], 29, 2) {
            1 => 2,
            2 => 3,
            _ => 0,
        };
        let filter = if mag >= 2 || min >= 2 {
            0x55 | reduction << 7
        } else {
            (min & 1) << 4 | (mag & 1) << 2 | u32::from(mip == 2) | reduction << 7
        };
        let lod = |v: u32| v as f32 / 256.0;
        // Without a mip filter the base level is sampled. The fields are
        // independent, so the maximum can sit below the minimum; the hardware
        // clamp then yields the minimum, which an equal pair also gives, and
        // a host sampler with `MaxLOD < MinLOD` is invalid (Vulkan's
        // VUID-VkSamplerCreateInfo-maxLod-01973 under vkd3d-proton).
        let (min_lod, max_lod) = if mip == 0 {
            (0.0, 0.0)
        } else {
            let min = lod(bits(w[1], 0, 12));
            (min, lod(bits(w[1], 12, 12)).max(min))
        };
        StaticSampler {
            filter,
            address: [0, 3, 6].map(|at| address(bits(w[0], at, 3))),
            mip_lod_bias: (((bits(w[2], 0, 14) << 18) as i32 >> 18) as f32 / 256.0)
                .clamp(MIP_LOD_BIAS.0, MIP_LOD_BIAS.1),
            max_anisotropy: 1 << bits(w[0], 9, 3).min(4),
            comparison: bits(w[0], 12, 3) + 1,
            // shortcut: a custom border colour (type 3, a table entry) becomes
            // transparent black; upgrade when a title uses one.
            border: bits(w[3], 30, 2) % 3,
            min_lod,
            max_lod,
        }
    }

    /// The same sampler for a comparison instruction (`image_sample_c*`).
    /// `D3D12_FILTER_REDUCTION_TYPE` (bits 7-8) holds one of filtering,
    /// comparison, minimum or maximum, so comparison replaces the reduction
    /// the sampler words give: setting bit 7 alone would turn minimum (2)
    /// into maximum (3) and leave maximum without comparison, a filter the
    /// shader's comparison sampler declaration does not match.
    pub fn comparison(self) -> Self {
        StaticSampler {
            filter: self.filter & !(3 << 7) | 1 << 7,
            ..self
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn console_compiled_samplers_decode() {
        // Words the console compiler wrote for static samplers of known
        // descriptions.
        let point = StaticSampler::from_words([0x3088, 0x0054_0100, 0x1400_0080, 0x8000_0000]);
        assert_eq!(
            point,
            StaticSampler {
                filter: 0,
                address: [1, 2, 3],
                mip_lod_bias: 0.5,
                max_anisotropy: 1,
                comparison: 4,
                border: 2,
                min_lod: 1.0,
                max_lod: 5.25,
            }
        );
        let aniso = StaticSampler::from_words([0x369E, 0x00FF_F000, 0x18F0_3E00, 0x8000_0000]);
        assert_eq!(
            (aniso.filter, aniso.address, aniso.max_anisotropy),
            (0x55, [4, 5, 3], 8)
        );
        assert_eq!((aniso.mip_lod_bias, aniso.max_lod), (-2.0, 4095.0 / 256.0));
        let compare = StaticSampler::from_words([0x3006, 0x00FF_F000, 0x1450_0000, 0x4000_0000]);
        assert_eq!(
            (compare.filter, compare.address, compare.border),
            (0x14, [4, 1, 1], 1)
        );
        let minimum =
            StaticSampler::from_words([0x2000_3003, 0x00FF_F000, 0x1850_0000, 0x8000_0000]);
        assert_eq!((minimum.filter, minimum.address[0]), (0x115, 5));
        let mag_mip = StaticSampler::from_words([0x3002, 0, 0x1810_0000, 0x8000_0000]);
        assert_eq!((mag_mip.filter, mag_mip.max_lod), (0x05, 0.0));
        let max_aniso =
            StaticSampler::from_words([0x4000_3800, 0x00FF_F000, 0x18F0_0F00, 0x8000_0000]);
        assert_eq!(
            (
                max_aniso.filter,
                max_aniso.max_anisotropy,
                max_aniso.mip_lod_bias
            ),
            (0x1D5, 16, 15.0)
        );
    }

    /// The bias field reaches -32 to 31.99; D3D12 refuses a static sampler
    /// past -16 or 15.99, so those ends are clamped and the rest kept.
    #[test]
    fn lod_bias_stays_in_the_host_range() {
        let bias = |field: u32| StaticSampler::from_words([0, 0, field, 0]).mip_lod_bias;
        assert_eq!(bias(0x0FFF), 15.99, "15.996 clamps");
        assert_eq!(bias(0x1FFF), 15.99, "31.996 clamps");
        assert_eq!(bias(0x2000), -16.0, "-32 clamps");
        assert_eq!(bias(0x3000), -16.0, "-16 is kept");
        assert_eq!(bias(0x0E80), 14.5, "in range is kept");
        assert_eq!(bias(0x3F80), -0.5, "negative in range is kept");
    }

    /// Comparison takes the reduction field whatever the sampler words
    /// asked for; the filter bits beside it are kept.
    #[test]
    fn comparison_replaces_the_reduction() {
        let filter = |w0: u32, w2: u32| {
            StaticSampler::from_words([w0, 0, w2, 0])
                .comparison()
                .filter
        };
        // Point, linear min/mag with linear mip, anisotropic.
        assert_eq!(filter(0, 0), 0x80);
        assert_eq!(filter(0, 0x0850_0000), 0x95);
        assert_eq!(filter(0, 0x00F0_0000), 0xD5);
        // Minimum (reduction 1) and maximum (2) reductions become comparison.
        assert_eq!(filter(0x2000_0000, 0x0850_0000), 0x95, "minimum");
        assert_eq!(filter(0x4000_0000, 0x0850_0000), 0x95, "maximum");
        assert_eq!(filter(0x4000_0000, 0x00F0_0000), 0xD5, "maximum aniso");
        // Everything else is unchanged.
        let s = StaticSampler::from_words([0x2000_3088, 0x0054_0100, 0x1400_0080, 0x8000_0000]);
        assert_eq!(
            StaticSampler {
                filter: s.filter,
                ..s.comparison()
            },
            s
        );
    }

    /// The LOD fields are independent; a maximum below the minimum is raised
    /// to it so the host sampler stays valid, and an ordered pair is kept.
    #[test]
    fn max_lod_is_never_below_min_lod() {
        // Linear mip filter, so the LOD fields are read.
        let lods = |w1: u32| {
            let s = StaticSampler::from_words([0, w1, 0x0800_0000, 0]);
            (s.min_lod, s.max_lod)
        };
        assert_eq!(lods(0x0010_0400), (4.0, 4.0), "max 1 below min 4");
        assert_eq!(lods(0x0000_0FFF), (4095.0 / 256.0, 4095.0 / 256.0));
        assert_eq!(lods(0x0040_0400), (4.0, 4.0), "equal is kept");
        assert_eq!(lods(0x0054_0100), (1.0, 5.25), "ordered is kept");
        // Without a mip filter only the base level is sampled.
        let base = StaticSampler::from_words([0, 0x0010_0400, 0, 0]);
        assert_eq!((base.min_lod, base.max_lod), (0.0, 0.0));
    }

    /// Each field of the sampler words maps on its own: every address mode on
    /// each axis, the anisotropy clamp at D3D12's 16, every comparison
    /// function, every border colour, and the reserved reduction.
    #[test]
    fn each_field_decodes_on_its_own() {
        let s = |w0: u32, w2: u32, w3: u32| StaticSampler::from_words([w0, 0, w2, w3]);
        // Wrap, mirror, clamp, mirror once; the half-border modes (4, 5) and
        // the border modes (6, 7) become clamp, mirror once, border and
        // mirror once.
        let modes = [1, 2, 3, 5, 3, 5, 4, 5];
        for (m, want) in modes.into_iter().enumerate() {
            let m = m as u32;
            assert_eq!(s(m, 0, 0).address, [want, 1, 1], "U mode {m}");
            assert_eq!(s(m << 3, 0, 0).address, [1, want, 1], "V mode {m}");
            assert_eq!(s(m << 6, 0, 0).address, [1, 1, want], "W mode {m}");
        }
        // log2 anisotropy 0-4 is 1-16; 5-7 would pass D3D12's 16 and clamp.
        let aniso: Vec<u32> = (0..8).map(|n| s(n << 9, 0, 0).max_anisotropy).collect();
        assert_eq!(aniso, [1, 2, 4, 8, 16, 16, 16, 16]);
        // Never through always: `D3D12_COMPARISON_FUNC` counts from 1.
        let funcs: Vec<u32> = (0..8).map(|n| s(n << 12, 0, 0).comparison).collect();
        assert_eq!(funcs, [1, 2, 3, 4, 5, 6, 7, 8]);
        // Transparent black, opaque black, opaque white; custom falls back to
        // transparent black.
        let borders: Vec<u32> = (0..4).map(|n| s(0, 0, n << 30).border).collect();
        assert_eq!(borders, [0, 1, 2, 0]);
        // The reserved reduction (3) filters, as 0 does, for point and
        // anisotropic filters alike.
        assert_eq!(s(0x6000_0000, 0, 0).filter, 0);
        assert_eq!(s(0x6000_0000, 0x00F0_0000, 0).filter, 0x55);
        // Fields outside the decoded bits change nothing.
        assert_eq!(s(0x8000_0000 | 0x0FFF_8000, 0, 0x3FFF_FFFF), s(0, 0, 0));
    }

    /// A single anisotropic filter, magnification or minification, makes the
    /// whole sampler anisotropic, and the mip filter does not change it.
    #[test]
    fn either_anisotropic_filter_is_anisotropic() {
        let filter = |w2: u32| StaticSampler::from_words([0, 0, w2, 0]).filter;
        for mag in 0..4u32 {
            for min in 0..4u32 {
                for mip in 0..3u32 {
                    let f = filter(mag << 20 | min << 22 | mip << 26);
                    if mag >= 2 || min >= 2 {
                        assert_eq!(f, 0x55, "mag {mag} min {min} mip {mip}");
                    } else {
                        let want = min << 4 | mag << 2 | u32::from(mip == 2);
                        assert_eq!(f, want, "mag {mag} min {min} mip {mip}");
                    }
                }
            }
        }
    }

    /// The minimum and maximum reductions sit beside every point and linear
    /// filter combination unchanged, as they do beside the anisotropic one.
    #[test]
    fn the_reduction_keeps_every_filter_combination() {
        let filter = |w0: u32, w2: u32| StaticSampler::from_words([w0, 0, w2, 0]).filter;
        for (field, reduction) in [(1u32, 2u32), (2, 3)] {
            for mag in 0..2u32 {
                for min in 0..2u32 {
                    for mip in 0..3u32 {
                        let w2 = mag << 20 | min << 22 | mip << 26;
                        let base = filter(0, w2);
                        assert_eq!(
                            filter(field << 29, w2),
                            base | reduction << 7,
                            "reduction {field} mag {mag} min {min} mip {mip}"
                        );
                    }
                }
            }
        }
    }

    /// The fourth mip mode is no linear mip filter, but it is a mip filter:
    /// the LOD range is read as for point.
    #[test]
    fn the_fourth_mip_mode_is_point_with_its_lod_range() {
        let s = |mip: u32| StaticSampler::from_words([0, 0x0054_0100, mip << 26, 0]);
        let fourth = s(3);
        assert_eq!(fourth, s(1));
        assert_eq!(fourth.filter, 0);
        assert_eq!((fourth.min_lod, fourth.max_lod), (1.0, 5.25));
        // The LOD fields' top value is 15.996, not the full 4.8 range of 16.
        let top = StaticSampler::from_words([0, 0x00FF_FFFF, 1 << 26, 0]);
        assert_eq!((top.min_lod, top.max_lod), (4095.0 / 256.0, 4095.0 / 256.0));
    }

    /// Bits of words 1 and 2 outside the LOD, bias and filter fields change
    /// nothing, with a mip filter set so the LOD fields are read.
    #[test]
    fn undecoded_bits_of_words_1_and_2_are_ignored() {
        let (w1, w2) = (0x0054_0100, 0x0850_0080);
        let base = StaticSampler::from_words([0, w1, w2, 0]);
        let unused_w1 = 0xFF00_0000;
        // 14-19 between the bias and the filters, 24-25 between the
        // minification and mip filters, 28-31 past the mip filter.
        let unused_w2 = 0x000F_C000 | 0x0300_0000 | 0xF000_0000;
        assert_eq!(
            StaticSampler::from_words([0, w1 | unused_w1, w2 | unused_w2, 0]),
            base
        );
        assert_eq!((base.filter, base.mip_lod_bias), (0x15, 0.5));
    }

    /// Bits of words 0 and 3 outside the address, anisotropy, comparison,
    /// reduction and border fields change nothing either: word 0's 15-28
    /// and 31 sit between and past the decoded fields, and word 3 is read
    /// only at its top two bits.
    #[test]
    fn undecoded_bits_of_words_0_and_3_are_ignored() {
        // Clamp, border and wrap, anisotropy 4, less-equal, minimum
        // reduction, opaque white, over a linear mip with an LOD range.
        let (w0, w3) = (0x2000_3432, 0x8000_0000);
        let (w1, w2) = (0x0054_0100, 0x0850_0080);
        let base = StaticSampler::from_words([w0, w1, w2, w3]);
        let unused_w0 = 0x1FFF_8000 | 0x8000_0000;
        let unused_w3 = 0x3FFF_FFFF;
        assert_eq!(
            StaticSampler::from_words([w0 | unused_w0, w1, w2, w3 | unused_w3]),
            base
        );
        assert_eq!(
            (base.address, base.max_anisotropy, base.comparison),
            ([3, 4, 1], 4, 4)
        );
        assert_eq!((base.filter, base.border), (0x115, 2));
        // The unused bits alone are the all-zero sampler.
        assert_eq!(
            StaticSampler::from_words([unused_w0, 0, 0, unused_w3]),
            StaticSampler::from_words([0; 4])
        );
    }

    /// The clamp sits exactly at D3D12's bounds: the last bias step below
    /// 15.99 is kept and the next clamps; -16 and the step above it are kept.
    #[test]
    fn lod_bias_clamps_at_the_exact_host_bound() {
        let bias = |field: u32| StaticSampler::from_words([0, 0, field, 0]).mip_lod_bias;
        assert_eq!(bias(0x0FFD), 4093.0 / 256.0, "15.988 is kept");
        assert_eq!(bias(0x0FFE), 15.99, "15.992 clamps");
        assert_eq!(bias(0x3001), -4095.0 / 256.0, "-15.996 is kept");
        assert_eq!(bias(0x2FFF), -16.0, "-16.004 clamps");
        assert_eq!(bias(0), 0.0);
    }

    /// Comparison is a reduction like any other: applied twice it is applied
    /// once, and the reserved reduction (3) becomes comparison as filtering
    /// does.
    #[test]
    fn comparison_is_idempotent_and_takes_the_reserved_reduction() {
        for w0 in [0, 0x2000_0000, 0x4000_0000, 0x6000_0000] {
            for w2 in [0, 0x0850_0000, 0x00F0_0000] {
                let once = StaticSampler::from_words([w0, 0, w2, 0]).comparison();
                assert_eq!(once.comparison(), once, "w0 {w0:#x} w2 {w2:#x}");
                assert_eq!(once.filter >> 7, 1, "w0 {w0:#x} w2 {w2:#x}");
            }
        }
        let reserved = |w2: u32| StaticSampler::from_words([0x6000_0000, 0, w2, 0]);
        assert_eq!(reserved(0).comparison().filter, 0x80);
        assert_eq!(reserved(0x00F0_0000).comparison().filter, 0xD5);
    }

    /// The anisotropy field is read whatever the filters say, and an
    /// anisotropic filter without a mip filter still samples only the base
    /// level: the LOD range follows the mip field alone.
    #[test]
    fn anisotropy_and_lod_range_are_independent_of_the_filter() {
        let s = |w0: u32, w2: u32| StaticSampler::from_words([w0, 0x0054_0100, w2, 0]);
        // log2 anisotropy 2 with point filters keeps 4, as with anisotropic.
        assert_eq!(s(2 << 9, 0).max_anisotropy, 4);
        assert_eq!(s(2 << 9, 0x00F0_0000).max_anisotropy, 4);
        // Anisotropic magnification and minification, no mip filter.
        let base = s(0, 0x00F0_0000);
        assert_eq!((base.filter, base.min_lod, base.max_lod), (0x55, 0.0, 0.0));
        // The same with a linear mip filter reads the LOD fields.
        let mipped = s(0, 0x08F0_0000);
        assert_eq!(
            (mipped.filter, mipped.min_lod, mipped.max_lod),
            (0x55, 1.0, 5.25)
        );
    }

    /// Whatever the four words hold, the result is a sampler D3D12 accepts:
    /// bias within its bounds, `MaxLOD >= MinLOD`, anisotropy 1-16, a
    /// comparison function, a static border colour, an address mode and a
    /// filter it defines; and the comparison form always takes the
    /// comparison reduction.
    #[test]
    fn every_word_set_decodes_to_a_valid_host_sampler() {
        // xorshift32 over a fixed seed, plus the all-zero and all-one words.
        let mut x = 0x9E37_79B9u32;
        let mut next = move || {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            x
        };
        let mut words = vec![[0; 4], [u32::MAX; 4]];
        words.extend((0..4096).map(|_| [next(), next(), next(), next()]));
        for w in words {
            let s = StaticSampler::from_words(w);
            let at = format!("{w:08x?}");
            assert!(
                (MIP_LOD_BIAS.0..=MIP_LOD_BIAS.1).contains(&s.mip_lod_bias),
                "{at}"
            );
            assert!(s.min_lod >= 0.0 && s.max_lod >= s.min_lod, "{at}");
            assert!(s.max_lod <= 4095.0 / 256.0, "{at}");
            assert!(
                s.max_anisotropy.is_power_of_two() && s.max_anisotropy <= 16,
                "{at}"
            );
            assert!((1..=8).contains(&s.comparison), "{at}");
            assert!(s.border <= 2, "{at}");
            assert!(s.address.iter().all(|a| (1..=5).contains(a)), "{at}");
            // Point/linear combinations, or anisotropic; reduction 0, 2 or 3.
            let reduction = s.filter >> 7;
            let base = s.filter & 0x7F;
            assert!(matches!(reduction, 0 | 2 | 3), "{at}");
            assert!(base == 0x55 || base & !0x15 == 0, "{at}");
            let c = s.comparison();
            assert_eq!((c.filter >> 7, c.filter & 0x7F), (1, base), "{at}");
        }
    }
}
