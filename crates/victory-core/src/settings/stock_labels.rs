//! English for the title's own Settings → Graphics rows, whose text this
//! build does not ship (the rows show blank).
//!
//! A row looks its name and help up by two 32-bit IDs (`joaat` of a label
//! key and of `key + "_desc"`). Rows are matched by those IDs, not by
//! position, so they keep their text if rows move. Which setting each row
//! controls was worked out from the title's settings metadata; the wording
//! is VIctory's own. All text lives in this file, one table per language,
//! so more languages can follow.

use super::text::Text;

/// One stock row and VIctory's text for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StockLabel {
    /// The row's name ID.
    pub name_id: u32,
    pub name: &'static str,
    /// The row's help ID, 0 for rows without help.
    pub help_id: u32,
    pub help: Option<&'static str>,
}

impl StockLabel {
    /// The row's name and (if any) help, as menu text.
    pub fn texts(&self) -> impl Iterator<Item = Text> {
        let name = Text::title(self.name_id, self.name);
        let help = self.help.map(|help| Text::title(self.help_id, help));
        std::iter::once(name).chain(help)
    }
}

/// English, for every stock Graphics row that has a name ID (row number
/// from the title's order in the trailing comment).
pub const EN: &[StockLabel] = &[
    StockLabel {
        name_id: 0x6896_de76,
        name: "Ambient lighting quality",
        help_id: 0x0651_dd7e,
        help: Some("Quality of the indirect light that fills shaded areas."),
    }, // G1
    StockLabel {
        name_id: 0x19c1_ad65,
        name: "Anisotropic filtering",
        help_id: 0xbc82_3ad0,
        help: Some("Keeps textures sharp when seen at an angle."),
    }, // G2
    StockLabel {
        name_id: 0x1861_e5a6,
        name: "Target frame rate",
        help_id: 0xd933_1a9f,
        help: Some("Frame rate the game aims for."),
    }, // G3
    StockLabel {
        name_id: 0x8916_3eb1,
        name: "Sun shadow quality",
        help_id: 0x713d_c7eb,
        help: Some("Resolution and detail of shadows cast by the sun and moon."),
    }, // G4
    StockLabel {
        name_id: 0x0b25_dfcf,
        name: "Upscaling quality",
        help_id: 0xda15_102a,
        help: Some("Render resolution used by the upscaler. Lower is faster, higher is sharper."),
    }, // G5
    StockLabel {
        name_id: 0x588b_587f,
        name: "Global illumination quality",
        help_id: 0x1c34_418e,
        help: Some("Quality of light bouncing between surfaces."),
    }, // G6
    StockLabel {
        name_id: 0x727f_c65c,
        name: "Graphics preset",
        help_id: 0x07ba_ef5d,
        help: Some("Sets many options at once, from low to ultra."),
    }, // G7
    StockLabel {
        name_id: 0x3e8c_994e,
        name: "Hair quality",
        help_id: 0x5c7b_b3e9,
        help: Some("Detail of character hair."),
    }, // G8
    StockLabel {
        name_id: 0x3fd4_5547,
        name: "Lighting quality",
        help_id: 0x4c71_6b1d,
        help: Some("Overall quality of lights in the scene."),
    }, // G9
    StockLabel {
        name_id: 0x82cb_a269,
        name: "Local shadow quality",
        help_id: 0x2fe9_195e,
        help: Some("Detail of shadows cast by lamps and other nearby lights."),
    }, // G10
    StockLabel {
        name_id: 0xb96b_da6d,
        name: "Mirror quality",
        help_id: 0x104a_c344,
        help: Some("Detail of reflections in mirrors."),
    }, // G11
    StockLabel {
        name_id: 0x544b_440f,
        name: "Particle quality",
        help_id: 0x965f_44b5,
        help: Some("Detail of smoke, sparks, dust and other effects."),
    }, // G12
    StockLabel {
        name_id: 0xcd67_6af6,
        name: "Reflection quality",
        help_id: 0xe8f5_471e,
        help: Some("Detail of reflections on shiny surfaces."),
    }, // G13
    StockLabel {
        name_id: 0xfa0f_3db5,
        name: "Subsurface scattering quality",
        help_id: 0x4e4c_6a04,
        help: Some("How light passes through skin and similar materials."),
    }, // G14
    StockLabel {
        name_id: 0x949a_6d1e,
        name: "Temporal anti-aliasing",
        help_id: 0x9016_bdb5,
        help: Some("Smooths jagged edges by blending recent frames."),
    }, // G15
    StockLabel {
        name_id: 0x0f98_6bb4,
        name: "Tessellation",
        help_id: 0x9e79_b35c,
        help: Some("Adds geometric detail to surfaces such as roads and rocks."),
    }, // G16
    StockLabel {
        name_id: 0x78f7_6acc,
        name: "Texture quality",
        help_id: 0xf3ad_0f6f,
        help: Some("Resolution of textures. Higher needs more video memory."),
    }, // G17
    StockLabel {
        name_id: 0x893e_9ed3,
        name: "Volumetrics quality",
        help_id: 0x3591_e44e,
        help: Some("Quality of fog, mist and light shafts."),
    }, // G18
    StockLabel {
        name_id: 0xc43d_3b9f,
        name: "Water quality",
        help_id: 0x0026_e7f8,
        help: Some("Overall detail of water."),
    }, // G19
    StockLabel {
        name_id: 0xd1cc_b6de,
        name: "Async compute",
        help_id: 0x133d_df31,
        help: Some("Runs some GPU work in parallel. Can help or hurt depending on the GPU."),
    }, // G20
    StockLabel {
        name_id: 0x21ea_1218,
        name: "Alpha transmission",
        help_id: 0x0000_0000,
        help: None,
    }, // G21
    StockLabel {
        name_id: 0x328d_c9f4,
        name: "Ray tracing BVH quality",
        help_id: 0xcb87_dff1,
        help: Some("Detail of the scene used for ray tracing."),
    }, // G22
    StockLabel {
        name_id: 0x29cb_2e71,
        name: "Damage models",
        help_id: 0xa16c_8c72,
        help: Some("Shows detailed damage on vehicles and objects."),
    }, // G23
    StockLabel {
        name_id: 0xc527_c1e7,
        name: "Decal quality",
        help_id: 0xdbb5_0da8,
        help: Some("Detail of marks such as tyre tracks, dirt and bullet holes."),
    }, // G24
    StockLabel {
        name_id: 0x32ac_bb73,
        name: "Deep surface quality",
        help_id: 0x0406_ae28,
        help: Some("Detail of surfaces like mud and snow that objects sink into."),
    }, // G25
    StockLabel {
        name_id: 0x053f_8e3b,
        name: "Screen-space sun shadows",
        help_id: 0x64b6_7320,
        help: Some("Adds fine contact shadows from the sun."),
    }, // G26
    StockLabel {
        name_id: 0x1e97_cf85,
        name: "Upscaling sharpness",
        help_id: 0xf45e_fb84,
        help: Some("How much the upscaler sharpens the image."),
    }, // G27
    StockLabel {
        name_id: 0x79fb_2767,
        name: "Fur quality",
        help_id: 0xbed6_0833,
        help: Some("Detail of animal fur."),
    }, // G28
    StockLabel {
        name_id: 0x2aa5_adda,
        name: "Grass draw distance",
        help_id: 0xee79_4b6c,
        help: Some("How far away grass is drawn."),
    }, // G29
    StockLabel {
        name_id: 0xf77d_32f9,
        name: "Hair memory budget",
        help_id: 0x75f8_0ed6,
        help: Some("Video memory set aside for hair."),
    }, // G30
    StockLabel {
        name_id: 0x8df8_8222,
        name: "Hair simulation quality",
        help_id: 0x4cad_0abe,
        help: Some("How realistically hair moves."),
    }, // G31
    StockLabel {
        name_id: 0xb95f_c799,
        name: "Level of detail distance",
        help_id: 0x3a79_f82a,
        help: Some("How far objects keep their full detail."),
    }, // G32
    StockLabel {
        name_id: 0xd6ce_6581,
        name: "Motion blur",
        help_id: 0xbe0d_49f4,
        help: Some("Blurs fast movement."),
    }, // G35
    StockLabel {
        name_id: 0x4d02_1b51,
        name: "Particle lighting quality",
        help_id: 0x17ff_61df,
        help: Some("How well effects such as smoke are lit."),
    }, // G36
    StockLabel {
        name_id: 0x601b_87d8,
        name: "Parallax occlusion mapping",
        help_id: 0x58c6_a7b7,
        help: Some("Adds depth to flat surfaces such as brick and cobblestone."),
    }, // G39
    StockLabel {
        name_id: 0x1501_c237,
        name: "Ray-traced sun shadows",
        help_id: 0x0000_0000,
        help: None,
    }, // G40
    StockLabel {
        name_id: 0x24e6_fd43,
        name: "Ray-traced local shadows",
        help_id: 0x0000_0000,
        help: None,
    }, // G41
    StockLabel {
        name_id: 0x7eb7_28dd,
        name: "Ray-traced distant light shadows",
        help_id: 0x0000_0000,
        help: None,
    }, // G42
    StockLabel {
        name_id: 0x8ef0_e277,
        name: "Ray-traced dynamic shadows",
        help_id: 0x0000_0000,
        help: None,
    }, // G43
    StockLabel {
        name_id: 0x0bf6_1c5c,
        name: "Upscaling mode",
        help_id: 0xc463_1129,
        help: Some("Ratio between the render resolution and the output resolution."),
    }, // G44
    StockLabel {
        name_id: 0x75c7_0485,
        name: "Atmospheric scattering quality",
        help_id: 0xd196_8c6c,
        help: Some("Quality of haze and light in the air."),
    }, // G45
    StockLabel {
        name_id: 0xe0b5_f504,
        name: "Transparent shadow quality",
        help_id: 0xaa79_3d00,
        help: Some("Detail of shadows from glass, foliage and similar surfaces."),
    }, // G46
    StockLabel {
        name_id: 0xe79c_3f8f,
        name: "Tinted shadows",
        help_id: 0x9d5c_3548,
        help: Some("Coloured shadows from tinted glass."),
    }, // G47
    StockLabel {
        name_id: 0x2282_abf1,
        name: "Grass shadows",
        help_id: 0xaf06_fc15,
        help: Some("Lets grass cast shadows."),
    }, // G48
    StockLabel {
        name_id: 0xe0fd_16f8,
        name: "Long shadows",
        help_id: 0x29c3_9892,
        help: Some("Draws shadows farther away."),
    }, // G49
    StockLabel {
        name_id: 0x6834_d987,
        name: "Particle shadows",
        help_id: 0x41f7_2f8c,
        help: Some("Lets smoke and other effects cast shadows."),
    }, // G50
    StockLabel {
        name_id: 0x73c2_58a0,
        name: "Soft shadows",
        help_id: 0xb520_3db4,
        help: Some("How soft shadow edges look."),
    }, // G51
    StockLabel {
        name_id: 0x9fc8_6f40,
        name: "Sharpening strength",
        help_id: 0xd822_7d6b,
        help: Some("How much the image is sharpened."),
    }, // G53
    StockLabel {
        name_id: 0x9c47_ea42,
        name: "Terrain quality",
        help_id: 0x6aba_c474,
        help: Some("Detail of the ground."),
    }, // G54
    StockLabel {
        name_id: 0x0a00_2f12,
        name: "Terrain shadow quality",
        help_id: 0xdb35_8169,
        help: Some("Detail of shadows cast by hills and terrain."),
    }, // G55
    StockLabel {
        name_id: 0xba2b_2657,
        name: "Tree quality",
        help_id: 0xffeb_863e,
        help: Some("Detail of trees."),
    }, // G56
    StockLabel {
        name_id: 0xa178_3cb0,
        name: "Tree tessellation",
        help_id: 0x2468_2e0b,
        help: Some("Adds geometric detail to trees."),
    }, // G57
    StockLabel {
        name_id: 0x479c_9364,
        name: "Vehicle detail distance",
        help_id: 0x4cc0_bc67,
        help: Some("How far vehicles keep their full detail."),
    }, // G59
    StockLabel {
        name_id: 0xaadb_87cf,
        name: "Vehicle texture quality",
        help_id: 0x0612_7053,
        help: Some("Resolution of vehicle textures."),
    }, // G60
    StockLabel {
        name_id: 0x8a44_6cc3,
        name: "Volumetric lighting quality",
        help_id: 0xd683_24f2,
        help: Some("Quality of light passing through fog and mist."),
    }, // G61
    StockLabel {
        name_id: 0xdaf1_5de8,
        name: "Full-resolution volumetrics",
        help_id: 0x7be6_9fcf,
        help: Some("Renders fog and light shafts without lowering their resolution."),
    }, // G62
    StockLabel {
        name_id: 0x0bd3_a384,
        name: "Water lighting quality",
        help_id: 0x70a9_f115,
        help: Some("How well water is lit."),
    }, // G63
    StockLabel {
        name_id: 0x5e40_2366,
        name: "Screen-space water reflections",
        help_id: 0xd517_62c0,
        help: Some("Adds reflections of nearby objects on water."),
    }, // G66
    StockLabel {
        name_id: 0x24cb_1f6c,
        name: "Water reflection quality",
        help_id: 0x4d14_a412,
        help: Some("Detail of reflections on water."),
    }, // G67
    StockLabel {
        name_id: 0x3df5_6743,
        name: "Water refraction quality",
        help_id: 0xcf76_9761,
        help: Some("Detail of what is seen through water."),
    }, // G68
    StockLabel {
        name_id: 0xd5c4_a187,
        name: "Water simulation quality",
        help_id: 0xa7f0_9a54,
        help: Some("How realistically water moves."),
    }, // G69
    StockLabel {
        name_id: 0x4b91_c0f1,
        name: "Wave simulation quality",
        help_id: 0x0e57_fd01,
        help: Some("Detail of waves on the water surface."),
    }, // G70
    StockLabel {
        name_id: 0xca83_aa05,
        name: "Water tessellation",
        help_id: 0x8956_da84,
        help: Some("Adds geometric detail to the water surface."),
    }, // G71
    StockLabel {
        name_id: 0xac7d_ca88,
        name: "Terrain height shadows",
        help_id: 0x7444_526c,
        help: Some("Quality of large shadows cast by the landscape."),
    }, // G72
    StockLabel {
        name_id: 0x9781_55a1,
        name: "Map anti-aliasing",
        help_id: 0x8d01_bf56,
        help: Some("Smooths edges on the map screen."),
    }, // G73
    StockLabel {
        name_id: 0xff7f_05de,
        name: "Display adapter",
        help_id: 0xd562_56b3,
        help: Some("The graphics card used for the game."),
    }, // G74
    StockLabel {
        name_id: 0x51b0_58b3,
        name: "Monitor",
        help_id: 0x18ec_72d6,
        help: Some("The display the game runs on."),
    }, // G75
    StockLabel {
        name_id: 0x4399_8925,
        name: "Triple buffering",
        help_id: 0xd06f_3e3c,
        help: Some("Smoother frame pacing with V-Sync, at the cost of some latency."),
    }, // G76
    StockLabel {
        name_id: 0xfb0e_70c2,
        name: "V-Sync",
        help_id: 0x690e_11f7,
        help: Some("Syncs frames to the display to prevent tearing."),
    }, // G77
];

