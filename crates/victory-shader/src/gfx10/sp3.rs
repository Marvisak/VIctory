//! Instruction text in AMD's SP3 assembly syntax.

use std::fmt::{self, Display, Write};

use super::fields::{Cache, Dpp, Fields, NULL, Operand, Valu};
use super::{Encoding, Instruction};

impl Instruction {
    /// The instruction in SP3 syntax, e.g. `v_add_f32 v1, 0.5, v8`.
    /// Branch targets are written `label_<byte offset>`.
    pub fn text(&self, wave64: bool) -> String {
        let name = self.name().unwrap_or("?");
        let ops = operands(self, name, &self.fields(wave64));
        if ops.is_empty() {
            name.to_owned()
        } else {
            format!("{name} {ops}")
        }
    }
}

impl Display for Operand {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            Operand::Scalar { code, dwords: 1 } => f.write_str(&scalar_name(code)),
            Operand::Scalar {
                code: 106,
                dwords: 2,
            } => f.write_str("vcc"),
            Operand::Scalar {
                code: 126,
                dwords: 2,
            } => f.write_str("exec"),
            Operand::Scalar { code, dwords } if code < 106 => {
                write!(f, "s[{code}:{}]", code + dwords - 1)
            }
            Operand::Scalar { code, dwords } if (108..124).contains(&code) => {
                write!(f, "ttmp[{}:{}]", code - 108, code - 108 + dwords - 1)
            }
            Operand::Scalar { code, .. } => f.write_str(&scalar_name(code)),
            Operand::Vector { reg, dwords: 1 } => write!(f, "v{reg}"),
            Operand::Vector { reg, dwords } => {
                write!(f, "v[{reg}:{}]", reg as u32 + dwords as u32 - 1)
            }
            Operand::Const(code) => f.write_str(&const_name(code)),
            Operand::Literal(value) => write!(f, "lit(0x{value:08x})"),
            Operand::Attr { index, chan, .. } => write!(
                f,
                "attr{index}.{}",
                ['x', 'y', 'z', 'w'][usize::from(chan & 3)]
            ),
        }
    }
}

fn scalar_name(code: u8) -> String {
    match code {
        0..=105 => format!("s{code}"),
        106 => "vcc_lo".into(),
        107 => "vcc_hi".into(),
        108..=123 => format!("ttmp{}", code - 108),
        124 => "m0".into(),
        125 => "null".into(),
        126 => "exec_lo".into(),
        _ => "exec_hi".into(),
    }
}

fn const_name(code: u8) -> String {
    match code {
        128..=192 => (i32::from(code) - 128).to_string(),
        193..=208 => (192 - i32::from(code)).to_string(),
        240 => "0.5".into(),
        241 => "-0.5".into(),
        242 => "1.0".into(),
        243 => "-1.0".into(),
        244 => "2.0".into(),
        245 => "-2.0".into(),
        246 => "4.0".into(),
        247 => "-4.0".into(),
        248 => "0.15915494".into(),
        235 => "src_shared_base".into(),
        236 => "src_shared_limit".into(),
        237 => "src_private_base".into(),
        238 => "src_private_limit".into(),
        239 => "src_pops_exiting_wave_id".into(),
        251 => "src_vccz".into(),
        252 => "src_execz".into(),
        253 => "src_scc".into(),
        254 => "src_lds_direct".into(),
        _ => format!("<src {code}>"),
    }
}

fn join(ops: impl IntoIterator<Item = String>) -> String {
    ops.into_iter().collect::<Vec<_>>().join(", ")
}

fn operands(inst: &Instruction, name: &str, fields: &Fields) -> String {
    let mut out = String::new();
    match fields {
        Fields::Sop { dst, src, imm } => {
            let regs = join(dst.iter().chain(src).map(|o| o.to_string()));
            out = match (name, imm) {
                ("s_getreg_b32", Some(imm)) => format!("{regs}, {}", hwreg(*imm)),
                ("s_setreg_b32" | "s_setreg_imm32_b32", Some(imm)) => {
                    format!("{}, {regs}", hwreg(*imm))
                }
                ("s_call_b64", Some(imm)) => format!("{regs}, {}", label(inst, *imm)),
                (_, Some(imm)) if regs.is_empty() => format!("0x{imm:04x}"),
                (_, Some(imm)) => format!("{regs}, 0x{imm:04x}"),
                (_, None) => regs,
            };
        }
        Fields::Sopp { imm } => {
            out = match name {
                "s_endpgm"
                | "s_barrier"
                | "s_code_end"
                | "s_endpgm_saved"
                | "s_endpgm_ordered_ps_done"
                | "s_wakeup"
                | "s_icache_inv"
                | "s_ttracedata" => String::new(),
                "s_branch" => label(inst, *imm),
                n if n.starts_with("s_cbranch_") => label(inst, *imm),
                "s_waitcnt" => waitcnt(*imm),
                "s_sendmsg" | "s_sendmsghalt" => sendmsg(*imm),
                _ => format!("0x{imm:04x}"),
            };
        }
        Fields::Smem {
            data,
            base,
            offset,
            soffset,
            cache,
        } => {
            if let Some(data) = data {
                let off = match (soffset, offset) {
                    (Operand::Scalar { code: NULL, .. }, 0) => soffset.to_string(),
                    (Operand::Scalar { code: NULL, .. }, _) => {
                        format!("0x{:06x}", offset & 0x1F_FFFF)
                    }
                    (_, 0) => soffset.to_string(),
                    _ => format!("{soffset} offset:0x{:06x}", offset & 0x1F_FFFF),
                };
                out = format!("{data}, {base}, {off}");
            }
            flags(&mut out, cache, false);
        }
        Fields::Valu(v) => out = valu(name, v, inst.encoding),
        Fields::Vintrp { dst, src, attr } => {
            let src = match (name, src) {
                ("v_interp_mov_f32", Operand::Const(p)) => ["p10", "p20", "p0"]
                    .get(usize::from(*p))
                    .map_or("p?".into(), |p| p.to_string()),
                _ => src.to_string(),
            };
            out = format!("{dst}, {src}, {attr}");
        }
        Fields::Ds {
            dst,
            addr,
            data,
            offset0,
            offset1,
            gds,
        } => {
            out = join(dst.iter().chain(addr).chain(data).map(|o| o.to_string()));
            if super::fields::ds_pair(name) {
                if *offset0 != 0 {
                    let _ = write!(out, " offset0:{offset0}");
                }
                if *offset1 != 0 {
                    let _ = write!(out, " offset1:{offset1}");
                }
            } else {
                let offset = u16::from(*offset1) << 8 | u16::from(*offset0);
                if offset != 0 {
                    let _ = write!(out, " offset:{offset}");
                }
            }
            if *gds {
                out.push_str(" gds");
            }
        }
        Fields::Buffer {
            data,
            addr,
            rsrc,
            soffset,
            offset,
            offen,
            idxen,
            format,
            lds,
            tfe,
            cache,
        } => {
            if let Some(data) = data {
                out = format!("{data}, {addr}, {rsrc}, {soffset}");
                if *idxen {
                    out.push_str(" idxen");
                }
                if *offen {
                    out.push_str(" offen");
                }
                if *offset != 0 {
                    let _ = write!(out, " offset:{offset}");
                }
                if let Some(format) = format {
                    let _ = write!(out, " format:[{}]", buffer_format(*format));
                }
            }
            flags(&mut out, cache, false);
            if *lds {
                out.push_str(" lds");
            }
            if *tfe {
                out.push_str(" tfe");
            }
            // The cache invalidations have no operands for a flag to follow.
            if out.starts_with(' ') {
                out.remove(0);
            }
        }
        Fields::Image {
            data,
            addr,
            rsrc,
            sampler,
            dmask,
            dim,
            unorm,
            r128,
            a16,
            d16,
            tfe,
            lwe,
            cache,
        } => {
            let addr = match addr.as_slice() {
                [one] => one.to_string(),
                many => format!(
                    "[{}]",
                    many.iter()
                        .map(|o| o.to_string())
                        .collect::<Vec<_>>()
                        .join(",")
                ),
            };
            out = join(
                [data.to_string(), addr, rsrc.to_string()]
                    .into_iter()
                    .chain(sampler.map(|s| s.to_string())),
            );
            if *dmask != 1 {
                let _ = write!(out, " dmask:0x{dmask:x}");
            }
            let dims = [
                "1D",
                "2D",
                "3D",
                "CUBE",
                "1D_ARRAY",
                "2D_ARRAY",
                "2D_MSAA",
                "2D_MSAA_ARRAY",
            ];
            let _ = write!(out, " dim:SQ_RSRC_IMG_{}", dims[usize::from(*dim & 7)]);
            for (on, text) in [(*r128, "r128"), (*unorm, "unorm")] {
                if on {
                    out.push(' ');
                    out.push_str(text);
                }
            }
            flags(&mut out, cache, true);
            for (on, text) in [(*a16, "a16"), (*d16, "d16"), (*tfe, "tfe"), (*lwe, "lwe")] {
                if on {
                    out.push(' ');
                    out.push_str(text);
                }
            }
        }
        Fields::Flat {
            dst,
            addr,
            data,
            saddr,
            offset,
            lds,
            cache,
        } => {
            let addr = addr.map_or("off".into(), |a| a.to_string());
            out = join(
                dst.iter()
                    .map(|o| o.to_string())
                    .chain([addr])
                    .chain(data.iter().chain(saddr).map(|o| o.to_string())),
            );
            if *offset != 0 {
                let _ = write!(out, " inst_offset:{offset}");
            }
            flags(&mut out, cache, false);
            if *lds {
                out.push_str(" lds");
            }
        }
        Fields::Exp {
            target,
            src,
            compr,
            done,
            vm,
        } => {
            let target = match *target {
                0..=7 => format!("mrt{target}"),
                8 => "mrtz".into(),
                9 => "null".into(),
                12..=15 => format!("pos{}", target - 12),
                20 => "prim".into(),
                32..=63 => format!("param{}", target - 32),
                _ => format!("<target {target}>"),
            };
            out = join(
                [target].into_iter().chain(
                    src.iter()
                        .map(|s| s.map_or("off".into(), |o| o.to_string())),
                ),
            );
            for (on, text) in [(*done, " done"), (*compr, " compr"), (*vm, " vm")] {
                if on {
                    out.push_str(text);
                }
            }
        }
    }
    out
}

