#include "bc5-references.hlsli"
// SRV: count, 64 rank bytes, 255 packed dictionary blocks, 24 bytes per record.
// UAV: 16 bytes per output. Caller validates extents. One stream per dispatch.
uint byte_at(ByteAddressBuffer src, uint offset) {
    return (src.Load(offset & ~3u) >> (8 * (offset & 3))) & 255;
}

uint zigzag(uint symbol) {
    return (symbol >> 1) ^ (0u - (symbol & 1));
}

uint2 predict(ByteAddressBuffer src, RWByteAddressBuffer dst, uint i, uint back, uint channel) {
    if (back == 0) return uint2(0x8080, 0); // Packed representation of logical zero.
    if (back <= i) return dst.Load2(16 * (i - back) + 8 * channel);
    return src.Load2(68 + 16 * (back - i - 1) + 8 * channel);
}

[numthreads(1, 1, 1)]
void main(uint3 id : SV_DispatchThreadID) {
    if (any(id != 0)) return;
    ByteAddressBuffer src = ResourceDescriptorHeap[0];
    RWByteAddressBuffer dst = ResourceDescriptorHeap[1];
    uint count = src.Load(0);
    for (uint i = 0; i < count; ++i) {
        uint offset = 4148 + 24 * i;
        uint word = src.Load(offset);
        uint2 back = bc5_references((word >> uint4(0, 8, 16, 24)) & 255);
        for (uint channel = 0; channel < 2; ++channel) {
            uint ep = predict(src, dst, i, back.x, channel).x;
            uint2 indices = predict(src, dst, i, back.y, channel);
            // Adding modulo 256 commutes with toggling the endpoint sign bit.
            uint low = (ep + zigzag(byte_at(src, offset + 4 + 2 * channel))) & 255;
            uint high = ((ep >> 8) + zigzag(byte_at(src, offset + 5 + 2 * channel))) & 255;
            uint2 packed = uint2(low | (high << 8), 0);
            for (uint p = 0; p < 16; ++p) {
                uint symbol = byte_at(src, offset + 8 + 8 * channel + p / 2) & 63;
                uint pair = byte_at(src, 4 + symbol);
                uint delta = (pair >> (4 * (p & 1))) & 7;
                uint shift = 16 + 3 * p;
                uint old;
                if (shift == 31) old = (indices.x >> 31) | (indices.y << 1);
                else if (shift < 32) old = indices.x >> shift;
                else old = indices.y >> (shift - 32);
                old &= 7;
                uint rank = old == 0 ? 0 : (old == 1 ? 7 : old - 1);
                rank = (rank + delta) & 7;
                uint code = rank == 0 ? 0 : (rank == 7 ? 1 : rank + 1);
                if (shift < 32) {
                    packed.x |= code << shift;
                    if (shift == 31) packed.y |= code >> 1;
                } else packed.y |= code << (shift - 32);
            }
            dst.Store2(16 * i + 8 * channel, packed);
        }
        DeviceMemoryBarrier();
    }
}