/// The table for a locale such as `en-US`; English when there is none.
pub fn for_locale(_locale: &str) -> &'static [StockLabel] {
    EN
}

/// Help for a stock row that has a name but no help ID: VIctory gives it
/// a help ID of its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExtraHelp {
    /// The row's name ID.
    pub row: u32,
    pub help: Text,
}

/// English help for the stock Graphics rows without a help ID.
pub const EN_EXTRA_HELP: &[ExtraHelp] = &[
    ExtraHelp {
        row: 0x21ea_1218, // Alpha transmission
        help: Text::own(
            "victory_stock_alpha_transmission_desc",
            "How light shines through thin surfaces such as leaves and fabric.",
        ),
    },
    ExtraHelp {
        row: 0x1501_c237, // Ray-traced sun shadows
        help: Text::own(
            "victory_stock_rt_sun_shadows_desc",
            "Uses ray tracing for shadows from the sun and moon.",
        ),
    },
    ExtraHelp {
        row: 0x24e6_fd43, // Ray-traced local shadows
        help: Text::own(
            "victory_stock_rt_local_shadows_desc",
            "Uses ray tracing for shadows from lamps and other nearby lights.",
        ),
    },
    ExtraHelp {
        row: 0x7eb7_28dd, // Ray-traced distant light shadows
        help: Text::own(
            "victory_stock_rt_distant_light_shadows_desc",
            "Uses ray tracing for shadows from far-away lights.",
        ),
    },
    ExtraHelp {
        row: 0x8ef0_e277, // Ray-traced dynamic shadows
        help: Text::own(
            "victory_stock_rt_dynamic_shadows_desc",
            "Uses ray tracing for shadows of moving characters and vehicles.",
        ),
    },
];

