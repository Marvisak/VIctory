//! Settings with a fixed list of options: a Rust enum, shown as a native
//! ◀ ▶ row in the title's menu and stored in `victory.toml`.

use thiserror::Error;

use super::text::Text;

/// Where a setting lives in `victory.toml`: `[section] key = 'value'`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConfigKey {
    pub section: &'static str,
    pub key: &'static str,
}

/// One option of a [`Choice`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Opt<C> {
    /// The enum value it stands for.
    pub value: C,
    /// What the menu shows for it.
    pub label: Text,
    /// How `victory.toml` spells it (must match the type's serde name).
    pub toml: &'static str,
}

/// A setting whose value is one of a few options: implement this on a
/// fieldless enum and the menu, the config file and the tests all follow.
///
/// [`validate`] checks an implementation; every implementation gets a test
/// that calls it (see `docs/native-settings.md`).
pub trait Choice: Copy + Eq + Default + std::fmt::Debug + Send + Sync + 'static {
    /// Where the value is stored.
    const CONFIG: ConfigKey;
    /// The row's label.
    const NAME: Text;
    /// The help shown beside the row.
    const HELP: Text;
    /// Every value once, in menu order.
    const OPTIONS: &'static [Opt<Self>];

    /// The value the loaded configuration holds.
    fn from_config(config: &crate::Config) -> Self;

    /// Position of `self` in [`Self::OPTIONS`].
    fn index(self) -> Option<usize> {
        Self::OPTIONS.iter().position(|o| o.value == self)
    }

    /// The option at menu position `index`.
    fn from_index(index: usize) -> Option<Self> {
        Self::OPTIONS.get(index).map(|o| o.value)
    }

    /// How `victory.toml` spells `self`.
    fn toml_name(self) -> Option<&'static str> {
        Self::OPTIONS
            .iter()
            .find(|o| o.value == self)
            .map(|o| o.toml)
    }

    /// The value `victory.toml` spells `name`.
    fn from_toml(name: &str) -> Option<Self> {
        Self::OPTIONS
            .iter()
            .find(|o| o.toml == name)
            .map(|o| o.value)
    }
}

