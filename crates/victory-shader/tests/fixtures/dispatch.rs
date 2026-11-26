//! Synthetic 64x1x1 FSPX kernel. Each invocation stores 1 at its global X.
//! The sole descriptor is a raw UAV at offset 0 in the table in s4.
pub fn kernel(barrier: bool) -> Vec<u8> {
    #[rustfmt::skip]
    let words: &[u32] = &[
        // DXBC with a single ACCC part at byte 36.
        u32::from_le_bytes(*b"DXBC"), 0, 0, 0, 0, 1, 324, 1, 36,
        u32::from_le_bytes(*b"ACCC"), 280,
        // FSPX header and (kind, byte offset, byte length, reserved) records.
        u32::from_le_bytes(*b"FSPX"), 1, 4, 280,
        0, 80, 28, 0, 1, 112, 20, 0, 6, 144, 48, 0, 7, 192, 88, 0,
        // Compute packet: 64x1x1, wave32, 5 user SGPRs and group ID X.
        0, 0, 0, 64 | 1 << 16, 1 | 16 << 16, 1, 5 << 1 | 1 << 7, 0,
        0, 0, 0, 0, 0, 0, 0, 0,
        0x7e04_0205, // v_mov_b32 v2, s5 (group X)
        0x3404_0486, // v_lshlrev_b32 v2, 6, v2
        0x4a04_0102, // v_add_nc_u32 v2, v2, v0 (local X)
        0x3404_0482, // v_lshlrev_b32 v2, 2, v2 (byte offset)
        0xbe85_0380, // s_mov_b32 s5, 0
        0xf408_0202, 0xfa00_0000, // s_load_dwordx4 s[8:11], s[4:5], 0
        0x7e06_0281, // v_mov_b32 v3, 1
        0xe070_1000, 0x8002_0302, // buffer_store_dword v3, v2, s[8:11], 0 offen
        if barrier { 0xbf8a_0000 } else { 0xbf80_0000 }, // s_barrier or s_nop
        0xbf81_0000, // s_endpgm
        0x403, 68, 12, 0, 0, 0x010440,
        0x41, 0x0303, 0, 0, 1, 0x000404, u32::MAX, 0, // UAV table at s4, slot 0
        0x43, 0, 0, 0, 0, 0, 0, 0,
    ];
    words.iter().flat_map(|w| w.to_le_bytes()).collect()
}
