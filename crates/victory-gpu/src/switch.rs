//! The `VICTORY_*` on/off switches (`VICTORY_D3D12_DEBUG=1` and its siblings).

use std::ffi::OsStr;
use std::sync::OnceLock;

/// Whether a switch's value turns it on: it is set, and is not `0`.
///
/// A switch used to be on whenever its variable existed, so `VICTORY_D3D12_WARP=0` (typed to switch it off, or left
/// over from comparing two runs) still selected the software adapter. Any other value keeps the old meaning.
fn value_is_on(value: Option<&OsStr>) -> bool {
    value.is_some_and(|v| v != "0")
}

/// Whether the environment variable `name` switches something on.
pub(crate) fn on(name: &str) -> bool {
    value_is_on(std::env::var_os(name).as_deref())
}

/// Whether title-buffer copies also keep a GPU-local card-side buffer.
/// This switch is deliberately exact: only `=1` enables the opt-in path.
pub(crate) fn card_copies() -> bool {
    #[cfg(test)]
    if let Some(on) = CARD_COPIES_OVERRIDE.with(|current| current.get()) {
        return on;
    }
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_CARD_COPIES").as_deref() == Some(OsStr::new("1")))
}

/// Minimum title-buffer size for a card-side copy, in bytes.
pub(crate) fn card_copies_min_bytes() -> u64 {
    static BYTES: OnceLock<u64> = OnceLock::new();
    *BYTES.get_or_init(|| mb("VICTORY_CARD_COPIES_MIN_MB", 1))
}

/// Total card-side allocation limit, in bytes.
pub(crate) fn card_copies_cap_bytes() -> u64 {
    static BYTES: OnceLock<u64> = OnceLock::new();
    *BYTES.get_or_init(|| mb("VICTORY_CARD_COPIES_MB", 1024))
}

fn mb(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(default)
        .saturating_mul(1 << 20)
}

#[cfg(test)]
std::thread_local! {
    static CARD_COPIES_OVERRIDE: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
}

/// A thread-local switch override used by GPU tests without changing the
/// process environment or affecting concurrently running tests.
#[cfg(test)]
pub(crate) struct CardCopiesOverride(Option<bool>);

#[cfg(test)]
pub(crate) fn force_card_copies_for_test(on: bool) -> CardCopiesOverride {
    let previous = CARD_COPIES_OVERRIDE.with(|current| current.replace(Some(on)));
    CardCopiesOverride(previous)
}

#[cfg(test)]
impl Drop for CardCopiesOverride {
    fn drop(&mut self) {
        CARD_COPIES_OVERRIDE.with(|current| current.set(self.0));
    }
}

#[cfg(test)]
mod tests {
    use super::value_is_on;
    use std::ffi::OsStr;

    #[test]
    fn unset_is_off() {
        assert!(!value_is_on(None));
    }

    #[test]
    fn zero_is_off() {
        assert!(!value_is_on(Some(OsStr::new("0"))));
    }

    #[test]
    fn one_is_on() {
        assert!(value_is_on(Some(OsStr::new("1"))));
    }

    #[test]
    fn other_values_keep_their_old_meaning() {
        // Documented as `=1`; anything else that was on before stays on: only `0` is new.
        for value in ["", "true", "yes", "2", "00", " 0", "0 "] {
            assert!(value_is_on(Some(OsStr::new(value))), "{value:?}");
        }
    }
}
