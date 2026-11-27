//! Device methods 0x1c0, 0x1c8, 0x210 and 0x218: the console's pipeline
//! state serializers and deserializers.
//!
//! Serialization is not implemented: a title is never handed more state
//! than it gave us, so both serializers answer `E_ACCESSDENIED` instead of
//! manufacturing a format a retail runtime would then have to accept.
//! Deserialization is the useful direction. Compute state is the driver's
//! FSPX payload without its container; the payload is rebuilt here and put
//! in the ordinary shader container, so the normal translation and cache
//! path sees the same bytes as any other compute shader. Graphics state is
//! a fixed-function driver format whose blobs are kept verbatim until that
//! format is decoded; such a pipeline is deliberately lazy and drops draws
//! rather than guessing render state.
//!
//! The descriptions below are ABI input, not Rust references into trusted
//! memory. Every pair is therefore range-checked before it is copied. A
//! pair of size zero means "absent" even when its pointer is not null.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use super::{shader, PIPELINE_IIDS};
use crate::object::{
    self, Iid, This, E_ACCESSDENIED, E_INVALIDARG, E_NOINTERFACE, E_POINTER, HRESULT, S_OK,
};

/// The largest pair accepted. One pipeline's state is a handful of small
/// blobs; this bound is only a guard against a corrupt size making the copy
/// consume unbounded memory.
const MAX_PAIR_SIZE: u64 = 16 << 20;

/// A pointer/size pair in a serialized description.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
struct Pair {
    data: *const u8,
    size: u64,
}

/// Device method 0x218's compute description: root signature, then the
/// dispatch state, opaque driver state, shader code and shader records.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub(crate) struct ComputeDesc {
    root: This,
    kind0: Pair,
    kind1: Pair,
    kind6: Pair,
    kind7: Pair,
}

/// One serialized shader stage of a graphics pipeline: machine code and the
/// records that describe how it is bound.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
struct StageDesc {
    code: Pair,
    records: Pair,
}

/// Device method 0x1c8's graphics description: root signature, two fixed
/// function state blobs, then seven shader-stage blocks.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub(crate) struct GraphicsDesc {
    root: This,
    state_a: Pair,
    state_b: Pair,
    stages: [StageDesc; 7],
}

const _: () = assert!(size_of::<Pair>() == 0x10);
const _: () = assert!(size_of::<ComputeDesc>() == 0x48);
const _: () = assert!(size_of::<StageDesc>() == 0x20);
const _: () = assert!(size_of::<GraphicsDesc>() == 0x108);
const _: () = assert!(std::mem::offset_of!(ComputeDesc, kind6) == 0x28);
const _: () = assert!(std::mem::offset_of!(GraphicsDesc, stages) == 0x28);

/// Copies `pair`, applying the ABI's absence and malformed-pair rules.
fn copied(pair: Pair) -> Result<Option<Vec<u8>>, ()> {
    if pair.size == 0 {
        return Ok(None);
    }
    if pair.data.is_null() || pair.size > MAX_PAIR_SIZE {
        return Err(());
    }
    let size = usize::try_from(pair.size).map_err(|_| ())?;
    // SAFETY: the caller passes a description whose non-null pair pointers
    // name `size` readable bytes, as the console ABI requires. The size is
    // capped above before the copy.
    Ok(Some(
        unsafe { std::slice::from_raw_parts(pair.data, size) }.to_vec(),
    ))
}

/// Logs one rejected deserialization with the shape every failure below shares.
macro_rules! rejection {
    ($pipeline:expr, $validator:expr, $reason:expr, $hresult:expr $(, $($fields:tt)*)?) => {
        tracing::debug!(
            pipeline = $pipeline,
            validator = $validator,
            reason = $reason,
            hresult = format_args!("{:#010x}", $hresult as u32),
            $($($fields)*)?
            "pipeline state deserialization rejected"
        );
    };
}

fn bad_pair_reason(pair: Pair) -> &'static str {
    match (pair.data.is_null(), pair.size > MAX_PAIR_SIZE) {
        (true, true) => "null data pointer and size over cap",
        (true, false) => "null data pointer",
        (false, true) => "size over cap",
        (false, false) => "size does not fit usize",
    }
}

fn log_bad_pair(pipeline: &'static str, pair_name: &str, pair: Pair) {
    rejection!(
        pipeline,
        "pair copy",
        bad_pair_reason(pair),
        E_INVALIDARG,
        pair = pair_name,
        size = pair.size,
        max_size = MAX_PAIR_SIZE,
        data_null = pair.data.is_null(),
    );
}

