//! Binary64 sums inside a guest Wave64 loop whose second iteration has fewer lanes.
use super::{
    Host, NONE, ONE_AT_A_TIME, S_ENDPGM, VCC, int, lower_program_for_wave, mubuf, run, s,
    s_load_dwordx4, sop1, sop2, v, validator, vop1, vop2, vop3, vopc,
};
use windows::Win32::Graphics::Direct3D12::*;

fn masked_loop() -> Vec<u32> {
    let mut w = vec![sop1(3, 5, int(0))];
    w.extend(s_load_dwordx4(8, 4, 0));
    w.extend(s_load_dwordx4(12, 4, 0x20));
    // Each lane loads one record: input double, trip count, untouched marker.
    w.extend(vop3(0x346, 10, [s(6), int(6), v(0)], NONE));
    w.push(vop2(0x1a, 10, int(4), 10));
    w.extend(mubuf(0x0e, 1, 10, 8));
    for r in 5..9 {
        w.push(vop1(1, r, v(4)));
    }
    w.extend(mubuf(0x1e, 5, 10, 12));
    w.push(vop1(1, 11, int(0))); // iteration
    let start = w.len();
    // Temporarily enable the full wave. Each inactive lane contributes +0.
    w.push(sop1(0x25, 20, 193)); // s_or_saveexec_b64 s[20:21], -1
    for d in 0..2 {
        w.extend(vop3(0x101, 5 + d, [int(0), v(1 + d), s(20)], NONE));
    }
    for ctrl in [0xb1, 0x4e, 0x164, 0x168] {
        for d in 0..2 {
            w.extend([
                vop1(1, 7 + d, 0xfa),
                0xff00_0000 | ctrl << 8 | u32::from(5 + d),
            ]);
        }
        w.extend(vop3(0x164, 5, [v(7), v(5), 0], NONE)); // v_add_f64
    }
    for d in 0..2 {
        w.extend(vop3(0x378, 7 + d, [v(5 + d), int(0), int(0)], NONE));
    }
    w.extend(vop3(0x164, 5, [v(7), v(5), 0], NONE)); // combine 16-lane rows
    for d in 0..2 {
        w.extend(vop3(0x360, 22 + d, [v(5 + d), int(63), 0], NONE));
    }
    w.extend(vop3(0x164, 5, [s(22), v(5), 0], NONE)); // combine half-wave sums
    for d in 0..2 {
        w.extend(vop3(0x360, 24 + d, [v(5 + d), int(31), 0], NONE));
    }
    w.push(sop1(4, 126, s(20))); // restore this iteration's EXEC
    for d in 0..2 {
        w.push(vop1(1, 5 + d, s(24 + d)));
    }
    w.extend(mubuf(0x1d, 5, 10, 12)); // write both dwords at this iteration's offset
    w.push(vop2(0x25, 10, int(8), 10));
    w.push(vop2(0x25, 11, int(1), 11));
    w.push(vopc(0xc1, v(11), 3)); // iteration < lane trip count
    w.push(sop2(0x0f, 126, 126, VCC));
    let back = i16::try_from(start as isize - w.len() as isize - 1).unwrap();
    w.push(0xbf89_0000 | u32::from(back as u16)); // s_cbranch_execnz
    w.push(S_ENDPGM);
    w
}

#[test]
fn a_wave64_binary64_sum_observes_early_loop_exits() {
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let mut shader = lower_program_for_wave(&masked_loop(), true).expect("lower guest Wave64");
    if let Ok(mut validator) =
        validator::Validator::load(validator::Validator::default_path().as_deref())
    {
        validator
            .validate(&mut shader.dxil)
            .expect("validate Wave64 DXIL");
    }
    let marker = 0xdead_beef;
    let mut inputs = Vec::new();
    let mut expected = Vec::new();
    // Multiples of 64 ulps at 1.0 keep every intermediate sum exact. The low
    // result word is nonzero, and different lanes contribute distinct values.
    let terms: [f64; 64] =
        std::array::from_fn(|i| f64::from_bits(1.0f64.to_bits() + 64 * (i as u64 + 1)));
    let first = terms.iter().sum::<f64>().to_bits();
    for mask in [u64::MAX << 32, u64::from(u32::MAX), 1 << 63] {
        let second = terms
            .iter()
            .enumerate()
            .filter(|(i, _)| mask >> i & 1 != 0)
            .map(|(_, x)| x)
            .sum::<f64>()
            .to_bits();
        for (i, term) in terms.into_iter().enumerate() {
            let active = mask >> i & 1 != 0;
            let bits = term.to_bits();
            inputs.push([
                bits as u32,
                (bits >> 32) as u32,
                if active { 2 } else { 1 },
                marker,
            ]);
            let tail = if active {
                [second as u32, (second >> 32) as u32]
            } else {
                [marker; 2]
            };
            expected.push([first as u32, (first >> 32) as u32, tail[0], tail[1]]);
        }
    }
    for (name, warp) in [("hardware", false), ("WARP", true)] {
        let Ok(host) = Host::new(warp) else {
            eprintln!("{name}: no SM 6.6 device; skipped");
            continue;
        };
        let mut wave = D3D12_FEATURE_DATA_D3D12_OPTIONS1::default();
        let mut base = D3D12_FEATURE_DATA_D3D12_OPTIONS::default();
        // SAFETY: both feature IDs match their output types and sizes.
        unsafe {
            host.device
                .CheckFeatureSupport(
                    D3D12_FEATURE_D3D12_OPTIONS1,
                    (&raw mut wave).cast(),
                    size_of_val(&wave) as u32,
                )
                .unwrap();
            host.device
                .CheckFeatureSupport(
                    D3D12_FEATURE_D3D12_OPTIONS,
                    (&raw mut base).cast(),
                    size_of_val(&base) as u32,
                )
                .unwrap();
        }
        if !wave.WaveOps.as_bool()
            || !(wave.WaveLaneCountMin..=wave.WaveLaneCountMax).contains(&64)
            || !base.DoublePrecisionFloatShaderOps.as_bool()
        {
            eprintln!(
                "{name}: native Wave64/FP64 unavailable (lanes {}..{}); skipped",
                wave.WaveLaneCountMin, wave.WaveLaneCountMax
            );
            continue;
        }
        let got = run(&host, &shader, &inputs, false).expect("dispatch masked loop");
        assert_eq!(got.len(), expected.len());
        let mismatches: Vec<_> = got
            .chunks(64)
            .zip(expected.chunks(64))
            .enumerate()
            .filter_map(|(group, (rows, reference))| {
                let lane = rows
                    .iter()
                    .zip(reference)
                    .position(|(row, want)| row != want)?;
                Some((group, lane, rows[lane], reference[lane]))
            })
            .collect();
        assert!(
            mismatches.is_empty(),
            "{name}, first mismatching (group, lane, got, expected) per group: {mismatches:x?}"
        );
        eprintln!("{name}: verified both iterations in all three Wave64 groups");
    }
}
