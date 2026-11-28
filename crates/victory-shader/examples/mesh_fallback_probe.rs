//! Validates and creates the generated vertex half of a mesh fallback for a
//! user-supplied DXBC shader. No game data is included in the repository.

#[cfg(all(windows, feature = "host"))]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use victory_shader::emit::{self, host, validator};
    use victory_shader::native::StageIo;
    use victory_shader::{Container, FourCC, HwStage, native_programs};

    let path = std::env::args().nth(1).ok_or("pass a mesh DXBC path")?;
    let bytes = std::fs::read(&path)?;
    let container = Container::parse(&bytes)?;
    let native = native_programs(container.part(FourCC::DXIL).ok_or("no DXIL part")?.data)?;
    let mesh = native
        .iter()
        .find(|program| program.stage == HwStage::Ms)
        .ok_or("no mesh program")?;
    let StageIo::Ms(io) = &mesh.io else {
        return Err("mesh I/O missing".into());
    };
    let translated = emit::translate(&bytes).map_err(|e| format!("translate: {e}"))?;
    eprintln!(
        "translated mesh: {} vertices, {} primitives",
        io.max_vertices, io.max_primitives
    );
    let (mut vertex, layout) =
        emit::mesh_fallback_vertex(&translated, io.max_vertices, io.max_primitives)
            .map_err(|e| format!("vertex fallback: {e}"))?;
    eprintln!("vertex fallback layout {layout:?}");
    let (mut compute, compute_layout) =
        emit::translate_mesh_compute(&bytes).map_err(|e| format!("compute producer: {e}"))?;
    assert_eq!(layout, compute_layout);
    let validator_path = validator::Validator::default_path();
    let mut validator = validator::Validator::load(validator_path.as_deref())?;
    validator
        .validate(&mut vertex.dxil)
        .map_err(|e| format!("vertex dxil: {e}"))?;
    eprintln!("vertex dxil validated");
    validator
        .validate(&mut compute.dxil)
        .map_err(|e| format!("compute dxil: {e}"))?;
    eprintln!("compute dxil validated");
    if let Ok(dir) = std::env::var("VICTORY_FALLBACK_DUMP") {
        std::fs::create_dir_all(&dir)?;
        std::fs::write(
            std::path::Path::new(&dir).join("mesh-fallback-vs.dxil"),
            &vertex.dxil,
        )?;
        std::fs::write(
            std::path::Path::new(&dir).join("mesh-fallback-cs.dxil"),
            &compute.dxil,
        )?;
    }
    let host = host::Host::new(false)?;
    let pixel = match std::env::args().nth(2) {
        Some(path) => emit::translate(&std::fs::read(path)?)?,
        None => host::stub_ps(),
    };
    eprintln!(
        "compute bindings dwords {} cbv {:?} srv {:?} uav {:?} mem {} writes {} views {} heap {} samplers {}",
        compute.user_data_dwords(),
        compute.root_cbvs,
        compute.root_srvs,
        compute.root_uavs,
        compute.memory,
        compute.memory_writes,
        compute.view_addresses,
        compute.heap,
        compute.static_samplers.len()
    );
    host.compute_pipeline(&compute)
        .map_err(|e| format!("compute pipeline: {e}"))?;
    eprintln!("compute pipeline created");
    host.pipeline(&vertex, &pixel)
        .map_err(|e| format!("graphics pipeline: {e}"))?;
    eprintln!("graphics pipeline created");
    println!(
        "fallback compute and vertex validated; both pipelines created: {} vertices/group, {} primitives/group, {} index bytes/group, {} vertex bytes/group",
        layout.max_vertices,
        layout.max_primitives,
        layout.index_group_stride,
        layout.vertex_group_stride
    );
    Ok(())
}

#[cfg(not(all(windows, feature = "host")))]
fn main() {
    eprintln!("This probe needs Windows and --features host");
}
