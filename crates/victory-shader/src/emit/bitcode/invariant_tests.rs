//! Negative tests for the builder invariants: each misuse used to
//! `panic!` (aborting the title when reached from pipeline creation); now
//! it is recorded and [`Module::write`] fails with
//! [`ShaderError::Internal`].

use super::*;

/// A module with an `i32` type and one block, as the emitter starts.
fn module() -> (Module, usize, Ty) {
    let mut m = Module::default();
    let i32t = m.int(32);
    let b = m.new_block();
    (m, b, i32t)
}

/// An instruction result (not a constant): `1 + 1`.
fn inst(m: &mut Module, b: usize) -> V {
    let one = m.i32(1);
    m.bin(b, Bin::Add, one, one)
}

/// The internal error `write` reports, which must name `what`.
fn assert_broken(m: &Module, what: &str) {
    match m.write() {
        Err(ShaderError::Internal(e)) => assert!(e.contains(what), "{e}"),
        Err(e) => panic!("expected an internal error, got {e}"),
        Ok(_) => panic!("a broken module was written"),
    }
}

#[test]
fn aggregate_of_a_non_constant() {
    let (mut m, b, i32t) = module();
    let (one, x) = (m.i32(1), inst(&mut m, b));
    let arr = m.array(2, i32t);
    let agg = m.aggregate(arr, &[one, x]);
    assert_eq!(m.type_of(agg), arr, "the placeholder keeps the type");
    assert_broken(&m, "aggregate of a non-constant");
}

#[test]
fn global_initialised_with_a_non_constant() {
    let (mut m, b, _) = module();
    let x = inst(&mut m, b);
    let g = m.constant_global("code", x);
    assert!(matches!(g, V::Global(_)));
    assert_broken(&m, "global code initialised with a non-constant");
}

#[test]
fn extract_from_a_scalar() {
    let (mut m, b, _) = module();
    let one = m.i32(1);
    m.extract(b, one, 0);
    assert_broken(&m, "extractvalue 0 from Int(32)");
}

#[test]
fn extract_past_the_fields() {
    let (mut m, b, i32t) = module();
    let s = m.named_struct("pair", vec![i32t, i32t]);
    let u = m.undef(s);
    m.extract(b, u, 2);
    assert_broken(&m, "extractvalue 2 from Struct");
}

#[test]
fn extract_past_an_array() {
    let (mut m, b, i32t) = module();
    let a = m.array(2, i32t);
    let u = m.undef(a);
    m.extract(b, u, 2);
    assert_broken(&m, "extractvalue 2 from Array");
}

#[test]
fn call_of_a_non_function() {
    let (mut m, b, _) = module();
    let one = m.i32(1);
    m.call(b, one, vec![]);
    assert_broken(&m, "call of a non-function");
}

#[test]
fn call_of_a_function_without_a_function_type() {
    let (mut m, b, i32t) = module();
    let f = m.function("f", i32t, None, false);
    m.call(b, f, vec![]);
    assert_broken(&m, "call of a function of type Int(32)");
}

#[test]
fn element_of_a_non_pointer() {
    let (mut m, b, _) = module();
    let one = m.i32(1);
    m.element(b, one, one);
    assert_broken(&m, "element of a non-pointer");
}

#[test]
fn element_of_a_non_array() {
    let (mut m, b, i32t) = module();
    let g = m.global("lds", i32t, 3);
    let zero = m.i32(0);
    m.element(b, g, zero);
    assert_broken(&m, "element of a pointer to a non-array");
}

#[test]
fn load_from_a_non_pointer() {
    let (mut m, b, _) = module();
    let one = m.i32(1);
    m.load(b, one);
    assert_broken(&m, "load from a non-pointer");
}

#[test]
fn set_phi_of_a_constant() {
    let (mut m, _, _) = module();
    let one = m.i32(1);
    m.set_phi(one, vec![]);
    assert_broken(&m, "incoming values set on a non-phi");
}

#[test]
fn set_phi_of_a_non_phi_instruction() {
    let (mut m, b, _) = module();
    let x = inst(&mut m, b);
    m.set_phi(x, vec![(x, b)]);
    assert_broken(&m, "incoming values set on a non-phi");
}

#[test]
fn the_first_broken_invariant_is_reported() {
    let (mut m, b, _) = module();
    let one = m.i32(1);
    // The placeholder of the first misuse feeds the second.
    let p = m.load(b, one);
    m.element(b, p, one);
    assert_broken(&m, "load from a non-pointer");
}

