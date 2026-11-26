//! Command signatures: the record layout of indirect execution. Kept for
//! translating it later.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use crate::object::{
    self, COMMAND_SIGNATURE, E_INVALIDARG, HRESULT, Iid, Object, Table, This, guid,
};
use crate::resource::PAGEABLE_IIDS;

const SIGNATURE_IIDS: [Iid; 4] = [
    guid(
        0xc36a797c,
        0xec80,
        0x4f0a,
        [0x89, 0x85, 0xa7, 0xb2, 0x47, 0x50, 0x82, 0xd1],
    ),
    PAGEABLE_IIDS[0],
    PAGEABLE_IIDS[1],
    PAGEABLE_IIDS[2],
];

#[repr(C)]
pub(crate) struct Desc {
    stride: u32,
    count: u32,
    /// Each a type and up to three 32-bit fields.
    arguments: *const [u32; 4],
    nodes: u32,
}

#[repr(C)]
struct Signature {
    object: Object,
    stride: u32,
    arguments: Vec<[u32; 4]>,
}

/// Indirect argument types (`D3D12_INDIRECT_ARGUMENT_TYPE`) that update
/// root-signature bindings on the pipeline (`CONSTANT`, `CONSTANT_BUFFER_VIEW`,
/// `SHADER_RESOURCE_VIEW`, `UNORDERED_ACCESS_VIEW`, `INCREMENTING_CONSTANT`).
fn binds_root(kind: u32) -> bool {
    matches!(kind, 5..=8 | 11)
}

/// Device method.
pub(crate) extern "system" fn create(
    _: This,
    desc: *const Desc,
    root: This,
    iid: *const Iid,
    out: *mut This,
) -> HRESULT {
    let clear_out = || {
        if !out.is_null() {
            // SAFETY: caller-supplied `_COM_Outptr_opt_` slot.
            unsafe { out.write(std::ptr::null()) };
        }
    };
    // SAFETY: the title passes a description (null is tolerated).
    let Some(desc) = (unsafe { desc.as_ref() }) else {
        clear_out();
        return E_INVALIDARG;
    };
    if !desc.stride.is_multiple_of(4) || (desc.arguments.is_null() && desc.count > 0) {
        clear_out();
        return E_INVALIDARG;
    }
    let arguments = if desc.count == 0 {
        Vec::new()
    } else {
        // SAFETY: the title passes `count` argument descriptions.
        unsafe { std::slice::from_raw_parts(desc.arguments, desc.count as usize) }.to_vec()
    };
    if root.is_null() && arguments.iter().any(|a| binds_root(a[0])) {
        clear_out();
        return E_INVALIDARG;
    }
    tracing::debug!(stride = desc.stride, ?arguments, "command signature");
    let signature = Box::new(Signature {
        object: Object::new(&FUNCTIONS),
        stride: desc.stride,
        arguments,
    });
    object::hand_out(COMMAND_SIGNATURE, signature, iid, &SIGNATURE_IIDS, out)
}

/// A signature's record stride and arguments (type, then up to three
/// fields each).
pub(crate) fn layout<'a>(this: This) -> Option<(u32, &'a [[u32; 4]])> {
    if this.is_null() {
        return None;
    }
    // SAFETY: the title passes one of our signatures.
    let s = unsafe { &*this.cast::<Signature>() };
    Some((s.stride, &s.arguments))
}

extern "system" fn query(this: This, iid: *const Iid, out: *mut This) -> HRESULT {
    let found = object::is(iid, &SIGNATURE_IIDS).then_some(this);
    object::answer(COMMAND_SIGNATURE, iid, found, out)
}
extern "system" fn release(this: This) -> u32 {
    object::release_boxed::<Signature>(this)
}

