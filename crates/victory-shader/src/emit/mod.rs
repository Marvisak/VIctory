//! DXIL emission (decision 0003, step 3): a console vertex, pixel, compute
//! or mesh shader as a PC DXIL container (SM 6.6) that host D3D12 accepts.
//! Compute shaders keep the console's thread-group size; one that depends
//! on the lane layout (cross-lane operations) declares the console's wave
//! size (`WaveSize`), so the host device must support it.
//!
//! What the host binds for a translated shader ([`HostShader`]):
//!
//! * **User data** at `b0, space 0` (32-bit root constants or a constant
//!   buffer, per stage): dword `index` holds a root parameter's value, as
//!   [`UserData`] lists. A root-constant dword is the constant. A
//!   descriptor table is the byte offset of its first descriptor in its
//!   heap: the descriptor index times 32 for CBV/SRV/UAV tables, times 16
//!   for sampler tables (the console's descriptor sizes; the code adds
//!   offsets in those units). The heap bases of dynamic resources are 0
//!   and not listed.
//! * **Descriptors** through SM 6.6 `ResourceDescriptorHeap[]` and
//!   `SamplerDescriptorHeap[]`, at the index the console address gives, so
//!   the host heaps keep the title's descriptor order. The root signature
//!   needs `CBV_SRV_UAV_HEAP_DIRECTLY_INDEXED` and
//!   `SAMPLER_HEAP_DIRECTLY_INDEXED`.
//! * **Immediate samplers** as static samplers `s<n>, space 0`, `n` the
//!   position in [`HostShader::samplers`] (the console sampler words) and
//!   [`HostShader::static_samplers`] (the same decoded into a D3D12 static
//!   sampler description). Comparison sampling (added 2026-11-23): an
//!   immediate sampler it uses has a comparison filter there (bit 0x80);
//!   a heap sampler it uses must be a comparison sampler on the host.
//! * **Root CBVs and root SRVs** (raw buffers) as root descriptors
//!   `b<i>` / `t<i>` in [`ROOT_CBV_SPACE`], `i` the title's root parameter
//!   index ([`HostShader::root_cbvs`], [`HostShader::root_srvs`]); the
//!   address the title sets is the buffer's.
//! * **Vertex fetch** (added 2026-11-23). Grand Theft Auto VI's mesh and
//!   vertex programs fetch vertices themselves with typed-format loads
//!   (`tbuffer_load_format_*`): the format (8_8_8_8 unorm, 16_16 snorm,
//!   2_10_10_10 unorm, 8_8_8_8 and 16_16_16_16 uint...) is in the
//!   instruction, so the translation reads raw bytes and converts them
//!   itself. Their descriptors are built around a root SRV (stride 1, no
//!   record limit): the host binds the root SRV `t<slot>` above with the
//!   address the title sets, nothing more. The vertex index, the stride
//!   and the attribute offset are the shader's own arithmetic on data in
//!   that buffer (per-stream headers), not descriptor fields.
//! * **Buffer views in the heap** are declared by how the shader reads
//!   them: raw (ByteAddressBuffer) for dword loads and stores, atomics and
//!   typed-format loads without an index; structured with the used-resource
//!   entry's stride when indexed; typed with four 32-bit float channels for
//!   `buffer_load_format_*` and `buffer_store_format_*`. The host's view at
//!   that index must be of that kind and start at the console descriptor's
//!   base address (raw: `R32_TYPELESS` with the raw flag), whatever kind the
//!   title's view description says. Typed-format loads from heap
//!   descriptors (1,024 world compute programs, ray-tracing material
//!   resolves) read raw views this way.
//!
//! * **Texture atomics** (added 2026-11-23): a texture UAV the shader uses
//!   atomically is declared a 32-bit unsigned single-channel view; the
//!   host's view at that index must be an R32 integer view (64-bit atomics
//!   stop the translation). Global loads from a root SRV read it raw.
//!
//! Entry points: [`translate`] (container bytes as the title passes them
//! at pipeline creation → [`HostShader`]), [`emit`] (the same with the
//! earlier stages already run). The root constants of each stage must
//! cover [`HostShader::user_data_dwords`]. Stability: these types and the
//! binding scheme are what pipeline creation builds on; changes are
//! announced.
//! * **Signatures** as the console container has them (same semantics,
//!   registers and masks), so the input layout and stage linking are the
//!   title's; pixel outputs are plain 32-bit values.
//! * **Mesh shaders** ([`HwStage::Ms`], `ms_6_6`): user data and
//!   descriptors as above (the root constants visible to the mesh stage);
//!   the pipeline is a mesh pipeline (pipeline-state stream, no input
//!   layout) with the title's pixel shader. The thread group is the
//!   console's in lanes (`[numthreads]` in the DXIL), the vertex and
//!   primitive maxima its own, triangles only, no wave size required. The
//!   console numbers groups in x only: the draw path passes the title's
//!   count as `DispatchMesh(x, 1, 1)`. Per-primitive outputs (`PSG1`) are
//!   the console's, without `SV_PrimitiveID` and `SV_CullPrimitive` (the
//!   console never exports them).

mod bitcode;
mod container;
#[cfg(all(test, windows))]
mod diff_tests;
#[cfg(all(windows, any(test, feature = "host")))]
pub mod host;
mod lower;
mod sampler;
mod shader;
#[cfg(windows)]
pub mod validator;
#[cfg(all(test, windows))]
mod valu_tests;
#[cfg(all(test, windows))]
mod waterfall_tests;
mod wave;

use crate::dxbc::{Container, FourCC};
use crate::native::{HwStage, NativeProgram, RootType};
use crate::{ShaderError, io, ir, resource, structure};

pub use sampler::StaticSampler;
pub use shader::{ROOT_CBV_SPACE, SAMPLER_SPACE, USER_DATA_SPACE};

/// The loop limit without an override: 2^20.
pub const DEFAULT_LOOP_LIMIT: u32 = lower::LOOP_LIMIT;

/// The iteration count after which a translated loop is left, per lane and
/// per entry into the loop: [`DEFAULT_LOOP_LIMIT`].
///
/// With the `diagnostics` feature, `VICTORY_SHADER_LOOP_LIMIT` (decimal or
/// `0x` hex, at least 1) sets it instead. A loop that hits the limit in
/// every lane of a full-screen pass still runs for seconds (2^20 times a
/// 4K screen is about 10^13 iterations); a much lower limit shows whether
/// such loops are what makes a frame slow. The value is part of the
/// translation, so the shader cache must key on it (`victory-gpu`'s does).
pub fn loop_limit() -> u32 {
    static LIMIT: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    *LIMIT.get_or_init(|| {
        if cfg!(feature = "diagnostics") {
            parse_loop_limit(std::env::var("VICTORY_SHADER_LOOP_LIMIT").ok().as_deref())
        } else {
            lower::LOOP_LIMIT
        }
    })
}

/// `VICTORY_SHADER_LOOP_LIMIT`'s value; the default when it is unset,
/// unreadable or 0. Pure, for testing.
fn parse_loop_limit(value: Option<&str>) -> u32 {
    let parsed = value
        .map(str::trim)
        .and_then(|v| match v.strip_prefix("0x") {
            Some(hex) => u32::from_str_radix(hex, 16).ok(),
            None => v.parse().ok(),
        });
    match parsed {
        Some(n) if n > 0 => n,
        _ => lower::LOOP_LIMIT,
    }
}

/// A translated shader and what the host binds for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostShader {
    pub stage: HwStage,
    /// The DXIL container, hashed as D3D12 expects.
    pub dxil: Vec<u8>,
    /// The user-data dwords the shader reads.
    pub user_data: Vec<UserData>,
    /// Immediate samplers: the four sampler words of `s<n>`.
    pub samplers: Vec<[u32; 4]>,
    /// The same samplers decoded, `s<n>` at position n: the static sampler
    /// descriptions the host's root signature declares.
    pub static_samplers: Vec<StaticSampler>,
    /// Root CBVs the shader reads: root parameter indices, each bound as
    /// `b<index>` in [`ROOT_CBV_SPACE`].
    pub root_cbvs: Vec<u8>,
    /// Root SRVs (raw buffers) the shader reads: `t<index>` in
    /// [`ROOT_CBV_SPACE`].
    pub root_srvs: Vec<u8>,
}

impl HostShader {
    /// Size of the user-data buffer the shader declares, in dwords (a
    /// multiple of 4; 0 when it reads none). Root constants must cover it.
    pub fn user_data_dwords(&self) -> u32 {
        let n = self
            .user_data
            .iter()
            .map(|u| u.index + 1)
            .max()
            .unwrap_or(0);
        n.next_multiple_of(4)
    }
}

/// One dword of user data and the root parameter that fills it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UserData {
    /// Dword in the user-data buffer (the console's user-data index).
    pub index: u32,
    /// Root parameter index in the title's root signature.
    pub slot: u8,
    /// `Constants` or `Table` (see the module notes for the value).
    pub root: RootType,
    /// Dword within the parameter.
    pub dword: u32,
}

