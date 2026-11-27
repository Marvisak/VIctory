//! VIctory's own settings, as typed Rust values the title's menu can show.
//!
//! A setting is a fieldless enum implementing [`Choice`]: its options, their
//! menu labels ([`Text`]) and their spelling in `victory.toml`. Everything
//! else follows from that one impl:
//!
//! * the native ◀ ▶ row in Settings → Graphics (in the `victory` app),
//! * reading the value at startup ([`crate::Config`]),
//! * writing it back when the player changes it ([`file::save`]),
//! * the checks every setting must pass ([`validate`]).
//!
//! How to add or change a setting: `docs/native-settings.md`.

pub mod choice;
pub mod file;
pub mod graphics;
pub mod stock_labels;
pub mod text;

pub use choice::{Choice, ConfigKey, Opt, SettingError, validate};
pub use text::{Text, joaat};