/// Cache flags; images write `dlc` first.
fn flags(out: &mut String, cache: &Cache, dlc_first: bool) {
    let dlc = (cache.dlc, " dlc");
    let (first, last) = if dlc_first {
        (dlc, (false, ""))
    } else {
        ((false, ""), dlc)
    };
    for (on, text) in [first, (cache.glc, " glc"), (cache.slc, " slc"), last] {
        if on {
            out.push_str(text);
        }
    }
    if out.starts_with(' ') {
        out.remove(0);
    }
}

fn label(inst: &Instruction, imm: u16) -> String {
    let target = inst.offset as i64 + 4 * inst.words.len() as i64 + 4 * i64::from(imm as i16);
    format!("label_{target:04X}")
}

fn hwreg(imm: u16) -> String {
    let (id, offset, size) = (imm & 0x3F, imm >> 6 & 0x1F, (imm >> 11 & 0x1F) + 1);
    let name = match id {
        1 => "HW_REG_MODE".into(),
        20 => "HW_REG_SHADER_FLAT_SCRATCH_LO".into(),
        21 => "HW_REG_SHADER_FLAT_SCRATCH_HI".into(),
        25 => "HW_REG_POPS_PACKER".into(),
        _ => id.to_string(),
    };
    if offset == 0 && size == 32 {
        format!("hwreg({name})")
    } else {
        format!("hwreg({name}, {offset}, {size})")
    }
}

/// `s_waitcnt` operand: only the counters that wait. Bit 7 belongs to no
/// counter, so an operand with it set stays raw hex rather than lose it.
fn waitcnt(imm: u16) -> String {
    if imm & 0x80 != 0 {
        return format!("0x{imm:04x}");
    }
    let vm = imm & 0xF | (imm >> 14) << 4;
    let exp = imm >> 4 & 7;
    let lgkm = imm >> 8 & 0x3F;
    let mut parts = Vec::new();
    if vm != 0x3F {
        parts.push(format!("vmcnt({vm})"));
    }
    if exp != 7 {
        parts.push(format!("expcnt({exp})"));
    }
    if lgkm != 0x3F {
        parts.push(format!("lgkmcnt({lgkm})"));
    }
    if matches!(parts.len(), 0 | 3) {
        return format!("0x{imm:04x}");
    }
    parts.join(" & ")
}

/// `s_sendmsg` operand, spelled like LLVM's AMDGPU assembler: the operation
/// only for messages that have one, the stream only for a GS emit or cut.
/// Bits no field accounts for leave the operand as raw hex.
fn sendmsg(imm: u16) -> String {
    let (msg, op, stream) = (imm & 0xF, imm >> 4 & 7, imm >> 8 & 3);
    let raw = || format!("0x{imm:04x}");
    if imm & !0x3FF != 0 || imm & 0x80 != 0 {
        return raw();
    }
    let msg_name = match msg {
        1 => "MSG_INTERRUPT",
        2 => "MSG_GS",
        3 => "MSG_GS_DONE",
        4 => "MSG_SAVEWAVE",
        5 => "MSG_STALL_WAVE_GEN",
        6 => "MSG_HALT_WAVES",
        7 => "MSG_ORDERED_PS_DONE",
        8 => "MSG_EARLY_PRIM_DEALLOC",
        9 => "MSG_GS_ALLOC_REQ",
        10 => "MSG_GET_DOORBELL",
        11 => "MSG_GET_DDID",
        15 => "MSG_SYSMSG",
        _ => return raw(),
    };
    match msg {
        2 | 3 => {
            let op_name =
                ["GS_OP_NOP", "GS_OP_CUT", "GS_OP_EMIT", "GS_OP_EMIT_CUT"].get(usize::from(op));
            match op_name {
                Some(op_name) if op == 0 && stream == 0 => {
                    format!("sendmsg({msg_name}, {op_name})")
                }
                Some(op_name) if op != 0 => format!("sendmsg({msg_name}, {op_name}, {stream})"),
                _ => raw(),
            }
        }
        15 if stream == 0 => {
            let op_name = [
                "SYSMSG_OP_ECC_ERR_INTERRUPT",
                "SYSMSG_OP_REG_RD",
                "SYSMSG_OP_HOST_TRAP_ACK",
                "SYSMSG_OP_TTRACE_PC",
            ]
            .get(usize::from(op).wrapping_sub(1));
            op_name.map_or_else(raw, |op_name| format!("sendmsg({msg_name}, {op_name})"))
        }
        _ if op == 0 && stream == 0 => format!("sendmsg({msg_name})"),
        _ => raw(),
    }
}

fn buffer_format(format: u8) -> String {
    const NAMES: [&str; 78] = [
        "INVALID",
        "8_UNORM",
        "8_SNORM",
        "8_USCALED",
        "8_SSCALED",
        "8_UINT",
        "8_SINT",
        "16_UNORM",
        "16_SNORM",
        "16_USCALED",
        "16_SSCALED",
        "16_UINT",
        "16_SINT",
        "16_FLOAT",
        "8_8_UNORM",
        "8_8_SNORM",
        "8_8_USCALED",
        "8_8_SSCALED",
        "8_8_UINT",
        "8_8_SINT",
        "32_UINT",
        "32_SINT",
        "32_FLOAT",
        "16_16_UNORM",
        "16_16_SNORM",
        "16_16_USCALED",
        "16_16_SSCALED",
        "16_16_UINT",
        "16_16_SINT",
        "16_16_FLOAT",
        "10_11_11_UNORM",
        "10_11_11_SNORM",
        "10_11_11_USCALED",
        "10_11_11_SSCALED",
        "10_11_11_UINT",
        "10_11_11_SINT",
        "10_11_11_FLOAT",
        "11_11_10_UNORM",
        "11_11_10_SNORM",
        "11_11_10_USCALED",
        "11_11_10_SSCALED",
        "11_11_10_UINT",
        "11_11_10_SINT",
        "11_11_10_FLOAT",
        "10_10_10_2_UNORM",
        "10_10_10_2_SNORM",
        "10_10_10_2_USCALED",
        "10_10_10_2_SSCALED",
        "10_10_10_2_UINT",
        "10_10_10_2_SINT",
        "2_10_10_10_UNORM",
        "2_10_10_10_SNORM",
        "2_10_10_10_USCALED",
        "2_10_10_10_SSCALED",
        "2_10_10_10_UINT",
        "2_10_10_10_SINT",
        "8_8_8_8_UNORM",
        "8_8_8_8_SNORM",
        "8_8_8_8_USCALED",
        "8_8_8_8_SSCALED",
        "8_8_8_8_UINT",
        "8_8_8_8_SINT",
        "32_32_UINT",
        "32_32_SINT",
        "32_32_FLOAT",
        "16_16_16_16_UNORM",
        "16_16_16_16_SNORM",
        "16_16_16_16_USCALED",
        "16_16_16_16_SSCALED",
        "16_16_16_16_UINT",
        "16_16_16_16_SINT",
        "16_16_16_16_FLOAT",
        "32_32_32_UINT",
        "32_32_32_SINT",
        "32_32_32_FLOAT",
        "32_32_32_32_UINT",
        "32_32_32_32_SINT",
        "32_32_32_32_FLOAT",
    ];
    NAMES
        .get(usize::from(format))
        .map_or(format!("{format}"), |n| format!("BUF_FMT_{n}"))
}

