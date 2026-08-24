//! C++ lowering integration tests (first-step C++ support).

mod common;

use common::{default_opts, fixture, fn_name};
use trace_analysis::{analyze, AnalysisResult, ResolutionKind};
use trace_ir::{FnId, Program};
use trace_parse::build_program;

fn direct_targets(program: &Program, analysis: &AnalysisResult, caller: &str) -> Vec<String> {
    analysis
        .call_edges
        .iter()
        .filter(|e| fn_name(&program, e.caller) == caller && e.resolution == ResolutionKind::Direct)
        .map(|e| fn_name(&program, e.callee))
        .collect()
}

#[test]
fn cpp_virtual_dispatch_expands_to_overrides() {
    let root = fixture("cpp_basic");
    let program = build_program(&root, &default_opts(&root)).expect("build");
    let (_pag, analysis) = analyze(&program);

    let targets = direct_targets(&program, &analysis, "main");
    assert!(
        targets.iter().any(|t| t == "gfx::Shape::area"),
        "virtual s->area should target base Shape::area, got {targets:?}"
    );
    assert!(
        targets.iter().any(|t| t == "gfx::Circle::area"),
        "virtual s->area should target override Circle::area, got {targets:?}"
    );
}

#[test]
fn cpp_non_virtual_member_call_exact() {
    let root = fixture("cpp_basic");
    let program = build_program(&root, &default_opts(&root)).expect("build");
    let (_pag, analysis) = analyze(&program);

    let hits = analysis
        .call_edges
        .iter()
        .filter(|e| {
            fn_name(&program, e.caller) == "main"
                && fn_name(&program, e.callee) == "gfx::Shape::common"
        })
        .count();
    assert_eq!(hits, 1, "s->common must resolve to exactly one site-edge");

    let common = program
        .symbols
        .resolve_function("gfx::Shape::common")
        .expect("common defined");
    assert!(
        program.symbols.function(common).is_defined,
        "out-of-class definition must be the merged entry"
    );
}

#[test]
fn cpp_header_inline_method_dedups_with_out_of_class_uses() {
    let root = fixture("cpp_basic");
    let program = build_program(&root, &default_opts(&root)).expect("build");
    let (_pag, analysis) = analyze(&program);

    // radius is defined inline in util.hpp; main calls it once.
    let hits = analysis
        .call_edges
        .iter()
        .filter(|e| fn_name(&program, e.callee) == "gfx::Circle::radius")
        .count();
    assert_eq!(hits, 1, "header-inline method should dedup across TUs");
}

#[test]
fn cpp_ctor_and_dtor_sites() {
    let root = fixture("cpp_basic");
    let program = build_program(&root, &default_opts(&root)).expect("build");
    let (_pag, analysis) = analyze(&program);

    let targets = direct_targets(&program, &analysis, "main");
    assert!(
        targets.iter().any(|t| t == "gfx::Circle::Circle"),
        "new Circle() should emit ctor edge"
    );
    assert!(
        targets.iter().any(|t| t == "gfx::Shape::~Shape"),
        "delete via base ptr should emit base dtor"
    );
    assert!(
        targets.iter().any(|t| t == "gfx::Circle::~Circle"),
        "virtual dtor expansion should include derived dtor"
    );
}

#[test]
fn cpp_overload_resolution_by_arity() {
    let root = fixture("cpp_basic");
    let program = build_program(&root, &default_opts(&root)).expect("build");
    let (_pag, analysis) = analyze(&program);

    let add_edges: Vec<FnId> = analysis
        .call_edges
        .iter()
        .filter(|e| {
            fn_name(&program, e.caller) == "main"
                && fn_name(&program, e.callee).rsplit("::").next() == Some("add")
        })
        .map(|e| e.callee)
        .collect();
    assert_eq!(add_edges.len(), 2, "each arity resolves one overload");

    for callee in add_edges {
        let params = program.symbols.function(callee).params.len();
        let body_marks = direct_targets(&program, &analysis, &fn_name(&program, callee));
        if params == 2 {
            assert!(body_marks.contains(&"mark_i".to_string()));
        } else if params == 1 {
            assert!(body_marks.contains(&"mark_d".to_string()));
        } else {
            panic!("unexpected add overload with {params} params");
        }
    }
}

#[test]
fn cpp_anonymous_namespace_is_internal() {
    let root = fixture("cpp_basic");
    let program = build_program(&root, &default_opts(&root)).expect("build");
    let (_pag, analysis) = analyze(&program);

    assert!(
        program.symbols.resolve_function("hidden").is_none(),
        "anon-namespace functions must not be in external lookup"
    );
    assert!(has_direct(&program, &analysis, "hidden", "util::tag"));
}

