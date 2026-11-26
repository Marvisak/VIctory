//! What lowering knows of image descriptors beyond the instruction that
//! reads one.

use super::Lower;
use crate::resource::Source;

impl Lower<'_> {
    /// Whether the image descriptor `src` is known to view an integer
    /// format: an image atomic of the program reads the same descriptor
    /// (the same source, or a load of the same table entry), and the
    /// host's typed atomics take R32_UINT or R32_SINT views (R32G32_UINT
    /// for 64-bit ones). Nothing else tells: the format is in the
    /// descriptor, which the program reads from memory when it runs.
    pub(super) fn integer_image(&self, src: &Source) -> bool {
        let entry = |s: &Source| match *s {
            Source::Loaded { load, first } => self.scalar.get(&load).map(|a| (*a, first)),
            _ => None,
        };
        self.descriptors.iter().any(|(&(site, sampler), &other)| {
            !sampler
                && self.x.p.blocks[site.0].insts[site.1]
                    .name()
                    .starts_with("image_atomic_")
                && (other == src || entry(other).is_some_and(|e| entry(src) == Some(e)))
        })
    }
}