/// IUnknown 3, object 4, device child 1.
static FUNCTIONS: Table<24> = {
    let mut f = object::d3d12_object(object::missing_table::<COMMAND_SIGNATURE, 24>());
    f[0] = query as *const ();
    f[2] = release as *const ();
    f[7] = object::parent_device as *const (); // 0x38
    Table(f)
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::{E_NOINTERFACE, E_POINTER, S_FALSE, S_OK, slot};

    #[test]
    fn null_or_argument_free_descriptions_are_rejected() {
        let null = std::ptr::null();
        let mut signature = null;
        // No description at all.
        assert_eq!(
            create(
                null,
                std::ptr::null(),
                null,
                &SIGNATURE_IIDS[0],
                &mut signature
            ),
            E_INVALIDARG
        );
        assert!(signature.is_null());
        // A count of arguments with nothing behind the pointer.
        let argumentless = Desc {
            stride: 16,
            count: 3,
            arguments: std::ptr::null(),
            nodes: 0,
        };
        assert_eq!(
            create(
                null,
                &argumentless,
                null,
                &SIGNATURE_IIDS[0],
                &mut signature
            ),
            E_INVALIDARG
        );
        assert!(signature.is_null());
    }

    #[test]
    fn signatures_without_arguments_stay_empty() {
        let null = std::ptr::null();
        // Zero arguments and no pointer is a valid, if useless, signature.
        let empty = Desc {
            stride: 16,
            count: 0,
            arguments: std::ptr::null(),
            nodes: 0,
        };
        let mut signature = null;
        assert_eq!(
            create(null, &empty, null, &SIGNATURE_IIDS[0], &mut signature),
            S_OK
        );
        let (stride, arguments) = layout(signature).expect("one of our signatures");
        assert_eq!(stride, 16);
        assert!(arguments.is_empty());
        assert_eq!(
            slot::<extern "system" fn(This) -> u32>(signature, 0x10)(signature),
            0
        );
        // The argument pointer is not read when the count is zero.
        let none: [[u32; 4]; 0] = [];
        let unread = Desc {
            stride: 16,
            count: 0,
            arguments: none.as_ptr(),
            nodes: 0,
        };
        let mut signature = null;
        assert_eq!(
            create(null, &unread, null, &SIGNATURE_IIDS[1], &mut signature),
            S_OK
        );
        let (_, arguments) = layout(signature).expect("one of our signatures");
        assert!(arguments.is_empty());
        assert_eq!(
            slot::<extern "system" fn(This) -> u32>(signature, 0x10)(signature),
            0
        );
    }

    #[test]
    fn arguments_are_copied_out_of_title_memory() {
        // A title reuses the memory its description lived in, while
        // ExecuteIndirect reads the layout long after creation.
        assert!(
            layout(std::ptr::null()).is_none(),
            "no signature, no layout"
        );
        let draw = [0u32, 3, 1, 0]; // three vertices, one instance
        let dispatch = [2u32, 4, 1, 1]; // dispatch 4x1x1 groups
        let expected = [draw, dispatch];
        let mut title_memory = expected;
        let desc = Desc {
            stride: 20,
            count: 2,
            arguments: title_memory.as_ptr(),
            nodes: 0,
        };
        let null = std::ptr::null();
        let mut signature = null;
        assert_eq!(
            create(null, &desc, null, &SIGNATURE_IIDS[0], &mut signature),
            S_OK
        );
        title_memory.fill([9, 9, 9, 9]);
        let (stride, arguments) = layout(signature).expect("one of our signatures");
        assert_eq!(stride, 20);
        assert_eq!(arguments, &expected[..]);
        assert_eq!(
            slot::<extern "system" fn(This) -> u32>(signature, 0x10)(signature),
            0
        );
    }

    #[test]
    fn signatures_answer_their_interfaces_and_count_references() {
        let arguments = [[2u32, 4, 1, 1]]; // dispatch 4x1x1 groups
        let desc = Desc {
            stride: 12,
            count: 1,
            arguments: arguments.as_ptr(),
            nodes: 0,
        };
        let null = std::ptr::null();
        let mut signature = null;
        assert_eq!(
            create(null, &desc, null, &SIGNATURE_IIDS[0], &mut signature),
            S_OK
        );
        assert!(!signature.is_null());
        let query =
            slot::<extern "system" fn(This, *const Iid, *mut This) -> HRESULT>(signature, 0x0);
        // Every interface the signature answers to hands out the same object.
        for iid in &SIGNATURE_IIDS {
            let mut out = null;
            assert_eq!(query(signature, iid, &mut out), S_OK);
            assert_eq!(out as usize, signature as usize);
        }
        // An unknown interface writes null behind and reports it missing.
        let unknown = [0u8; 16];
        let mut out = signature;
        assert_eq!(query(signature, &unknown, &mut out), E_NOINTERFACE);
        assert!(out.is_null());
        // A null interface id is tolerated rather than matched.
        let mut out = signature;
        assert_eq!(query(signature, std::ptr::null(), &mut out), E_NOINTERFACE);
        assert!(out.is_null());
        // No room for the answer is a pointer error.
        assert_eq!(query(signature, &unknown, std::ptr::null_mut()), E_POINTER);
        // References: one from creation, four from the queries, one added;
        // each release hands back what is left, the last frees the signature.
        assert_eq!(
            slot::<extern "system" fn(This) -> u32>(signature, 0x8)(signature),
            6
        );
        let release = slot::<extern "system" fn(This) -> u32>(signature, 0x10);
        for left in [5, 4, 3, 2, 1, 0] {
            assert_eq!(release(signature), left);
        }
    }

    #[test]
    fn creation_without_an_out_pointer_only_probes() {
        let arguments = [[2u32, 4, 1, 1]]; // dispatch 4x1x1 groups
        let desc = Desc {
            stride: 12,
            count: 1,
            arguments: arguments.as_ptr(),
            nodes: 0,
        };
        // A known interface with no out pointer asks whether creation would
        // succeed; the signature is dropped instead of handed out.
        assert_eq!(
            create(
                std::ptr::null(),
                &desc,
                std::ptr::null(),
                &SIGNATURE_IIDS[0],
                std::ptr::null_mut()
            ),
            S_FALSE
        );
        // A null interface id is tolerated: no interface, no signature.
        let mut signature = std::ptr::null();
        assert_eq!(
            create(
                std::ptr::null(),
                &desc,
                std::ptr::null(),
                std::ptr::null(),
                &mut signature
            ),
            E_NOINTERFACE
        );
        assert!(signature.is_null());
    }

    #[test]
    fn misaligned_strides_and_rootless_binding_arguments_are_rejected_and_clear_out() {
        let null = std::ptr::null();
        let sentinel = std::ptr::dangling::<Object>();
        let dispatch = [[2u32, 4, 1, 1]];

        // Early `E_INVALIDARG` paths clear a non-null `_COM_Outptr_opt_` slot.
        let mut out = sentinel;
        assert_eq!(
            create(null, std::ptr::null(), null, &SIGNATURE_IIDS[0], &mut out),
            E_INVALIDARG
        );
        assert!(out.is_null());

        let mut out = sentinel;
        let missing_args = Desc {
            stride: 16,
            count: 1,
            arguments: std::ptr::null(),
            nodes: 0,
        };
        assert_eq!(
            create(null, &missing_args, null, &SIGNATURE_IIDS[0], &mut out),
            E_INVALIDARG
        );
        assert!(out.is_null());

        // `ByteStride` must be 4-byte aligned.
        for bad_stride in [1u32, 2, 3, 6, 18] {
            let mut out = sentinel;
            let desc = Desc {
                stride: bad_stride,
                count: 1,
                arguments: dispatch.as_ptr(),
                nodes: 0,
            };
            assert_eq!(
                create(null, &desc, null, &SIGNATURE_IIDS[0], &mut out),
                E_INVALIDARG,
                "stride {bad_stride}"
            );
            assert!(out.is_null(), "stride {bad_stride}");
        }

        // Root-binding arguments (`CONSTANT` 5, `CBV` 6, `SRV` 7, `UAV` 8,
        // `INCREMENTING_CONSTANT` 11) require a non-null root signature.
        let blob = [1u8, 2, 3, 4];
        let root_iid = guid(
            0xc54a6b66,
            0x72df,
            0x4ee8,
            [0x8b, 0xe5, 0xa9, 0x46, 0xa1, 0x42, 0x92, 0x14],
        );
        let mut root = null;
        assert_eq!(
            crate::root::create(null, 0, blob.as_ptr(), blob.len(), &root_iid, &mut root),
            S_OK
        );
        for kind in [5u32, 6, 7, 8, 11] {
            let args = [[kind, 0, 0, 1]];
            let desc = Desc {
                stride: 16,
                count: 1,
                arguments: args.as_ptr(),
                nodes: 0,
            };
            let mut out = sentinel;
            assert_eq!(
                create(null, &desc, null, &SIGNATURE_IIDS[0], &mut out),
                E_INVALIDARG,
                "kind {kind} without root"
            );
            assert!(out.is_null(), "kind {kind}");

            let mut out = null;
            assert_eq!(
                create(null, &desc, root, &SIGNATURE_IIDS[0], &mut out),
                S_OK,
                "kind {kind} with root"
            );
            assert_eq!(object::release(out), 0);
        }
        assert_eq!(object::release(root), 0);

        // Non-binding argument types succeed with a null root signature.
        for kind in [0u32, 1, 2, 3, 4, 9, 10] {
            let args = [[kind, 0, 0, 0]];
            let desc = Desc {
                stride: 68,
                count: 1,
                arguments: args.as_ptr(),
                nodes: 0,
            };
            let mut out = null;
            assert_eq!(
                create(null, &desc, null, &SIGNATURE_IIDS[0], &mut out),
                S_OK,
                "non-binding kind {kind}"
            );
            assert_eq!(object::release(out), 0);
        }
    }

    #[cfg(windows)]
    #[test]
    fn host_device_agrees_on_stride_alignment_and_root_requirement() {
        use windows::Win32::Graphics::Direct3D12::*;

        let Some(host) = crate::host::host() else {
            return;
        };
        let dispatch_arg = D3D12_INDIRECT_ARGUMENT_DESC {
            Type: D3D12_INDIRECT_ARGUMENT_TYPE_DISPATCH,
            ..Default::default()
        };
        let misaligned = D3D12_COMMAND_SIGNATURE_DESC {
            ByteStride: 18,
            NumArgumentDescs: 1,
            pArgumentDescs: &dispatch_arg,
            NodeMask: 0,
        };
        let mut sig: Option<ID3D12CommandSignature> = None;
        // SAFETY: valid stack description and out pointer.
        let err = unsafe { host.device.CreateCommandSignature(&misaligned, None, &mut sig) }
            .expect_err("misaligned ByteStride must fail on D3D12");
        assert_eq!(err.code().0, E_INVALIDARG);
        assert!(sig.is_none());

        let cbv_arg = D3D12_INDIRECT_ARGUMENT_DESC {
            Type: D3D12_INDIRECT_ARGUMENT_TYPE_CONSTANT_BUFFER_VIEW,
            Anonymous: D3D12_INDIRECT_ARGUMENT_DESC_0 {
                ConstantBufferView: D3D12_INDIRECT_ARGUMENT_DESC_0_2 {
                    RootParameterIndex: 0,
                },
            },
        };
        let rootless_cbv = D3D12_COMMAND_SIGNATURE_DESC {
            ByteStride: 8,
            NumArgumentDescs: 1,
            pArgumentDescs: &cbv_arg,
            NodeMask: 0,
        };
        // SAFETY: valid stack description and out pointer.
        let err = unsafe { host.device.CreateCommandSignature(&rootless_cbv, None, &mut sig) }
            .expect_err("root-binding argument without pRootSignature must fail on D3D12");
        assert_eq!(err.code().0, E_INVALIDARG);
        assert!(sig.is_none());
    }
}