fn valu(name: &str, v: &Valu, encoding: Encoding) -> String {
    let bit = |mask: u8, i: usize| mask >> i & 1 != 0;
    let mix = name.starts_with("v_fma_mix");
    let src = |i: usize| {
        let o = v.src[i];
        let mut s = o.to_string();
        if let Some(sdwa) = v.sdwa
            && bit(sdwa.sext, i)
        {
            s = format!("sext({s})");
        }
        if encoding == Encoding::Vop3p {
            if mix {
                if bit(v.opsel_hi, i) {
                    s = format!("{}({s})", if bit(v.opsel, i) { "sel_hi" } else { "sel_lo" });
                }
                if bit(v.neg_hi, i) {
                    s = format!("abs({s})");
                }
                if bit(v.neg, i) {
                    s = format!("-{s}");
                }
            } else {
                if bit(v.neg, i) && bit(v.neg_hi, i) {
                    s = format!("-{s}");
                }
                match (bit(v.opsel, i), bit(v.opsel_hi, i)) {
                    (false, true) => {}
                    (false, false) => s = format!("sel_lo({s})"),
                    (true, true) => s = format!("sel_hi({s})"),
                    (true, false) => s = format!("sel_hi_lo({s})"),
                }
            }
        } else {
            if bit(v.abs, i) {
                s = format!("abs({s})");
            }
            if bit(v.neg, i) {
                s = format!("-{s}");
            }
            // v_permlane*: the opsel bits are fi and bound_ctrl.
            if bit(v.opsel, i) && !name.starts_with("v_permlane") {
                s = format!("sel_hi({s})");
            }
        }
        s
    };
    let mut ops: Vec<String> = v
        .dst
        .iter()
        .chain(&v.carry)
        .map(|o| o.to_string())
        .collect();
    let mut order: Vec<usize> = (0..v.src.len()).collect();
    if name.starts_with("v_interp_") && order.len() > 1 {
        // dst, i/j, attribute, then the p1 result for p2.
        order.swap(0, 1);
    }
    ops.extend(order.into_iter().map(src));
    let mut out = ops.join(", ");
    let mut suffix = |s: &str| {
        out.push(' ');
        out.push_str(s);
    };
    if let Some(Operand::Attr { high: true, .. }) = v.src.first() {
        suffix("high");
    }
    let omod = ["", "mul:2", "mul:4", "div:2"][usize::from(v.omod & 3)];
    if name.starts_with("v_permlane") {
        if v.opsel & 1 != 0 {
            suffix("fi:1");
        }
        if v.opsel & 2 != 0 {
            suffix("bound_ctrl:1");
        }
    }
    if let Some(sdwa) = v.sdwa {
        if v.clamp {
            suffix("clamp");
        }
        if !omod.is_empty() {
            suffix(omod);
        }
        let sel = [
            "BYTE_0", "BYTE_1", "BYTE_2", "BYTE_3", "WORD_0", "WORD_1", "DWORD", "?",
        ];
        if sdwa.dst_sel != 6 {
            suffix(&format!("dst_sel:{}", sel[usize::from(sdwa.dst_sel & 7)]));
        }
        if sdwa.dst_unused != 0 {
            let unused = ["UNUSED_PAD", "UNUSED_SEXT", "UNUSED_PRESERVE", "?"];
            suffix(&format!(
                "dst_unused:{}",
                unused[usize::from(sdwa.dst_unused & 3)]
            ));
        }
        for (i, s) in sdwa.src_sel.iter().enumerate() {
            if *s != 6 {
                suffix(&format!("src{i}_sel:{}", sel[usize::from(s & 7)]));
            }
        }
    } else {
        if !omod.is_empty() {
            suffix(omod);
        }
        if v.clamp {
            suffix("clamp");
        }
    }
    match v.dpp {
        Some(Dpp::Row {
            ctrl,
            row_mask,
            bank_mask,
            bound_ctrl,
            fi,
        }) => {
            suffix(&match ctrl {
                0x000..=0x0FF => format!(
                    "quad_perm:[{},{},{},{}]",
                    ctrl & 3,
                    ctrl >> 2 & 3,
                    ctrl >> 4 & 3,
                    ctrl >> 6 & 3
                ),
                0x101..=0x10F => format!("row_shl:{}", ctrl & 0xF),
                0x111..=0x11F => format!("row_shr:{}", ctrl & 0xF),
                0x121..=0x12F => format!("row_ror:{}", ctrl & 0xF),
                0x140 => "row_mirror".into(),
                0x141 => "row_half_mirror".into(),
                0x150..=0x15F => format!("row_share:{}", ctrl & 0xF),
                0x160..=0x16F => format!("row_xmask:{}", ctrl & 0xF),
                _ => format!("dpp_ctrl:0x{ctrl:x}"),
            });
            if row_mask != 0xF {
                suffix(&format!("row_mask:0x{row_mask:x}"));
            }
            if bank_mask != 0xF {
                suffix(&format!("bank_mask:0x{bank_mask:x}"));
            }
            if bound_ctrl {
                suffix("bound_ctrl:1");
            }
            if fi {
                suffix("fi:1");
            }
        }
        Some(Dpp::Lanes { sel, fi }) => {
            let lanes: Vec<String> = (0..8).map(|i| (sel >> (3 * i) & 7).to_string()).collect();
            suffix(&format!("dpp8:[{}]", lanes.join(",")));
            if fi {
                suffix("fi:1");
            }
        }
        None => {}
    }
    out
}

#[cfg(test)]
mod tests {
    use super::buffer_format;
    use crate::gfx10::{Fields, Operand, decode};

    /// Text of each instruction in `words` (a straight-line program).
    fn texts(words: &[u32], wave64: bool) -> Vec<String> {
        let mut code: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
        code.extend(0xBF81_0000u32.to_le_bytes()); // s_endpgm
        let insts = decode(&code).unwrap();
        insts[..insts.len() - 1]
            .iter()
            .map(|i| i.text(wave64))
            .collect()
    }

    fn one(words: &[u32], wave64: bool) -> String {
        let t = texts(words, wave64);
        assert_eq!(t.len(), 1, "{t:?}");
        t[0].clone()
    }

    #[test]
    fn scalar() {
        assert_eq!(
            texts(&[0xBF88_0001, 0xBF8C_0070, 0xBE80_03FF, 0x1234_5678], true),
            [
                "s_cbranch_execz label_0008",
                "s_waitcnt vmcnt(0) & lgkmcnt(0)",
                "s_mov_b32 s0, lit(0x12345678)",
            ]
        );
        assert_eq!(one(&[0x8782_027E], true), "s_and_b64 s[2:3], exec, s[2:3]");
        assert_eq!(
            one(&[0xF408_0501, 0xFA00_0040], true),
            "s_load_dwordx4 s[20:23], s[2:3], 0x000040"
        );
        assert_eq!(
            one(&[0xF420_0144, 0x0400_0000], true),
            "s_buffer_load_dword s5, s[8:11], s2"
        );
    }

    #[test]
    fn lane_masks_follow_the_wave_size() {
        let cmp = [0x7C08_06F0]; // v_cmp_gt_f32 with 0.5 and v3
        assert_eq!(one(&cmp, false), "v_cmp_gt_f32 vcc_lo, 0.5, v3");
        assert_eq!(one(&cmp, true), "v_cmp_gt_f32 vcc, 0.5, v3");
        let vop3 = [0xD401_0106, 0x0001_E501]; // v_cmp_lt_f32 to s6, |v1| < 1.0
        assert_eq!(one(&vop3, false), "v_cmp_lt_f32 s6, abs(v1), 1.0");
        assert_eq!(one(&vop3, true), "v_cmp_lt_f32 s[6:7], abs(v1), 1.0");
        assert_eq!(
            one(&[0x0202_0480], false),
            "v_cndmask_b32 v1, 0, v2, vcc_lo"
        );
        assert_eq!(
            one(&[0x5006_0801], true),
            "v_add_co_ci_u32 v3, vcc, s1, v4, vcc"
        );
    }

    #[test]
    fn vector_modifiers() {
        assert_eq!(
            one(&[0x1004_0AFF, 0x3F00_0001], true),
            "v_mul_f32 v2, lit(0x3f000001), v5"
        );
        assert_eq!(
            one(&[0xD54B_8004, 0x2BC0_0501], true),
            "v_fma_f32 v4, -v1, s2, 0.5 mul:2 clamp"
        );
        assert_eq!(
            one(&[0x7E02_16F9, 0x0605_0602], true),
            "v_cvt_f32_f16 v1, v2 src0_sel:WORD_1"
        );
        assert_eq!(
            one(&[0x7E06_02E9, 0x0539_7701], true),
            "v_mov_b32 v3, v1 dpp8:[7,6,5,4,3,2,1,0]"
        );
        assert_eq!(
            one(&[0x7E0A_02FA, 0xFF08_B106], true),
            "v_mov_b32 v5, v6 quad_perm:[1,0,3,2] bound_ctrl:1"
        );
        assert_eq!(
            one(&[0xCC20_0C01, 0x2C10_0702], true),
            "v_fma_mix_f32 v1, -sel_hi(v2), s3, abs(v4)"
        );
    }