fn has_direct(program: &Program, analysis: &AnalysisResult, caller: &str, callee: &str) -> bool {
    analysis.call_edges.iter().any(|e| {
        fn_name(&program, e.caller) == caller
            && fn_name(&program, e.callee) == callee
            && e.resolution == ResolutionKind::Direct
    })
}

// --- cpp_more: overload ties, templates, multiple inheritance,
// ctor-initializer lists, static member functions ---

fn edges_to(
    program: &Program,
    analysis: &AnalysisResult,
    caller: &str,
    callee_suffix: &str,
    resolution: ResolutionKind,
) -> Vec<String> {
    analysis
        .call_edges
        .iter()
        .filter(|e| {
            fn_name(&program, e.caller) == caller
                && fn_name(&program, e.callee).ends_with(callee_suffix)
                && e.resolution == resolution
        })
        .map(|e| fn_name(&program, e.callee))
        .collect()
}

#[test]
fn cpp_overload_tie_emits_both_sites() {
    let root = fixture("cpp_more");
    let program = build_program(&root, &default_opts(&root)).expect("build");
    let (_pag, analysis) = analyze(&program);

    let hits = edges_to(&program, &analysis, "drive", "tie", ResolutionKind::Direct);
    assert_eq!(
        hits.len(),
        2,
        "same-arity overload tie must emit one site per candidate"
    );
}

#[test]
fn cpp_template_class_method_resolves_by_primary_name() {
    let root = fixture("cpp_more");
    let program = build_program(&root, &default_opts(&root)).expect("build");
    let (_pag, analysis) = analyze(&program);

    assert!(
        has_direct(&program, &analysis, "drive", "Box::put"),
        "Box<Widget>::put call should resolve under primary name"
    );
}

#[test]
fn cpp_virtual_call_through_base_of_multiple_inheritance() {
    let root = fixture("cpp_more");
    let program = build_program(&root, &default_opts(&root)).expect("build");
    let (_pag, analysis) = analyze(&program);

    assert!(has_direct(&program, &analysis, "drive", "A::fa"));
    assert!(
        has_direct(&program, &analysis, "drive", "AB::fa"),
        "virtual expansion must include the multiple-inheritance override"
    );
    assert!(!has_direct(&program, &analysis, "drive", "B::fb"));
}

#[test]
fn cpp_ctor_initializer_list_targets() {
    let root = fixture("cpp_more");
    let program = build_program(&root, &default_opts(&root)).expect("build");
    let (_pag, analysis) = analyze(&program);

    assert!(has_direct(&program, &analysis, "D::D", "Base::Base"));
    assert!(has_direct(&program, &analysis, "D::D", "Member::Member"));
    // D d2(5): constructor-declaration with argument list.
    assert!(has_direct(&program, &analysis, "drive", "D::D"));
}

#[test]
fn cpp_static_member_function_resolves() {
    let root = fixture("cpp_more");
    let program = build_program(&root, &default_opts(&root)).expect("build");
    let (_pag, analysis) = analyze(&program);

    let hits = edges_to(
        &program,
        &analysis,
        "drive",
        "S::Make",
        ResolutionKind::Direct,
    );
    assert!(hits.len() >= 2, "both S::Make calls should resolve");
}

#[test]
fn cpp_inherited_non_virtual_via_derived_receiver() {
    let root = fixture("cpp_more");
    let program = build_program(&root, &default_opts(&root)).expect("build");
    let (_pag, analysis) = analyze(&program);

    assert!(
        has_direct(&program, &analysis, "drive", "Base::base_value"),
        "d2.base_value() should walk up to Base"
    );
    assert!(has_direct(&program, &analysis, "sink_w", "Widget::make"));
}

// --- cpp_flow: cross-language C dispatcher + C++ impl (HDF sbuf pattern) ---

#[test]
fn cpp_impl_registered_into_c_ops_table_resolves_indirect() {
    let root = fixture("cpp_flow");
    let program = build_program(&root, &default_opts(&root)).expect("build");
    let (_pag, analysis) = analyze(&program);

    for target in ["RawImplRead", "MParcelImplRead"] {
        let hits = edges_to(
            &program,
            &analysis,
            "Read",
            target,
            ResolutionKind::Indirect,
        );
        assert_eq!(
            hits.len(),
            1,
            "{target} must be an indirect target of s->impl->read exactly once"
        );
    }
}
