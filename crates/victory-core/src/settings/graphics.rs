//! The settings VIctory adds to the title's Settings → Graphics tab.
//!
//! To add one: an enum in [`crate::config`] (read from `victory.toml`), a
//! [`Choice`] impl here, a test that calls [`validate`], and its row in the
//! app's menu list. Step by step: `docs/native-settings.md`.

use super::choice::{Choice, ConfigKey, Opt};
use super::text::Text;
use crate::config::{FrameGeneration, GraphicsUpscaler};

/// The hidden tab's name in the tab bar. A tab shows the text of its
/// section's *screen* ID (Display's "Video" is keyed by its screen
/// `c399a717`, read from the running title), and VIctory registers the
/// Graphics section with screen `9a46cdb1`. This build of the title has no
/// text for it, so VIctory supplies it.
pub const GRAPHICS_TAB: Text = Text::title(0x9a46_cdb1, "Graphics");

/// The heading of VIctory's section inside the tab.
pub const UPSCALING_GROUP: Text = Text::own("victory_upscaling", "Upscaling");

impl Choice for GraphicsUpscaler {
    const CONFIG: ConfigKey = ConfigKey {
        section: "graphics",
        key: "upscaler",
    };
    const NAME: Text = Text::own("victory_upscaler", "Upscaler");
    const HELP: Text = Text::own(
        "victory_upscaler_desc",
        "Choose how VIctory upscales the title. Applies after a restart.",
    );
    const OPTIONS: &'static [Opt<Self>] = &[
        Opt {
            value: GraphicsUpscaler::TitleDefault,
            label: Text::own("victory_upscaler_title_default", "Title default"),
            toml: "title_default",
        },
        Opt {
            value: GraphicsUpscaler::AmdFsr,
            label: Text::own("victory_upscaler_amd_fsr", "AMD FSR"),
            toml: "amd_fsr",
        },
    ];

    fn from_config(config: &crate::Config) -> Self {
        config.graphics_upscaler
    }
}

impl Choice for FrameGeneration {
    const CONFIG: ConfigKey = ConfigKey {
        section: "graphics",
        key: "frame_generation",
    };
    const NAME: Text = Text::own("victory_frame_generation", "Frame generation");
    const HELP: Text = Text::own(
        "victory_frame_generation_desc",
        "Inserts generated frames between rendered ones for smoother motion.          Applies after a restart.",
    );
    const OPTIONS: &'static [Opt<Self>] = &[
        Opt {
            value: FrameGeneration::Off,
            label: Text::own("victory_frame_generation_off", "Off"),
            toml: "off",
        },
        Opt {
            value: FrameGeneration::AmdFsr,
            label: Text::own("victory_frame_generation_amd_fsr", "AMD FSR"),
            toml: "amd_fsr",
        },
    ];

    fn from_config(config: &crate::Config) -> Self {
        config.frame_generation
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::validate;

    /// Every option's `toml` spelling must be what serde reads, or a value
    /// saved from the menu would not load again.
    fn assert_matches_serde<C>()
    where
        C: Choice + serde::de::DeserializeOwned,
    {
        for option in C::OPTIONS {
            let doc = format!("value = '{}'", option.toml);
            let parsed: std::collections::HashMap<String, C> = toml::from_str(&doc)
                .unwrap_or_else(|e| panic!("`{}` does not load: {e}", option.toml));
            assert_eq!(parsed["value"], option.value, "`{}`", option.toml);
        }
    }

    #[test]
    fn upscaler_is_well_formed() {
        assert_eq!(validate::<GraphicsUpscaler>(), Ok(()));
    }

    #[test]
    fn upscaler_spellings_load_back() {
        assert_matches_serde::<GraphicsUpscaler>();
    }

    #[test]
    fn upscaler_lists_every_variant() {
        // A new variant makes this match fail to compile until it is listed
        // here, and then the assertion until it is in OPTIONS.
        let all = |v: GraphicsUpscaler| match v {
            GraphicsUpscaler::TitleDefault | GraphicsUpscaler::AmdFsr => v.index().is_some(),
        };
        assert!(all(GraphicsUpscaler::TitleDefault) && all(GraphicsUpscaler::AmdFsr));
    }

    #[test]
    fn frame_generation_is_well_formed() {
        assert_eq!(validate::<FrameGeneration>(), Ok(()));
    }

    #[test]
    fn frame_generation_spellings_load_back() {
        assert_matches_serde::<FrameGeneration>();
    }

    #[test]
    fn frame_generation_lists_every_variant() {
        let all = |v: FrameGeneration| match v {
            FrameGeneration::Off | FrameGeneration::AmdFsr => v.index().is_some(),
        };
        assert!(all(FrameGeneration::Off) && all(FrameGeneration::AmdFsr));
    }

    #[test]
    fn graphics_tab_uses_the_screen_id() {
        // The screen the Graphics section is registered with (menu_trace.rs).
        assert_eq!(GRAPHICS_TAB.id(), 0x9a46_cdb1);
    }

    #[test]
    fn menu_texts_do_not_collide() {
        let mut ids: Vec<u32> = [
            GRAPHICS_TAB,
            UPSCALING_GROUP,
            GraphicsUpscaler::NAME,
            GraphicsUpscaler::HELP,
            FrameGeneration::NAME,
            FrameGeneration::HELP,
        ]
        .iter()
        .chain(GraphicsUpscaler::OPTIONS.iter().map(|o| &o.label))
        .chain(FrameGeneration::OPTIONS.iter().map(|o| &o.label))
        .map(|t| t.id())
        .collect();
        let count = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), count);
    }
}
