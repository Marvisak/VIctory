//! Byte and half LDS accesses, and the elements of typed buffer views.

use super::*;

/// A byte or half LDS access (`ds_read_u8`..., `ds_write_b8`..., their
/// `_d16` and `_d16_hi` forms), from the opcode name: its bits, and
/// whether a read extends the sign.
pub(super) fn lds_sub_dword(name: &str) -> Option<(u32, bool)> {
    if let Some(rest) = name.strip_prefix("ds_read") {
        return match rest.trim_end_matches("_hi").trim_end_matches("_d16") {
            "_u8" => Some((8, false)),
            "_i8" => Some((8, true)),
            "_u16" => Some((16, false)),
            "_i16" => Some((16, true)),
            _ => None,
        };
    }
    match name.strip_prefix("ds_write")?.trim_end_matches("_d16_hi") {
        "_b8" => Some((8, false)),
        "_b16" => Some((16, false)),
        _ => None,
    }
}

/// Whether `name` is a byte or half LDS access.
#[cfg(test)]
pub(in crate::emit) fn sub_dword(name: &str) -> bool {
    lds_sub_dword(name).is_some()
}

impl Lower<'_> {
    /// The element of a typed view (`buffer_load_format_*`,
    /// `buffer_store_format_*`) the console addresses at `index` (none:
    /// 0) and byte offset `off` (`fixed` when constant): the console adds
    /// `index * stride + off` to the base; the host's typed view numbers
    /// elements of its format, and DXIL typed loads and stores take an
    /// element, not a byte offset. So the offset must be whole elements:
    /// `index + off / size`.
    ///
    /// shortcut: the element size is the used-resource entry's stride (the
    /// console's typed descriptors have their format's size as stride);
    /// without one, only offset 0 translates. A computed offset is taken
    /// as whole elements (a part of an element is dropped); a constant one
    /// that is not stops the translation.
    pub(super) fn typed_element(
        &mut self,
        index: Option<V>,
        off: V,
        fixed: Option<u32>,
        used: Option<&UsedResource>,
    ) -> Result<V, Why> {
        let c = self.cur;
        let index = index.unwrap_or_else(|| self.s.m.i32(0));
        if fixed == Some(0) {
            return Ok(index);
        }
        let size = used.map_or(0, |u| u.stride);
        if size == 0 {
            return Err("a typed buffer access at an offset, of unknown element size");
        }
        let elements = match fixed {
            Some(f) if f % size != 0 => {
                return Err("a typed buffer access at an offset inside an element");
            }
            Some(f) => self.s.m.i32(f / size),
            None => {
                let k = self.s.m.i32(size);
                self.s.m.bin(c, Bin::UDiv, off, k)
            }
        };
        Ok(self.s.m.bin(c, Bin::Add, index, elements))
    }
}