/// The extra-help table for a locale; English when there is none.
pub fn extra_help_for_locale(_locale: &str) -> &'static [ExtraHelp] {
    EN_EXTRA_HELP
}

/// A heading for a stock settings group the title builds without one. The
/// group is recognised by a row it holds (that row's name ID), not by its
/// place in the menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GroupHeading {
    /// Name ID of a row in the group.
    pub holds: u32,
    pub heading: Text,
}

/// English headings for the untitled stock Graphics groups.
pub const EN_HEADINGS: &[GroupHeading] = &[
    GroupHeading {
        holds: 0xff7f_05de, // Display adapter
        heading: Text::own("victory_stock_group_display", "Display"),
    },
    GroupHeading {
        holds: 0xd1cc_b6de, // Async compute
        heading: Text::own("victory_stock_group_advanced", "Advanced"),
    },
];

/// The heading table for a locale; English when there is none.
pub fn headings_for_locale(_locale: &str) -> &'static [GroupHeading] {
    EN_HEADINGS
}

/// A stock row the title builds with no name ID at all, so no text can be
/// looked up for it. It is recognised by the named row before it: the
/// `nth` nameless row (from 1) right after the row whose name ID is
/// `after`. VIctory gives it IDs of its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnnamedRow {
    pub after: u32,
    pub nth: u8,
    pub name: Text,
    pub help: Text,
}

