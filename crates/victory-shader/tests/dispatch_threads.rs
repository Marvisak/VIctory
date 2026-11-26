#[path = "fixtures/dispatch.rs"]
mod fixture;

use victory_shader::emit::{self, DISPATCH_THREADS};

#[test]
fn partial_and_group_dispatch_translate_to_distinct_programs() {
    let kernel = fixture::kernel(false);
    let options = emit::Options { view_marks: true };
    let full = emit::translate_with(&kernel, options).unwrap();
    let partial = emit::translate_partial_dispatch(&kernel, options).unwrap();
    assert_ne!(partial.dxil, full.dxil);
    let limits: Vec<_> = partial
        .user_data
        .iter()
        .filter(|u| u.root == DISPATCH_THREADS)
        .collect();
    assert_eq!(limits.len(), 1);
    assert_eq!(limits[0].dword, 0);
    assert!(full.user_data.iter().all(|u| u.root != DISPATCH_THREADS));
    assert!(full.user_data.iter().all(|u| u.index < limits[0].index));

    #[cfg(windows)]
    if let Some(path) = emit::validator::Validator::default_path() {
        let mut validator = emit::validator::Validator::load(Some(&path)).unwrap();
        for mut dxil in [full.dxil, partial.dxil] {
            validator.validate(&mut dxil).unwrap();
        }
    }
}

#[test]
fn partial_dispatch_refuses_a_divergent_group_barrier() {
    let kernel = fixture::kernel(true);
    emit::translate(&kernel).expect("uniform group barrier");
    let error = emit::translate_partial_dispatch(&kernel, emit::Options::default()).unwrap_err();
    assert!(matches!(
        error,
        victory_shader::ShaderError::Unsupported {
            what: "a partial dispatch with a group barrier",
            ..
        }
    ));
}
