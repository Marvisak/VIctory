use super::*;

#[cfg(windows)]
#[path = "../../../victory-shader/tests/fixtures/dispatch.rs"]
mod fixture;

#[test]
fn thread_dimensions_cover_partial_groups_on_each_axis() {
    assert_eq!(
        thread_groups([65, 1, 1], [64, 1, 1]),
        Some(([2, 1, 1], true))
    );
    assert_eq!(
        thread_groups([10, 16, 5], [8, 8, 4]),
        Some(([2, 2, 2], true))
    );
    assert_eq!(
        thread_groups([82_606, 1, 1], [32, 1, 1]),
        Some(([2582, 1, 1], true))
    );
    assert_eq!(
        thread_groups([64, 8, 4], [32, 8, 4]),
        Some(([2, 1, 1], false))
    );
    assert_eq!(thread_groups([0, 1, 1], [64, 1, 1]), None);
    assert_eq!(thread_groups([u32::MAX, 1, 1], [64, 1, 1]), None);
}

#[cfg(windows)]
#[test]
#[ignore = "requires a D3D12 device with SM 6.6 support"]
fn dispatch_x_writes_only_requested_threads() {
    #[repr(C)]
    struct ComputeDesc {
        root: This,
        code: *const u8,
        size: usize,
    }
    let host = crate::host::host().expect("D3D12 device");
    let null = std::ptr::null();
    let code = fixture::kernel(false);
    let desc = ComputeDesc {
        root: null,
        code: code.as_ptr(),
        size: code.len(),
    };
    let (mut pipeline, mut allocator, mut list, mut heap) = (null, null, null, null);
    assert_eq!(
        crate::pipeline::create_compute(
            null,
            std::ptr::from_ref(&desc).cast(),
            &PAGEABLE_IIDS[0],
            &mut pipeline
        ),
        S_OK
    );
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    while crate::pipeline::host_pipeline(pipeline).is_none() {
        assert!(
            std::time::Instant::now() < deadline,
            "compute PSO unavailable"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert_eq!(
        create_allocator(null, 0, &ALLOCATOR_IIDS[0], &mut allocator),
        S_OK
    );
    assert_eq!(
        create_list(null, 0, 0, allocator, pipeline, &LIST_IIDS[1], &mut list),
        S_OK
    );
    let heap_desc = [0u32, 8, 1, 0, 0, 0, 0, 0];
    assert_eq!(
        crate::descriptor::create_heap(
            null,
            heap_desc.as_ptr().cast(),
            &PAGEABLE_IIDS[0],
            &mut heap
        ),
        S_OK
    );
    state::heaps(list, 1, &heap);
    // Separate output for every command, on one list and guest pipeline.
    // The final control reproduces the old forwarding of 65 as a group count.
    const SLOTS: u32 = 65 * 64;
    let mut outputs = Vec::new();
    for (x, flags, writes) in [
        (65, Some(0x20), 65),
        (2, None, 128),
        (2, Some(0x8000), 128),
        (65, Some(0), SLOTS),
    ] {
        let bytes = u64::from(SLOTS) * 4;
        let buffer = crate::resource::test_buffer(bytes);
        let address = crate::resource::buffer_bytes(buffer).unwrap().0 as u64;
        let handle = crate::descriptor::heap(heap).start + 32 * outputs.len() as u64;
        let uav = [39, 1, 0, 0, SLOTS, 0, 0, 0, 1, 0];
        crate::descriptor::create_uav(null, buffer, null, &uav, handle);
        state::compute_table(list, 0, handle);
        match flags {
            Some(flags) => dispatch_x(list, x, 1, 1, flags),
            None => state::dispatch(list, x, 1, 1),
        }
        outputs.push((buffer, address, writes));
    }
    assert_eq!(close(list), S_OK);
    host.execute(&[host_list(list).unwrap()]);
    let release = |o| object::slot::<extern "system" fn(This) -> u32>(o, 0x10)(o);
    for (buffer, address, writes) in outputs {
        let shadow = crate::shadow::find(address, u64::from(SLOTS) * 4).unwrap();
        let output = shadow.read_host(address, SLOTS as usize);
        for (lane, value) in output.into_iter().enumerate() {
            assert_eq!(
                value,
                u32::from(lane < writes as usize),
                "slot {lane}, expected {writes} writes"
            );
        }
        release(buffer);
    }
    for o in [list, allocator, pipeline, heap] {
        release(o);
    }
}
