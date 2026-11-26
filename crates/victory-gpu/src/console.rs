//! Which console the title runs on. Titles pick settings (resolution,
//! memory budgets) by it, so the user chooses; Series X unless told.

use std::sync::atomic::{AtomicBool, Ordering};

const GIB: usize = 1 << 30;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Console {
    #[default]
    SeriesX,
    SeriesS,
}

impl Console {
    /// Title memory in bytes: (GPU-optimal, standard). Series X in the
    /// advanced configuration; Series S has one range, counted as standard
    /// (7,776 MiB, the published console-mode figure).
    // shortcut: later system updates gave Series S titles more (amount not
    // published here); Grand Theft Auto VI sizes nothing from this figure
    // (it goes by the device type), so update it if a title does.
    pub const fn title_memory(self) -> (usize, usize) {
        match self {
            Console::SeriesX => (10 * GIB, 7 * GIB / 2),
            Console::SeriesS => (0, 7_776 << 20),
        }
    }

    /// Compute units of the GPU.
    pub const fn compute_units(self) -> u32 {
        match self {
            Console::SeriesX => 52,
            Console::SeriesS => 20,
        }
    }

    pub const fn name(self) -> &'static str {
        match self {
            Console::SeriesX => "Xbox Series X",
            Console::SeriesS => "Xbox Series S",
        }
    }
}

static SERIES_S: AtomicBool = AtomicBool::new(false);

/// Set before the title starts.
pub fn set(console: Console) {
    victory_formats::texture::set_series_s_tiling(console == Console::SeriesS);
    SERIES_S.store(console == Console::SeriesS, Ordering::Relaxed);
}

pub fn current() -> Console {
    if SERIES_S.load(Ordering::Relaxed) {
        Console::SeriesS
    } else {
        Console::SeriesX
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use victory_formats::texture::{Desc, Dimension, Layout, Swizzle};

    /// The console chosen picks the render-target XOR addresses a layout
    /// gives: element 8 of a 4-byte `SW_64KB_R_X` row toggles bit 3 of x,
    /// at 0x2000 on the Series X and 0x800 on the Series S.
    #[test]
    fn the_console_set_chooses_render_target_xor_addresses() {
        let desc = Desc {
            dimension: Dimension::Tex2d,
            width: 256,
            height: 256,
            depth: 1,
            mips: 1,
            element_bytes: 4,
            block: 1,
            samples: 1,
            swizzle: Swizzle::SW_64KB_R_X,
        };
        let at = || Layout::new(&desc).unwrap().offset(0, 8, 0, 0).unwrap();
        set(Console::SeriesS);
        let series_s = at();
        set(Console::SeriesX);
        assert_eq!((series_s, at()), (0x800, 0x2000));
    }
}
