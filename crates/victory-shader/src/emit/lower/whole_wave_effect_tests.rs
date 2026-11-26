use super::tests::{lowers, lowers_structured, lowers_valu};
use crate::structure;

const S_ENDPGM: u32 = 0xBF81_0000;

#[test]
fn whole_wave_sections_reject_side_effects() {
    let save = 0xBE86_3DC1; // s_or_saveexec_b32 s6, -1
    let restore = 0xBEFE_0306; // s_mov_b32 exec_lo, s6
    let barrier = [0xBF8A_0000]; // s_barrier
    let store = [0xE070_0000, 0x8002_0000]; // buffer_store_dword v0, off, s[8:11], 0
    let mov = 0x7E00_0301; // v_mov_b32 v0, v1
    let load_lds = [0xE031_0000, 0x8002_0000]; // buffer_load_dword to LDS

    for effect in [barrier.as_slice(), store.as_slice()] {
        let outside = [&[mov, save, restore], effect].concat();
        lowers_valu(&outside).unwrap();

        let inside = [&[mov, save], effect, &[restore]].concat();
        let error = lowers_valu(&inside).expect_err("the translator cannot preserve this effect");
        assert!(
            matches!(
                error,
                crate::ShaderError::Unsupported {
                    offset: 36,
                    what: "a side effect in a whole-wave section",
                }
            ),
            "{error}"
        );
    }
    assert!(matches!(
        lowers_valu(&[mov, save, load_lds[0], load_lds[1], restore]),
        Err(crate::ShaderError::Unsupported {
            offset: 36,
            what: "a side effect in a whole-wave section",
        })
    ));
}

#[test]
fn whole_wave_sections_reject_structured_discard() {
    let words = [
        0xBE86_3DC1, // s_or_saveexec_b32 s6, -1
        0x7E00_0280, // v_mov_b32 v0, 0
        0xBEFE_0306, // s_mov_b32 exec_lo, s6
        S_ENDPGM,
    ];
    lowers(&words, 0).unwrap();
    // Supply the structured discard annotation on an otherwise pure instruction.
    let result = lowers_structured(&words, 0, |st| {
        st.discards.insert((0, 1), structure::Bool::TRUE);
    });
    assert!(
        matches!(
            result,
            Err(crate::ShaderError::Unsupported {
                offset: 4,
                what: "a side effect in a whole-wave section",
            })
        ),
        "{result:?}"
    );
}

#[test]
fn whole_wave_sections_reject_global_loads_into_lds() {
    let words = [
        0x7E04_0280, // v_mov_b32 v2, 0
        0xBE84_0380, // s_mov_b32 s4, 0
        0xBE85_0380, // s_mov_b32 s5, 0
        0xBE86_3DC1, // s_or_saveexec_b32 s6, -1
        0xDC30_A400,
        0x0004_0002, // global_load_dword v2, s[4:5] inst_offset:1024 lds
        0xBEFE_0306, // s_mov_b32 exec_lo, s6
        S_ENDPGM,
    ];
    let result = lowers(&words, 6 << 1);
    assert!(
        matches!(
            result,
            Err(crate::ShaderError::Unsupported {
                offset: 16,
                what: "a side effect in a whole-wave section",
            })
        ),
        "{result:?}"
    );
}
