//! Vertex half of a mesh-to-compute fallback. A compute pass will write the
//! mesh group's indices and vertex exports into separate raw buffers; this
//! shader reads the vertex exports after an indexed draw fetches the index.

use super::bitcode::{Attr, Bin, Cast, Term};
use super::container::SigElement;
use super::shader::{Shader, MESH_FALLBACK_SLOT};
use super::{HostShader, signature};
use crate::ShaderError;
use crate::dxbc::{Container, FourCC};
use crate::native::HwStage;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MeshFallbackLayout {
    pub max_vertices: u32,
    pub max_primitives: u32,
    /// Bytes per vertex: four dwords per output register.
    pub vertex_stride: u32,
    pub index_group_stride: u32,
    pub vertex_group_stride: u32,
}

impl MeshFallbackLayout {
    pub(crate) fn new(
        outputs: &[SigElement],
        max_vertices: u32,
        max_primitives: u32,
    ) -> Option<Self> {
        let rows = outputs.iter().filter_map(|e| e.row).max()?.checked_add(1)?;
        let vertex_stride = rows.checked_mul(16)?;
        let index_group_stride = max_primitives.checked_mul(12)?;
        let vertex_group_stride = max_vertices.checked_mul(vertex_stride)?;
        Some(Self {
            max_vertices,
            max_primitives,
            vertex_stride,
            index_group_stride,
            vertex_group_stride,
        })
    }

    pub fn bytes_for_groups(self, groups: u32) -> Option<(u64, u64)> {
        u64::from(self.max_vertices)
            .checked_mul(u64::from(groups))
            .filter(|&vertices| vertices <= u64::from(u32::MAX))?;
        Some((
            u64::from(self.index_group_stride).checked_mul(u64::from(groups))?,
            u64::from(self.vertex_group_stride).checked_mul(u64::from(groups))?,
        ))
    }

    /// The generated index buffer is contiguous across groups. The indexed
    /// draw uses base vertex zero and the whole index buffer in one call.
    pub fn index_count(self, groups: u32) -> Option<u32> {
        self.max_primitives.checked_mul(3)?.checked_mul(groups)
    }
}

/// A conventional vertex stage for a translated mesh shader's output
/// signature. Its raw SRV is `t0` in space 3 (root slot
/// [`MESH_FALLBACK_SLOT`]), and the indexed draw supplies a global vertex id
/// (`group * max_vertices + local vertex`). The vertex buffer layout is
/// returned so the compute producer can use the same offsets.
pub fn mesh_fallback_vertex(
    mesh: &HostShader,
    max_vertices: u32,
    max_primitives: u32,
) -> Result<(HostShader, MeshFallbackLayout), ShaderError> {
    if mesh.stage != HwStage::Ms || max_vertices == 0 || max_primitives == 0 {
        return Err(ShaderError::Malformed(
            "mesh fallback needs a mesh stage and nonzero limits",
        ));
    }
    let container = Container::parse(&mesh.dxil)?;
    let outputs = signature(
        container
            .part(FourCC::OSG1)
            .ok_or(ShaderError::Malformed("mesh output signature missing"))?
            .data,
    )?;
    if let Some(prims) = container.part(FourCC::PSG1) {
        let mut prims = signature(prims.data)?;
        // Primitive id and the cull flag are not outputs the vertex stage
        // can read. Anything left is a real per-primitive value.
        prims.retain(|e| !matches!(e.kind, 10 | 30));
        if !prims.is_empty() {
            return Err(ShaderError::Unsupported {
                offset: 0,
                what: "per-primitive mesh outputs in the vertex fallback",
            });
        }
    }
    let layout = MeshFallbackLayout::new(&outputs, max_vertices, max_primitives)
        .ok_or(ShaderError::Malformed("mesh output layout is too large"))?;
    let mut s = Shader::new(false);
    s.inputs.push(SigElement {
        name: "SV_VertexID".into(),
        index: 0,
        kind: 1,
        comp: 5,
        interp: 0,
        row: Some(0),
        col: 0,
        cols: 1,
        usage: 1,
        half: false,
    });
    let b = s.entry;
    let (zero, column, undef) = (s.m.i32(0), s.m.i8(0), s.m.undef(s.t.i32));
    let id = s.op(
        b,
        "loadInput.i32",
        Attr::ReadNone,
        s.t.i32,
        4,
        &[zero, zero, column, undef],
    );
    let max_v = s.m.i32(max_vertices);
    let group = s.m.bin(b, Bin::UDiv, id, max_v);
    let lane = s.m.bin(b, Bin::URem, id, max_v);
    let group_stride = s.m.i32(layout.vertex_group_stride);
    let vertex_stride = s.m.i32(layout.vertex_stride);
    let base = s.m.bin(b, Bin::Mul, group, group_stride);
    let offset = s.m.bin(b, Bin::Mul, lane, vertex_stride);
    let base = s.m.bin(b, Bin::Add, base, offset);
    let handle = s.fallback_srv();
    for (element, e) in outputs.iter().enumerate() {
        let row = e
            .row
            .ok_or(ShaderError::Malformed("mesh output row unallocated"))?;
        for col in 0..e.cols {
            let word = row * 16 + u32::from(e.col + col) * 4;
            let word = s.m.i32(word);
            let at = s.m.bin(b, Bin::Add, base, word);
            let args = [handle, at, s.m.undef(s.t.i32), s.m.i8(1), s.m.i32(4)];
            let value = s.op(
                b,
                "rawBufferLoad.f32",
                Attr::ReadOnly,
                s.t.res_ret,
                139,
                &args,
            );
            let value = s.m.extract(b, value, 0);
            let (suffix, value) = if e.comp == 9 {
                (".f32", value)
            } else {
                (".i32", s.m.cast(b, Cast::Bitcast, value, s.t.i32))
            };
            let args = [s.m.i32(element as u32), zero, s.m.i8(col), value];
            s.op(
                b,
                &format!("storeOutput{suffix}"),
                Attr::NoUnwind,
                s.t.void,
                5,
                &args,
            );
        }
    }
    s.outputs = outputs
        .into_iter()
        .map(|mut e| {
            e.usage = (1 << e.cols) - 1;
            e
        })
        .collect();
    s.m.terminate(b, Term::Ret);
    Ok((
        HostShader {
            stage: HwStage::Vs,
            dxil: s.finish(),
            user_data: Vec::new(),
            samplers: Vec::new(),
            static_samplers: Vec::new(),
            root_cbvs: Vec::new(),
            root_srvs: vec![MESH_FALLBACK_SLOT],
            root_uavs: Vec::new(),
            memory: false,
            memory_writes: false,
            view_addresses: false,
            heap: false,
            heap_writes: false,
            view_marks: false,
        },
        layout,
    ))
}

#[cfg(test)]
mod tests {
    use super::MeshFallbackLayout;

    #[test]
    fn group_ranges_are_contiguous() {
        let layout = MeshFallbackLayout {
            max_vertices: 128,
            max_primitives: 128,
            vertex_stride: 48,
            index_group_stride: 128 * 12,
            vertex_group_stride: 128 * 48,
        };
        assert_eq!(layout.bytes_for_groups(2), Some((3072, 12_288)));
        assert_eq!(layout.index_count(2), Some(768));
    }
}
