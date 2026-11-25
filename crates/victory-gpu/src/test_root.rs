//! Root signatures for the test shaders, made from the `RS` string each
//! source defines (`#define RS "..."`) and passed with the pipeline. Not
//! every compiler keeps a `[RootSignature]` attribute in the code: Wine's
//! (vkd3d-shader) drops it, and has no root signature target to compile
//! the string alone, so a test that took the shader's own failed there.
//!
//! The forms the tests use: descriptor tables of SRV, UAV and CBV ranges
//! (`numDescriptors`, also `unbounded`, and `space`), root SRVs, UAVs and
//! CBVs, and root constants. Version 1.0: every descriptor and its data
//! volatile, as permissive as a host takes.

use windows::Win32::Graphics::Direct3D12::*;

/// The root signature `source` defines as `RS`.
pub(crate) fn from_source(device: &ID3D12Device, source: &str) -> ID3D12RootSignature {
    let rs = source
        .lines()
        .find_map(|l| l.trim().strip_prefix("#define RS "))
        .and_then(|rs| rs.trim().strip_prefix('"')?.strip_suffix('"'))
        .expect("a source that defines RS");
    make(device, rs)
}

/// The root signature in `rs`, a root signature string.
pub(crate) fn make(device: &ID3D12Device, rs: &str) -> ID3D12RootSignature {
    let parameters: Vec<(&str, Vec<&str>)> = items(rs).into_iter().map(call).collect();
    // Each table's ranges, kept until the description is serialised.
    let tables: Vec<Vec<D3D12_DESCRIPTOR_RANGE>> = parameters
        .iter()
        .filter(|(name, _)| *name == "DescriptorTable")
        .map(|(_, ranges)| ranges.iter().map(|r| range(r)).collect())
        .collect();
    let mut table = tables.iter();
    let parameters: Vec<D3D12_ROOT_PARAMETER> = parameters
        .iter()
        .map(|(name, args)| {
            let (kind, anonymous) = match *name {
                "DescriptorTable" => {
                    let ranges = table.next().unwrap();
                    (
                        D3D12_ROOT_PARAMETER_TYPE_DESCRIPTOR_TABLE,
                        D3D12_ROOT_PARAMETER_0 {
                            DescriptorTable: D3D12_ROOT_DESCRIPTOR_TABLE {
                                NumDescriptorRanges: ranges.len() as u32,
                                pDescriptorRanges: ranges.as_ptr(),
                            },
                        },
                    )
                }
                "RootConstants" => (
                    D3D12_ROOT_PARAMETER_TYPE_32BIT_CONSTANTS,
                    D3D12_ROOT_PARAMETER_0 {
                        Constants: D3D12_ROOT_CONSTANTS {
                            ShaderRegister: register(args, 'b'),
                            RegisterSpace: number(args, "space").unwrap_or(0),
                            Num32BitValues: number(args, "num32BitConstants")
                                .expect("a constant count"),
                        },
                    },
                ),
                root => {
                    let (kind, class) = match root {
                        "SRV" => (D3D12_ROOT_PARAMETER_TYPE_SRV, 't'),
                        "UAV" => (D3D12_ROOT_PARAMETER_TYPE_UAV, 'u'),
                        "CBV" => (D3D12_ROOT_PARAMETER_TYPE_CBV, 'b'),
                        other => panic!("root parameter {other} in {rs}"),
                    };
                    (
                        kind,
                        D3D12_ROOT_PARAMETER_0 {
                            Descriptor: D3D12_ROOT_DESCRIPTOR {
                                ShaderRegister: register(args, class),
                                RegisterSpace: number(args, "space").unwrap_or(0),
                            },
                        },
                    )
                }
            };
            D3D12_ROOT_PARAMETER {
                ParameterType: kind,
                Anonymous: anonymous,
                ShaderVisibility: D3D12_SHADER_VISIBILITY_ALL,
            }
        })
        .collect();
    let desc = D3D12_ROOT_SIGNATURE_DESC {
        NumParameters: parameters.len() as u32,
        pParameters: parameters.as_ptr(),
        ..Default::default()
    };
    let mut blob = None;
    let mut errors = None;
    // SAFETY: the description and its ranges live through the call; the
    // blobs are read while they live.
    unsafe {
        let made = D3D12SerializeRootSignature(
            &desc,
            D3D_ROOT_SIGNATURE_VERSION_1,
            &mut blob,
            Some(&mut errors),
        );
        let text = errors.map(|e| {
            String::from_utf8_lossy(std::slice::from_raw_parts(
                e.GetBufferPointer().cast::<u8>(),
                e.GetBufferSize(),
            ))
            .into_owned()
        });
        assert!(made.is_ok(), "root signature {rs}: {text:?}");
        let blob = blob.unwrap();
        let bytes =
            std::slice::from_raw_parts(blob.GetBufferPointer().cast::<u8>(), blob.GetBufferSize());
        device.CreateRootSignature(0, bytes).unwrap()
    }
}

