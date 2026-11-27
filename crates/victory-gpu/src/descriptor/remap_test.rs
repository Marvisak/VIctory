//! Capped descriptor heap overflow remapping tests (`> PORTABLE_VIEWS` title
//! view heaps on Tier-2 / NVIDIA hosts and uncapped hosts alike).

use super::tests::{placed_memory, placed_tex_desc, placed_tex_srv_desc};
use super::*;

#[test]
fn capped_view_heap_remaps_high_slots_and_recycles_overflow_entries() {
    let Some(host) = host() else {
        return; // no host GPU
    };
    if !host.capped_heap {
        return; // uncapped hosts keep every title slot direct
    }
    let null = std::ptr::null();
    // 1,245,184 title view slots (> 1,000,000 `PORTABLE_VIEWS`).
    let total_title_slots: u32 = 1_245_184;
    let heap_desc: [u32; 8] = [0, total_title_slots, 1, 0, 0, 0, 0, 0];
    let mut raw_heap = null;
    assert_eq!(
        crate::descriptor::create_heap(
            null,
            heap_desc.as_ptr().cast(),
            &crate::descriptor::HEAP_IIDS[0],
            &mut raw_heap
        ),
        crate::object::S_OK
    );
    let title_heap = crate::descriptor::heap(raw_heap);
    let h = title_heap.host().expect("capped host view heap");
    assert_eq!(h.count(), PORTABLE_VIEWS - MIN_DYNAMIC - OVERFLOW_VIEWS);
    assert_eq!(h.overflow, OVERFLOW_VIEWS);
    assert_eq!(h.dynamic, MIN_DYNAMIC - NULL_RESERVE);

    // Initialize `remap` and `view_addresses` so live updates are verified too.
    assert!(h.remap().is_some());
    assert!(h.view_addresses().is_some());
    let remap_ptr = h.remap.get().unwrap().as_ref().unwrap().1 as *const u32;
    let stride_ptr = h.strides.as_ref().unwrap().1 as *const u32;

    // Register a 64 KiB page of title memory so placed buffer views build real SRVs.
    let pool = crate::watch::tests::pool(1);
    let base_va = pool as u64;
    crate::shadow::register(pool, base_va, crate::watch::PAGE as u64);

    let mut buf_res = [0u32; 14];
    (buf_res[0], buf_res[4]) = (1, 256);
    let mut buf_desc = [0u32; 16];
    (buf_desc[1], buf_desc[2], buf_desc[6], buf_desc[7]) =
        (1, D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING, 16, 16);
    (buf_desc[10], buf_desc[11]) = (base_va as u32, (base_va >> 32) as u32);

    let high_slots = [737_856u32, 950_000, 1_200_000];
    let mut phys_assigned = Vec::new();
    for &slot_idx in &high_slots {
        let handle = title_heap.start + u64::from(slot_idx) * 32;
        crate::descriptor::create_placed_srv(null, &buf_res, &buf_desc, handle);
        assert!(h.made(slot_idx), "high slot {slot_idx} recorded");
        assert!(h.kept(slot_idx), "high slot {slot_idx} holds host SRV");

        // SAFETY: `slot_idx < REMAP_VIEWS` in mapped upload buffer.
        let phys = unsafe { remap_ptr.add(slot_idx as usize).read_volatile() };
        assert!(
            phys >= h.count() && phys < h.count() + h.overflow,
            "high slot {slot_idx} mapped to overflow slot {phys}"
        );
        assert_eq!(h.title_slot(phys), Some(slot_idx));
        assert_eq!(h.address_at(phys), Some(base_va));
        // SAFETY: `phys < HEAP_VIEWS` in mapped stride table.
        let stride = unsafe { stride_ptr.add(phys as usize).read_volatile() };
        assert_eq!(stride, 16);
        phys_assigned.push(phys);
    }

    // Placed texture in a high slot (`1,210,000`): deferred on `mirror(false)`,
    // materialized when `make_read(&[phys])` runs with the physical overflow slot!
    let tex_slot: u32 = 1_210_000;
    let tex_va = placed_memory();
    let tex_res = placed_tex_desc(4, 4, 28, 0);
    let tex_desc = placed_tex_srv_desc(tex_va, 28);
    let tex_handle = title_heap.start + u64::from(tex_slot) * 32;
    crate::descriptor::create_placed_srv(null, &tex_res, &tex_desc, tex_handle);
    assert!(h.made(tex_slot));
    assert!(!h.kept(tex_slot), "placed texture deferred until read");
    // SAFETY: `tex_slot < REMAP_VIEWS`.
    let tex_phys = unsafe { remap_ptr.add(tex_slot as usize).read_volatile() };
    assert_eq!(h.title_slot(tex_phys), Some(tex_slot));
    crate::descriptor::make_read(&[tex_phys]);
    assert!(h.kept(tex_slot), "make_read materialized high-slot placed texture");

    // Clearing a high slot resets `remap[slot]` to `CAPPED_HEAP_VIEWS - 1` and returns
    // its physical overflow slot to the free list for reuse.
    let cleared = high_slots[1];
    let freed_phys = phys_assigned[1];
    let cleared_handle = title_heap.start + u64::from(cleared) * 32;
    // SAFETY: valid descriptor slot in `title_heap`.
    unsafe { std::ptr::write_bytes(cleared_handle as *mut u8, 0, 32) };
    h.sync_slots(cleared, cleared + 1, title_heap.start, 32);
    assert!(!h.made(cleared) && !h.kept(cleared));
    // SAFETY: `cleared < REMAP_VIEWS`.
    let unmapped = unsafe { remap_ptr.add(cleared as usize).read_volatile() };
    assert_eq!(unmapped, victory_shader::emit::CAPPED_HEAP_VIEWS - 1);
    assert_eq!(h.title_slot(freed_phys), None);

    // Next high slot reuses `freed_phys` (LIFO free list).
    let next_high: u32 = 1_220_000;
    let next_handle = title_heap.start + u64::from(next_high) * 32;
    crate::descriptor::create_placed_srv(null, &buf_res, &buf_desc, next_handle);
    // SAFETY: `next_high < REMAP_VIEWS`.
    let reused_phys = unsafe { remap_ptr.add(next_high as usize).read_volatile() };
    assert_eq!(reused_phys, freed_phys);
    assert_eq!(h.title_slot(reused_phys), Some(next_high));

    // SAFETY: plain device check.
    assert!(unsafe { host.device.GetDeviceRemovedReason() }.is_ok());

    crate::descriptor::release(raw_heap);
    crate::shadow::unregister(pool, base_va);
    crate::watch::tests::free(pool);
}
