//! A pixel shader that reads and writes nothing.
//!
//! The software adapter (WARP) loses the device at the first draw of a mesh
//! pipeline that has no pixel shader (a depth-only pass): seen with a mesh
//! shader of our own drawing one triangle, directly and through
//! ExecuteIndirect. With this shader bound the same draws run and write the
//! same depth, so the host gives such pipelines one on that adapter.

use super::bitcode::Term;
use super::shader::Shader;

/// The DXIL container of a pixel shader with no inputs and no outputs.
pub fn empty_ps() -> Vec<u8> {
    let mut s = Shader::new(true);
    let b = s.entry;
    s.m.terminate(b, Term::Ret);
    s.finish()
}

#[cfg(all(test, windows))]
mod tests {
    use super::super::validator::Validator;

    /// The system's validator takes it (skipped without one).
    #[test]
    fn the_empty_pixel_shader_validates() {
        let ps = super::empty_ps();
        assert!(!ps.is_empty());
        if let Ok(mut v) = Validator::load(Validator::default_path().as_deref()) {
            v.validate(&mut ps.clone()).unwrap();
        }
    }
}