/// A table's range: `SRV(t0, numDescriptors=2)`.
fn range(item: &str) -> D3D12_DESCRIPTOR_RANGE {
    let (name, args) = call(item);
    let (kind, class) = match name {
        "SRV" => (D3D12_DESCRIPTOR_RANGE_TYPE_SRV, 't'),
        "UAV" => (D3D12_DESCRIPTOR_RANGE_TYPE_UAV, 'u'),
        "CBV" => (D3D12_DESCRIPTOR_RANGE_TYPE_CBV, 'b'),
        other => panic!("descriptor range {other} in {item}"),
    };
    let count = match option(&args, "numDescriptors") {
        Some("unbounded") => u32::MAX,
        Some(n) => n.parse().expect("a descriptor count"),
        None => 1,
    };
    D3D12_DESCRIPTOR_RANGE {
        RangeType: kind,
        NumDescriptors: count,
        BaseShaderRegister: register(&args, class),
        RegisterSpace: number(&args, "space").unwrap_or(0),
        OffsetInDescriptorsFromTableStart: D3D12_DESCRIPTOR_RANGE_OFFSET_APPEND,
    }
}

/// `text`'s comma-separated items, split outside parentheses.
fn items(text: &str) -> Vec<&str> {
    let (mut items, mut depth, mut start) = (Vec::new(), 0, 0);
    for (i, c) in text.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => depth -= 1,
            ',' if depth == 0 => {
                items.push(text[start..i].trim());
                start = i + 1;
            }
            _ => {}
        }
    }
    items.push(text[start..].trim());
    items.retain(|i| !i.is_empty());
    items
}

/// An item's name and arguments: `SRV(t0, space=2)` is `SRV`, `t0` and
/// `space=2`.
fn call(item: &str) -> (&str, Vec<&str>) {
    let (name, rest) = item
        .split_once('(')
        .unwrap_or_else(|| panic!("no arguments in {item}"));
    let args = rest
        .trim_end()
        .strip_suffix(')')
        .unwrap_or_else(|| panic!("unclosed {item}"));
    (name.trim(), items(args))
}

/// The register of class `class` among `args`: 0 for `t0`.
fn register(args: &[&str], class: char) -> u32 {
    args.iter()
        .find_map(|a| a.strip_prefix(class)?.parse().ok())
        .unwrap_or_else(|| panic!("no {class} register in {args:?}"))
}

/// The value of `key=value` among `args`.
fn option<'a>(args: &[&'a str], key: &str) -> Option<&'a str> {
    args.iter().find_map(|a| {
        let (k, v) = a.split_once('=')?;
        (k.trim() == key).then(|| v.trim())
    })
}

/// The number in `key=value` among `args`.
fn number(args: &[&str], key: &str) -> Option<u32> {
    option(args, key).map(|v| {
        v.parse()
            .unwrap_or_else(|_| panic!("{key}={v} is no number"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_signature_strings_split_into_their_parts() {
        let rs = "DescriptorTable(SRV(t0, numDescriptors=unbounded)), SRV(t0, space=2), UAV(u0), RootConstants(num32BitConstants=3, b0)";
        let parts: Vec<_> = items(rs).into_iter().map(call).collect();
        assert_eq!(
            parts,
            [
                ("DescriptorTable", vec!["SRV(t0, numDescriptors=unbounded)"]),
                ("SRV", vec!["t0", "space=2"]),
                ("UAV", vec!["u0"]),
                ("RootConstants", vec!["num32BitConstants=3", "b0"]),
            ]
        );
        let r = range("SRV(t0, numDescriptors=unbounded)");
        assert_eq!(
            (r.RangeType, r.NumDescriptors, r.BaseShaderRegister),
            (D3D12_DESCRIPTOR_RANGE_TYPE_SRV, u32::MAX, 0)
        );
        let r = range("UAV(u3, numDescriptors=2, space=1)");
        assert_eq!(
            (
                r.RangeType,
                r.NumDescriptors,
                r.BaseShaderRegister,
                r.RegisterSpace
            ),
            (D3D12_DESCRIPTOR_RANGE_TYPE_UAV, 2, 3, 1)
        );
        assert_eq!(
            number(&["num32BitConstants=3", "b0"], "num32BitConstants"),
            Some(3)
        );
        assert_eq!(register(&["num32BitConstants=3", "b1"], 'b'), 1);
    }

    /// Every form the tests use makes a root signature on the host.
    #[test]
    fn the_tests_root_signatures_are_made() {
        let Some(host) = crate::host::host() else {
            return; // no host GPU here
        };
        for rs in [
            "DescriptorTable(SRV(t0), UAV(u0), SRV(t1))",
            "DescriptorTable(SRV(t0)), DescriptorTable(UAV(u0)), RootConstants(num32BitConstants=2, b0)",
            "DescriptorTable(SRV(t0)), UAV(u1)",
            "DescriptorTable(UAV(u0, numDescriptors=2))",
            "DescriptorTable(SRV(t0, numDescriptors=2)), UAV(u1)",
            "DescriptorTable(SRV(t0, numDescriptors=unbounded)), SRV(t0, space=2), UAV(u0), RootConstants(num32BitConstants=3, b0)",
        ] {
            make(&host.device, rs);
        }
        let source = "#define RS \"DescriptorTable(UAV(u0))\"\nRWTexture2D<uint> o : register(u0);";
        from_source(&host.device, source);
    }
}