#[test]
fn a_broken_module_fails_the_shader() {
    let mut s = super::super::shader::Shader::new(true);
    let one = s.m.i32(1);
    s.m.load(s.entry, one);
    s.m.terminate(s.entry, Term::Ret);
    assert!(matches!(s.finish(), Err(ShaderError::Internal(_))));
}

#[test]
fn a_function_value_has_its_pointer_type() {
    let (mut m, _, i32t) = module();
    let fty = m.func_ty(i32t, vec![]);
    // No `m.ptr(fty)` first: declaring the function is enough.
    let f = m.function("f", fty, None, false);
    let t = m.type_of(f);
    assert_eq!(t, m.ptr(fty));
}

#[test]
fn a_module_without_blocks() {
    assert_broken(&Module::default(), "no blocks");
}

#[test]
fn an_unterminated_block() {
    let (mut m, b, _) = module();
    inst(&mut m, b);
    assert_broken(&m, "block 0 is not terminated");
}

#[test]
fn a_use_of_a_void_value() {
    let (mut m, b, _) = module();
    let void = m.void();
    let fty = m.func_ty(void, vec![]);
    let f = m.function("f", fty, None, false);
    let v = m.call(b, f, vec![]);
    m.bin(b, Bin::Add, v, v);
    m.terminate(b, Term::Ret);
    assert_broken(&m, "use of a void or unplaced value");
}

#[test]
fn a_use_of_a_value_from_an_unreachable_block() {
    let (mut m, b, _) = module();
    let dead = m.new_block();
    let x = inst(&mut m, dead);
    m.terminate(dead, Term::Ret);
    m.bin(b, Bin::Add, x, x);
    m.terminate(b, Term::Ret);
    assert_broken(&m, "use of a void or unplaced value");
}

#[test]
fn a_forward_reference() {
    let (mut m, b, _) = module();
    let later = m.new_block();
    let x = inst(&mut m, later);
    m.bin(b, Bin::Add, x, x);
    m.terminate(b, Term::Br(later));
    m.terminate(later, Term::Ret);
    assert_broken(&m, "forward reference");
}

#[test]
fn a_branch_on_a_later_value() {
    let (mut m, b, _) = module();
    let (t, f) = (m.new_block(), m.new_block());
    let one = m.i32(1);
    let c = m.cmp(t, pred::EQ, one, one);
    m.terminate(b, Term::CondBr(c, t, f));
    m.terminate(t, Term::Ret);
    m.terminate(f, Term::Ret);
    assert_broken(&m, "forward reference");
}

#[test]
fn an_instruction_in_a_missing_block() {
    let (mut m, b, _) = module();
    let one = m.i32(1);
    m.bin(b + 7, Bin::Add, one, one);
    m.terminate(b, Term::Ret);
    assert_broken(&m, "block 7 does not exist");
}

#[test]
fn a_phi_in_a_missing_block() {
    let (mut m, b, i32t) = module();
    m.phi(b + 7, i32t);
    m.terminate(b, Term::Ret);
    assert_broken(&m, "block 7 does not exist");
}

#[test]
fn a_branch_to_a_missing_block() {
    let (mut m, b, _) = module();
    m.terminate(b, Term::Br(b + 7));
    assert_broken(&m, "block 7 does not exist");
}

#[test]
fn a_terminator_of_a_missing_block() {
    let (mut m, b, _) = module();
    m.terminate(b, Term::Ret);
    m.terminate(b + 7, Term::Ret);
    assert_broken(&m, "block 7 does not exist");
}

#[test]
fn a_phi_from_a_missing_block() {
    let (mut m, b, i32t) = module();
    let next = m.new_block();
    let p = m.phi(next, i32t);
    let one = m.i32(1);
    m.set_phi(p, vec![(one, b + 7)]);
    m.bin(next, Bin::Add, p, p);
    m.terminate(b, Term::Br(next));
    m.terminate(next, Term::Ret);
    assert_broken(&m, "block 7 does not exist");
}

#[test]
fn a_block_closed_twice() {
    let mut w = Writer::default();
    w.enter(8, 3);
    assert_eq!(w.exit(), Ok(()));
    assert!(matches!(w.exit(), Err(ShaderError::Internal(_))));
}