    #[test]
    fn memory_and_export() {
        assert_eq!(
            one(&[0xE034_7010, 0x0102_0402], true),
            "buffer_load_dwordx2 v[4:5], v[2:3], s[8:11], s1 idxen offen offset:16 glc"
        );
        assert_eq!(
            one(&[0xD8DC_0302, 0x0600_0001], true),
            "ds_read2_b32 v[6:7], v1 offset0:2 offset1:3"
        );
        assert_eq!(
            one(&[0xF800_1C0F, 0x0000_0302], true),
            "exp mrt0, v2, v2, v3, v3 done compr vm"
        );
    }

    #[test]
    fn sendmsg_names_only_the_fields_a_message_has() {
        // s_sendmsg is SOPP opcode 0x10: 0xBF90_xxxx.
        let msg = |imm: u32| one(&[0xBF90_0000 | imm], true);
        assert_eq!(msg(0x0009), "s_sendmsg sendmsg(MSG_GS_ALLOC_REQ)");
        assert_eq!(msg(0x0007), "s_sendmsg sendmsg(MSG_ORDERED_PS_DONE)");
        assert_eq!(msg(0x0008), "s_sendmsg sendmsg(MSG_EARLY_PRIM_DEALLOC)");
        assert_eq!(msg(0x0003), "s_sendmsg sendmsg(MSG_GS_DONE, GS_OP_NOP)");
        assert_eq!(msg(0x0122), "s_sendmsg sendmsg(MSG_GS, GS_OP_EMIT, 1)");
        assert_eq!(
            msg(0x0033),
            "s_sendmsg sendmsg(MSG_GS_DONE, GS_OP_EMIT_CUT, 0)"
        );
        assert_eq!(
            msg(0x002F),
            "s_sendmsg sendmsg(MSG_SYSMSG, SYSMSG_OP_REG_RD)"
        );
        // A NOP has no stream, a message without operations has none, an
        // unknown id has no name: each stays raw rather than lose bits.
        assert_eq!(msg(0x0102), "s_sendmsg 0x0102");
        assert_eq!(msg(0x0019), "s_sendmsg 0x0019");
        assert_eq!(msg(0x000C), "s_sendmsg 0x000c");
        assert_eq!(msg(0x0409), "s_sendmsg 0x0409");
    }

    /// The edges of `s_sendmsg`'s operand: every message without operations
    /// is named bare; a system message takes operations 1-4 only, and no
    /// stream; a GS message's operation 4-7 has no name; bit 7, between the
    /// operation and the stream, and bits 10-15 are no field; and
    /// `s_sendmsghalt` spells its operand the same way.
    #[test]
    fn sendmsg_edges_stay_raw_rather_than_lose_bits() {
        let msg = |imm: u32| one(&[0xBF90_0000 | imm], true);
        let bare = [
            (1, "MSG_INTERRUPT"),
            (4, "MSG_SAVEWAVE"),
            (5, "MSG_STALL_WAVE_GEN"),
            (6, "MSG_HALT_WAVES"),
            (10, "MSG_GET_DOORBELL"),
            (11, "MSG_GET_DDID"),
        ];
        for (id, name) in bare {
            assert_eq!(msg(id), format!("s_sendmsg sendmsg({name})"), "id {id}");
            // An operation or a stream on a message that has neither.
            assert_eq!(msg(id | 0x10), format!("s_sendmsg 0x{:04x}", id | 0x10));
            assert_eq!(msg(id | 0x100), format!("s_sendmsg 0x{:04x}", id | 0x100));
        }
        // Message 0 and 12-14 have no name.
        for id in [0, 12, 13, 14] {
            assert_eq!(msg(id), format!("s_sendmsg 0x{id:04x}"), "id {id}");
        }
        // System message operations: 1-4 are named, 0 and 5-7 are not, and
        // a stream is never one of its fields.
        let sys: Vec<String> = (0..8).map(|op| msg(op << 4 | 15)).collect();
        assert_eq!(
            sys,
            [
                "s_sendmsg 0x000f",
                "s_sendmsg sendmsg(MSG_SYSMSG, SYSMSG_OP_ECC_ERR_INTERRUPT)",
                "s_sendmsg sendmsg(MSG_SYSMSG, SYSMSG_OP_REG_RD)",
                "s_sendmsg sendmsg(MSG_SYSMSG, SYSMSG_OP_HOST_TRAP_ACK)",
                "s_sendmsg sendmsg(MSG_SYSMSG, SYSMSG_OP_TTRACE_PC)",
                "s_sendmsg 0x005f",
                "s_sendmsg 0x006f",
                "s_sendmsg 0x007f",
            ]
        );
        assert_eq!(msg(0x021F), "s_sendmsg 0x021f");
        // A GS message: NOP with no stream, CUT with stream 3, and the
        // operations past EMIT_CUT.
        assert_eq!(msg(0x0002), "s_sendmsg sendmsg(MSG_GS, GS_OP_NOP)");
        assert_eq!(msg(0x0312), "s_sendmsg sendmsg(MSG_GS, GS_OP_CUT, 3)");
        for op in 4..8 {
            let imm = op << 4 | 2;
            assert_eq!(msg(imm), format!("s_sendmsg 0x{imm:04x}"), "op {op}");
        }
        // Bit 7 and each of bits 10-15 leave an otherwise named operand raw.
        assert_eq!(msg(0x0089), "s_sendmsg 0x0089");
        assert_eq!(msg(0x00A2), "s_sendmsg 0x00a2");
        for bit in 10..16 {
            let imm = 1 << bit | 0x0122;
            assert_eq!(msg(imm), format!("s_sendmsg 0x{imm:04x}"), "bit {bit}");
        }
        // s_sendmsghalt is SOPP opcode 0x11 and shares the spelling.
        let halt = |imm: u32| one(&[0xBF91_0000 | imm], true);
        assert_eq!(halt(0x0009), "s_sendmsghalt sendmsg(MSG_GS_ALLOC_REQ)");
        assert_eq!(halt(0x0122), "s_sendmsghalt sendmsg(MSG_GS, GS_OP_EMIT, 1)");
        assert_eq!(halt(0x8009), "s_sendmsghalt 0x8009");
    }

    #[test]
    fn waitcnt_names_the_counters_that_wait() {
        // s_waitcnt is SOPP opcode 0x0C: 0xBF8C_xxxx.
        let wait = |imm: u32| one(&[0xBF8C_0000 | imm], true).replacen("s_waitcnt ", "", 1);
        // vmcnt's high bits live at 15:14, above lgkmcnt.
        assert_eq!(wait(0x7F7F), "vmcnt(31)");
        assert_eq!(wait(0x7F70), "vmcnt(16)");
        assert_eq!(wait(0xFF0F), "expcnt(0)");
        assert_eq!(wait(0xC57F), "lgkmcnt(5)");
        assert_eq!(wait(0x3F23), "vmcnt(3) & expcnt(2)");
        assert_eq!(wait(0xBF3F), "vmcnt(47) & expcnt(3)");
        // Nothing waits, or everything does: raw hex.
        assert_eq!(wait(0xFF7F), "0xff7f");
        assert_eq!(wait(0x0000), "0x0000");
        // Bit 7 is no counter's; the operand stays raw rather than drop it.
        assert_eq!(wait(0x00F0), "0x00f0");
        assert_eq!(wait(0xC0FF), "0xc0ff");
        assert_eq!(wait(0xFFFF), "0xffff");
    }

    #[test]
    fn hwreg_spells_whole_registers_by_name_and_slices_by_range() {
        // SOPK: 0xB000_0000 | op << 23 | sdst << 16 | simm16.
        let sopk = |op: u32, sdst: u32, imm: u32| 0xB000_0000 | op << 23 | sdst << 16 | imm;
        let hw = |id: u32, offset: u32, size: u32| id | offset << 6 | (size - 1) << 11;
        assert_eq!(
            one(&[sopk(0x12, 3, hw(1, 0, 32))], true),
            "s_getreg_b32 s3, hwreg(HW_REG_MODE)"
        );
        assert_eq!(
            one(&[sopk(0x12, 3, hw(1, 4, 4))], true),
            "s_getreg_b32 s3, hwreg(HW_REG_MODE, 4, 4)"
        );
        // A full-width read from a nonzero offset is still a slice.
        assert_eq!(
            one(&[sopk(0x12, 0, hw(20, 1, 32))], true),
            "s_getreg_b32 s0, hwreg(HW_REG_SHADER_FLAT_SCRATCH_LO, 1, 32)"
        );
        // Unnamed registers keep their number, up to the field's top.
        assert_eq!(
            one(&[sopk(0x12, 0, hw(63, 31, 1))], true),
            "s_getreg_b32 s0, hwreg(63, 31, 1)"
        );
        // The register leads for a write, both from an SGPR and a literal.
        assert_eq!(
            one(&[sopk(0x13, 7, hw(21, 0, 32))], true),
            "s_setreg_b32 hwreg(HW_REG_SHADER_FLAT_SCRATCH_HI), s7"
        );
        assert_eq!(
            one(&[sopk(0x15, 0, hw(25, 0, 3)), 0x0000_0005], true),
            "s_setreg_imm32_b32 hwreg(HW_REG_POPS_PACKER, 0, 3), lit(0x00000005)"
        );
    }

