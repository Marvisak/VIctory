//! Renderer port 5: read a root constant buffer through a raw root SRV
//! (`VICTORY_PORT_CBV=1`, off by default). The shader side is
//! [`victory_shader::emit::constants_raw`]; this module is the host's view
//! of the same switch. The host already binds that root slot as an SRV from
//! `draw::root_view`, so no separate host descriptor work is needed. Table
//! constant buffers keep the bounds of their host views, and every other
//! opt-in path stays inside the existing root-view and low-memory limits.

/// Whether raw constant-buffer reads are on. Both translation and cache
/// keying use this, so a cached shader cannot outlive its switch.
pub(crate) fn on() -> bool {
    #[cfg(test)]
    if let Some(on) = PORT_CBV_IN_TEST.get() {
        return on;
    }
    victory_shader::emit::constants_raw()
}

#[cfg(test)]
thread_local! {
    /// [`on`], for the calling test alone: off unless a test opts in.
    pub(crate) static PORT_CBV_IN_TEST: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(Some(false)) };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_switch_is_hermetic() {
        PORT_CBV_IN_TEST.set(Some(false));
        assert!(!on());
        PORT_CBV_IN_TEST.set(Some(true));
        assert!(on());
        PORT_CBV_IN_TEST.set(None);
        assert_eq!(on(), victory_shader::emit::constants_raw());
        PORT_CBV_IN_TEST.set(Some(false));
    }
}
