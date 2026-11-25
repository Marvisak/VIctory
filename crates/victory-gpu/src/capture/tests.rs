use super::*;
use crate::capture_diff::{compare, write_submission};
use windows::Win32::Foundation::RECT;
use windows::core::{IUnknown, Interface};

#[test]
fn a_readback_reports_one_changed_alpha_byte() {
    let Some(host) = crate::host::host() else {
        eprintln!("capture readback skipped: no D3D12 host");
        return;
    };
    // A 12-byte logical row in a 256-byte D3D12 row pitch.
    let resource = title_target(1);
    let before = tempfile::tempdir().unwrap();
    let after = tempfile::tempdir().unwrap();
    let mut pixels = [0u8; 24];
    for (directory, alpha) in [(before.path(), 0), (after.path(), 255)] {
        let texture = clear_rgba8(host, &resource, alpha);
        let target = submission_target(
            host,
            "test",
            (0, std::ptr::null(), "test writer"),
            &texture,
            D3D12_RESOURCE_STATE_RENDER_TARGET.0 as u32,
            None,
        )
        .unwrap();
        pixels[23] = alpha;
        assert_eq!(target.hash, Some(blake3::hash(&pixels)));
        write_submission(directory, 7, 12, &[target]).unwrap();
    }
    let difference = compare(before.path(), after.path()).unwrap().unwrap();
    assert!(
        difference.contains("target contents changed"),
        "{difference}"
    );
    assert!(difference.contains("test writer"), "{difference}");
}

/// A title-owned texture of the kind passed to dump_submission.
fn title_target(samples: u32) -> IUnknown {
    let desc = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
        Width: 3,
        Height: 2,
        DepthOrArraySize: 1,
        MipLevels: 1,
        Format: DXGI_FORMAT_R8G8B8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: samples,
            Quality: 0,
        },
        Flags: D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET,
        ..Default::default()
    };
    let heap = D3D12_HEAP_PROPERTIES {
        Type: D3D12_HEAP_TYPE_DEFAULT,
        ..Default::default()
    };
    let mut resource = std::ptr::null();
    // The console's heap and resource descriptions use the D3D12 layouts.
    assert_eq!(
        crate::resource::create_committed(
            std::ptr::null(),
            (&raw const heap).cast(),
            0,
            (&raw const desc).cast(),
            D3D12_RESOURCE_STATE_RENDER_TARGET.0 as u32,
            std::ptr::null(),
            &crate::object::IUNKNOWN,
            &mut resource,
        ),
        crate::object::S_OK
    );
    // SAFETY: a successful create_committed hands back an owned IUnknown reference.
    unsafe { IUnknown::from_raw(resource.cast_mut().cast()) }
}

/// Clear the texture, setting only the bottom-right pixel's alpha to the given byte.
fn clear_rgba8(host: &crate::host::Host, resource: &IUnknown, alpha: u8) -> ID3D12Resource {
    let title = resource.as_raw().cast();
    let texture = crate::resource::host_resource(title).unwrap();
    let list = host.new_list().unwrap();
    crate::list::ensure_state(&list, title, D3D12_RESOURCE_STATE_RENDER_TARGET.0 as u32);
    let rtv = host.rtv(&texture, None);
    // SAFETY: the list is open, the texture is a render target and the rectangle fits.
    unsafe {
        list.list.ClearRenderTargetView(rtv, &[0.; 4], None);
        let last = RECT {
            left: 2,
            top: 1,
            right: 3,
            bottom: 2,
        };
        list.list
            .ClearRenderTargetView(rtv, &[0., 0., 0., f32::from(alpha) / 255.], Some(&[last]));
    }
    list.close();
    host.execute(&[&list]);
    texture
}

#[test]
fn matching_single_sample_targets_do_not_hide_an_msaa_target() {
    let Some(host) = crate::host::host() else {
        eprintln!("capture readback skipped: no D3D12 host");
        return;
    };
    let single = title_target(1);
    clear_rgba8(host, &single, 0);
    let msaa = title_target(4);
    let targets = [
        (single.as_raw() as usize, "single writer".into()),
        (msaa.as_raw() as usize, "MSAA writer".into()),
    ];
    let before = tempfile::tempdir().unwrap();
    let after = tempfile::tempdir().unwrap();
    dump_submission_to(host, before.path(), 7, 11, &targets);
    dump_submission_to(host, after.path(), 7, 12, &targets);
    let error = compare(before.path(), after.path())
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("slot 1 plane -: target readback unavailable"),
        "{error}"
    );
    assert!(
        error.contains("3x2x1 format 28") && error.contains("writer MSAA writer"),
        "{error}"
    );
}

#[test]
fn an_msaa_only_submission_keeps_its_place_before_the_next_capture() {
    let Some(host) = crate::host::host() else {
        eprintln!("capture readback skipped: no D3D12 host");
        return;
    };
    let msaa = title_target(4);
    let single = title_target(1);
    clear_rgba8(host, &single, 0);
    let before = tempfile::tempdir().unwrap();
    let after = tempfile::tempdir().unwrap();
    dump_submission_to(
        host,
        before.path(),
        7,
        9,
        &[(msaa.as_raw() as usize, "MSAA".into())],
    );
    let later = [(single.as_raw() as usize, "later writer".into())];
    dump_submission_to(host, before.path(), 7, 10, &later);
    dump_submission_to(host, after.path(), 7, 15, &later);
    let error = compare(before.path(), after.path())
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("submission 1 (before 9, after 15)"),
        "{error}"
    );
    assert!(
        error.contains("target readback unavailable") && error.contains("writer MSAA"),
        "{error}"
    );
}