fn copy_logged(pipeline: &'static str, pair_name: &str, pair: Pair) -> Result<Option<Vec<u8>>, ()> {
    let copied = copied(pair);
    if copied.is_err() {
        log_bad_pair(pipeline, pair_name, pair);
    }
    copied
}

fn copy_stage_logged(
    pipeline: &'static str,
    index: usize,
    pair_name: &str,
    pair: Pair,
) -> Result<Option<Vec<u8>>, ()> {
    let copied = copied(pair);
    if copied.is_err() {
        log_bad_pair(pipeline, &format!("stage{index}.{pair_name}"), pair);
    }
    copied
}

fn required(pipeline: &'static str, pair_name: &str, copied: Option<Vec<u8>>) -> Option<Vec<u8>> {
    if copied.is_none() {
        rejection!(
            pipeline,
            "required pair",
            "absent (size zero)",
            E_INVALIDARG,
            pair = pair_name,
            size = 0,
        );
    }
    copied
}

/// Validates the output argument and interface before reading a
/// description. COM requires the output to be nulled on every failure that
/// gets far enough to have an output at all.
fn prepare_output(pipeline: &'static str, iid: *const Iid, out: *mut This) -> HRESULT {
    if out.is_null() {
        rejection!(pipeline, "output", "null output pointer", E_POINTER);
        return E_POINTER;
    }
    // SAFETY: the caller passes a writable output location.
    unsafe { out.write(std::ptr::null()) };
    if iid.is_null() {
        rejection!(pipeline, "interface", "null interface id", E_POINTER);
        E_POINTER
    } else if object::is(iid, &PIPELINE_IIDS) {
        S_OK
    } else {
        // SAFETY: checked non-null above; the caller passes an interface id.
        let iid = unsafe { &*iid };
        rejection!(
            pipeline,
            "interface",
            "unsupported interface",
            E_NOINTERFACE,
            iid = %object::guid_text(iid),
        );
        E_NOINTERFACE
    }
}

struct ComputeParts {
    state: Vec<u8>,
    opaque: Option<Vec<u8>>,
    code: Vec<u8>,
    records: Option<Vec<u8>>,
}

fn le32(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().expect("four bytes"))
}

/// The state of a serialized compute pipeline, or `None` when its
/// description is structurally inconsistent. Shader translation itself
/// remains lazy, just as it is for pipelines created the ordinary way.
fn read_compute(desc: *const ComputeDesc) -> Option<ComputeParts> {
    // SAFETY: the caller passes the console description or null.
    let desc = match unsafe { desc.as_ref() } {
        Some(desc) => desc,
        None => {
            rejection!("compute", "description", "null pointer", E_INVALIDARG);
            return None;
        }
    };
    let state = required(
        "compute",
        "kind0",
        copy_logged("compute", "kind0", desc.kind0).ok()?,
    )?;
    let opaque = copy_logged("compute", "kind1", desc.kind1).ok()?;
    let code = required(
        "compute",
        "kind6",
        copy_logged("compute", "kind6", desc.kind6).ok()?,
    )?;
    let records = copy_logged("compute", "kind7", desc.kind7).ok()?;
    // The dispatch packet's seven words are the only part of kind 0 the
    // translator consumes. Machine code is dword machine code, and a record
    // blob has the 0x18-byte FSPX metadata header before its records.
    if state.len() < 0x1c {
        rejection!(
            "compute",
            "kind0 length",
            "shorter than the seven-word dispatch packet",
            E_INVALIDARG,
            kind0_size = state.len(),
            kind0_hash = %hash(&state),
        );
        return None;
    }
    if code.len() % 4 != 0 {
        rejection!(
            "compute",
            "kind6 alignment",
            "size is not dword-aligned",
            E_INVALIDARG,
            kind6_size = code.len(),
            kind6_hash = %hash(&code),
        );
        return None;
    }
    if let Some(records) = &records {
        if records.len() < 0x18 {
            rejection!(
                "compute",
                "kind7 header",
                "shorter than the FSPX metadata header",
                E_INVALIDARG,
                kind7_size = records.len(),
                kind7_hash = %hash(records),
            );
            return None;
        }
        // FSPX names the code length in dwords. A mismatch cannot become a
        // valid shader payload, so this is a corrupt description rather
        // than merely a shader the host may refuse later.
        let record_code_dwords = le32(records, 8) as usize;
        if record_code_dwords != code.len() / 4 {
            rejection!(
                "compute",
                "kind7 code length",
                "record code length differs from kind6",
                E_INVALIDARG,
                record_code_dwords,
                kind6_size = code.len(),
                kind6_hash = %hash(&code),
                kind7_size = records.len(),
                kind7_hash = %hash(records),
            );
            return None;
        }
    }
    Some(ComputeParts {
        state,
        opaque,
        code,
        records,
    })
}

