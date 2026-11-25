#ifndef BC5_REFERENCES_HLSL
#define BC5_REFERENCES_HLSL


// Spread the low 4 bits of v to the even bit positions: bit k -> bit 2k.
// Bits above the low nibble are discarded, as in the Rust `spread` (k in 0..4).
uint bc5_spread(uint v)
{
    v &= 0x0Fu;
    v = (v | (v << 2)) & 0x33u;
    v = (v | (v << 1)) & 0x55u;
    return v;
}

// a occupies even bits, b occupies odd bits; result is always in 0..255.
uint bc5_interleave(uint a, uint b)
{
    return bc5_spread(a) | (bc5_spread(b) << 1);
}

// symbols: four byte values unpacked to uint, in stream order s[0..3].
// Returns x = endpoint distance, y = index distance.
uint2 bc5_references(uint4 symbols)
{
    uint d  = bc5_interleave(symbols.x, symbols.y);
    uint ix = bc5_interleave(symbols.z, symbols.w);

    // Zigzag decode in u8: (d >> 1) ^ wrapping_neg(d & 1).
    // 0u - 1u is 0xFFFFFFFF in 32-bit, so mask back to 8 bits.
    uint delta = ((d >> 1) ^ (0u - (d & 1u))) & 0xFFu;

    // u8 wrapping_add: the mask reproduces the wrap at 256.
    uint endpoint = (ix + delta) & 0xFFu;

    return uint2(endpoint, ix);
}

#endif // BC5_REFERENCES_HLSL