/// Translates the vertex, pixel, compute or mesh program of a console
/// shader container.
pub fn translate(container: &[u8]) -> Result<HostShader, ShaderError> {
    let c = Container::parse(container)?;
    let dxil = c
        .part(FourCC::DXIL)
        .ok_or(ShaderError::Malformed("no DXIL part"))?;
    let native = crate::native_programs(dxil.data)?
        .into_iter()
        .find(|p| {
            matches!(
                p.stage,
                HwStage::Vs | HwStage::Ps | HwStage::Cs | HwStage::Ms
            )
        })
        .ok_or(ShaderError::Unsupported {
            offset: 0,
            what: "a container without a vertex, pixel, compute or mesh program",
        })?;
    let p = ir::build(&native.patched_code(), native.wave64, native.vgprs)?;
    // Structurisation first: tracing per-lane descriptors needs it (its
    // failure is reported after the earlier stages').
    let st = structure::structure(&p);
    let res = resource::track(&p, &native.layout, native.stage, st.as_ref().ok())?;
    let io = io::raise(&p, &native)?;
    let st = st?;
    emit(&c, &native, &p, &st, &io, &res)
}

/// Emits a program whose earlier stages already ran.
pub fn emit(
    c: &Container,
    native: &NativeProgram,
    p: &ir::Program,
    st: &structure::Structured,
    io: &io::Io,
    res: &resource::Resources,
) -> Result<HostShader, ShaderError> {
    let sig = |fourcc| -> Result<Vec<container::SigElement>, ShaderError> {
        match c.part(fourcc) {
            Some(part) => signature(part.data),
            None => Ok(Vec::new()),
        }
    };
    let pixel = native.stage == HwStage::Ps;
    let mut inputs = sig(FourCC::ISG1)?;
    let mut outputs = sig(FourCC::OSG1)?;
    // Interpolation as DXC declares it by default; pixel inputs that are
    // read get the mode their barycentrics say.
    let interpolated = if pixel { &mut inputs } else { &mut outputs };
    for e in interpolated {
        e.interp = match (e.kind, e.comp) {
            (3, _) => 4,
            // Constant, as DXC declares it (the AMD compiler crashed on a
            // front face without a mode).
            (13, _) => 1,
            (0 | 6 | 7, 9) => 2,
            (0, _) => 1,
            _ => 0,
        };
    }
    // With a conservative depth output (SV_DepthLessEqual, GreaterEqual) DXIL
    // wants the position centroid (or per sample), as DXC declares it then.
    if pixel && outputs.iter().any(|e| matches!(e.kind, 18 | 19)) {
        for e in inputs.iter_mut().filter(|e| e.kind == 3) {
            e.interp = 5;
        }
    }
    // Per-primitive outputs are constant. The console never exports a
    // primitive id or cull flag a mesh shader writes (it culls with the
    // primitive export), so those rows are left out.
    let mut prims = sig(FourCC::PSG1)?;
    prims.retain(|e| !matches!(e.kind, 10 | 30));
    for e in &mut prims {
        e.interp = 1;
    }
    // A mesh group's id arrives multiplied by max(threads, vertices,
    // primitives) (the code divides by that); the thread count is in PSV0
    // from runtime info version 2 on.
    // shortcut: older containers fall back to the group's lane count, right
    // when the HLSL group fills its waves (Grand Theft Auto VI's do).
    let mesh_stride = match &native.io {
        crate::native::StageIo::Ms(ms) => {
            let threads = mesh_group_threads(c.part(FourCC::PSV0).map(|p| p.data))?;
            threads.map_or(ms.threads, |t| {
                t.max(ms.max_vertices).max(ms.max_primitives)
            })
        }
        _ => 0,
    };
    let l = lower::lower(&lower::Parts {
        native,
        p,
        st,
        io,
        res,
        inputs,
        outputs,
        prims,
        mesh_stride,
    })?;
    Ok(HostShader {
        stage: native.stage,
        dxil: l.dxil,
        user_data: l.user_data,
        static_samplers: l
            .samplers
            .iter()
            .enumerate()
            .map(|(n, &w)| {
                let s = StaticSampler::from_words(w);
                if l.cmp_samplers >> n & 1 != 0 {
                    s.comparison()
                } else {
                    s
                }
            })
            .collect(),
        samplers: l.samplers,
        root_cbvs: l.root_cbvs,
        root_srvs: l.root_srvs,
    })
}

/// `D3D12_MS_THREAD_GROUP_MAX_THREAD_COUNT`: a mesh group's `numthreads`.
const MESH_GROUP_THREADS: u32 = 128;

/// The threads of a mesh program's HLSL group, from `PSV0` runtime info
/// version 2 on (`None` before it): `NumThreadsX * Y * Z`.
///
/// The group id arrives multiplied by the whole group's thread count, so a
/// `numthreads(8, 8, 1)` group strides by 64. Reading `NumThreadsX` alone
/// strode by 8 and gave groups ids that overlap. The three sizes are raw
/// `u32`s of the title's part; a group D3D12 cannot run (an empty axis, or
/// more than 128 threads) is refused rather than multiplied.
fn mesh_group_threads(psv: Option<&[u8]>) -> Result<Option<u32>, ShaderError> {
    use crate::dxbc::u32_at;
    let Some(d) = psv.filter(|d| u32_at(d, 0).is_ok_and(|n| n >= 48)) else {
        return Ok(None);
    };
    let mut total = 1u32;
    for k in 0..3 {
        let n = u32_at(d, 4 + 36 + 4 * k)?;
        total = total
            .checked_mul(n)
            .filter(|&t| n != 0 && t <= MESH_GROUP_THREADS)
            .ok_or(ShaderError::Malformed(
                "a mesh thread group past D3D12's limits",
            ))?;
    }
    Ok(Some(total))
}

/// The registers D3D12 links one stage's signature to the next through.
const SIGNATURE_REGISTERS: u32 = 32;

/// `D3D12_SIMULTANEOUS_RENDER_TARGET_COUNT`: an `SV_Target` names one of them.
const RENDER_TARGETS: u32 = 8;

/// The elements a signature part may list. `PSV0` counts each signature's
/// elements in a byte, and lowering can add two input rows of its own (the
/// vertex and instance ids the console signature leaves out).
const SIGNATURE_ELEMENTS: usize = u8::MAX as usize - 2;

/// The longest semantic name DXC's validator accepts.
const SEMANTIC_LEN: usize = 64;