fn align16(at: usize) -> usize {
    (at + 15) & !15
}

fn put32(out: &mut [u8], at: usize, value: u32) {
    out[at..at + 4].copy_from_slice(&value.to_le_bytes());
}

/// Rebuilds the FSPX payload whose parts a serialized compute description
/// carries separately: a 16-byte header, a 16-byte directory entry per
/// kind, then each blob 16-byte aligned.
fn fspx_payload(parts: &ComputeParts) -> Vec<u8> {
    let mut pieces: Vec<(u32, &[u8])> = vec![(0, &parts.state), (6, &parts.code)];
    if let Some(opaque) = &parts.opaque {
        pieces.insert(1, (1, opaque));
    }
    if let Some(records) = &parts.records {
        pieces.push((7, records));
    }

    let directory = 16 + 16 * pieces.len();
    let mut entries = Vec::with_capacity(pieces.len());
    let mut at = align16(directory);
    for (kind, bytes) in &pieces {
        entries.push((*kind, at, bytes.len()));
        at = align16(at + bytes.len());
    }

    let mut out = vec![0; at];
    out[..4].copy_from_slice(b"FSPX");
    put32(&mut out, 4, 1);
    put32(&mut out, 8, pieces.len() as u32);
    put32(&mut out, 12, at as u32);
    for (index, ((kind, at, size), (_, bytes))) in entries.iter().zip(&pieces).enumerate() {
        let entry = 16 + 16 * index;
        put32(&mut out, entry, *kind);
        put32(&mut out, entry + 4, *at as u32);
        put32(&mut out, entry + 8, *size as u32);
        out[*at..*at + bytes.len()].copy_from_slice(bytes);
    }
    out
}

/// Wraps one payload in the smallest standard shader container. The rest
/// of the emulator already knows how to translate and cache this form.
fn accc_container(payload: &[u8]) -> Vec<u8> {
    let parts_at = 0x20 + 4;
    let payload_at = parts_at + 8;
    let total = payload_at + 8 + payload.len();
    let mut out = vec![0; total];
    out[..4].copy_from_slice(b"DXBC");
    // Bytes 4..20 are the container's optional digest. It is not checked
    // for DXIL/ACCC input here and stays zero.
    put32(&mut out, 20, 1); // container version 1.0 (two little-endian u16s)
    put32(&mut out, 24, total as u32);
    put32(&mut out, 28, 1);
    put32(&mut out, 32, payload_at as u32);
    out[payload_at..payload_at + 4].copy_from_slice(b"ACCC");
    put32(&mut out, payload_at + 4, payload.len() as u32);
    out[payload_at + 8..].copy_from_slice(payload);
    out
}

fn hash(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex()[..16].to_owned()
}

fn pair_len_hash(pair: Option<&[u8]>) -> (usize, String) {
    pair.map_or((0, "-".to_owned()), |bytes| (bytes.len(), hash(bytes)))
}

/// Device methods 0x1c0 and 0x210. Producing a console serialization needs
/// the driver-private fixed-function format; returning access denied is the
/// documented console behaviour for an unsupported privileged operation.
pub(crate) extern "system" fn serialize_denied(_: This, _: usize, _: usize, _: usize) -> HRESULT {
    E_ACCESSDENIED
}

fn trace_compute_entry(desc: *const ComputeDesc) {
    if !tracing::enabled!(tracing::Level::TRACE) {
        return;
    }
    // SAFETY: null is checked by `as_ref`; a non-null description names
    // readable memory under the console ABI. Only scalar pair sizes are read.
    match unsafe { desc.as_ref() } {
        Some(desc) => tracing::trace!(
            pipeline = "compute",
            desc_null = false,
            kind0_size = desc.kind0.size,
            kind1_size = desc.kind1.size,
            kind6_size = desc.kind6.size,
            kind7_size = desc.kind7.size,
            "pipeline state deserialization entry"
        ),
        None => tracing::trace!(
            pipeline = "compute",
            desc_null = true,
            "pipeline state deserialization entry"
        ),
    }
}