    #[test]
    fn image_d16_data_is_packed() {
        // Three f16 channels occupy two VGPRs; NSA lists the addresses.
        let words = [0xF09C_070A, 0x8024_0801, 0x0000_0005];
        assert_eq!(
            one(&words, true),
            "image_sample_lz v[8:9], [v1,v5], s[16:23], s[4:7] dmask:0x7 dim:SQ_RSRC_IMG_2D d16"
        );
        let code: Vec<u8> = words
            .iter()
            .chain(&[0xBF81_0000])
            .flat_map(|w| w.to_le_bytes())
            .collect();
        let Fields::Image { data, addr, .. } = decode(&code).unwrap()[0].fields(true) else {
            panic!("not an image instruction");
        };
        assert_eq!(data, Operand::Vector { reg: 8, dwords: 2 });
        assert_eq!(addr.len(), 2);
    }

    /// Scalar operand codes name their registers at every boundary: the last
    /// SGPR, both VCC halves, the trap temporaries, m0, null and both EXEC
    /// halves; a pair at VCC or EXEC takes the whole-register name, and SGPR
    /// and trap temporary ranges keep their own numbering.
    #[test]
    fn scalar_codes_name_each_register_at_its_bounds() {
        let s = |code: u8, dwords: u8| Operand::Scalar { code, dwords }.to_string();
        let names: Vec<String> = [0, 105, 106, 107, 108, 123, 124, 125, 126, 127]
            .into_iter()
            .map(|code| s(code, 1))
            .collect();
        assert_eq!(
            names,
            [
                "s0", "s105", "vcc_lo", "vcc_hi", "ttmp0", "ttmp15", "m0", "null", "exec_lo",
                "exec_hi"
            ]
        );
        assert_eq!(s(106, 2), "vcc");
        assert_eq!(s(126, 2), "exec");
        assert_eq!(s(104, 2), "s[104:105]");
        assert_eq!(s(0, 16), "s[0:15]");
        assert_eq!(s(108, 4), "ttmp[0:3]");
        assert_eq!(s(120, 4), "ttmp[12:15]");
    }

    /// Inline constants: 0-64 and -1 to -16 at both ends of their codes, the
    /// float constants including 1/(2*pi), the special sources, and codes in
    /// no range (the gap after -16, 249-250, the literal's 255) as a marked
    /// raw code rather than a misleading number.
    #[test]
    fn inline_constants_name_each_range_at_its_bounds() {
        let c = |code: u8| Operand::Const(code).to_string();
        assert_eq!(
            (c(128), c(129), c(192)),
            ("0".into(), "1".into(), "64".into())
        );
        assert_eq!((c(193), c(208)), ("-1".into(), "-16".into()));
        let floats: Vec<String> = (240..=248).map(c).collect();
        assert_eq!(
            floats,
            [
                "0.5",
                "-0.5",
                "1.0",
                "-1.0",
                "2.0",
                "-2.0",
                "4.0",
                "-4.0",
                "0.15915494"
            ]
        );
        let special: Vec<String> = [235, 236, 237, 238, 239, 251, 252, 253, 254]
            .into_iter()
            .map(c)
            .collect();
        assert_eq!(
            special,
            [
                "src_shared_base",
                "src_shared_limit",
                "src_private_base",
                "src_private_limit",
                "src_pops_exiting_wave_id",
                "src_vccz",
                "src_execz",
                "src_scc",
                "src_lds_direct"
            ]
        );
        for code in [209, 234, 249, 250, 255] {
            assert_eq!(c(code), format!("<src {code}>"));
        }
    }

    /// Buffer formats name every entry of the table from INVALID to the last
    /// four-channel float, and a format past the table stays a bare number.
    #[test]
    fn buffer_formats_name_the_table_and_number_the_rest() {
        assert_eq!(buffer_format(0), "BUF_FMT_INVALID");
        assert_eq!(buffer_format(1), "BUF_FMT_8_UNORM");
        assert_eq!(buffer_format(22), "BUF_FMT_32_FLOAT");
        assert_eq!(buffer_format(77), "BUF_FMT_32_32_32_32_FLOAT");
        assert_eq!(buffer_format(78), "78");
        assert_eq!(buffer_format(127), "127");
        // Every named entry is distinct, so no format reads as another.
        let names: std::collections::HashSet<String> = (0..78).map(buffer_format).collect();
        assert_eq!(names.len(), 78);
    }

    /// Every DPP16 row control the console has a name for is spelled at both
    /// ends of its range, and the gaps between them (row_shl:0, row_shr:0,
    /// row_ror:0, the GFX9 broadcasts, past row_xmask) stay a raw `dpp_ctrl`
    /// rather than read as a neighbour.
    #[test]
    fn dpp_row_controls_name_each_range_at_its_bounds() {
        // v_mov_b32 v5, v6 with DPP16, full row and bank masks.
        let dpp = |ctrl: u32| one(&[0x7E0A_02FA, 0xFF00_0006 | ctrl << 8], true);
        let cases = [
            (0x000, "quad_perm:[0,0,0,0]"),
            (0x0E4, "quad_perm:[0,1,2,3]"),
            (0x0FF, "quad_perm:[3,3,3,3]"),
            (0x100, "dpp_ctrl:0x100"),
            (0x101, "row_shl:1"),
            (0x10F, "row_shl:15"),
            (0x110, "dpp_ctrl:0x110"),
            (0x111, "row_shr:1"),
            (0x11F, "row_shr:15"),
            (0x120, "dpp_ctrl:0x120"),
            (0x121, "row_ror:1"),
            (0x12F, "row_ror:15"),
            (0x130, "dpp_ctrl:0x130"),
            (0x13F, "dpp_ctrl:0x13f"),
            (0x140, "row_mirror"),
            (0x141, "row_half_mirror"),
            (0x142, "dpp_ctrl:0x142"),
            (0x150, "row_share:0"),
            (0x15F, "row_share:15"),
            (0x160, "row_xmask:0"),
            (0x16F, "row_xmask:15"),
            (0x170, "dpp_ctrl:0x170"),
            (0x1FF, "dpp_ctrl:0x1ff"),
        ];
        for (ctrl, text) in cases {
            assert_eq!(dpp(ctrl), format!("v_mov_b32 v5, v6 {text}"), "{ctrl:#x}");
        }
    }

    /// DPP16 masks are written only when they disable something, each in its
    /// own hex field, then bound_ctrl and fi; dpp8's fetch-inactive form (the
    /// 0xEA source) carries fi too.
    #[test]
    fn dpp_masks_and_flags_are_written_only_when_set() {
        let ext = |row: u32, bank: u32, bound: u32, fi: u32| {
            row << 28 | bank << 24 | bound << 19 | fi << 18 | 0x0E4 << 8 | 6
        };
        let dpp = |e: u32| one(&[0x7E0A_02FA, e], true);
        let base = "v_mov_b32 v5, v6 quad_perm:[0,1,2,3]";
        assert_eq!(dpp(ext(0xF, 0xF, 0, 0)), base);
        assert_eq!(dpp(ext(0x3, 0xF, 0, 0)), format!("{base} row_mask:0x3"));
        assert_eq!(dpp(ext(0xF, 0x5, 0, 0)), format!("{base} bank_mask:0x5"));
        assert_eq!(
            dpp(ext(0x0, 0x0, 0, 0)),
            format!("{base} row_mask:0x0 bank_mask:0x0")
        );
        assert_eq!(dpp(ext(0xF, 0xF, 0, 1)), format!("{base} fi:1"));
        assert_eq!(
            dpp(ext(0x1, 0xE, 1, 1)),
            format!("{base} row_mask:0x1 bank_mask:0xe bound_ctrl:1 fi:1")
        );
        // dpp8 identity selects, with and without fi.
        assert_eq!(
            one(&[0x7E06_02E9, 0xFAC6_8801], true),
            "v_mov_b32 v3, v1 dpp8:[0,1,2,3,4,5,6,7]"
        );
        assert_eq!(
            one(&[0x7E06_02EA, 0xFAC6_8801], true),
            "v_mov_b32 v3, v1 dpp8:[0,1,2,3,4,5,6,7] fi:1"
        );
    }

