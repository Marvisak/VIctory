//! The keyboard's stand-in gamepad: which key drives each gamepad control.
//!
//! Input reads the keys through it, and anything that shows a control to
//! the player names the key from it, so both agree. It knows nothing of
//! readings or devices.

/// The Windows virtual key of Backspace.
pub const VK_BACK: u8 = 0x08;
/// The Windows virtual key of Tab.
pub const VK_TAB: u8 = 0x09;
/// The Windows virtual key of Enter.
pub const VK_RETURN: u8 = 0x0D;
/// The Windows virtual key of Esc.
pub const VK_ESCAPE: u8 = 0x1B;
/// The Windows virtual key of the left arrow.
pub const VK_LEFT: u8 = 0x25;
/// The Windows virtual key of the up arrow.
pub const VK_UP: u8 = 0x26;
/// The Windows virtual key of the right arrow.
pub const VK_RIGHT: u8 = 0x27;
/// The Windows virtual key of the down arrow.
pub const VK_DOWN: u8 = 0x28;

/// Declares the gamepad's controls and the list of all of them from one
/// list, so that the list cannot miss a control.
macro_rules! pad_controls {
    ($($control:ident),* $(,)?) => {
        /// A control of a gamepad.
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        pub enum PadControl {
            $($control),*
        }

        impl PadControl {
            /// Every control, in the order declared.
            pub const ALL: &[Self] = &[$(Self::$control),*];
        }
    };
}

pad_controls!(
    Menu,
    View,
    A,
    B,
    X,
    Y,
    Up,
    Down,
    Left,
    Right,
    LeftShoulder,
    RightShoulder,
    LeftStick,
    RightStick,
    LeftTrigger,
    RightTrigger,
    LeftStickUp,
    LeftStickDown,
    LeftStickLeft,
    LeftStickRight,
    RightStickUp,
    RightStickDown,
    RightStickLeft,
    RightStickRight,
);

/// The virtual key that drives `control` on the keyboard's stand-in gamepad;
/// README lists them.
#[must_use]
pub const fn fallback_key(control: PadControl) -> u8 {
    match control {
        PadControl::Menu => VK_ESCAPE,
        PadControl::View => VK_TAB,
        PadControl::A => VK_RETURN,
        PadControl::B => VK_BACK,
        PadControl::X => b'X',
        PadControl::Y => b'Y',
        PadControl::Up => VK_UP,
        PadControl::Down => VK_DOWN,
        PadControl::Left => VK_LEFT,
        PadControl::Right => VK_RIGHT,
        PadControl::LeftShoulder => b'Q',
        PadControl::RightShoulder => b'E',
        PadControl::LeftStick => b'F',
        PadControl::RightStick => b'H',
        PadControl::LeftTrigger => b'Z',
        PadControl::RightTrigger => b'C',
        PadControl::LeftStickUp => b'W',
        PadControl::LeftStickDown => b'S',
        PadControl::LeftStickLeft => b'A',
        PadControl::LeftStickRight => b'D',
        PadControl::RightStickUp => b'I',
        PadControl::RightStickDown => b'K',
        PadControl::RightStickLeft => b'J',
        PadControl::RightStickRight => b'L',
    }
}

#[cfg(test)]
mod tests {
    use super::{PadControl, fallback_key};

    #[test]
    fn every_control_has_a_key_of_its_own() {
        let mut keys: Vec<u8> = PadControl::ALL.iter().map(|&c| fallback_key(c)).collect();
        keys.sort_unstable();
        keys.dedup();

        assert_eq!(keys.len(), 24);
    }
}