/// Device method 0x218: deserializes a compute pipeline.
pub(crate) extern "system" fn compute(
    _: This,
    desc: *const ComputeDesc,
    iid: *const Iid,
    out: *mut This,
) -> HRESULT {
    trace_compute_entry(desc);
    let valid = prepare_output("compute", iid, out);
    if valid != S_OK {
        return valid;
    }
    let Some(parts) = read_compute(desc) else {
        return E_INVALIDARG;
    };
    let payload = fspx_payload(&parts);
    let container = accc_container(&payload);
    let (kind0_size, kind0_hash) = pair_len_hash(Some(parts.state.as_slice()));
    let (kind1_size, kind1_hash) = pair_len_hash(parts.opaque.as_deref());
    let (kind6_size, kind6_hash) = pair_len_hash(Some(parts.code.as_slice()));
    let (kind7_size, kind7_hash) = pair_len_hash(parts.records.as_deref());
    tracing::debug!(
        kind0_size,
        kind0_hash = %kind0_hash,
        kind1_size,
        kind1_hash = %kind1_hash,
        kind6_size,
        kind6_hash = %kind6_hash,
        kind7_size,
        kind7_hash = %kind7_hash,
        container = container.len(),
        "compute pipeline state deserialized"
    );
    super::hand_out(
        vec![shader("compute", &container)],
        #[cfg(windows)]
        None,
        None,
        iid,
        out,
    )
}

/// One graphics stage retained from a serialized description.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct OfflineStage {
    pub(crate) code: Option<Vec<u8>>,
    pub(crate) records: Option<Vec<u8>>,
}

/// The retained console-side form of a serialized graphics pipeline. It is
/// not enough to build a safe host pipeline yet, but keeping it in the
/// object makes the lazy object inspectable and gives the later decoder all
/// of the input without touching title memory again.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct OfflineGraphics {
    pub(crate) state_a: Option<Vec<u8>>,
    pub(crate) state_b: Vec<u8>,
    pub(crate) stages: [Option<OfflineStage>; 7],
}

/// The two measured shapes of fixed-function state blob A: 380 or 464
/// bytes, followed by up to five or seven eight-byte records. Word zero is
/// a version/type tag whose low bits 3 and 4 vary.
fn state_a_shape(blob: &[u8]) -> bool {
    let size = blob.len();
    let measured = (380..=420).contains(&size) && (size - 380).is_multiple_of(8)
        || (464..=520).contains(&size) && (size - 464).is_multiple_of(8);
    measured && blob.len() >= 4 && (le32(blob, 0) & !0x18) == 0x4005_d800
}

fn stage_sizes(stages: &[StageDesc; 7]) -> String {
    stages
        .iter()
        .enumerate()
        .map(|(index, stage)| format!("{index}:{}:{}", stage.code.size, stage.records.size))
        .collect::<Vec<_>>()
        .join(",")
}

fn read_graphics(desc: *const GraphicsDesc) -> Option<OfflineGraphics> {
    // SAFETY: the caller passes the console description or null.
    let desc = match unsafe { desc.as_ref() } {
        Some(desc) => desc,
        None => {
            rejection!("graphics", "description", "null pointer", E_INVALIDARG);
            return None;
        }
    };
    let state_a = copy_logged("graphics", "state_a", desc.state_a).ok()?;
    let state_b = required(
        "graphics",
        "state_b",
        copy_logged("graphics", "state_b", desc.state_b).ok()?,
    )?;
    if let Some(blob) = &state_a
        && !state_a_shape(blob)
    {
        rejection!(
            "graphics",
            "state A shape",
            "size or tag does not match measured state blobs",
            E_INVALIDARG,
            state_a_size = blob.len(),
            state_a_hash = %hash(blob),
        );
        return None;
    }
    if state_b.len() < 0x50 {
        rejection!(
            "graphics",
            "state B length",
            "shorter than 0x50 bytes",
            E_INVALIDARG,
            state_b_size = state_b.len(),
            state_b_hash = %hash(&state_b),
        );
        return None;
    }

    let mut stages: [Option<OfflineStage>; 7] = std::array::from_fn(|_| None);
    let mut complete = 0;
    for (index, stage) in desc.stages.iter().enumerate() {
        let code = copy_stage_logged("graphics", index, "code", stage.code).ok()?;
        let records = copy_stage_logged("graphics", index, "records", stage.records).ok()?;
        if let Some(code) = &code
            && code.len() % 4 != 0
        {
            rejection!(
                "graphics",
                "stage code alignment",
                "size is not dword-aligned",
                E_INVALIDARG,
                stage = index,
                code_size = code.len(),
                code_hash = %hash(code),
            );
            return None;
        }
        if code.is_some() || records.is_some() {
            stages[index] = Some(OfflineStage { code, records });
            complete += 1;
        }
    }
    if complete == 0 {
        rejection!(
            "graphics",
            "stage coverage",
            "no stage has code or records",
            E_INVALIDARG,
            stages = %stage_sizes(&desc.stages),
        );
        return None;
    }
    Some(OfflineGraphics {
        state_a,
        state_b,
        stages,
    })
}