    /// SDWA names a destination select and unused-bits mode other than the
    /// defaults (DWORD, pad), writes clamp before the output modifier (the
    /// reverse of VOP3), wraps sign extension inside abs and negation, and
    /// reads an SGPR source through its s0 bit.
    #[test]
    fn sdwa_destination_and_source_modifiers() {
        // v_mov_b32 v1, v2 with SDWA; source selects at DWORD unless set.
        let sdwa = |ext: u32| one(&[0x7E02_02F9, 0x0606_0002 | ext], true);
        assert_eq!(sdwa(6 << 8), "v_mov_b32 v1, v2");
        assert_eq!(
            sdwa(2 << 11),
            "v_mov_b32 v1, v2 dst_sel:BYTE_0 dst_unused:UNUSED_PRESERVE"
        );
        assert_eq!(
            sdwa(5 << 8 | 1 << 11),
            "v_mov_b32 v1, v2 dst_sel:WORD_1 dst_unused:UNUSED_SEXT"
        );
        assert_eq!(sdwa(6 << 8 | 3 << 11), "v_mov_b32 v1, v2 dst_unused:?");
        assert_eq!(sdwa(3 << 8), "v_mov_b32 v1, v2 dst_sel:BYTE_3");
        assert_eq!(
            sdwa(6 << 8 | 1 << 13 | 3 << 14),
            "v_mov_b32 v1, v2 clamp div:2"
        );
        assert_eq!(sdwa(6 << 8 | 1 << 14), "v_mov_b32 v1, v2 mul:2");
        assert_eq!(sdwa(6 << 8 | 1 << 19), "v_mov_b32 v1, sext(v2)");
        assert_eq!(sdwa(6 << 8 | 7 << 19), "v_mov_b32 v1, -abs(sext(v2))");
        assert_eq!(sdwa(6 << 8 | 1 << 23), "v_mov_b32 v1, s2");
        assert_eq!(
            one(&[0x7E02_02F9, 0x0601_0602], true),
            "v_mov_b32 v1, v2 src0_sel:BYTE_1"
        );
    }

    /// v_permlane*'s opsel bits are fi and bound_ctrl, written as suffixes and
    /// never as `sel_hi` on a source; another VOP3 instruction's opsel stays a
    /// source select.
    #[test]
    fn permlane_opsel_is_fi_and_bound_ctrl() {
        // v_permlane16_b32 v1, v2, s3, s4.
        let src = 0x0010_0702;
        let permlane = |opsel: u32| one(&[0xD777_0001 | opsel << 11, src], true);
        assert_eq!(permlane(0), "v_permlane16_b32 v1, v2, s3, s4");
        assert_eq!(permlane(1), "v_permlane16_b32 v1, v2, s3, s4 fi:1");
        assert_eq!(permlane(2), "v_permlane16_b32 v1, v2, s3, s4 bound_ctrl:1");
        assert_eq!(
            permlane(3),
            "v_permlane16_b32 v1, v2, s3, s4 fi:1 bound_ctrl:1"
        );
        assert_eq!(
            one(&[0xD778_1801, src], true),
            "v_permlanex16_b32 v1, v2, s3, s4 fi:1 bound_ctrl:1"
        );
    }

    /// Packed math writes each source's half selects only when they differ
    /// from the default (low for the low half, high for the high), and a
    /// negation only when it covers both halves.
    #[test]
    fn packed_sources_name_their_half_selects() {
        // v_pk_add_f16 v1, v2, v3; opsel in word 0, opsel_hi and neg in word 1.
        let pk = |opsel: u32, opsel_hi: u32, neg: u32, neg_hi: u32| {
            one(
                &[
                    0xCC0F_0001 | opsel << 11 | neg_hi << 8,
                    0x0002_0702 | opsel_hi << 27 | neg << 29,
                ],
                true,
            )
        };
        assert_eq!(pk(0, 3, 0, 0), "v_pk_add_f16 v1, v2, v3");
        assert_eq!(pk(1, 3, 0, 0), "v_pk_add_f16 v1, sel_hi(v2), v3");
        assert_eq!(pk(0, 1, 0, 0), "v_pk_add_f16 v1, v2, sel_lo(v3)");
        assert_eq!(pk(2, 1, 0, 0), "v_pk_add_f16 v1, v2, sel_hi_lo(v3)");
        assert_eq!(pk(0, 3, 1, 1), "v_pk_add_f16 v1, -v2, v3");
        assert_eq!(pk(1, 3, 2, 2), "v_pk_add_f16 v1, sel_hi(v2), -v3");
    }

    /// Export targets by range: render targets 0-7, depth, null, positions
    /// 0-3, the primitive and parameters 0-31 are named; the gaps between
    /// them (10-11, 16-19, 21-31) keep their number. A disabled channel is
    /// `off`, and an export without `done` writes no flag.
    #[test]
    fn export_targets_name_each_range_at_its_bounds() {
        // EXP: 0xF800_0000 | vm << 12 | done << 11 | target << 4 | en.
        let target = |t: u32| {
            one(&[0xF800_0801 | t << 4, 0x0000_0002], true)
                .strip_prefix("exp ")
                .and_then(|t| t.strip_suffix(", v2, off, off, off done"))
                .unwrap()
                .to_owned()
        };
        let names: Vec<String> = [0, 7, 8, 9, 10, 11, 12, 15, 16, 19, 20, 21, 31, 32, 63]
            .into_iter()
            .map(target)
            .collect();
        assert_eq!(
            names,
            [
                "mrt0",
                "mrt7",
                "mrtz",
                "null",
                "<target 10>",
                "<target 11>",
                "pos0",
                "pos3",
                "<target 16>",
                "<target 19>",
                "prim",
                "<target 21>",
                "<target 31>",
                "param0",
                "param31",
            ]
        );
        // Channels 0 and 2 enabled, each reading its own byte of dword 1.
        assert_eq!(
            one(&[0xF800_0005, 0x0403_0201], true),
            "exp mrt0, v1, off, v3, off"
        );
    }

    /// A single-address LDS access joins both offset bytes into one 16-bit
    /// offset; a paired access (`read2`, `write2`) names each byte on its own
    /// and only when nonzero. `gds` follows the offsets either way.
    #[test]
    fn ds_offsets_join_for_one_address_and_split_for_a_pair() {
        // DS: 0xD800_0000 | op << 18 | gds << 17 | offset1 << 8 | offset0.
        let ds = |op: u32, o0: u32, o1: u32, gds: u32, w1: u32| {
            one(
                &[0xD800_0000 | op << 18 | gds << 17 | o1 << 8 | o0, w1],
                true,
            )
        };
        // ds_read_b32 (0x36): v6 from the address in v1.
        assert_eq!(
            ds(0x36, 0x02, 0x03, 0, 0x0600_0001),
            "ds_read_b32 v6, v1 offset:770"
        );
        assert_eq!(
            ds(0x36, 0x00, 0xFF, 0, 0x0600_0001),
            "ds_read_b32 v6, v1 offset:65280"
        );
        assert_eq!(
            ds(0x36, 0xFF, 0xFF, 0, 0x0600_0001),
            "ds_read_b32 v6, v1 offset:65535"
        );
        assert_eq!(
            ds(0x36, 0x00, 0x00, 1, 0x0600_0001),
            "ds_read_b32 v6, v1 gds"
        );
        // ds_write_b32 (0x0D): address v1, data v2.
        assert_eq!(
            ds(0x0D, 0x04, 0x00, 0, 0x0000_0201),
            "ds_write_b32 v1, v2 offset:4"
        );
        // ds_write2_b32 (0x0E): address v1, data v2 and v3.
        assert_eq!(
            ds(0x0E, 0x00, 0xFF, 0, 0x0003_0201),
            "ds_write2_b32 v1, v2, v3 offset1:255"
        );
        assert_eq!(
            ds(0x0E, 0x01, 0x00, 1, 0x0003_0201),
            "ds_write2_b32 v1, v2, v3 offset0:1 gds"
        );
        assert_eq!(
            ds(0x0E, 0x00, 0x00, 0, 0x0003_0201),
            "ds_write2_b32 v1, v2, v3"
        );
    }

    /// FLAT's instruction offset is 11 unsigned bits (bit 11 is not part of
    /// it); global and scratch take 12 signed bits, so 0x800 is -2048. A
    /// global access with an SGPR base takes a 32-bit VGPR offset, and a
    /// scratch access with one takes no VGPR at all: its address is `off`.
    #[test]
    fn flat_offsets_and_addresses_follow_the_segment() {
        // FLAT: 0xDC00_0000 | op << 18 | seg << 14 | offset.
        let fl = |op: u32, seg: u32, off: u32, w1: u32| {
            one(&[0xDC00_0000 | op << 18 | seg << 14 | off, w1], true)
        };
        // *_load_dword (12) into v3 from v1, SGPR base s4.
        assert_eq!(fl(12, 0, 0, 0x0300_0001), "flat_load_dword v3, v[1:2]");
        assert_eq!(
            fl(12, 0, 0xFFF, 0x0300_0001),
            "flat_load_dword v3, v[1:2] inst_offset:2047"
        );
        assert_eq!(
            fl(12, 2, 0x7FF, 0x0304_0001),
            "global_load_dword v3, v1, s[4:5] inst_offset:2047"
        );
        assert_eq!(
            fl(12, 2, 0x800, 0x0304_0001),
            "global_load_dword v3, v1, s[4:5] inst_offset:-2048"
        );
        assert_eq!(
            fl(12, 2, 0xFFF, 0x0304_0001),
            "global_load_dword v3, v1, s[4:5] inst_offset:-1"
        );
        assert_eq!(
            fl(12, 1, 8, 0x0304_0001),
            "scratch_load_dword v3, off, s4 inst_offset:8"
        );
        // *_store_dword (28) of v2.
        assert_eq!(fl(28, 1, 0, 0x0004_0201), "scratch_store_dword off, v2, s4");
        assert_eq!(
            fl(28, 2, 0, 0x0004_0201),
            "global_store_dword v1, v2, s[4:5]"
        );
    }