/// English for the nameless stock Graphics rows (the setting each controls
/// in the comment).
pub const EN_UNNAMED: &[UnnamedRow] = &[
    UnnamedRow {
        after: 0xb95f_c799, // Level of detail distance
        nth: 1,
        name: Text::own(
            "victory_stock_texture_streaming",
            "Texture streaming quality",
        ),
        help: Text::own(
            "victory_stock_texture_streaming_desc",
            "How quickly detailed textures load in.",
        ),
    }, // textureStreamingQuality
    UnnamedRow {
        after: 0xb95f_c799,
        nth: 2,
        name: Text::own("victory_stock_lens_flare_res", "Lens flare resolution"),
        help: Text::own(
            "victory_stock_lens_flare_res_desc",
            "How much lens flares are scaled down. Higher is faster.",
        ),
    }, // lensFlaresScreenResDivExponent
    UnnamedRow {
        after: 0x4d02_1b51, // Particle lighting quality
        nth: 1,
        name: Text::own(
            "victory_stock_character_textures",
            "Character texture quality",
        ),
        help: Text::own(
            "victory_stock_character_textures_desc",
            "Resolution of character textures.",
        ),
    }, // pedMaxTextureQuality
    UnnamedRow {
        after: 0x4d02_1b51,
        nth: 2,
        name: Text::own(
            "victory_stock_accessory_textures",
            "Accessory texture quality",
        ),
        help: Text::own(
            "victory_stock_accessory_textures_desc",
            "Resolution of textures on hats, glasses and other accessories.",
        ),
    }, // pedPropMaxTextureQuality
    UnnamedRow {
        after: 0x73c2_58a0, // Soft shadows
        nth: 1,
        name: Text::own("victory_stock_sharpening", "Sharpening"),
        help: Text::own(
            "victory_stock_sharpening_desc",
            "Turns image sharpening on or off.",
        ),
    }, // sharpenEnabled
    UnnamedRow {
        after: 0xa178_3cb0, // Tree tessellation
        nth: 1,
        name: Text::own("victory_stock_distant_objects", "Distant object quality"),
        help: Text::own(
            "victory_stock_distant_objects_desc",
            "Detail of the simplified stand-ins drawn for far-away objects.",
        ),
    }, // imposterQuality
    UnnamedRow {
        after: 0x0bd3_a384, // Water lighting quality
        nth: 1,
        name: Text::own(
            "victory_stock_water_reflection_width",
            "Water reflection width",
        ),
        help: Text::own(
            "victory_stock_water_reflection_width_desc",
            "Horizontal resolution of water reflections.",
        ),
    }, // waterReflectionResX
    UnnamedRow {
        after: 0x0bd3_a384,
        nth: 2,
        name: Text::own(
            "victory_stock_water_reflection_height",
            "Water reflection height",
        ),
        help: Text::own(
            "victory_stock_water_reflection_height_desc",
            "Vertical resolution of water reflections.",
        ),
    }, // waterReflectionResY
];

