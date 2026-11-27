//! VIctory's settings as the native list rows see them: no title memory
//! here, only the current choice, its range and where it is saved.
//!
//! Each [`ListSetting`] is made from a [`Choice`] (`ListSetting::of::<T>()`);
//! the native side ([`super::native_list`]) only asks it for a count and an
//! index and tells it what the player picked.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};

use victory_core::Config;
use victory_core::config::{FrameGeneration, GraphicsUpscaler};
use victory_core::settings::{Choice, Text, file};

/// Settings → Graphics → Upscaling → Upscaler.
pub(super) static UPSCALER: ListSetting = ListSetting::of::<GraphicsUpscaler>();

/// Settings → Graphics → Upscaling → Frame generation.
pub(super) static FRAME_GENERATION: ListSetting = ListSetting::of::<FrameGeneration>();

/// Every list setting, in menu order: loaded at startup and shown as rows.
pub(super) static ALL: [&ListSetting; 2] = [&UPSCALER, &FRAME_GENERATION];

/// The config file choices are saved to.
static CONFIG_FILE: OnceLock<PathBuf> = OnceLock::new();

/// A [`Choice`] with its type erased, plus the option the player has now.
pub(super) struct ListSetting {
    name: Text,
    help: Text,
    count: usize,
    chosen: AtomicUsize,
    initial: fn(&Config) -> usize,
    save: fn(&Path, usize) -> anyhow::Result<()>,
}

impl ListSetting {
    /// The list setting for `C`.
    pub(super) const fn of<C: Choice>() -> Self {
        Self {
            name: C::NAME,
            help: C::HELP,
            count: C::OPTIONS.len(),
            chosen: AtomicUsize::new(0),
            initial: initial_index::<C>,
            save: save_index::<C>,
        }
    }

    pub(super) fn name(&self) -> Text {
        self.name
    }

    pub(super) fn help(&self) -> Text {
        self.help
    }

    /// The last option's index (the row's max; options are 0..=last).
    pub(super) fn last(&self) -> usize {
        self.count.saturating_sub(1)
    }

    /// The option chosen now.
    pub(super) fn chosen(&self) -> usize {
        self.chosen.load(Ordering::Relaxed)
    }

    /// The option the row's raw value stands for (clamped to the options).
    pub(super) fn index_of(&self, raw: i32) -> usize {
        usize::try_from(raw).unwrap_or(0).min(self.last())
    }

    /// Takes the player's pick: saves it first and only then makes it the
    /// choice, so a failed save keeps the old choice and can be retried.
    /// Returns the new index if it changed.
    pub(super) fn choose(&self, raw: i32) -> anyhow::Result<Option<usize>> {
        let index = self.index_of(raw);
        if index == self.chosen() {
            return Ok(None);
        }
        if let Some(path) = CONFIG_FILE.get() {
            (self.save)(path, index)?;
        }
        self.chosen.store(index, Ordering::Relaxed);
        Ok(Some(index))
    }

    fn load(&self, config: &Config) {
        self.chosen.store((self.initial)(config), Ordering::Relaxed);
    }
}

fn initial_index<C: Choice>(config: &Config) -> usize {
    C::from_config(config).index().unwrap_or(0)
}

fn save_index<C: Choice>(path: &Path, index: usize) -> anyhow::Result<()> {
    let value = C::from_index(index).ok_or_else(|| anyhow::anyhow!("no option {index}"))?;
    file::save(path, value)
}

/// Loads every setting's choice from `config` and remembers where to save.
pub(super) fn load(config: &Config) {
    // Absolute now: the title can change the working directory later.
    let path = config
        .source
        .clone()
        .unwrap_or_else(|| PathBuf::from("victory.toml"));
    let _ = CONFIG_FILE.set(std::path::absolute(&path).unwrap_or(path));
    for setting in ALL {
        setting.load(config);
    }
}

/// Which setting each native row shows. Rows come from the title's pool, so an address is reused: binding
/// it again replaces the old entry.
pub(super) struct Rows(Mutex<Vec<Bound>>);

#[derive(Clone, Copy)]
pub(super) struct Bound {
    pub(super) setting: &'static ListSetting,
    row: usize,
}

impl Rows {
    pub(super) const fn new() -> Self {
        Self(Mutex::new(Vec::new()))
    }

    pub(super) fn bind(&self, row: usize, setting: &'static ListSetting) {
        let mut rows = self.0.lock().unwrap_or_else(|e| e.into_inner());
        rows.retain(|b| b.row != row);
        rows.push(Bound { setting, row });
    }

    pub(super) fn get(&self, row: usize) -> Option<Bound> {
        let rows = self.0.lock().unwrap_or_else(|e| e.into_inner());
        rows.iter().find(|b| b.row == row).copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh() -> ListSetting {
        ListSetting::of::<GraphicsUpscaler>()
    }

    #[test]
    fn range_follows_the_options() {
        assert_eq!(fresh().last(), GraphicsUpscaler::OPTIONS.len() - 1);
    }

    #[test]
    fn choose_keeps_a_valid_index_and_reports_the_change() {
        let s = fresh();
        assert_eq!(s.choose(1).unwrap(), Some(1));
        assert_eq!(s.chosen(), 1);
        assert_eq!(s.choose(1).unwrap(), None, "no change, nothing to save");
    }

    #[test]
    fn choose_clamps_out_of_range_values() {
        let s = fresh();
        assert_eq!(s.index_of(99), s.last());
        assert_eq!(s.index_of(-5), 0);
    }

    #[test]
    fn loads_the_configured_value() {
        let s = fresh();
        let config = Config {
            graphics_upscaler: GraphicsUpscaler::AmdFsr,
            ..Config::default()
        };
        s.load(&config);
        assert_eq!(s.chosen(), GraphicsUpscaler::AmdFsr.index().unwrap());
    }

    #[test]
    fn saving_writes_the_chosen_option() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("victory.toml");
        std::fs::write(&path, "# mine\n[graphics]\nupscaler = 'title_default'\n").unwrap();
        let s = fresh();
        (s.save)(&path, GraphicsUpscaler::AmdFsr.index().unwrap()).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "# mine\n[graphics]\nupscaler = 'amd_fsr'\n"
        );
    }

    #[test]
    fn a_reused_row_address_is_bound_to_the_new_setting() {
        static OTHER: ListSetting = ListSetting::of::<GraphicsUpscaler>();
        let rows = Rows::new();
        rows.bind(0x1000, &UPSCALER);
        rows.bind(0x1000, &OTHER);
        let bound = rows.get(0x1000).unwrap();
        assert!(std::ptr::eq(bound.setting, &OTHER));
        assert!(rows.get(0x2000).is_none());
    }
}
