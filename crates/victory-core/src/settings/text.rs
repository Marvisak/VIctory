//! Menu text: the 32-bit IDs the title looks labels up by, with our English.

/// Rockstar's `joaat` (Jenkins one-at-a-time) hash of `key`, lowercased as
/// the title does. Every menu label is looked up by such an ID.
///
/// ```
/// use victory_core::settings::joaat;
/// assert_eq!(joaat("gfx_fsr"), 0x0b25_dfcf);
/// assert_eq!(joaat("GFX_FSR"), joaat("gfx_fsr"));
/// ```
pub const fn joaat(key: &str) -> u32 {
    let bytes = key.as_bytes();
    let mut hash: u32 = 0;
    let mut i = 0;
    while i < bytes.len() {
        hash = hash.wrapping_add(bytes[i].to_ascii_lowercase() as u32);
        hash = hash.wrapping_add(hash << 10);
        hash ^= hash >> 6;
        i += 1;
    }
    hash = hash.wrapping_add(hash << 3);
    hash ^= hash >> 11;
    hash.wrapping_add(hash << 15)
}

/// Prefix of every text key VIctory owns: a naming rule that keeps our keys
/// apart from the title's. It cannot rule out a 32-bit hash collision; the
/// settings tests check our IDs against each other.
pub const OWN_PREFIX: &str = "victory_";

/// One label in the title's menu: the ID it is looked up by and the English
/// VIctory supplies for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Text {
    id: u32,
    english: &'static str,
}

impl Text {
    /// VIctory's own label. `key` must start with [`OWN_PREFIX`]; in a
    /// `const` this is checked at compile time.
    ///
    /// ```
    /// use victory_core::settings::{Text, joaat};
    /// const UPSCALER: Text = Text::own("victory_upscaler", "Upscaler");
    /// assert_eq!(UPSCALER.id(), joaat("victory_upscaler"));
    /// ```
    ///
    /// ```compile_fail
    /// use victory_core::settings::Text;
    /// const BAD: Text = Text::own("gfx_fsr", "FSR"); // not ours: rejected
    /// ```
    pub const fn own(key: &'static str, english: &'static str) -> Self {
        assert!(
            starts_with(key, OWN_PREFIX),
            "VIctory text keys start with `victory_`"
        );
        Self {
            id: joaat(key),
            english,
        }
    }

    /// English for a label the title already uses (by its ID), such as the
    /// hidden Graphics tab whose text this build does not load.
    pub const fn title(id: u32, english: &'static str) -> Self {
        Self { id, english }
    }

    /// The ID the title looks this label up by.
    pub const fn id(self) -> u32 {
        self.id
    }

    /// The English text VIctory supplies.
    pub const fn english(self) -> &'static str {
        self.english
    }
}

const fn starts_with(s: &str, prefix: &str) -> bool {
    let (s, p) = (s.as_bytes(), prefix.as_bytes());
    if s.len() < p.len() {
        return false;
    }
    let mut i = 0;
    while i < p.len() {
        if s[i] != p[i] {
            return false;
        }
        i += 1;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn joaat_matches_ids_read_from_the_title() {
        // Label IDs read from the title's menu rows and confirmed by name.
        assert_eq!(joaat("gfx_fsr"), 0x0b25_dfcf);
        assert_eq!(joaat("gfx_fsr_desc"), 0xda15_102a);
        assert_eq!(joaat("gfx_hair_quality"), 0x3e8c_994e);
        assert_eq!(joaat("rtlodlightshadows"), 0x7eb7_28dd);
    }

    #[test]
    fn joaat_ignores_case() {
        assert_eq!(joaat("rtLODLightShadows"), joaat("rtlodlightshadows"));
    }

    #[test]
    fn joaat_of_nothing_is_zero() {
        assert_eq!(joaat(""), 0);
    }

    #[test]
    fn own_text_hashes_its_key() {
        let text = Text::own("victory_upscaler", "Upscaler");
        assert_eq!(text.id(), joaat("victory_upscaler"));
        assert_eq!(text.english(), "Upscaler");
    }

    #[test]
    #[should_panic(expected = "start with `victory_`")]
    fn own_text_refuses_a_foreign_key_at_run_time_too() {
        let key = String::from("gfx_fsr").leak();
        let _ = Text::own(key, "FSR");
    }

    #[test]
    fn title_text_keeps_the_given_id() {
        assert_eq!(Text::title(0x7a62_ca2d, "Graphics").id(), 0x7a62_ca2d);
    }
}