    /// Branch and call targets count signed dwords from the next
    /// instruction: a back branch can land on itself, a forward one skips
    /// what it passes, and `s_call_b64` names its return pair first.
    #[test]
    fn branch_and_call_targets_count_signed_dwords_from_the_next_instruction() {
        // SOPP: 0xBF80_0000 | op << 16 | simm16; s_cbranch_scc0 0x04, scc1 0x05.
        let sopp = |op: u32, imm: u16| 0xBF80_0000 | op << 16 | u32::from(imm);
        let v_nop = 0x7E00_0000;
        assert_eq!(
            texts(&[sopp(4, 0xFFFF), sopp(5, 1), v_nop], true),
            [
                "s_cbranch_scc0 label_0000",
                "s_cbranch_scc1 label_000C",
                "v_nop"
            ]
        );
        // SOPK s_call_b64 (0x16) to s[30:31]: two dwords back, and to the
        // instruction right after it.
        let call = |imm: u32| 0xB000_0000 | 0x16 << 23 | 30 << 16 | imm;
        assert_eq!(
            texts(&[v_nop, call(0xFFFE)], true)[1],
            "s_call_b64 s[30:31], label_0000"
        );
        assert_eq!(one(&[call(0)], true), "s_call_b64 s[30:31], label_0004");
    }

    /// A scalar load writes its offset alone against `null`, its SGPR alone
    /// for a zero offset, and both otherwise; the offset is the 21-bit signed
    /// field, so a negative one is printed in its 21 bits.
    #[test]
    fn scalar_memory_offsets_name_only_the_parts_that_are_set() {
        // s_load_dword s4, s[2:3]: op 0, sdata 4, sbase 1; word 1 is soffset << 25 | offset.
        let load = |w1: u32| one(&[0xF400_0101, w1], true);
        assert_eq!(load(0x7D << 25), "s_load_dword s4, s[2:3], null");
        assert_eq!(load(0x7D << 25 | 0x10), "s_load_dword s4, s[2:3], 0x000010");
        assert_eq!(load(6 << 25), "s_load_dword s4, s[2:3], s6");
        assert_eq!(
            load(6 << 25 | 0x10),
            "s_load_dword s4, s[2:3], s6 offset:0x000010"
        );
        assert_eq!(
            load(0x7D << 25 | 0x0F_FFFF),
            "s_load_dword s4, s[2:3], 0x0fffff"
        );
        assert_eq!(
            load(0x7D << 25 | 0x1F_FFFC),
            "s_load_dword s4, s[2:3], 0x1ffffc"
        );
        // Bits 21-24 between offset and soffset are not part of either.
        assert_eq!(
            load(6 << 25 | 0x01E0_0010),
            "s_load_dword s4, s[2:3], s6 offset:0x000010"
        );
        // glc and dlc follow the operands, glc first.
        assert_eq!(
            one(&[0xF401_4101, 6 << 25], true),
            "s_load_dword s4, s[2:3], s6 glc dlc"
        );
    }

    /// Every image dimension is named and sizes the address by its
    /// coordinates; images write `dlc` before `glc slc`, where buffers write
    /// it last.
    #[test]
    fn image_dimensions_size_the_address_and_dlc_leads_the_cache_flags() {
        // image_load v8, v[0:n], s[16:23]: op 0, dmask 1, srsrc 4, vdata 8.
        let load = |dim: u32, w0: u32| one(&[0xF000_0100 | dim << 3 | w0, 0x0004_0800], true);
        let dims = [
            ("v0", "1D"),
            ("v[0:1]", "2D"),
            ("v[0:2]", "3D"),
            ("v[0:2]", "CUBE"),
            ("v[0:1]", "1D_ARRAY"),
            ("v[0:2]", "2D_ARRAY"),
            ("v[0:2]", "2D_MSAA"),
            ("v[0:3]", "2D_MSAA_ARRAY"),
        ];
        for (dim, (addr, name)) in (0..).zip(dims) {
            assert_eq!(
                load(dim, 0),
                format!("image_load v8, {addr}, s[16:23] dim:SQ_RSRC_IMG_{name}")
            );
        }
        // dlc (bit 7), glc (bit 13) and slc (bit 25) together.
        assert_eq!(
            load(0, 1 << 7 | 1 << 13 | 1 << 25),
            "image_load v8, v0, s[16:23] dim:SQ_RSRC_IMG_1D dlc glc slc"
        );
        // A buffer with the same three writes dlc last.
        assert_eq!(
            one(&[0xE030_C000, 0x0042_0400], true),
            "buffer_load_dword v4, v0, s[8:11], s0 glc slc dlc"
        );
    }

    /// The image flags past the cache bits each print once, in a fixed
    /// order (r128 and unorm before the cache flags; a16, d16, tfe and lwe
    /// after), and each reshapes the operands it governs: r128 halves the
    /// resource, a16 packs the coordinates in pairs, and tfe or lwe (one or
    /// both) add a single status dword to the data.
    #[test]
    fn image_flags_print_in_order_and_reshape_their_operands() {
        // image_load v8, v0, s[16:23]: op 0, dmask 1, srsrc 4, vdata 8.
        let load = |w0: u32, w1: u32| one(&[0xF000_0100 | w0, 0x0004_0800 | w1], true);
        let (r128, unorm, tfe, lwe, dlc, glc, slc) =
            (1 << 15, 1 << 12, 1 << 16, 1 << 17, 1 << 7, 1 << 13, 1 << 25);
        let (a16, d16) = (1 << 30, 1 << 31);
        let base = "image_load v8, v0, s[16:23] dim:SQ_RSRC_IMG_1D";
        assert_eq!(load(0, 0), base);
        assert_eq!(
            load(r128, 0),
            "image_load v8, v0, s[16:19] dim:SQ_RSRC_IMG_1D r128"
        );
        assert_eq!(load(unorm, 0), format!("{base} unorm"));
        assert_eq!(load(0, a16), format!("{base} a16"));
        assert_eq!(load(0, d16), format!("{base} d16"));
        for (w0, text) in [(tfe, "tfe"), (lwe, "lwe"), (tfe | lwe, "tfe lwe")] {
            assert_eq!(
                load(w0, 0),
                format!("image_load v[8:9], v0, s[16:23] dim:SQ_RSRC_IMG_1D {text}")
            );
        }
        // A 2D address takes two VGPRs, or one with a16.
        assert_eq!(
            load(1 << 3, 0),
            "image_load v8, v[0:1], s[16:23] dim:SQ_RSRC_IMG_2D"
        );
        assert_eq!(
            load(1 << 3, a16),
            "image_load v8, v0, s[16:23] dim:SQ_RSRC_IMG_2D a16"
        );
        // Every flag at once; d16 packs the one channel and tfe adds one.
        assert_eq!(
            load(r128 | unorm | tfe | lwe | dlc | glc | slc, a16 | d16),
            "image_load v[8:9], v0, s[16:19] dim:SQ_RSRC_IMG_1D \
             r128 unorm dlc glc slc a16 d16 tfe lwe"
        );
    }

    /// VINTRP names the i/j VGPR, or for `v_interp_mov_f32` the parameter
    /// (p10, p20, p0, and a raw `p?` for the unused code 3), then the
    /// attribute channel. The VOP3 forms put the attribute last like VINTRP
    /// and write its `high` half as a trailing flag.
    #[test]
    fn interpolation_names_the_parameter_and_puts_the_attribute_last() {
        // v_interp_mov_f32 v2, <vsrc>, attr1.y
        let mov = |vsrc: u32| one(&[0xC80A_0000 | 2 << 18 | 1 << 10 | 1 << 8 | vsrc], true);
        for (vsrc, p) in [(0, "p10"), (1, "p20"), (2, "p0"), (3, "p?")] {
            assert_eq!(mov(vsrc), format!("v_interp_mov_f32 v2, {p}, attr1.y"));
        }
        // v_interp_p1_f32 / v_interp_p2_f32 v2, v3, attr1.w
        assert_eq!(
            one(&[0xC808_0000 | 2 << 18 | 1 << 10 | 3 << 8 | 3], true),
            "v_interp_p1_f32 v2, v3, attr1.w"
        );
        assert_eq!(
            one(&[0xC809_0000 | 2 << 18 | 1 << 10 | 3 << 8 | 3], true),
            "v_interp_p2_f32 v2, v3, attr1.w"
        );
        // VOP3 v_interp_p1ll_f16 v2, v3, attr1.z: src0 holds the attribute.
        let p1ll = |high: u32| {
            one(
                &[0xD742_0002, (256 + 3) << 9 | high << 8 | 2 << 6 | 1],
                true,
            )
        };
        assert_eq!(p1ll(0), "v_interp_p1ll_f16 v2, v3, attr1.z");
        assert_eq!(p1ll(1), "v_interp_p1ll_f16 v2, v3, attr1.z high");
    }