/// What can be wrong with a [`Choice`] implementation.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum SettingError {
    #[error("{0}: no options")]
    NoOptions(&'static str),
    #[error("{setting}: option {index} repeats an earlier value")]
    RepeatedValue { setting: &'static str, index: usize },
    #[error("{setting}: two options are spelled `{toml}` in victory.toml")]
    RepeatedToml {
        setting: &'static str,
        toml: &'static str,
    },
    #[error("{setting}: `{toml}` is not a plain name (a-z, 0-9, _)")]
    BadToml {
        setting: &'static str,
        toml: &'static str,
    },
    #[error("{setting}: two options share the label ID {id:08x}")]
    RepeatedLabel { setting: &'static str, id: u32 },
    #[error("{0}: the default value is not among the options")]
    DefaultMissing(&'static str),
    #[error("{0}: the name and help share one ID")]
    NameIsHelp(&'static str),
}

/// Checks everything the type system cannot: options unique, spelled
/// plainly, the default among them.
pub fn validate<C: Choice>() -> Result<(), SettingError> {
    let setting = C::CONFIG.key;
    let options = C::OPTIONS;
    if options.is_empty() {
        return Err(SettingError::NoOptions(setting));
    }
    if C::NAME.id() == C::HELP.id() {
        return Err(SettingError::NameIsHelp(setting));
    }
    for (index, option) in options.iter().enumerate() {
        let earlier = &options[..index];
        if earlier.iter().any(|o| o.value == option.value) {
            return Err(SettingError::RepeatedValue { setting, index });
        }
        if earlier.iter().any(|o| o.toml == option.toml) {
            return Err(SettingError::RepeatedToml {
                setting,
                toml: option.toml,
            });
        }
        if earlier.iter().any(|o| o.label.id() == option.label.id()) {
            return Err(SettingError::RepeatedLabel {
                setting,
                id: option.label.id(),
            });
        }
        if !is_plain_name(option.toml) {
            return Err(SettingError::BadToml {
                setting,
                toml: option.toml,
            });
        }
    }
    if C::default().index().is_none() {
        return Err(SettingError::DefaultMissing(setting));
    }
    Ok(())
}

/// Lowercase ASCII letters, digits and `_`: needs no quoting rules.
fn is_plain_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
    enum Mode {
        #[default]
        Off,
        On,
    }

    const OFF: Text = Text::own("victory_test_off", "Off");
    const ON: Text = Text::own("victory_test_on", "On");

    impl Choice for Mode {
        const CONFIG: ConfigKey = ConfigKey {
            section: "test",
            key: "mode",
        };
        const NAME: Text = Text::own("victory_test_mode", "Mode");
        const HELP: Text = Text::own("victory_test_mode_desc", "Mode help");
        const OPTIONS: &'static [Opt<Self>] = &[
            Opt {
                value: Mode::Off,
                label: OFF,
                toml: "off",
            },
            Opt {
                value: Mode::On,
                label: ON,
                toml: "on",
            },
        ];
        fn from_config(_: &crate::Config) -> Self {
            Self::default()
        }
    }

    /// A deliberately broken setting: `OPTIONS` is swapped per test.
    macro_rules! broken {
        ($name:ident, $options:expr) => {
            // Each case uses only the variants its OPTIONS name.
            #[allow(dead_code)]
            #[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
            enum $name {
                #[default]
                A,
                B,
            }
            impl Choice for $name {
                const CONFIG: ConfigKey = ConfigKey {
                    section: "test",
                    key: stringify!($name),
                };
                const NAME: Text = Text::own("victory_test_name", "Name");
                const HELP: Text = Text::own("victory_test_help", "Help");
                const OPTIONS: &'static [Opt<Self>] = $options;
                fn from_config(_: &crate::Config) -> Self {
                    Self::default()
                }
            }
        };
    }

    #[test]
    fn a_well_formed_setting_validates() {
        assert_eq!(validate::<Mode>(), Ok(()));
    }

    #[test]
    fn index_and_value_round_trip() {
        for (i, option) in Mode::OPTIONS.iter().enumerate() {
            assert_eq!(option.value.index(), Some(i));
            assert_eq!(Mode::from_index(i), Some(option.value));
        }
        assert_eq!(Mode::from_index(Mode::OPTIONS.len()), None);
    }

    #[test]
    fn toml_names_round_trip() {
        assert_eq!(Mode::On.toml_name(), Some("on"));
        assert_eq!(Mode::from_toml("off"), Some(Mode::Off));
        assert_eq!(Mode::from_toml("ON"), None, "names are exact");
        assert_eq!(Mode::from_toml("maybe"), None);
    }

    #[test]
    fn no_options_is_refused() {
        broken!(Empty, &[]);
        assert_eq!(validate::<Empty>(), Err(SettingError::NoOptions("Empty")));
    }

    #[test]
    fn a_repeated_value_is_refused() {
        broken!(
            Twice,
            &[
                Opt {
                    value: Twice::A,
                    label: OFF,
                    toml: "a"
                },
                Opt {
                    value: Twice::A,
                    label: ON,
                    toml: "b"
                },
            ]
        );
        assert_eq!(
            validate::<Twice>(),
            Err(SettingError::RepeatedValue {
                setting: "Twice",
                index: 1
            })
        );
    }

    #[test]
    fn a_repeated_spelling_is_refused() {
        broken!(
            Spelled,
            &[
                Opt {
                    value: Spelled::A,
                    label: OFF,
                    toml: "same"
                },
                Opt {
                    value: Spelled::B,
                    label: ON,
                    toml: "same"
                },
            ]
        );
        assert_eq!(
            validate::<Spelled>(),
            Err(SettingError::RepeatedToml {
                setting: "Spelled",
                toml: "same"
            })
        );
    }

    #[test]
    fn a_repeated_label_is_refused() {
        broken!(
            Labels,
            &[
                Opt {
                    value: Labels::A,
                    label: OFF,
                    toml: "a"
                },
                Opt {
                    value: Labels::B,
                    label: OFF,
                    toml: "b"
                },
            ]
        );
        assert_eq!(
            validate::<Labels>(),
            Err(SettingError::RepeatedLabel {
                setting: "Labels",
                id: OFF.id()
            })
        );
    }

    #[test]
    fn a_spelling_that_needs_quoting_is_refused() {
        broken!(
            Quoted,
            &[Opt {
                value: Quoted::A,
                label: OFF,
                toml: "it's"
            }]
        );
        assert_eq!(
            validate::<Quoted>(),
            Err(SettingError::BadToml {
                setting: "Quoted",
                toml: "it's"
            })
        );
    }

    #[test]
    fn a_default_outside_the_options_is_refused() {
        broken!(
            Missing,
            &[Opt {
                value: Missing::B,
                label: OFF,
                toml: "b"
            }]
        );
        assert_eq!(
            validate::<Missing>(),
            Err(SettingError::DefaultMissing("Missing"))
        );
    }
}