/// The nameless-row table for a locale; English when there is none.
pub fn unnamed_for_locale(_locale: &str) -> &'static [UnnamedRow] {
    EN_UNNAMED
}

/// The entry for the `nth` nameless row after the row named `after`.
pub fn unnamed(table: &'static [UnnamedRow], after: u32, nth: u8) -> Option<&'static UnnamedRow> {
    table
        .iter()
        .find(|row| row.after == after && row.nth == nth)
}

#[cfg(test)]
mod tests {
    use super::super::text::joaat;
    use super::*;

    #[test]
    fn known_label_keys_hash_to_the_ids() {
        // Keys recovered for some rows: their hashes are the rows' IDs, which
        // confirms the ID scheme (help = key + "_desc").
        for (key, has_help) in [
            ("gfx_amblightingq", true),
            ("gfx_fsr", true),
            ("gfx_hair_quality", true),
            ("gfx_shader_taa", true),
            ("rtlodlightshadows", false),
        ] {
            let label = EN.iter().find(|l| l.name_id == joaat(key)).expect(key);
            let help = if has_help {
                joaat(&format!("{key}_desc"))
            } else {
                0
            };
            assert_eq!(label.help_id, help, "{key}");
        }
    }

    #[test]
    fn ids_are_unique_and_help_matches_its_id() {
        let mut ids: Vec<u32> = EN
            .iter()
            .flat_map(StockLabel::texts)
            .map(Text::id)
            .collect();
        let count = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), count, "an ID repeats");
        for label in EN {
            assert_ne!(label.name_id, 0);
            assert_eq!(label.help.is_some(), label.help_id != 0, "{}", label.name);
        }
    }

    #[test]
    fn every_unnamed_row_follows_a_labelled_row_and_has_unique_ids() {
        for row in EN_UNNAMED {
            assert!(
                EN.iter().any(|l| l.name_id == row.after),
                "{}",
                row.name.english()
            );
            assert!(row.nth >= 1);
            assert_eq!(unnamed(EN_UNNAMED, row.after, row.nth), Some(row));
        }
        let mut ids: Vec<u32> = EN_UNNAMED
            .iter()
            .flat_map(|r| [r.name.id(), r.help.id()])
            .collect();
        ids.extend(EN.iter().flat_map(StockLabel::texts).map(Text::id));
        let count = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), count, "an ID repeats");
    }

    #[test]
    fn extra_help_and_headings_point_at_labelled_rows() {
        for extra in EN_EXTRA_HELP {
            let row = EN
                .iter()
                .find(|l| l.name_id == extra.row)
                .expect("labelled row");
            assert_eq!(row.help_id, 0, "{} already has help", row.name);
        }
        for heading in EN_HEADINGS {
            assert!(EN.iter().any(|l| l.name_id == heading.holds));
        }
        let mut ids: Vec<u32> = EN_EXTRA_HELP.iter().map(|e| e.help.id()).collect();
        ids.extend(EN_HEADINGS.iter().map(|h| h.heading.id()));
        ids.extend(EN_UNNAMED.iter().flat_map(|r| [r.name.id(), r.help.id()]));
        ids.extend(EN.iter().flat_map(StockLabel::texts).map(Text::id));
        let count = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), count, "an ID repeats");
    }

    #[test]
    fn no_text_is_empty() {
        assert!(
            EN.iter()
                .flat_map(StockLabel::texts)
                .all(|t| !t.english().is_empty())
        );
    }

    #[test]
    fn stock_ids_never_collide_with_our_own() {
        let ours = [
            super::super::graphics::GRAPHICS_TAB.id(),
            super::super::graphics::UPSCALING_GROUP.id(),
        ];
        assert!(
            EN.iter()
                .flat_map(StockLabel::texts)
                .all(|t| !ours.contains(&t.id()))
        );
    }
}