fn trace_graphics_entry(desc: *const GraphicsDesc) {
    if !tracing::enabled!(tracing::Level::TRACE) {
        return;
    }
    // SAFETY: null is checked by `as_ref`; a non-null description names
    // readable memory under the console ABI. Only scalar pair sizes are read.
    match unsafe { desc.as_ref() } {
        Some(desc) => tracing::trace!(
            pipeline = "graphics",
            desc_null = false,
            state_a_size = desc.state_a.size,
            state_b_size = desc.state_b.size,
            stages = %stage_sizes(&desc.stages),
            "pipeline state deserialization entry"
        ),
        None => tracing::trace!(
            pipeline = "graphics",
            desc_null = true,
            "pipeline state deserialization entry"
        ),
    }
}

/// Device method 0x1c8: deserializes a graphics pipeline into a valid,
/// lazy pipeline object. Its blobs are validated and retained, but not
/// translated into host fixed-function state yet.
pub(crate) extern "system" fn graphics(
    _: This,
    desc: *const GraphicsDesc,
    iid: *const Iid,
    out: *mut This,
) -> HRESULT {
    trace_graphics_entry(desc);
    let valid = prepare_output("graphics", iid, out);
    if valid != S_OK {
        return valid;
    }
    let Some(offline) = read_graphics(desc) else {
        return E_INVALIDARG;
    };
    let (a_size, a_hash) = pair_len_hash(offline.state_a.as_deref());
    let (b_size, b_hash) = pair_len_hash(Some(offline.state_b.as_slice()));
    let stages = offline
        .stages
        .iter()
        .enumerate()
        .filter_map(|(index, stage)| {
            stage.as_ref().map(|stage| {
                format!(
                    "{index}:{}:{}:{}:{}",
                    stage.code.as_ref().map_or(0, Vec::len),
                    stage
                        .code
                        .as_deref()
                        .map(hash)
                        .unwrap_or_else(|| "-".into()),
                    stage.records.as_ref().map_or(0, Vec::len),
                    stage
                        .records
                        .as_deref()
                        .map(hash)
                        .unwrap_or_else(|| "-".into())
                )
            })
        })
        .collect::<Vec<_>>()
        .join(",");
    tracing::debug!(
        state_a_size = a_size,
        state_a_hash = %a_hash,
        state_b_size = b_size,
        state_b_hash = %b_hash,
        stages = %stages,
        "graphics pipeline state deserialized"
    );
    super::hand_out(
        Vec::new(),
        #[cfg(windows)]
        None,
        Some(offline),
        iid,
        out,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::slot;
    use std::sync::Arc;
    use victory_shader::native::StageIo;
    use victory_shader::{Container, FourCC, HwStage};

    fn pair(bytes: &[u8]) -> Pair {
        Pair {
            data: bytes.as_ptr(),
            size: bytes.len() as u64,
        }
    }

    fn words(words: &[u32]) -> Vec<u8> {
        words.iter().flat_map(|word| word.to_le_bytes()).collect()
    }

    struct ComputeInput {
        state: Vec<u8>,
        opaque: Vec<u8>,
        code: Vec<u8>,
        records: Vec<u8>,
    }

    impl ComputeInput {
        fn new() -> Self {
            // Seven dispatch-packet words: threads 32x1x1, wave32, sixteen
            // VGPRs either way, and a nonzero COMPUTE_PGM_RSRC2.
            let state = words(&[0, 0, 0, 32 | 1 << 16, 1 | 0x10 << 16, 1, 0x9c]);
            let code = vec![0x00, 0x00, 0x81, 0xbf]; // s_endpgm
            let mut records = words(&[0x403, 0, code.len() as u32 / 4, 0, 0]);
            records.extend_from_slice(&[0x40, 4, 0, 0]);
            let mut hash_record = [0; 32];
            hash_record[0] = 0x43;
            records.extend_from_slice(&hash_record);
            let len = records.len() as u32 - 0x14;
            records[4..8].copy_from_slice(&len.to_le_bytes());
            Self {
                state,
                opaque: vec![0; 0x14],
                code,
                records,
            }
        }

        fn desc(&self) -> ComputeDesc {
            ComputeDesc {
                root: std::ptr::null(),
                kind0: pair(&self.state),
                kind1: pair(&self.opaque),
                kind6: pair(&self.code),
                kind7: pair(&self.records),
            }
        }
    }

    struct GraphicsInput {
        state_a: Vec<u8>,
        state_b: Vec<u8>,
        code: Vec<u8>,
        records: Vec<u8>,
    }

    impl GraphicsInput {
        fn new() -> Self {
            let mut state_a = vec![0; 380];
            state_a[..4].copy_from_slice(&0x4005_d800u32.to_le_bytes());
            let mut records = vec![0; 32];
            records[0] = 0x43;
            Self {
                state_a,
                state_b: vec![0; 0x50],
                code: vec![0x00, 0x00, 0x81, 0xbf],
                records,
            }
        }

        fn desc(&self) -> GraphicsDesc {
            let absent = Pair {
                data: std::ptr::null(),
                size: 0,
            };
            let mut stages = [StageDesc {
                code: absent,
                records: absent,
            }; 7];
            stages[0] = StageDesc {
                code: pair(&self.code),
                records: pair(&self.records),
            };
            GraphicsDesc {
                root: std::ptr::null(),
                state_a: pair(&self.state_a),
                state_b: pair(&self.state_b),
                stages,
            }
        }
    }

    type ComputeDeserialize =
        extern "system" fn(This, *const ComputeDesc, *const Iid, *mut This) -> HRESULT;
    type GraphicsDeserialize =
        extern "system" fn(This, *const GraphicsDesc, *const Iid, *mut This) -> HRESULT;
    type Serializer = extern "system" fn(This, usize, usize, usize) -> HRESULT;
    type Refcount = extern "system" fn(This) -> u32;

    fn release(pipeline: This) -> u32 {
        slot::<Refcount>(pipeline, 0x10)(pipeline)
    }

    #[test]
    fn device_slots_56_57_66_and_67_are_the_console_methods() {
        let device = crate::device::device();
        assert_eq!(
            slot::<Serializer>(device, 0x1c0)(device, 0, 0, 0),
            E_ACCESSDENIED
        );
        assert_eq!(
            slot::<Serializer>(device, 0x210)(device, 0, 0, 0),
            E_ACCESSDENIED
        );
        let input = ComputeInput::new();
        let desc = input.desc();
        assert_eq!(
            slot::<ComputeDeserialize>(device, 0x218)(
                device,
                &desc,
                &PIPELINE_IIDS[0],
                std::ptr::null_mut()
            ),
            E_POINTER
        );
        let graphics = GraphicsInput::new();
        let desc = graphics.desc();
        assert_eq!(
            slot::<GraphicsDeserialize>(device, 0x1c8)(
                device,
                &desc,
                &PIPELINE_IIDS[0],
                std::ptr::null_mut()
            ),
            E_POINTER
        );
    }

    #[test]
    fn compute_com_validation_nulls_output_and_counts_references() {
        let device = crate::device::device();
        let input = ComputeInput::new();
        let desc = input.desc();
        let deserialize = slot::<ComputeDeserialize>(device, 0x218);

        let mut out = device;
        assert_eq!(
            deserialize(device, std::ptr::null(), &PIPELINE_IIDS[0], &mut out),
            E_INVALIDARG
        );
        assert!(out.is_null());
        let mut out = device;
        assert_eq!(
            deserialize(device, &desc, &[0; 16], &mut out),
            E_NOINTERFACE
        );
        assert!(out.is_null());
        let mut out = device;
        assert_eq!(
            deserialize(device, &desc, std::ptr::null(), &mut out),
            E_POINTER
        );
        assert!(out.is_null());
        assert_eq!(
            deserialize(device, &desc, &PIPELINE_IIDS[0], std::ptr::null_mut()),
            E_POINTER
        );

        let mut pipeline = std::ptr::null();
        assert_eq!(
            deserialize(device, &desc, &PIPELINE_IIDS[0], &mut pipeline),
            S_OK
        );
        assert!(!pipeline.is_null());
        assert_eq!(slot::<Refcount>(pipeline, 8)(pipeline), 2);
        assert_eq!(release(pipeline), 1);
        assert_eq!(release(pipeline), 0);
    }

    #[test]
    fn pair_rules_make_zero_absent_and_cap_present_pairs() {
        let wild = std::ptr::dangling::<u8>();
        assert!(copied(Pair {
            data: wild,
            size: 0
        })
        .unwrap()
        .is_none());
        assert!(copied(Pair {
            data: std::ptr::null(),
            size: 1
        })
        .is_err());
        assert!(copied(Pair {
            data: wild,
            size: MAX_PAIR_SIZE + 1
        })
        .is_err());
        assert!(copied(Pair {
            data: wild,
            size: u64::MAX
        })
        .is_err());
        let big = vec![0x5a; MAX_PAIR_SIZE as usize];
        let copied = copied(pair(&big)).unwrap().unwrap();
        assert_eq!(copied.len(), MAX_PAIR_SIZE as usize);
        assert_eq!((copied[0], copied[copied.len() - 1]), (0x5a, 0x5a));
    }

    #[test]
    fn compute_parts_become_the_fspx_program_in_an_accc_container() {
        let input = ComputeInput::new();
        let desc = input.desc();
        let parts = read_compute(&desc).expect("well-formed compute description");
        let payload = fspx_payload(&parts);
        assert_eq!(&payload[..4], b"FSPX");
        let container = accc_container(&payload);
        let parsed = Container::parse(&container).expect("valid container");
        assert_eq!(parsed.version, (1, 0));
        let accc = parsed.part(FourCC::ACCC).expect("ACCC part");
        assert_eq!(accc.data, payload);
        let programs = victory_shader::native_programs(accc.data).expect("valid FSPX");
        victory_shader::emit::translate(&container).expect("the synthetic shader translates");
        assert_eq!(programs.len(), 1);
        let program = &programs[0];
        assert_eq!(program.stage, HwStage::Cs);
        assert!(!program.wave64);
        assert_eq!(program.vgprs, 16);
        assert_eq!(program.code, input.code);
        let StageIo::Cs(io) = &program.io else {
            panic!("not a compute program");
        };
        assert_eq!((io.threads, io.rsrc2), ([32, 1, 1], 0x9c));
    }

    #[test]
    fn compute_descriptions_hand_out_pipelines_and_share_shader_containers() {
        let device = crate::device::device();
        let input = ComputeInput::new();
        let desc = input.desc();
        let deserialize = slot::<ComputeDeserialize>(device, 0x218);
        let (mut first, mut second) = (std::ptr::null(), std::ptr::null());
        assert_eq!(
            deserialize(device, &desc, &PIPELINE_IIDS[0], &mut first),
            S_OK
        );
        assert_eq!(
            deserialize(device, &desc, &PIPELINE_IIDS[0], &mut second),
            S_OK
        );
        assert!(!first.is_null());
        let first_pipeline = crate::pipeline::pipeline(first);
        let second_pipeline = crate::pipeline::pipeline(second);
        assert_eq!(first_pipeline.shaders.len(), 1);
        assert!(Arc::ptr_eq(
            &first_pipeline.shaders[0],
            &second_pipeline.shaders[0]
        ));
        let container = &first_pipeline.shaders[0].container;
        let parsed = Container::parse(container).expect("pipeline keeps the synthetic container");
        victory_shader::native_programs(parsed.part(FourCC::ACCC).expect("ACCC").data)
            .expect("FSPX");
        assert!(first_pipeline.offline.is_none());
        assert_eq!(release(first), 0);
        assert_eq!(release(second), 0);
    }

    #[test]
    fn malformed_compute_descriptions_are_rejected() {
        let input = ComputeInput::new();
        let rejected = |desc: &ComputeDesc| {
            assert!(read_compute(desc).is_none());
            let device = crate::device::device();
            let mut out = device;
            assert_eq!(
                compute(device, desc, &PIPELINE_IIDS[0], &mut out),
                E_INVALIDARG
            );
            assert!(out.is_null());
        };
        let mut desc = input.desc();
        desc.kind0.size = 0;
        rejected(&desc);
        let mut desc = input.desc();
        desc.kind6.size = 0;
        rejected(&desc);
        let mut desc = input.desc();
        desc.kind0.size = 0x1b;
        rejected(&desc);
        let mut desc = input.desc();
        desc.kind6.size = 3;
        rejected(&desc);
        let mut desc = input.desc();
        desc.kind1.data = std::ptr::null();
        rejected(&desc);
        let mut records = input.records.clone();
        records[8] = 2; // says two code dwords; kind 6 has one
        let mut desc = input.desc();
        desc.kind7 = pair(&records);
        rejected(&desc);
    }

    #[test]
    fn state_blob_a_must_have_a_measured_shape_and_tag() {
        let input = GraphicsInput::new();
        assert!(state_a_shape(&input.state_a));
        for size in [388, 420, 464, 472, 520] {
            let mut blob = vec![0; size];
            blob[..4].copy_from_slice(&0x4005_d810u32.to_le_bytes());
            assert!(state_a_shape(&blob), "{size}");
        }
        for size in [379, 421, 463, 521] {
            let mut blob = vec![0; size];
            blob[..4].copy_from_slice(&0x4005_d800u32.to_le_bytes());
            assert!(!state_a_shape(&blob), "{size}");
        }
        let mut tagged = input.state_a.clone();
        tagged[0] ^= 1;
        assert!(!state_a_shape(&tagged));
    }

    #[test]
    fn graphics_descriptions_hand_out_lazy_pipelines_that_bind_safely() {
        let device = crate::device::device();
        let input = GraphicsInput::new();
        let desc = input.desc();
        let deserialize = slot::<GraphicsDeserialize>(device, 0x1c8);
        let mut pipeline = std::ptr::null();
        assert_eq!(
            deserialize(device, &desc, &PIPELINE_IIDS[0], &mut pipeline),
            S_OK
        );
        assert!(!pipeline.is_null());
        let object = crate::pipeline::pipeline(pipeline);
        assert!(object.shaders.is_empty());
        let offline = object.offline.as_ref().expect("offline graphics state");
        assert_eq!(offline.state_a.as_deref(), Some(input.state_a.as_slice()));
        assert_eq!(offline.state_b, input.state_b);
        let stage = offline.stages[0].as_ref().expect("pixel stage");
        assert_eq!(stage.code.as_deref(), Some(input.code.as_slice()));
        assert_eq!(stage.records.as_deref(), Some(input.records.as_slice()));
        assert!(offline.stages[1..].iter().all(Option::is_none));
        crate::pipeline::bound(pipeline);
        #[cfg(windows)]
        assert!(crate::pipeline::host_pipeline(pipeline).is_none());
        assert_eq!(release(pipeline), 0);
    }

    #[test]
    fn zero_sized_graphics_pairs_are_absent_even_with_nonnull_pointers() {
        let input = GraphicsInput::new();
        let mut desc = input.desc();
        desc.state_a.data = std::ptr::dangling::<u8>();
        desc.state_a.size = 0;
        let parsed = read_graphics(&desc).expect("state A is absent, not malformed");
        assert!(parsed.state_a.is_none());

        // Either half of a stage block may be present alone: the records
        // can name the original container even when the code pair is empty.
        let mut desc = input.desc();
        desc.stages[0].records.data = std::ptr::dangling::<u8>();
        desc.stages[0].records.size = 0;
        let parsed = read_graphics(&desc).expect("a code-only stage block");
        let stage = parsed.stages[0].as_ref().expect("stage");
        assert!(stage.code.is_some());
        assert!(stage.records.is_none());
        let mut desc = input.desc();
        desc.stages[0].code.data = std::ptr::dangling::<u8>();
        desc.stages[0].code.size = 0;
        let parsed = read_graphics(&desc).expect("a records-only stage block");
        let stage = parsed.stages[0].as_ref().expect("stage");
        assert!(stage.code.is_none());
        assert!(stage.records.is_some());
    }

    #[test]
    fn malformed_graphics_descriptions_are_rejected() {
        let input = GraphicsInput::new();
        assert!(read_graphics(&input.desc()).is_some());
        let rejected = |desc: &GraphicsDesc| {
            assert!(read_graphics(desc).is_none());
            let device = crate::device::device();
            let mut out = device;
            assert_eq!(
                graphics(device, desc, &PIPELINE_IIDS[0], &mut out),
                E_INVALIDARG
            );
            assert!(out.is_null());
        };
        let mut desc = input.desc();
        desc.state_b.size = 0;
        rejected(&desc);
        let mut desc = input.desc();
        desc.state_b.size = 0x4f;
        rejected(&desc);
        let mut desc = input.desc();
        desc.state_a.size = 421;
        rejected(&desc);
        let mut desc = input.desc();
        desc.stages[0].code.size = 3;
        rejected(&desc);
        let mut desc = input.desc();
        desc.stages[0].code.size = 0;
        desc.stages[0].records.size = 0;
        rejected(&desc);
    }

    #[test]
    fn graphics_com_validation_matrix_matches_compute() {
        let device = crate::device::device();
        let input = GraphicsInput::new();
        let desc = input.desc();
        let deserialize = slot::<GraphicsDeserialize>(device, 0x1c8);
        let mut out = device;
        assert_eq!(
            deserialize(device, std::ptr::null(), &PIPELINE_IIDS[0], &mut out),
            E_INVALIDARG
        );
        assert!(out.is_null());
        let mut out = device;
        assert_eq!(
            deserialize(device, &desc, &[0; 16], &mut out),
            E_NOINTERFACE
        );
        assert!(out.is_null());
        assert_eq!(
            deserialize(device, &desc, &PIPELINE_IIDS[0], std::ptr::null_mut()),
            E_POINTER
        );
    }
}