/// The rows of an `ISG1`/`OSG1` part.
fn signature(part: &[u8]) -> Result<Vec<container::SigElement>, ShaderError> {
    use crate::dxbc::u32_at;
    let count = u32_at(part, 0)? as usize;
    let base = u32_at(part, 4)? as usize;
    // Rows are 32 bytes each from `base`; a count the part cannot hold
    // would otherwise reserve room for up to 2^32 rows (hundreds of gigabytes) before
    // the first row is read, and the failed allocation aborts the process.
    if count
        .checked_mul(32)
        .and_then(|rows| rows.checked_add(base))
        .is_none_or(|end| end > part.len())
    {
        return Err(ShaderError::Malformed("signature rows out of bounds"));
    }
    // The count is a raw `u32` of the title's part. Past what `PSV0` can
    // count, the emitted container's byte wrapped (256 rows counted as 0)
    // and disagreed with its own `ISG1`/`OSG1`, so D3D12 refused it.
    if count > SIGNATURE_ELEMENTS {
        return Err(ShaderError::Malformed(
            "signature with more elements than PSV0 counts",
        ));
    }
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        let at = base + 32 * i;
        let name_at = u32_at(part, at + 4)? as usize;
        // The name is a NUL-terminated string inside the part. One with no
        // terminator ran to the end of the part, taking whatever followed
        // (other names, the next part's padding) as the semantic.
        let name = part
            .get(name_at..)
            .and_then(|s| s.iter().position(|&b| b == 0).map(|end| &s[..end]))
            .ok_or(ShaderError::Malformed("signature name out of bounds"))?;
        // The name goes into the emitted signature metadata as it is, and
        // DXC's validator refuses a semantic outside 1 to 64 characters
        // (`Meta.SemanticLen`), so the pipeline was lost after translation.
        if !(1..=SEMANTIC_LEN).contains(&name.len()) {
            return Err(ShaderError::Malformed(
                "signature name outside 1 to 64 characters",
            ));
        }
        // HLSL semantics are identifiers. The name is raw bytes of the
        // title's part, and a byte outside printable ASCII went through a
        // lossy UTF-8 conversion: each invalid byte became U+FFFD, three
        // bytes, so a name that passed the length check above reached the
        // validator past 64 characters, and two different names became one
        // semantic (one refused as listed twice, or a link to the wrong row).
        if !name.iter().all(u8::is_ascii_graphic) {
            return Err(ShaderError::Malformed(
                "signature name outside printable ASCII",
            ));
        }
        let mask = *part
            .get(at + 24)
            .ok_or(ShaderError::Malformed("truncated"))?;
        // A row has four components, and an element fills a run of them.
        // The mask is a raw byte of the title's part: one with bits past w
        // counted up to eight columns (`1u8 << 8` panicked building the
        // container's mask in debug builds, and reads spilled into the next
        // row), and a gap (x and z) was taken for the run x and y.
        let run = mask >> mask.trailing_zeros().min(7);
        if mask > 0xF || run & run.wrapping_add(1) != 0 {
            return Err(ShaderError::Malformed(
                "signature mask is not a run of a row's four components",
            ));
        }
        if u32_at(part, at + 28)? != 0 {
            return Err(ShaderError::Unsupported {
                offset: 0,
                what: "a minimum-precision signature element",
            });
        }
        if u32_at(part, at)? != 0 {
            return Err(ShaderError::Unsupported {
                offset: 0,
                what: "a signature element of another stream",
            });
        }
        let kind = match u32_at(part, at + 12)? {
            0 => 0,
            1 => 3,
            2 => 6,
            3 => 7,
            4 => 4,
            5 => 5,
            6 => 1,
            7 => 10,
            8 => 2,
            9 => 13,
            10 => 12,
            64 => 16,
            65 => 17,
            66 => 14,
            67 => 19,
            68 => 18,
            69 => 20,
            70 => 15,
            25 => 30, // cull primitive
            _ => {
                return Err(ShaderError::Unsupported {
                    offset: 0,
                    what: "a system value in the signature",
                });
            }
        };
        // 16-bit elements are declared 32-bit; reading or writing one
        // converts (16-bit values travel packed on the console).
        let raw = u32_at(part, at + 16)?;
        let comp = match (kind, raw) {
            // DXIL declares the front face a uint (the validator insists).
            (13, _) => 5,
            (_, 1 | 4) => 5,
            (_, 2 | 5) => 4,
            (_, 3 | 6) => 9,
            (_, c) => {
                return Err(ShaderError::Unsupported {
                    offset: 0,
                    what: match c {
                        0 => "a signature element of unknown type",
                        _ => "a 64-bit signature element",
                    },
                });
            }
        };
        // D3D12 links stages through 32 registers. The register is a raw
        // `u32` of the title's part, and the container multiplies the row by
        // four and writes a dependency mask for every input scalar, so a
        // register far past them overflowed (a panic in debug builds) or
        // asked for gigabytes of masks.
        let register = u32_at(part, at + 20)?;
        if register != u32::MAX && register >= SIGNATURE_REGISTERS {
            return Err(ShaderError::Malformed(
                "signature register past the 32 D3D12 has",
            ));
        }
        // An `SV_Target`'s register is the render target it writes. The
        // pipeline description has eight format slots, indexed by it; a
        // target past them read past `RTVFormats` (a panic), and no colour
        // export reaches it.
        if kind == 16 && register != u32::MAX && register >= RENDER_TARGETS {
            return Err(ShaderError::Malformed("render target past the 8 D3D12 has"));
        }
        // An `SV_Target`'s semantic index names the render target in the
        // emitted metadata, while the pipeline description and lowering take
        // the register. Both are raw fields of the title's row: a target
        // without a register went into format slot 0 and no export reached
        // it, and one whose index disagreed with its register was refused by
        // DXC's validator (`SM.PSTargetIndexMatchesRow`), losing the pipeline.
        if kind == 16 && (register == u32::MAX || register != u32_at(part, at + 8)?) {
            return Err(ShaderError::Malformed(
                "render target index disagrees with its register",
            ));
        }
        // Each component of a register belongs to one element. Lowering
        // finds an element by its register and component, so a second one
        // over the same components was never read or written, and D3D12
        // refuses a signature whose elements overlap.
        let row = (register != u32::MAX).then_some(register);
        let (col, cols) = (
            mask.trailing_zeros().min(3) as u8,
            mask.count_ones().max(1) as u8,
        );
        if row.is_some()
            && out.iter().any(|e: &container::SigElement| {
                e.row == row && col < e.col + e.cols && e.col < col + cols
            })
        {
            return Err(ShaderError::Malformed(
                "two signature elements in one register component",
            ));
        }
        // A semantic (name and index, the name without regard to case) names
        // one element. The next stage links by it, and D3D12 refuses a
        // signature that lists one twice, so the pipeline was lost; a second
        // element in another register passed every check above.
        let name = String::from_utf8(name.to_vec()).expect("checked ASCII above");
        let index = u32_at(part, at + 8)?;
        if out
            .iter()
            .any(|e| e.index == index && e.name.eq_ignore_ascii_case(&name))
        {
            return Err(ShaderError::Malformed(
                "two signature elements with one semantic",
            ));
        }
        // A system value other than a target, clip or cull distance is one
        // value of the stage: there is one position, one depth, one front
        // face. The kind comes from the row's raw system value, not its
        // name, so a second row (under another name) passed the semantic
        // check above, and DXC's validator refuses a signature listing one
        // twice (`Meta.DuplicateSysValue`).
        if !matches!(kind, 0 | 6 | 7 | 16) && out.iter().any(|e| e.kind == kind) {
            return Err(ShaderError::Malformed(
                "a system value listed twice in a signature",
            ));
        }
        out.push(container::SigElement {
            name,
            index,
            kind,
            comp,
            interp: 0,
            row,
            col,
            cols,
            usage: 0,
            half: (4..=6).contains(&raw),
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::lower::LOOP_LIMIT;
    use super::parse_loop_limit;

    #[test]
    fn the_loop_limit_switch_parses_or_falls_back() {
        assert_eq!(parse_loop_limit(None), LOOP_LIMIT);
        assert_eq!(parse_loop_limit(Some("4096")), 4096);
        assert_eq!(parse_loop_limit(Some(" 0x1000 ")), 4096);
        assert_eq!(parse_loop_limit(Some("1")), 1);
        for bad in ["", "0", "-5", "lots", "0x", "4294967296"] {
            assert_eq!(parse_loop_limit(Some(bad)), LOOP_LIMIT, "{bad:?}");
        }
    }

    /// The switch's edges: the largest value in either base, a leading
    /// plus, leading zeros read as decimal (not octal), and surrounding
    /// tabs and newlines trimmed. The prefix is lower-case `0x` only, a
    /// hex zero is still zero, and a sign or space after the prefix is no
    /// number.
    #[test]
    fn the_loop_limit_switch_at_its_edges() {
        assert_eq!(parse_loop_limit(Some("4294967295")), u32::MAX);
        assert_eq!(parse_loop_limit(Some("0xFFFFFFFF")), u32::MAX);
        assert_eq!(parse_loop_limit(Some("0xffff")), 0xFFFF);
        assert_eq!(parse_loop_limit(Some("+5")), 5);
        assert_eq!(parse_loop_limit(Some("010")), 10);
        assert_eq!(parse_loop_limit(Some("\t64\n")), 64);
        for bad in ["0X10", "0x0", "0x100000000", "0x-5", "0x 10", "1 0", "1e3"] {
            assert_eq!(parse_loop_limit(Some(bad)), LOOP_LIMIT, "{bad:?}");
        }
    }
    use super::container::SigElement;
    use super::*;
    use crate::native::{ComputeIo, StageIo};

    /// A `PSV0` part with runtime info version 2: `[x, y, z]` at 36.
    fn psv_with_group(size: u32, [x, y, z]: [u32; 3]) -> Vec<u8> {
        let mut d = size.to_le_bytes().to_vec();
        d.resize(4 + 36, 0);
        for n in [x, y, z] {
            d.extend(n.to_le_bytes());
        }
        d
    }

    fn shader_reading(indices: &[u32]) -> HostShader {
        HostShader {
            stage: HwStage::Ps,
            dxil: Vec::new(),
            user_data: indices
                .iter()
                .map(|&index| UserData {
                    index,
                    slot: 0,
                    root: RootType::Constants,
                    dword: index,
                })
                .collect(),
            samplers: Vec::new(),
            static_samplers: Vec::new(),
            root_cbvs: Vec::new(),
            root_srvs: Vec::new(),
        }
    }

    /// The user-data buffer reaches the highest dword read, rounded up to
    /// a whole vector of four: dword 3 still fits in 4, dword 4 needs 8.
    /// Only the highest index counts, in any order, read once or twice,
    /// and a shader that reads none declares no buffer.
    #[test]
    fn user_data_dwords_round_the_highest_index_up_to_four() {
        for (indices, dwords) in [
            (&[][..], 0),
            (&[0][..], 4),
            (&[3][..], 4),
            (&[4][..], 8),
            (&[7, 0, 2][..], 8),
            (&[8, 8][..], 12),
            (&[1, 15, 15, 9][..], 16),
            (&[63][..], 64),
        ] {
            assert_eq!(
                shader_reading(indices).user_data_dwords(),
                dwords,
                "{indices:?}"
            );
        }
    }

    /// [`translate`] runs after the container parses: one without a `DXIL`
    /// part is refused before native code is read.
    #[test]
    fn translate_refuses_a_container_with_no_dxil_part() {
        use crate::ShaderError;
        use crate::dxbc::{build, FourCC};
        let bytes = build(&[(FourCC::SFI0, &[0; 8]), (FourCC::HASH, &[0; 20])]);
        assert_eq!(
            translate(&bytes).unwrap_err(),
            ShaderError::Malformed("no DXIL part")
        );
    }

    /// A PC-layout `DXIL` part with no `XDAT` native entry is not one of the
    /// stage programs [`translate`] looks for.
    #[test]
    fn translate_refuses_a_dxil_part_with_no_native_program() {
        use crate::ShaderError;
        use crate::dxbc::{build, FourCC};
        use crate::dxil::build_standard;
        let bitcode = [0x42, 0x43, 0xC0, 0xDE, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        let dxil = build_standard(0, (6, 0), 0x100, &bitcode);
        let bytes = build(&[(FourCC::DXIL, &dxil[..])]);
        assert_eq!(
            translate(&bytes).unwrap_err(),
            ShaderError::Unsupported {
                offset: 0,
                what: "a container without a vertex, pixel, compute or mesh program",
            }
        );
    }

    /// A mesh group id strides by the HLSL group's whole thread count, not
    /// its x size alone: `numthreads(8, 8, 1)` is 64 threads. A group
    /// D3D12 cannot run is refused before its sizes are multiplied.
    #[test]
    fn a_mesh_group_counts_every_axis() {
        for (group, threads) in [
            ([32, 1, 1], 32),
            ([8, 8, 1], 64),
            ([4, 4, 8], 128),
            ([1, 1, 128], 128),
            ([1, 1, 1], 1),
        ] {
            let psv = psv_with_group(48, group);
            assert_eq!(
                mesh_group_threads(Some(&psv)),
                Ok(Some(threads)),
                "{group:?}"
            );
        }
        let refused = Err(ShaderError::Malformed(
            "a mesh thread group past D3D12's limits",
        ));
        for group in [
            [0, 1, 1],
            [1, 0, 1],
            [1, 1, 0],
            [129, 1, 1],
            [16, 16, 1],
            [65536, 65536, 1],
            [u32::MAX, u32::MAX, u32::MAX],
        ] {
            let psv = psv_with_group(48, group);
            assert_eq!(mesh_group_threads(Some(&psv)), refused, "{group:?}");
        }
        // Runtime info before version 2 has no group size; nor does no part.
        assert_eq!(
            mesh_group_threads(Some(&psv_with_group(36, [0; 3]))),
            Ok(None)
        );
        assert_eq!(mesh_group_threads(Some(&[])), Ok(None));
        assert_eq!(mesh_group_threads(None), Ok(None));
        // A version-2 size with the sizes cut off is malformed.
        let cut = &psv_with_group(48, [8, 8, 1])[..4 + 40];
        assert!(mesh_group_threads(Some(cut)).is_err());
    }

    /// Only the runtime info size in `PSV0`'s first word decides whether
    /// the group sizes are read: any size below version 2's 48 bytes leaves
    /// the stride to the mesh header, while 48 or more (later versions grow
    /// the struct) reads them, and then the sizes must be in the part.
    #[test]
    fn the_runtime_info_size_gates_the_group_read() {
        for size in [0, 1, 36, 40, 47] {
            let psv = psv_with_group(size, [8, 8, 1]);
            assert_eq!(mesh_group_threads(Some(&psv)), Ok(None), "size {size}");
            // Bytes past an older struct are not read, even garbage ones.
            let psv = psv_with_group(size, [0, u32::MAX, 0]);
            assert_eq!(mesh_group_threads(Some(&psv)), Ok(None), "size {size}");
        }
        for size in [48, 52, 4096, u32::MAX] {
            let psv = psv_with_group(size, [8, 8, 1]);
            assert_eq!(mesh_group_threads(Some(&psv)), Ok(Some(64)), "size {size}");
        }
        // A size word alone, or one cut inside it, says nothing to read.
        assert_eq!(mesh_group_threads(Some(&[48, 0, 0])), Ok(None));
        let refused = |d: &[u8]| mesh_group_threads(Some(d)).is_err();
        assert!(refused(&48u32.to_le_bytes()));
        let full = psv_with_group(48, [8, 8, 1]);
        for cut in [4 + 36, 4 + 36 + 3, 4 + 36 + 4, 4 + 36 + 11] {
            assert!(refused(&full[..cut]), "cut at {cut}");
        }
        // Bytes past the z size are never needed.
        let mut long = full.clone();
        long.extend([0xFF; 16]);
        assert_eq!(mesh_group_threads(Some(&long)), Ok(Some(64)));
    }

    /// The refusal is checked as the product grows, so a zero axis after an
    /// axis already past 128 is refused for the size, and a product that
    /// lands exactly on 128 through every axis still runs.
    #[test]
    fn the_group_limit_is_checked_axis_by_axis() {
        for (group, threads) in [
            ([2, 64, 1], 128),
            ([2, 2, 32], 128),
            ([128, 1, 1], 128),
            ([1, 128, 1], 128),
            ([3, 5, 7], 105),
        ] {
            let psv = psv_with_group(48, group);
            assert_eq!(
                mesh_group_threads(Some(&psv)),
                Ok(Some(threads)),
                "{group:?}"
            );
        }
        for group in [
            [129, 0, 1],
            [2, 65, 1],
            [2, 2, 33],
            [1, 129, 0],
            [0, 0, 0],
            [0x8000_0000, 2, 1],
            [3, 5, 9],
        ] {
            let psv = psv_with_group(48, group);
            assert!(mesh_group_threads(Some(&psv)).is_err(), "{group:?}");
        }
    }

    /// An `ISG1`/`OSG1` part whose row count it cannot hold is refused
    /// before anything is reserved for the rows. A count of 2^32 - 1 asked
    /// for hundreds of gigabytes up front, and the failed allocation took
    /// the whole process down.
    #[test]
    fn a_signature_row_count_past_the_part_is_refused() {
        // Header (count, offset of the rows), one 32-byte row, its name.
        let part = |count: u32, base: u32| {
            let mut p = Vec::new();
            for w in [count, base, 0, 40, 0, 0, 3, 1, 0x0F, 0] {
                p.extend_from_slice(&u32::to_le_bytes(w));
            }
            p.extend_from_slice(b"TEXCOORD\0");
            p
        };
        let rows = signature(&part(1, 8)).expect("one row inside the part");
        assert_eq!(rows.len(), 1);
        assert_eq!(
            (rows[0].name.as_str(), rows[0].row, rows[0].cols),
            ("TEXCOORD", Some(1), 4)
        );
        assert!(signature(&part(0, 8)).unwrap().is_empty());
        let refused = ShaderError::Malformed("signature rows out of bounds");
        for (count, base) in [
            (u32::MAX, 8),
            (0x0800_0000, 8),
            (2, 8),
            (1, 30),
            (1, u32::MAX),
        ] {
            assert_eq!(
                signature(&part(count, base)).unwrap_err().to_string(),
                refused.to_string(),
                "count {count:#x}, rows at {base:#x}"
            );
        }
    }

    /// A signature register is a raw `u32` of the title's part. One past
    /// D3D12's 32 is refused; a register of 2^30 used to overflow the
    /// container's `row * 4` (a panic in debug builds), and smaller ones
    /// sized the dependency masks by the register rather than the rows.
    #[test]
    fn a_signature_register_past_32_is_refused() {
        let part = |register: u32| {
            let mut p = Vec::new();
            for w in [1, 8, 0, 40, 0, 0, 3, register, 0x0F, 0] {
                p.extend_from_slice(&u32::to_le_bytes(w));
            }
            p.extend_from_slice(b"TEXCOORD\0");
            p
        };
        for (register, row) in [(0, Some(0)), (31, Some(31)), (u32::MAX, None)] {
            let rows = signature(&part(register)).expect("a register D3D12 has");
            assert_eq!(rows[0].row, row, "register {register:#x}");
        }
        let refused = ShaderError::Malformed("signature register past the 32 D3D12 has");
        for register in [32, 255, 256, 0x4000_0000, u32::MAX - 1] {
            assert_eq!(
                signature(&part(register)).unwrap_err().to_string(),
                refused.to_string(),
                "register {register:#x}"
            );
        }
    }

    /// `PSV0` counts a signature's elements in a byte. A part listing more
    /// than fit (with room for the two rows lowering may add) is refused,
    /// rather than wrapping the emitted count; up to the limit it parses.
    #[test]
    fn a_signature_past_psv0s_element_count_is_refused() {
        // `count` rows without a register, all sharing one name after them.
        let part = |count: u32| {
            let base = 8;
            let name = base + 32 * count;
            let mut p = Vec::new();
            for w in [count, base] {
                p.extend_from_slice(&u32::to_le_bytes(w));
            }
            for index in 0..count {
                for w in [0, name, index, 0, 3, u32::MAX, 0x0F, 0] {
                    p.extend_from_slice(&u32::to_le_bytes(w));
                }
            }
            p.extend_from_slice(b"TEXCOORD\0");
            p
        };
        let limit = SIGNATURE_ELEMENTS as u32;
        assert_eq!(limit, 253);
        for count in [1, limit] {
            let rows = signature(&part(count)).expect("a count PSV0 can hold");
            assert_eq!(rows.len(), count as usize);
            assert!(rows.iter().all(|r| r.row.is_none() && r.name == "TEXCOORD"));
        }
        let refused = ShaderError::Malformed("signature with more elements than PSV0 counts");
        for count in [limit + 1, 255, 256, 300] {
            assert_eq!(
                signature(&part(count)).unwrap_err(),
                refused,
                "{count} rows"
            );
        }
    }

    /// An `SV_Target` names one of D3D12's eight render targets by its
    /// register; one past them is refused before the pipeline description
    /// indexes its eight format slots with it. Other elements keep the 32.
    #[test]
    fn a_render_target_past_8_is_refused() {
        let part = |sv: u32, register: u32| {
            let mut p = Vec::new();
            for w in [1, 8, 0, 40, register, sv, 3, register, 0x0F, 0] {
                p.extend_from_slice(&u32::to_le_bytes(w));
            }
            p.extend_from_slice(b"SV_Target\0");
            p
        };
        for register in [0, 7] {
            let rows = signature(&part(64, register)).expect("a render target D3D12 has");
            assert_eq!((rows[0].kind, rows[0].row), (16, Some(register)));
        }
        let refused = ShaderError::Malformed("render target past the 8 D3D12 has");
        for register in [8, 31] {
            assert_eq!(
                signature(&part(64, register)).unwrap_err().to_string(),
                refused.to_string(),
                "register {register}"
            );
        }
        assert_eq!(signature(&part(0, 8)).unwrap()[0].row, Some(8));
    }

    /// An `SV_Target`'s semantic index and its register both name the
    /// render target, so they must agree, and a target needs a register.
    /// Other elements keep any index.
    #[test]
    fn a_render_target_index_matches_its_register() {
        let part = |sv: u32, index: u32, register: u32| {
            let mut p = Vec::new();
            for w in [1, 8, 0, 40, index, sv, 3, register, 0x0F, 0] {
                p.extend_from_slice(&u32::to_le_bytes(w));
            }
            p.extend_from_slice(b"SV_Target\0");
            p
        };
        for n in [0, 3, 7] {
            let rows = signature(&part(64, n, n)).expect("a target named twice alike");
            assert_eq!((rows[0].index, rows[0].row), (n, Some(n)));
        }
        let refused = ShaderError::Malformed("render target index disagrees with its register");
        for (index, register) in [(1, 0), (0, 1), (7, 6), (8, 0), (u32::MAX, 0), (0, u32::MAX)] {
            assert_eq!(
                signature(&part(64, index, register))
                    .unwrap_err()
                    .to_string(),
                refused.to_string(),
                "index {index}, register {register}"
            );
        }
        // Other elements keep their index whatever their register.
        assert_eq!(signature(&part(0, 5, 2)).unwrap()[0].index, 5);
        assert_eq!(signature(&part(0, 1, u32::MAX)).unwrap()[0].row, None);
    }

    /// A system value other than a target, clip or cull distance names one
    /// value of the stage, so a second row with it is refused even under
    /// another name. Targets, clip and cull distances and arbitrary
    /// elements may each appear more than once.
    #[test]
    fn a_system_value_listed_twice_is_refused() {
        let part = |rows: &[(u32, &[u8])]| {
            let mut p = Vec::new();
            p.extend_from_slice(&u32::to_le_bytes(rows.len() as u32));
            p.extend_from_slice(&u32::to_le_bytes(8));
            let names = 8 + 32 * rows.len() as u32;
            for (i, (sv, _)) in rows.iter().enumerate() {
                let i = i as u32;
                for w in [0, names + 32 * i, i, *sv, 3, i, 0x0F, 0] {
                    p.extend_from_slice(&u32::to_le_bytes(w));
                }
            }
            for (_, name) in rows {
                let mut n = name.to_vec();
                n.resize(32, 0);
                p.extend_from_slice(&n);
            }
            p
        };
        for rows in [
            &[(64, &b"SV_Target"[..]), (64, b"SV_Target")][..],
            &[(2, b"SV_ClipDistance"), (2, b"SV_ClipDistance")],
            &[(3, b"SV_CullDistance"), (3, b"SV_CullDistance")],
            &[(0, b"TEXCOORD"), (0, b"COLOR")],
            &[(1, b"SV_Position"), (0, b"TEXCOORD")],
        ] {
            let read = signature(&part(rows)).expect("a signature D3D12 takes");
            assert_eq!(read.len(), 2);
        }
        let refused = ShaderError::Malformed("a system value listed twice in a signature");
        for (sv, a, b) in [
            (1, &b"SV_Position"[..], &b"POS"[..]),
            (1, b"SV_Position", b"SV_Position"),
            (6, b"SV_RenderTargetArrayIndex", b"LAYER"),
            (9, b"SV_IsFrontFace", b"FACE"),
        ] {
            assert_eq!(
                signature(&part(&[(sv, a), (sv, b)]))
                    .unwrap_err()
                    .to_string(),
                refused.to_string(),
                "system value {sv}"
            );
        }
    }

    /// A signature mask is a raw byte of the title's part. One with bits
    /// past w, or with a gap, is refused: `0xFF` counted eight columns and
    /// building the container's mask shifted a `u8` by eight.
    #[test]
    fn a_signature_mask_outside_one_row_is_refused() {
        let part = |mask: u32| {
            let mut p = Vec::new();
            for w in [1, 8, 0, 40, 0, 0, 3, 1, mask, 0] {
                p.extend_from_slice(&u32::to_le_bytes(w));
            }
            p.extend_from_slice(b"TEXCOORD\0");
            p
        };
        for (mask, col, cols) in [
            (0x1, 0, 1),
            (0x8, 3, 1),
            (0x6, 1, 2),
            (0xE, 1, 3),
            (0xF, 0, 4),
            // The read-write mask in the next byte is not the element's.
            (0x0F03, 0, 2),
        ] {
            let rows = signature(&part(mask)).expect("a run inside the row");
            assert_eq!((rows[0].col, rows[0].cols), (col, cols), "mask {mask:#x}");
            assert!(rows[0].col + rows[0].cols <= 4, "mask {mask:#x}");
        }
        let refused =
            ShaderError::Malformed("signature mask is not a run of a row's four components");
        for mask in [0x10, 0x1F, 0x80, 0xFF, 0x5, 0x9, 0xB, 0xD] {
            assert_eq!(
                signature(&part(mask)).unwrap_err().to_string(),
                refused.to_string(),
                "mask {mask:#x}"
            );
        }
    }

    /// Two elements over the same components of one register are refused:
    /// lowering finds an element by register and component, so the second
    /// was never read or written. Elements in one register's other
    /// components, or without a register, still share it.
    #[test]
    fn signature_elements_sharing_a_component_are_refused() {
        // One 32-byte row per `(register, mask)`, all named after the rows.
        let part = |rows: &[(u32, u32)]| {
            let name = 8 + 32 * rows.len() as u32;
            let mut p = Vec::new();
            for w in [rows.len() as u32, 8] {
                p.extend_from_slice(&u32::to_le_bytes(w));
            }
            for (index, &(register, mask)) in rows.iter().enumerate() {
                for w in [0, name, index as u32, 0, 3, register, mask, 0] {
                    p.extend_from_slice(&u32::to_le_bytes(w));
                }
            }
            p.extend_from_slice(b"TEXCOORD\0");
            p
        };
        for rows in [
            &[(0, 0x3), (0, 0xC)][..],
            &[(0, 0x1), (0, 0x2), (0, 0x4), (0, 0x8)],
            &[(0, 0xF), (1, 0xF)],
            &[(0, 0x7), (1, 0x7), (0, 0x8)],
            &[(u32::MAX, 0x1), (u32::MAX, 0x1)],
        ] {
            let parsed = signature(&part(rows)).expect("disjoint components");
            assert_eq!(parsed.len(), rows.len(), "{rows:x?}");
        }
        let refused = ShaderError::Malformed("two signature elements in one register component");
        for rows in [
            &[(0, 0xF), (0, 0xF)][..],
            &[(0, 0x3), (0, 0x6)],
            &[(0, 0x1), (0, 0xF)],
            &[(2, 0x8), (1, 0xF), (2, 0xC)],
            &[(31, 0xE), (31, 0x2)],
        ] {
            assert_eq!(signature(&part(rows)).unwrap_err(), refused, "{rows:x?}");
        }
    }

    /// A semantic name and index name one element. A second element with
    /// them (the name in any case), in another register or none, is
    /// refused; the same name with another index, or another name with the
    /// same index, still parses.
    #[test]
    fn a_semantic_listed_twice_is_refused() {
        // One 32-byte row per `(name, index, register)`, names after the rows.
        let part = |rows: &[(&str, u32, u32)]| {
            let mut names = Vec::new();
            let mut p = Vec::new();
            for w in [rows.len() as u32, 8] {
                p.extend_from_slice(&u32::to_le_bytes(w));
            }
            for &(name, index, register) in rows {
                let at = 8 + 32 * rows.len() + names.len();
                names.extend_from_slice(name.as_bytes());
                names.push(0);
                for w in [0, at as u32, index, 0, 3, register, 0xF, 0] {
                    p.extend_from_slice(&u32::to_le_bytes(w));
                }
            }
            p.extend_from_slice(&names);
            p
        };
        for rows in [
            &[("TEXCOORD", 0, 0), ("TEXCOORD", 1, 1)][..],
            &[("TEXCOORD", 0, 0), ("COLOR", 0, 1)],
            &[("TEXCOORD", 0, 0), ("TEXCOORD0", 0, 1)],
            &[("A", 7, u32::MAX), ("A", 8, u32::MAX)],
        ] {
            let parsed = signature(&part(rows)).expect("distinct semantics");
            let named: Vec<_> = parsed.iter().map(|e| (e.name.as_str(), e.index)).collect();
            let want: Vec<_> = rows.iter().map(|r| (r.0, r.1)).collect();
            assert_eq!(named, want);
        }
        let refused = ShaderError::Malformed("two signature elements with one semantic");
        for rows in [
            &[("TEXCOORD", 0, 0), ("TEXCOORD", 0, 1)][..],
            &[("TEXCOORD", 3, 0), ("texcoord", 3, 1)],
            &[("COLOR", 0, 0), ("TEXCOORD", 0, 1), ("Color", 0, 2)],
            &[("A", 7, u32::MAX), ("A", 7, u32::MAX)],
            &[("SV_Position", 0, 0), ("SV_POSITION", 0, 1)],
        ] {
            assert_eq!(signature(&part(rows)).unwrap_err(), refused, "{rows:?}");
        }
    }

    /// A signature name is NUL-terminated inside the part; one that runs
    /// off the end is refused rather than taking the rest of the part.
    #[test]
    fn a_signature_name_without_its_terminator_is_refused() {
        let part = |name: &[u8]| {
            let mut p = Vec::new();
            for w in [1, 8, 0, 40, 0, 0, 3, 1, 0x3, 0] {
                p.extend_from_slice(&u32::to_le_bytes(w));
            }
            p.extend_from_slice(name);
            p
        };
        let rows = signature(&part(b"TEXCOORD\0")).expect("a terminated name");
        assert_eq!(rows[0].name, "TEXCOORD");
        for name in [&b"TEXCOORD"[..], b""] {
            assert_eq!(
                signature(&part(name)).unwrap_err().to_string(),
                ShaderError::Malformed("signature name out of bounds").to_string(),
                "name {name:?}"
            );
        }
    }

    /// DXC's validator refuses a semantic name that is empty or longer
    /// than 64 characters, so one is refused while the part is read.
    #[test]
    fn a_signature_name_outside_1_to_64_characters_is_refused() {
        let part = |name: &[u8]| {
            let mut p = Vec::new();
            for w in [1, 8, 0, 40, 0, 0, 3, 1, 0x3, 0] {
                p.extend_from_slice(&u32::to_le_bytes(w));
            }
            p.extend_from_slice(name);
            p.push(0);
            p
        };
        let longest = vec![b'A'; 64];
        let rows = signature(&part(&longest)).expect("a 64-character name");
        assert_eq!(rows[0].name.len(), 64);
        assert_eq!(signature(&part(b"T")).unwrap()[0].name, "T");
        for name in [&b""[..], &[b'A'; 65][..]] {
            assert_eq!(
                signature(&part(name)).unwrap_err().to_string(),
                ShaderError::Malformed("signature name outside 1 to 64 characters").to_string(),
                "name of {} characters",
                name.len()
            );
        }
    }

    /// A semantic name with a byte outside printable ASCII is refused. A
    /// lossy UTF-8 conversion used to turn each invalid byte into three, so
    /// 64 bytes of `0xFF` became a 192-byte name the validator refuses, and
    /// `0xFE` and `0xFF` became the same semantic.
    #[test]
    fn a_signature_name_outside_printable_ascii_is_refused() {
        let part = |name: &[u8]| {
            let mut p = Vec::new();
            for w in [1, 8, 0, 40, 0, 0, 3, 1, 0x3, 0] {
                p.extend_from_slice(&u32::to_le_bytes(w));
            }
            p.extend_from_slice(name);
            p.push(0);
            p
        };
        for name in [&b"TEXCOORD"[..], b"SV_Position", b"_a1~!"] {
            assert_eq!(signature(&part(name)).unwrap()[0].name.as_bytes(), name);
        }
        for name in [
            &[0xFF; 64][..],
            &[0xFE][..],
            b"TEX\x80COORD",
            "T\u{E9}XCOORD".as_bytes(),
            b"TEX COORD",
            b"TEX\tCOORD",
            b"\x7F",
        ] {
            assert_eq!(
                signature(&part(name)).unwrap_err().to_string(),
                ShaderError::Malformed("signature name outside printable ASCII").to_string(),
                "name {name:?}"
            );
        }
    }

    /// A pixel shader writing SV_StencilRef (channel y of `exp mrtz`).
    /// DXC sets ShaderFlags.StencilRef (bit 11) for a pixel shader whose
    /// output signature has SV_StencilRef, and the feature bit
    /// D3D_SHADER_FEATURE_STENCIL_REF (0x200) in SFI0; the validator checks
    /// both against the module.
    #[test]
    fn a_stencil_export_declares_the_stencil_ref_feature() {
        let words = [
            0x7E00_02F2u32, // v_mov_b32 v0, 1.0
            0x7E02_0285,    // v_mov_b32 v1, 5
            0xF800_1883,
            0x0000_0100, // exp mrtz v0, v1, off, off done vm
            0xBF81_0000,
        ];
        let native = NativeProgram {
            stage: HwStage::Ps,
            wave64: false,
            vgprs: 2,
            layout: vec![],
            used: vec![],
            io: StageIo::Ps {
                inputs: vec![],
                input_addr: 0,
                db_shader_control: 0,
                patches: vec![],
            },
            code: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
        };
        let el = |name: &str, kind, comp| SigElement {
            name: name.into(),
            index: 0,
            kind,
            comp,
            interp: 0,
            row: None,
            col: 0,
            cols: 1,
            usage: 1,
            half: false,
        };
        let osg1 =
            container::signature(&[el("SV_Depth", 17, 9), el("SV_StencilRef", 20, 5)], false);
        let bytes = crate::dxbc::build(&[(FourCC::OSG1, &osg1)]);
        let c = Container::parse(&bytes).unwrap();
        let p = ir::build(&native.patched_code(), native.wave64, native.vgprs).unwrap();
        let st = structure::structure(&p).unwrap();
        let res = resource::track(&p, &native.layout, native.stage, Some(&st)).unwrap();
        let io = io::raise(&p, &native).unwrap();
        let out = emit(&c, &native, &p, &st, &io, &res).unwrap();
        // dxil.dll, where there is one, checks the flags against the module.
        #[cfg(windows)]
        if let Ok(mut v) = super::validator::Validator::load(
            super::validator::Validator::default_path().as_deref(),
        ) {
            v.validate(&mut out.dxil.clone()).unwrap();
        }
        let host = Container::parse(&out.dxil).unwrap();
        let sfi0 = host.part(FourCC::SFI0).unwrap().data;
        let features = u64::from_le_bytes(sfi0.try_into().unwrap());
        assert_ne!(features & 0x200, 0, "SFI0 {features:#x}: no StencilRef");
    }

    /// A wave64 compute program that writes only EXEC's high dword
    /// (`s_mov_b32 exec_hi, 0`) before a vector instruction. The new mask is
    /// opaque and keyed by the unchanged low dword, so building its lane bit
    /// needed the mask's own value: lowering recursed until the stack
    /// overflowed, which takes the whole process down (found by
    /// random-input testing). Now the instruction fails like any other that
    /// cannot be expressed: its result is missing, and only a side effect
    /// that uses it would stop the translation.
    #[test]
    fn an_exec_high_write_does_not_overflow_the_stack() {
        // s_mov_b32 exec_hi, 0; v_mov_b32 v1, v0; s_endpgm
        let words = [0xBEFF_0380u32, 0x7E02_0300, 0xBF81_0000];
        let native = NativeProgram {
            stage: HwStage::Cs,
            wave64: true,
            vgprs: 2,
            layout: vec![],
            used: vec![],
            io: StageIo::Cs(ComputeIo {
                rsrc2: 0,
                threads: [64, 1, 1],
            }),
            code: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
        };
        let p = ir::build(&native.patched_code(), native.wave64, native.vgprs).unwrap();
        let st = structure::structure(&p).unwrap();
        let res = resource::track(&p, &native.layout, native.stage, Some(&st)).unwrap();
        let io = io::raise(&p, &native).unwrap();
        let container = crate::dxbc::build(&[]);
        let c = Container::parse(&container).unwrap();
        if let Err(e) = emit(&c, &native, &p, &st, &io, &res) {
            assert!(e.to_string().contains("its own value"), "{e}");
        }
    }

    /// A pixel shader writing a constant colour. Every host builds it:
    /// `constant_color_is_a_well_formed_container` checks it without D3D12,
    /// and the Windows tests run it through dxil.dll and a device.
    fn constant_color() -> Vec<u8> {
        use super::bitcode::Attr;
        use super::shader::Shader;

        let mut s = Shader::new(true);
        s.outputs.push(SigElement {
            name: "SV_Target".into(),
            index: 0,
            kind: 16,
            comp: 9,
            interp: 0,
            row: Some(0),
            col: 0,
            cols: 4,
            usage: 15,
            half: false,
        });
        let b = s.entry;
        for (c, v) in [1.0f32, 0.5, 0.25, 1.0].into_iter().enumerate() {
            let args = [
                s.m.i32(0),
                s.m.i32(0),
                s.m.i8(c as u8),
                s.m.f32(v.to_bits()),
            ];
            s.op(b, "storeOutput.f32", Attr::NoUnwind, s.t.void, 5, &args);
        }
        s.m.terminate(b, super::bitcode::Term::Ret);
        s.finish()
    }

    /// What D3D12 and the validator check before reading the bitcode, without
    /// either: the container parses, its size field is its length, its parts
    /// are the ones a pixel shader carries, and the digest is the one
    /// `container::hash` gives for the bytes after it.
    #[test]
    fn constant_color_is_a_well_formed_container() {
        let c = constant_color();
        let parsed = Container::parse(&c).unwrap();
        assert_eq!(crate::dxbc::u32_at(&c, 0x18).unwrap() as usize, c.len());
        let fourccs: Vec<_> = parsed.parts.iter().map(|p| p.fourcc.0).collect();
        assert_eq!(
            fourccs,
            [*b"SFI0", *b"ISG1", *b"OSG1", *b"PSV0", *b"HASH", *b"DXIL"]
        );
        let dxil = parsed.part(crate::dxbc::FourCC(*b"DXIL")).unwrap();
        assert_eq!(&dxil.data[8..12], b"DXIL");
        assert_eq!(parsed.digest, super::container::hash(&c[20..]));
    }

    #[test]
    #[cfg(windows)]
    fn stub_pipeline_is_created() {
        let Ok(host) = super::host::Host::new(false) else {
            eprintln!("no SM 6.6 device; skipped");
            return;
        };
        let ps = super::HostShader {
            stage: crate::HwStage::Ps,
            dxil: constant_color(),
            user_data: Vec::new(),
            samplers: Vec::new(),
            static_samplers: Vec::new(),
            root_cbvs: Vec::new(),
            root_srvs: Vec::new(),
        };
        let file = |var: &str, stage| {
            std::env::var(var).ok().map(|p| super::HostShader {
                stage,
                dxil: std::fs::read(p).unwrap(),
                user_data: Vec::new(),
                samplers: Vec::new(),
                static_samplers: Vec::new(),
                root_cbvs: Vec::new(),
                root_srvs: Vec::new(),
            })
        };
        let ps = file("VICTORY_REF_PS", crate::HwStage::Ps).unwrap_or(ps);
        let vs = file("VICTORY_REF_VS", crate::HwStage::Vs)
            .unwrap_or_else(|| super::host::stub_vs(&ps).unwrap());
        if let Ok(mut v) = super::validator::Validator::load(
            super::validator::Validator::default_path().as_deref(),
        ) {
            v.validate(&mut vs.dxil.clone()).unwrap();
        }
        host.pipeline(&vs, &ps).unwrap();
        // The runtime checks the container hash: no hash or a wrong one is
        // refused. (Recent runtimes also take sixteen 0x01 bytes, a bypass
        // value; ours works with older ones too.)
        let mut wrong = ps.dxil[4..20].to_vec();
        wrong[0] ^= 1;
        for digest in [&[0u8; 16][..], &wrong] {
            let mut other = ps.clone();
            other.dxil[4..20].copy_from_slice(digest);
            assert!(
                host.pipeline(&vs, &other).is_err(),
                "digest {digest:02x?} accepted"
            );
        }
    }

    #[test]
    #[cfg(windows)]
    fn constant_color_validates() {
        let mut c = constant_color();
        let path = super::validator::Validator::default_path();
        let Ok(mut v) = super::validator::Validator::load(path.as_deref()) else {
            eprintln!("no dxil.dll; skipped");
            return;
        };
        let ours = c[4..20].to_vec();
        v.validate(&mut c).unwrap();
        assert_eq!(c[4..20], ours[..], "our hash differs from the validator's");
    }

    /// A pixel shader interpolating at sample positions (PERSP_SAMPLE
    /// barycentrics, `SPI_PS_INPUT_ADDR` bit 0) runs at sample frequency:
    /// DXC's PSV0 says so in PSInfo.SampleFrequency (runtime info byte 1)
    /// whenever an input is interpolated per sample.
    #[test]
    fn sample_interpolated_inputs_set_psv_sample_frequency() {
        let words = [
            0xC808_0000u32, // v_interp_p1_f32 v2, v0, attr0.x
            0xC809_0001,    // v_interp_p2_f32 v2, v1, attr0.x
            0xF800_180F,
            0x0202_0202, // exp mrt0, v2, v2, v2, v2 done vm
            0xBF81_0000,
        ];
        let psv_of = |input_addr: u32| {
            let native = super::NativeProgram {
                stage: super::HwStage::Ps,
                wave64: false,
                vgprs: 4,
                layout: vec![],
                used: vec![],
                io: crate::native::StageIo::Ps {
                    inputs: vec![crate::native::PsInput {
                        usage: crate::native::Usage::Generic,
                        row: 0,
                        mask: 0b1111,
                        attr: 0,
                        flat: false,
                        fp16: false,
                        pack2: false,
                        row2: 0,
                    }],
                    input_addr,
                    db_shader_control: 0,
                    patches: vec![],
                },
                code: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
            };
            let el = |name: &str, kind, usage| SigElement {
                name: name.into(),
                index: 0,
                kind,
                comp: 9,
                interp: 0,
                row: Some(0),
                col: 0,
                cols: 4,
                usage,
                half: false,
            };
            let isg1 = super::container::signature(&[el("TEXCOORD", 0, 0)], true);
            let osg1 = super::container::signature(&[el("SV_Target", 16, 15)], false);
            let bytes =
                crate::dxbc::build(&[(super::FourCC::ISG1, &isg1), (super::FourCC::OSG1, &osg1)]);
            let c = super::Container::parse(&bytes).unwrap();
            let p = crate::ir::build(&native.patched_code(), native.wave64, native.vgprs).unwrap();
            let st = crate::structure::structure(&p).unwrap();
            let res =
                crate::resource::track(&p, &native.layout, native.stage, Some(&st)).unwrap();
            let io = crate::io::raise(&p, &native).unwrap();
            let out = super::emit(&c, &native, &p, &st, &io, &res).unwrap();
            // dxil.dll, where there is one, checks PSV0 against the module.
            #[cfg(windows)]
            if let Ok(mut v) = super::validator::Validator::load(
                super::validator::Validator::default_path().as_deref(),
            ) {
                v.validate(&mut out.dxil.clone()).unwrap();
            }
            let host = super::Container::parse(&out.dxil).unwrap();
            host.part(super::FourCC::PSV0).unwrap().data.to_vec()
        };
        // Runtime info starts after its size; PSInfo is {DepthOutput, SampleFrequency}.
        let center = psv_of(1 << 1);
        assert_eq!(center[4 + 1], 0, "center interpolation is per pixel");
        let sample = psv_of(1 << 0);
        assert_eq!(sample[4 + 1], 1, "PSInfo.SampleFrequency");
    }

    /// A pixel shader reading channel z of an input whose element starts at
    /// w. Looking the channel up subtracted the element's first channel
    /// before checking the channel lies in it, so in a build with overflow
    /// checks (tests, debug) any read below an element that starts at z or
    /// w panicked, whichever row it was in (found by random-input testing).
    #[test]
    fn reading_a_channel_below_an_input_element_does_not_underflow() {
        // v_interp_mov_f32 v3, p0, attr0.z; s_endpgm
        let words = [0xC80E_0202u32, 0xBF81_0000];
        let native = super::NativeProgram {
            stage: super::HwStage::Ps,
            wave64: false,
            vgprs: 4,
            layout: vec![],
            used: vec![],
            io: crate::native::StageIo::Ps {
                inputs: vec![crate::native::PsInput {
                    usage: crate::native::Usage::Generic,
                    row: 0,
                    mask: 0b1000,
                    attr: 0,
                    flat: false,
                    fp16: false,
                    pack2: false,
                    row2: 0,
                }],
                input_addr: 0,
                db_shader_control: 0,
                patches: vec![],
            },
            code: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
        };
        let w_only = SigElement {
            name: "TEXCOORD".into(),
            index: 0,
            kind: 0,
            comp: 9,
            interp: 0,
            row: Some(0),
            col: 3,
            cols: 1,
            usage: 0,
            half: false,
        };
        let isg1 = super::container::signature(&[w_only], true);
        let bytes = crate::dxbc::build(&[(super::FourCC::ISG1, &isg1)]);
        let c = super::Container::parse(&bytes).unwrap();
        let p = crate::ir::build(&native.patched_code(), native.wave64, native.vgprs).unwrap();
        let st = crate::structure::structure(&p).unwrap();
        let res = crate::resource::track(&p, &native.layout, native.stage, Some(&st)).unwrap();
        let io = crate::io::raise(&p, &native).unwrap();
        // Translated or refused with a reason; it must not panic.
        let _ = super::emit(&c, &native, &p, &st, &io, &res);
    }

    /// A pixel shader exporting all four channels of `mrt0` to an
    /// `SV_Target` narrower than a row. The export's channels are register
    /// components; ones the element does not cover were stored as columns
    /// past it (z and w of a `float2`), which DXC's validator refuses, and
    /// the emitted `OSG1` claimed them written.
    #[test]
    fn a_colour_export_stays_inside_its_element() {
        // v_mov_b32 v0, 1.0; exp mrt0 v0, v0, v0, v0 done; s_endpgm
        let words = [0x7E00_02F2u32, 0xF800_080F, 0x0000_0000, 0xBF81_0000];
        let native = super::NativeProgram {
            stage: super::HwStage::Ps,
            wave64: false,
            vgprs: 4,
            layout: vec![],
            used: vec![],
            io: StageIo::Ps {
                inputs: vec![],
                input_addr: 0,
                db_shader_control: 0,
                patches: vec![],
            },
            code: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
        };
        let p = crate::ir::build(&native.patched_code(), native.wave64, native.vgprs).unwrap();
        let st = crate::structure::structure(&p).unwrap();
        let res = crate::resource::track(&p, &native.layout, native.stage, Some(&st)).unwrap();
        let io = crate::io::raise(&p, &native).unwrap();
        // The emitted `OSG1` row's mask and never-written mask.
        let masks = |col: u8, cols: u8| {
            let target = SigElement {
                name: "SV_Target".into(),
                index: 0,
                kind: 16,
                comp: 9,
                interp: 0,
                row: Some(0),
                col,
                cols,
                usage: 0,
                half: false,
            };
            let osg1 = super::container::signature(&[target], false);
            let bytes = crate::dxbc::build(&[(FourCC::OSG1, &osg1[..])]);
            let c = Container::parse(&bytes).unwrap();
            let shader = emit(&c, &native, &p, &st, &io, &res).expect("a colour export");
            let c = Container::parse(&shader.dxil).unwrap();
            let osg1 = c.part(FourCC::OSG1).unwrap().data;
            (osg1[8 + 24], osg1[8 + 25])
        };
        // Every component the element has is written, and only those: the
        // never-written mask is exactly the components it lacks.
        for (col, cols, mask) in [
            (0, 4, 0xF),
            (0, 3, 0x7),
            (0, 2, 0x3),
            (0, 1, 0x1),
            (2, 2, 0xC),
            (3, 1, 0x8),
        ] {
            assert_eq!(masks(col, cols), (mask, !mask & 0xF), "{mask:#x}");
        }
    }

    /// A vertex shader reading its vertex id. The console signature leaves
    /// the id out, so lowering gives it the row after the last one the
    /// signature uses. After r31 that row is r32, past the 32 registers
    /// D3D12 links stages through, and the pipeline was refused.
    #[test]
    fn a_vertex_id_after_r31_is_refused() {
        // exp pos0 v0, v0, v0, v0 done; s_endpgm
        let words = [0xF800_08CFu32, 0x0000_0000, 0xBF81_0000];
        let native = super::NativeProgram {
            stage: super::HwStage::Vs,
            wave64: false,
            vgprs: 4,
            layout: vec![],
            used: vec![],
            io: StageIo::Vs {
                inputs: vec![],
                outputs: vec![],
            },
            code: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
        };
        let p = crate::ir::build(&native.patched_code(), native.wave64, native.vgprs).unwrap();
        let st = crate::structure::structure(&p).unwrap();
        let res = crate::resource::track(&p, &native.layout, native.stage, Some(&st)).unwrap();
        let io = crate::io::raise(&p, &native).unwrap();
        let emit_with = |register: u32| {
            let input = SigElement {
                name: "TEXCOORD".into(),
                index: 0,
                kind: 0,
                comp: 9,
                interp: 0,
                row: Some(register),
                col: 0,
                cols: 4,
                usage: 0,
                half: false,
            };
            let position = SigElement {
                name: "SV_Position".into(),
                index: 0,
                kind: 3,
                comp: 9,
                interp: 0,
                row: Some(0),
                col: 0,
                cols: 4,
                usage: 0,
                half: false,
            };
            let isg1 = super::container::signature(&[input], true);
            let osg1 = super::container::signature(&[position], false);
            let bytes = crate::dxbc::build(&[(FourCC::ISG1, &isg1[..]), (FourCC::OSG1, &osg1[..])]);
            let c = Container::parse(&bytes).unwrap();
            emit(&c, &native, &p, &st, &io, &res)
        };
        for register in [0, 30] {
            let shader = emit_with(register).expect("the id fits below r32");
            let c = Container::parse(&shader.dxil).unwrap();
            let isg1 = c.part(FourCC::ISG1).unwrap().data;
            let rows = crate::dxbc::u32_at(isg1, 0).unwrap() as usize;
            let registers: Vec<u32> = (0..rows)
                .map(|i| crate::dxbc::u32_at(isg1, 8 + 32 * i + 20).unwrap())
                .collect();
            assert_eq!(registers, [register, register + 1], "r{register}");
        }
        let Err(why) = emit_with(31) else {
            panic!("a vertex id at r32 was emitted");
        };
        assert!(
            format!("{why:?}").contains("a system input past the 32 input registers"),
            "{why:?}"
        );
    }

    /// One `ISG1`/`OSG1` row: stream, semantic index, system value,
    /// component type, register, mask and minimum precision as the title's
    /// part has them.
    fn row(stream: u32, system: u32, comp: u32, min_precision: u32) -> Vec<u8> {
        let mut p = Vec::new();
        for w in [1, 8, stream, 40, 0, system, comp, 0, 0x01, min_precision] {
            p.extend_from_slice(&u32::to_le_bytes(w));
        }
        p.extend_from_slice(b"ATTR\0");
        p
    }

    /// A row's component type is a raw `u32` of the title's part. 32-bit
    /// types keep their kind, 16-bit ones are declared 32-bit with `half`
    /// set, and a type without a 32-bit form is refused rather than
    /// declared as something else.
    #[test]
    fn a_signature_component_type_maps_or_is_refused() {
        // (raw type, declared type, half): uint, int, float in 32 and 16 bits.
        for (raw, comp, half) in [
            (1, 5, false),
            (2, 4, false),
            (3, 9, false),
            (4, 5, true),
            (5, 4, true),
            (6, 9, true),
        ] {
            let rows = signature(&row(0, 0, raw, 0)).expect("a 32- or 16-bit type");
            assert_eq!((rows[0].comp, rows[0].half), (comp, half), "type {raw}");
        }
        let unsupported = |what| ShaderError::Unsupported { offset: 0, what };
        assert_eq!(
            signature(&row(0, 0, 0, 0)).unwrap_err(),
            unsupported("a signature element of unknown type")
        );
        for raw in [7, 8, 9, 0xFF, u32::MAX] {
            assert_eq!(
                signature(&row(0, 0, raw, 0)).unwrap_err(),
                unsupported("a 64-bit signature element"),
                "type {raw:#x}"
            );
        }
        // The front face is declared a uint whatever type the row gives,
        // the unknown one included (DXIL's validator insists).
        for raw in [0, 1, 3, 6, 7] {
            let rows = signature(&row(0, 9, raw, 0)).expect("a front face");
            assert_eq!((rows[0].kind, rows[0].comp), (13, 5), "type {raw}");
        }
    }

    /// Rows of another geometry stream, of minimum precision, or naming a
    /// system value lowering does not know are refused, not linked as an
    /// ordinary attribute.
    #[test]
    fn a_signature_row_lowering_cannot_link_is_refused() {
        let unsupported = |what| ShaderError::Unsupported { offset: 0, what };
        for stream in [1, 3, u32::MAX] {
            assert_eq!(
                signature(&row(stream, 0, 3, 0)).unwrap_err(),
                unsupported("a signature element of another stream"),
                "stream {stream:#x}"
            );
        }
        for precision in [1, 2, 8, u32::MAX] {
            assert_eq!(
                signature(&row(0, 0, 3, precision)).unwrap_err(),
                unsupported("a minimum-precision signature element"),
                "precision {precision:#x}"
            );
        }
        for system in [11, 24, 26, 63, 71, u32::MAX] {
            assert_eq!(
                signature(&row(0, system, 3, 0)).unwrap_err(),
                unsupported("a system value in the signature"),
                "system value {system:#x}"
            );
        }
        // Each known system value maps to its DXIL kind.
        for (system, kind) in [
            (0, 0),
            (1, 3),
            (2, 6),
            (3, 7),
            (4, 4),
            (5, 5),
            (6, 1),
            (7, 10),
            (8, 2),
            (9, 13),
            (10, 12),
            (25, 30),
            (64, 16),
            (65, 17),
            (66, 14),
            (67, 19),
            (68, 18),
            (69, 20),
            (70, 15),
        ] {
            let rows = signature(&row(0, system, 3, 0)).expect("a known system value");
            assert_eq!(rows[0].kind, kind, "system value {system}");
        }
    }
}
