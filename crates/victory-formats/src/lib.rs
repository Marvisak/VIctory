//! Platform-level data formats of Xbox Series titles and the middleware they ship.
//!
//! Each module is pure (bytes in, bytes out, no I/O): [`texture`] (the
//! GPU's tiled texture layouts and copies to and from linear rows), [`gfx`]
//! (the tags and vector shapes of Scaleform UI movies). Planned:
//! BCPack textures, audio codecs (XMA2, Opus), and the zlib streams used by
//! hardware decompression.

pub mod bcpack;
pub mod gfx;
pub mod texture;
pub mod xma;