    /// The cache invalidations `buffer_gl0_inv` and `buffer_gl1_inv` take
    /// no operands, so their address, resource and offset fields print
    /// nothing; stray cache, lds and tfe bits are still named, one space
    /// after the mnemonic like any other operand text.
    #[test]
    fn buffer_invalidations_print_no_operands_and_only_their_stray_flags() {
        let inv = |op: u32, w0: u32, w1: u32| one(&[0xE000_0000 | op << 18 | w0, w1], true);
        for (op, name) in [(0x71, "buffer_gl0_inv"), (0x72, "buffer_gl1_inv")] {
            assert_eq!(inv(op, 0, 0), name);
            // Address, data, resource, soffset, offset, offen and idxen
            // fields have nothing to print into.
            assert_eq!(inv(op, 0x3FFF, 0xFF1F_FFFF), name);
            assert_eq!(inv(op, 1 << 14, 0), format!("{name} glc"));
            assert_eq!(inv(op, 0, 1 << 22), format!("{name} slc"));
            assert_eq!(inv(op, 1 << 15, 0), format!("{name} dlc"));
            assert_eq!(inv(op, 1 << 16, 0), format!("{name} lds"));
            assert_eq!(inv(op, 0, 1 << 23), format!("{name} tfe"));
            assert_eq!(
                inv(op, 1 << 14 | 1 << 15 | 1 << 16, 1 << 22 | 1 << 23),
                format!("{name} glc slc dlc lds tfe")
            );
        }
    }

    /// SOPP instructions whose 16-bit field carries no operand print the
    /// bare mnemonic, program ends included; every other SOPP that is not
    /// a branch, wait or message prints the field as four hex digits.
    #[test]
    fn sopp_immediates_print_as_hex_unless_the_instruction_has_none() {
        let sopp = |op: u32, imm: u32| 0xBF80_0000 | op << 16 | imm;
        for (op, name) in [
            (0x03, "s_wakeup"),
            (0x0A, "s_barrier"),
            (0x13, "s_icache_inv"),
            (0x16, "s_ttracedata"),
        ] {
            assert_eq!(one(&[sopp(op, 0)], true), name);
        }
        // Program ends: decode stops at each, so read it on its own.
        for (op, name) in [
            (0x01, "s_endpgm"),
            (0x1B, "s_endpgm_saved"),
            (0x1E, "s_endpgm_ordered_ps_done"),
            (0x1F, "s_code_end"),
        ] {
            let insts = decode(&sopp(op, 0).to_le_bytes()).unwrap();
            assert_eq!(insts[0].text(true), name);
        }
        for (op, imm, text) in [
            (0x00, 0, "s_nop 0x0000"),
            (0x00, 7, "s_nop 0x0007"),
            (0x00, 0xFFFF, "s_nop 0xffff"),
            (0x0B, 1, "s_setkill 0x0001"),
            (0x0D, 1, "s_sethalt 0x0001"),
            (0x0E, 0x10, "s_sleep 0x0010"),
            (0x0F, 3, "s_setprio 0x0003"),
            (0x12, 2, "s_trap 0x0002"),
            (0x14, 1, "s_incperflevel 0x0001"),
            (0x20, 1, "s_inst_prefetch 0x0001"),
            (0x24, 0xF, "s_round_mode 0x000f"),
            (0x25, 0xF, "s_denorm_mode 0x000f"),
            (0x28, 0x1234, "s_ttracedata_imm 0x1234"),
        ] {
            assert_eq!(one(&[sopp(op, imm)], true), text);
        }
    }

    /// SOPK puts its register before the immediate, whether the register
    /// is written (`s_movk_i32`) or only read (`s_cmpk_*`,
    /// `s_waitcnt_vscnt`); `s_version` has no register, so its immediate
    /// stands alone.
    #[test]
    fn sopk_registers_lead_the_immediate_and_s_version_has_none() {
        let sopk = |op: u32, sdst: u32, imm: u32| 0xB000_0000 | op << 23 | sdst << 16 | imm;
        assert_eq!(one(&[sopk(0x00, 3, 0x1234)], true), "s_movk_i32 s3, 0x1234");
        assert_eq!(one(&[sopk(0x00, 3, 0)], true), "s_movk_i32 s3, 0x0000");
        assert_eq!(
            one(&[sopk(0x09, 3, 0xFFFF)], true),
            "s_cmpk_eq_u32 s3, 0xffff"
        );
        assert_eq!(
            one(&[sopk(0x17, 125, 0)], true),
            "s_waitcnt_vscnt null, 0x0000"
        );
        // s_version's register field is ignored.
        for sdst in [0, 125] {
            assert_eq!(one(&[sopk(0x01, sdst, 4)], true), "s_version 0x0004");
        }
    }

    /// A single VGPR is `vN`; a range is `v[a:b]` at both ends of the
    /// architectural limits, and a one-dword range still uses the bracketed
    /// form when dwords > 1 would wrap past 255.
    #[test]
    fn vector_codes_name_single_registers_and_ranges_at_their_bounds() {
        let v = |reg: u8, dwords: u8| Operand::Vector { reg, dwords }.to_string();
        assert_eq!(v(0, 1), "v0");
        assert_eq!(v(1, 1), "v1");
        assert_eq!(v(255, 1), "v255");
        assert_eq!(v(254, 2), "v[254:255]");
        assert_eq!(v(0, 4), "v[0:3]");
        assert_eq!(v(252, 4), "v[252:255]");
    }

    /// Attribute operands name every channel at both ends of the index and
    /// component ranges VINTRP and VOP3 carry.
    #[test]
    fn attribute_channels_name_each_bound() {
        use std::fmt::Write as _;
        let a = |index: u8, chan: u8| {
            let mut s = String::new();
            let _ = write!(
                s,
                "{}",
                Operand::Attr {
                    index,
                    chan,
                    high: false
                }
            );
            s
        };
        assert_eq!((a(0, 0), a(0, 3)), ("attr0.x".into(), "attr0.w".into()));
        assert_eq!((a(63, 0), a(63, 3)), ("attr63.x".into(), "attr63.w".into()));
        for (chan, name) in [(0, "x"), (1, "y"), (2, "z"), (3, "w")] {
            assert_eq!(a(31, chan), format!("attr31.{name}"));
        }
    }

    /// MTBUF spells the format word after the operands; table entries keep
    /// the `BUF_FMT_` prefix and past the table a bare number is used.
    #[test]
    fn typed_buffer_loads_spell_their_format_after_the_operands() {
        // tbuffer_load_format_xyzw: MTBUF encoding 0x3A << 26, opcode 3, format in bits 19..25.
        let mtbuf = |format: u8| {
            let w0 = 0xE800_0000 | 3 << 16 | u32::from(format) << 19;
            one(&[w0, 0x0002_0400], true)
        };
        let base = "tbuffer_load_format_xyzw v[4:7], v0, s[8:11], s0";
        assert_eq!(mtbuf(0), format!("{base} format:[BUF_FMT_INVALID]"));
        assert_eq!(mtbuf(1), format!("{base} format:[BUF_FMT_8_UNORM]"));
        assert_eq!(mtbuf(77), format!("{base} format:[BUF_FMT_32_32_32_32_FLOAT]"));
        assert_eq!(mtbuf(78), format!("{base} format:[78]"));
    }

    /// An image writes `dmask:0x…` only when the mask is not the default
    /// single channel; zero and every channel are both spelled out.
    #[test]
    fn image_dmask_is_omitted_at_one_and_spelled_otherwise() {
        // image_load v8, v0, s[16:23]: dmask in bits 8..11 of word 0.
        let load = |dmask: u32| one(&[0xF000_0000 | dmask << 8, 0x0004_0800], true);
        let ops = "image_load v8, v0, s[16:23]";
        let dim = "dim:SQ_RSRC_IMG_1D";
        assert_eq!(load(1), format!("{ops} {dim}"));
        assert_eq!(load(0), format!("{ops} dmask:0x0 {dim}"));
        assert_eq!(
            load(0xF),
            format!("image_load v[8:11], v0, s[16:23] dmask:0xf {dim}")
        );
    }
}
