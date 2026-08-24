use crate::deps::IncludeGraph;
use crate::discover::discover_source_files;
use crate::index_cache::IndexSourceCache;
use crate::merge::{merge_unit_index, UnitIndex};
use crate::parse::node_text;
use rayon::prelude::*;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use trace_ir::{
    CallSite, Diagnostic, DiagnosticSeverity, FieldId, FlowConstraint, FnId, Function, Linkage,
    Program, ReturnFlow, Span, StorageClass, TypeDesc, VarId, Variable,
};
use trace_preproc::{macro_table_from_defines, MacroTable, PreprocessOptions};
use tree_sitter::Node;

/// A function-name reference whose resolution was deferred because the
/// function is only defined later in the translation unit. C requires no
/// forward declaration for these uses when the definition appears later
/// in the same file, so lowering must not depend on encounter order.
enum PendingFnRef {
    /// `base.field = FnName`: emit `AddrOfFn` into a temp and `Store` it
    /// into the already-materialized field address `dst`.
    FieldStore {
        dst: VarId,
        name: String,
        span: Span,
    },
    /// `dst = FnName` in an initializer/assignment RHS.
    RhsIdent { dst: VarId, name: String },
    /// `dst = &FnName`.
    AddrOfIdent { dst: VarId, name: String },
    /// `return FnName;` from function `owner`.
    ReturnIdent { owner: FnId, name: String },
    /// `return &FnName;` from function `owner`.
    ReturnAddrOf { owner: FnId, name: String },
}

struct LowerContext {
    current_fn: Option<FnId>,
    current_file: trace_ir::FileId,
    locals: HashMap<String, VarId>,
    /// Origin map for the preprocessed text being lowered. Lookup failures
    /// fall back to TU-anchored spans.
    line_map: Option<std::sync::Arc<trace_preproc::LineMap>>,
    primary_path: PathBuf,
    /// Function references deferred to end-of-unit resolution.
    /// `RefCell` keeps `&LowerContext` receivers usable in expression
    /// helpers while still allowing deferred entries to be recorded.
    pending: RefCell<Vec<PendingFnRef>>,
    /// C++ namespace stack; `None` = anonymous namespace level.
    ns_stack: Vec<Option<String>>,
    /// Namespaces made visible by `using namespace X;` (bare spellings).
    using_nss: Vec<String>,
    /// Enclosing class while lowering in-class member definitions.
    class_ctx: Option<ClassCtx>,
    /// True for .cpp/.cc/.cxx TUs — gates the C++-specific lowering paths.
    is_cpp: bool,
}

/// C++ class scope during member lowering.
#[derive(Clone)]
struct ClassCtx {
    /// Fully qualified class name (`ns::Cls`) — matches function/type names.
    qual_name: String,
}

impl LowerContext {
    fn qualify(&self, name: &str) -> String {
        let mut parts: Vec<String> = self.ns_stack.iter().flatten().cloned().collect();
        parts.push(name.to_string());
        parts.join("::")
    }

    fn in_anonymous_namespace(&self) -> bool {
        self.ns_stack.iter().any(|level| level.is_none())
    }
}

fn register_local(ctx: &mut LowerContext, name: String, id: VarId) {
    if ctx.current_fn.is_some() {
        ctx.locals.insert(name, id);
    }
}

pub fn build_program(root: &Path, opts: &PreprocessOptions) -> Result<Program, String> {
    let jobs = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        .max(1);
    build_program_with_jobs(root, opts, jobs)
}

pub fn build_program_with_jobs(
    root: &Path,
    opts: &PreprocessOptions,
    jobs: usize,
) -> Result<Program, String> {
    let jobs = jobs.max(1);
    let mut program = Program::new(root.to_path_buf());
    program.include_paths = opts.include_paths.clone();
    program.defines = opts
        .defines
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();

    let (files, headers) = discover_source_files(root);
    let files = normalize_discovered_paths(files);
    let headers = normalize_discovered_paths(headers);
    if files.is_empty() && headers.is_empty() {
        return Err(format!(
            "no C/C++ source files found under {}",
            root.display()
        ));
    }

    let include_graph = IncludeGraph::build(root, &files, &headers);
    let file_order = include_graph.index_order(&files);

    let basename_index = Arc::new(include_graph.basename_index.clone());
    let include_expansion_cache = Arc::new(std::sync::RwLock::new(HashMap::new()));
    let eff_opts = project_preprocess_opts(root, opts, &include_graph)
        .for_indexing()
        .with_include_expansion_cache(Arc::clone(&include_expansion_cache))
        .with_basename_index(basename_index);

    // Warm each header under a FRESH macro environment seeded only from the
    // command-line defines. Sharing one accumulating table across headers let
    // include guards defined by earlier-warmed headers starve later headers'
    // expansions: the starved (empty) text was frozen into the expansion cache
    // and replayed to translation units, silently dropping every declaration
    // behind those guards (verified FN class on real corpora). Dedup comes
    // from the shared expansion cache instead; per-header tables only prevent
    // cross-header guard leakage.
    //
    // Translation units still inherit a single table — the union of all
    // headers' final macro states — because cached expansions are replayed
    // without executing their #define directives, so TU-local code needs the
    // macros those headers define.
    let union_macros: Arc<std::sync::RwLock<MacroTable>> = Arc::new(std::sync::RwLock::new(
        macro_table_from_defines(&opts.defines),
    ));
    let project_headers: Vec<PathBuf> = include_graph
        .project_files
        .iter()
        .filter(|p| {
            p.extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("h"))
        })
        .cloned()
        .collect();
    let c_sources: HashSet<PathBuf> = files.iter().cloned().collect();
    let reachable_from_c = include_graph.reachable_from(&c_sources);
    let headers_for_macro_warm: Vec<PathBuf> = include_graph.index_order(
        &project_headers
            .iter()
            .filter(|p| reachable_from_c.contains(*p))
            .cloned()
            .collect::<Vec<_>>(),
    );
    // Headers never #included from any `.c` must be indexed separately; the rest
    // are already expanded into translation units during TU preprocess.
    let orphan_headers: Vec<PathBuf> = include_graph.index_order(
        &project_headers
            .iter()
            .filter(|p| !reachable_from_c.contains(*p))
            .cloned()
            .collect::<Vec<_>>(),
    );

    let source_cache = IndexSourceCache::new();
    for path in &headers_for_macro_warm {
        let header_macros: Arc<std::sync::RwLock<MacroTable>> = Arc::new(std::sync::RwLock::new(
            macro_table_from_defines(&opts.defines),
        ));
        let header_prep_opts = eff_opts
            .clone()
            .with_shared_macros(Arc::clone(&header_macros))
            .with_accumulate_macros(true);
        if let Err(e) = source_cache.get_or_preprocess(path, &include_graph, &header_prep_opts) {
            program.add_diagnostic(Diagnostic {
                severity: DiagnosticSeverity::Warning,
                file: None,
                line: 0,
                message: format!("macro warm preprocess failed for {}: {e}", path.display()),
                stage: "preprocess".into(),
            });
            continue;
        }
        if let Ok(mut union) = union_macros.write() {
            if let Ok(done) = header_macros.read() {
                for (name, def) in done.iter() {
                    union.insert(name.clone(), def.clone());
                }
            }
        }
    }

    // Parallel phases must treat the expansion cache as read-only: warm-pass
    // entries were produced sequentially and deterministically, while worker
    // inserts are first-writer-wins races that make output scheduling-
    // dependent. Misses expand inline under each TU's own macro/guard state.
    let index_opts = eff_opts
        .with_shared_macros(union_macros)
        .with_frozen_expansion_cache(true);

    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(jobs)
        .build()
        .map_err(|e| e.to_string())?;

    pool.install(|| {
        let mut header_units: HashMap<PathBuf, UnitIndex> = orphan_headers
            .par_iter()
            .map(|path| {
                (
                    path.clone(),
                    index_source_file(path, root, &include_graph, &index_opts, &source_cache),
                )
            })
            .collect();
        for path in &orphan_headers {
            if let Some(unit) = header_units.remove(path) {
                merge_unit_index(&mut program, unit);
            }
        }
    });

    pool.install(|| {
        if jobs == 1 {
            for path in &file_order {
                merge_unit_index(
                    &mut program,
                    index_source_file(path, root, &include_graph, &index_opts, &source_cache),
                );
            }
        } else {
            let mut units: HashMap<PathBuf, UnitIndex> = file_order
                .par_iter()
                .map(|path| {
                    (
                        path.clone(),
                        index_source_file(path, root, &include_graph, &index_opts, &source_cache),
                    )
                })
                .collect();
            for path in &file_order {
                if let Some(unit) = units.remove(path) {
                    merge_unit_index(&mut program, unit);
                }
            }
        }
    });

    program.include_deps = include_graph.edge_list();
    for dir in &include_graph.include_dirs {
        if !program.include_paths.iter().any(|p| p == dir) {
            program.include_paths.push(dir.clone());
        }
    }

    finalize_extern_callees(&mut program);

    Ok(program)
}

/// Classify plain-identifier calls that resolve to no tree-local symbol
/// (libc without tree headers, logging backends referenced only inside
/// macros, vendor externs). Each unique name becomes one synthesized
/// `Function` row with `is_defined: false`, and its call sites are marked
/// resolved-to-external so the solver emits `External` edges instead of
/// counting them as unresolved indirect noise. Synthesized entries are
/// pushed directly (not via `add_function`) so they stay out of the name
/// resolution maps and cannot shadow real definitions or feed param wiring.
fn finalize_extern_callees(program: &mut Program) {
    let mut names: Vec<(String, trace_ir::FileId, u32)> = program
        .symbols
        .call_sites
        .iter()
        .filter(|cs| {
            !cs.is_direct
                && cs.callee_var.is_none()
                && cs.callee_fn_id.is_none()
                && is_plain_ident(&cs.callee_name)
        })
        .map(|cs| (cs.callee_name.clone(), cs.span.file, cs.span.line))
        .collect();
    names.sort();
    names.dedup_by(|a, b| a.0 == b.0);
    for (name, file, line) in names {
        // A symbol already exists for this name (in-tree prototype or
        // definition): leave the site untouched so the solver's name-based
        // recovery classifies it — defined-elsewhere resolves to a real
        // Direct edge with param wiring; prototype-only becomes External.
        // Synthesizing over it would orphan the real definition.
        if program.symbols.resolve_function(&name).is_some() {
            continue;
        }
        let fid = program.symbols.alloc_fn_id();
        program.symbols.functions.push(trace_ir::Function {
            id: fid,
            name: name.clone(),
            linkage: trace_ir::Linkage::External,
            return_type: trace_ir::TypeId(0),
            params: Vec::new(),
            locals: Vec::new(),
            is_cpp: false,
            span: trace_ir::Span { file, line, col: 0 },
            file,
            is_defined: false,
            is_virtual: false,
        });
        for cs in program.symbols.call_sites.iter_mut() {
            if !cs.is_direct && cs.callee_var.is_none() && cs.callee_name == name {
                cs.is_direct = true;
                cs.callee_fn_id = Some(fid);
            }
        }
    }
}

/// A call target that is syntactically a bare identifier — no field access,
/// subscript, cast noise, or macro-artifact punctuation.
fn is_plain_ident(name: &str) -> bool {
    !name.is_empty() && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

fn normalize_discovered_paths(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    paths
        .into_iter()
        .map(|p| trace_ir::canonicalize(&p))
        .collect()
}

fn project_preprocess_opts(
    root: &Path,
    opts: &PreprocessOptions,
    graph: &IncludeGraph,
) -> PreprocessOptions {
    let mut eff = opts.clone();
    for dir in &graph.include_dirs {
        if !eff.include_paths.iter().any(|p| p == dir) {
            eff.include_paths.push(dir.clone());
        }
    }
    if eff.source_cache.is_none() && !graph.source_cache.is_empty() {
        eff.source_cache = Some(Arc::new(graph.source_cache.clone()));
    }
    let _ = root;
    eff
}

fn index_source_file(
    path: &Path,
    root: &Path,
    graph: &IncludeGraph,
    index_opts: &PreprocessOptions,
    source_cache: &IndexSourceCache,
) -> UnitIndex {
    let mut program = Program::new(root.to_path_buf());
    match process_indexed_file(&mut program, path, graph, index_opts, source_cache) {
        Ok(()) => {
            if std::env::var_os("TRACE_DEBUG_UNIT").is_some() {
                let hdr = program
                    .symbols
                    .functions
                    .iter()
                    .filter(|f| {
                        program
                            .symbols
                            .files
                            .get(f.span.file.0 as usize)
                            .is_some_and(|fi| {
                                fi.path
                                    .extension()
                                    .is_some_and(|e| e.eq_ignore_ascii_case("h"))
                            })
                    })
                    .count();
                eprintln!(
                    "[unit] {} fns={} header_origin_fns={}",
                    path.display(),
                    program.symbols.functions.len(),
                    hdr
                );
            }
            program_into_unit(path.to_path_buf(), program)
        }
        Err(e) => UnitIndex {
            path: path.to_path_buf(),
            diagnostics: vec![Diagnostic {
                severity: DiagnosticSeverity::Error,
                file: None,
                line: 0,
                message: e,
                stage: "parse".into(),
            }],
            ..Default::default()
        },
    }
}

fn process_indexed_file(
    program: &mut Program,
    path: &Path,
    graph: &IncludeGraph,
    index_opts: &PreprocessOptions,
    source_cache: &IndexSourceCache,
) -> Result<(), String> {
    let pre = source_cache.get_or_preprocess(path, graph, index_opts)?;
    if let Some(dir) = std::env::var_os("TRACE_DUMP_TU_DIR") {
        let fname = format!(
            "tu_{}.i",
            path.file_stem().and_then(|s| s.to_str()).unwrap_or("x")
        );
        let _ = std::fs::write(std::path::Path::new(&dir).join(fname), pre.text.as_ref());
    }
    let lang = crate::parse::SourceLang::from_path(path);
    let parsed = crate::parse::parse_source_with_lang(pre.text.as_ref(), lang)?;
    if crate::parse::has_parse_errors(&parsed.tree) {
        program.add_diagnostic(Diagnostic {
            severity: DiagnosticSeverity::Warning,
            file: None,
            line: 0,
            message: format!("parse errors in {}", path.display()),
            stage: "parse".into(),
        });
    }

    let file_id = program.symbols.add_file(path.to_path_buf());
    let mut ctx = LowerContext {
        current_fn: None,
        current_file: file_id,
        locals: HashMap::new(),
        line_map: Some(std::sync::Arc::clone(&pre.line_map)),
        primary_path: trace_ir::canonicalize(path),
        pending: RefCell::new(Vec::new()),
        ns_stack: Vec::new(),
        using_nss: Vec::new(),
        class_ctx: None,
        is_cpp: crate::parse::SourceLang::from_path(path) == crate::parse::SourceLang::Cpp,
    };
    lower_tree(program, &mut ctx, &parsed.source, parsed.tree.root_node());
    resolve_pending_fn_refs(program, &ctx);
    Ok(())
}

/// Second-chance resolution for references recorded while lowering: the
/// whole unit's symbol table is now populated, so definitions that appear
/// after their use site are visible.
fn resolve_pending_fn_refs(program: &mut Program, ctx: &LowerContext) {
    let pending: Vec<PendingFnRef> = ctx.pending.borrow_mut().drain(..).collect();
    for item in pending {
        match item {
            PendingFnRef::FieldStore { dst, name, span } => {
                if let Some(callee) = resolve_function_named(program, ctx, &name) {
                    let tmp = alloc_ret_temp_spanned(program, ctx, span);
                    program
                        .flow
                        .push(FlowConstraint::AddrOfFn { dst: tmp, callee });
                    program.flow.push(FlowConstraint::Store { dst, src: tmp });
                }
            }
            PendingFnRef::RhsIdent { dst, name } => {
                if let Some(callee) = resolve_function_named(program, ctx, &name) {
                    program.flow.push(FlowConstraint::AddrOfFn { dst, callee });
                } else if let Some(src) = lookup_var(ctx, program, &name) {
                    // A tentative global defined after the use site.
                    program.flow.push(FlowConstraint::Copy { dst, src });
                }
            }
            PendingFnRef::AddrOfIdent { dst, name } => {
                if let Some(callee) = resolve_function_named(program, ctx, &name) {
                    program.flow.push(FlowConstraint::AddrOfFn { dst, callee });
                } else if let Some(src) = lookup_var(ctx, program, &name) {
                    program.flow.push(FlowConstraint::AddrOfVar { dst, src });
                }
            }
            PendingFnRef::ReturnIdent { owner, name } => {
                if let Some(callee) = resolve_function_named(program, ctx, &name) {
                    program
                        .fn_returns
                        .entry(owner)
                        .or_default()
                        .push(ReturnFlow::AddrOfFn { callee });
                } else if let Some(src) = lookup_var(ctx, program, &name) {
                    program
                        .fn_returns
                        .entry(owner)
                        .or_default()
                        .push(ReturnFlow::Copy { src });
                }
            }
            PendingFnRef::ReturnAddrOf { owner, name } => {
                if let Some(callee) = resolve_function_named(program, ctx, &name) {
                    program
                        .fn_returns
                        .entry(owner)
                        .or_default()
                        .push(ReturnFlow::AddrOfFn { callee });
                } else if let Some(src) = lookup_var(ctx, program, &name) {
                    program
                        .fn_returns
                        .entry(owner)
                        .or_default()
                        .push(ReturnFlow::AddrOfVar { src });
                }
            }
        }
    }
}

fn program_into_unit(path: PathBuf, mut program: Program) -> UnitIndex {
    UnitIndex {
        files: program
            .symbols
            .files
            .iter()
            .map(|f| f.path.clone())
            .collect(),
        path,
        types: program.types,
        functions: program.symbols.functions,
        variables: program.symbols.variables,
        call_sites: program.symbols.call_sites,
        flow: program.flow,
        fn_returns: program.fn_returns.into_iter().collect(),
        diagnostics: program.diagnostics,
        anon_type_counter: program.anon_type_counter,
        inheritance: std::mem::take(&mut program.inheritance),
    }
}

fn lower_typedef(program: &mut Program, ctx: &mut LowerContext, source: &str, node: Node) {
    if let Some(decl) = node.child_by_field_name("declarator") {
        let (alias, _) = parse_declarator_name(source, decl);
        if let Some(type_node) = node.child_by_field_name("type") {
            if type_node.kind() == "struct_specifier" || type_node.kind() == "union_specifier" {
                let tag = lower_struct_specifier(program, ctx, source, type_node);
                if !alias.is_empty() && !tag.is_empty() && alias != tag {
                    let kind = if type_node.kind() == "union_specifier" {
                        TypeDesc::Union {
                            name: tag.clone(),
                            fields: Vec::new(),
                        }
                    } else {
                        TypeDesc::Struct {
                            name: tag.clone(),
                            fields: Vec::new(),
                        }
                    };
                    program.types.intern(kind);
                }
            } else if let Some(desc) = typedef_underlying_desc(program, ctx, source, node) {
                program.types.register_alias(&alias, desc);
            }
        }
    }
}

fn lower_tree(program: &mut Program, ctx: &mut LowerContext, source: &str, node: Node) {
    match node.kind() {
        "function_definition" => lower_function(program, ctx, source, node),
        "declaration" => lower_declaration(program, ctx, source, node, None),
        "struct_specifier" | "union_specifier" | "class_specifier" => {
            let tag = lower_struct_specifier(program, ctx, source, node);
            if node.kind() == "class_specifier" {
                lower_class_members(program, ctx, source, node, &tag);
            }
        }
        "namespace_definition" => lower_namespace(program, ctx, source, node),
        "using_declaration" => lower_using_declaration(ctx, source, node),
        // Templates are lowered once, as a merged representative of all
        // instantiations; explicit specializations fold into the same entry
        // (documented imprecision). The inner definition carries everything.
        "template_declaration" => {
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if child.kind() == "template_parameter_list" {
                    continue;
                }
                lower_tree(program, ctx, source, child);
            }
        }
        "type_definition" => lower_typedef(program, ctx, source, node),
        _ => {
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                lower_tree(program, ctx, source, child);
            }
        }
    }
}

fn lower_namespace(program: &mut Program, ctx: &mut LowerContext, source: &str, node: Node) {
    let name = node.children(&mut node.walk()).find_map(|c| {
        if c.kind() == "namespace_identifier" {
            Some(normalize_qualified(node_text(source, &c)))
        } else {
            None
        }
    });
    ctx.ns_stack.push(name);
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "declaration_list" {
            let mut inner = child.walk();
            for decl in child.children(&mut inner) {
                lower_tree(program, ctx, source, decl);
            }
        }
    }
    ctx.ns_stack.pop();
}

fn lower_using_declaration(ctx: &mut LowerContext, source: &str, node: Node) {
    let mut is_ns_using = false;
    let mut target: Option<String> = None;
    for child in node.children(&mut node.walk()) {
        match child.kind() {
            "namespace" => is_ns_using = true,
            "identifier" | "qualified_identifier" | "namespace_identifier" => {
                target = Some(normalize_qualified(node_text(source, &child)));
            }
            _ => {}
        }
    }
    if is_ns_using {
        if let Some(t) = target {
            if !ctx.using_nss.contains(&t) {
                ctx.using_nss.push(t);
            }
        }
    }
}

/// Does this member declaration declare a function (method/ctor/dtor)
/// rather than a data member? Function-pointer members (`int (*cb)(int);`)
/// parse through a `function_declarator` too but their name sits behind a
/// parenthesized/pointer declarator — those are data.
fn member_decl_is_function(node: Node) -> bool {
    fn walk(n: Node) -> bool {
        match n.kind() {
            "destructor_name" => return true,
            "function_declarator" => {
                if let Some(inner) = n.child_by_field_name("declarator") {
                    return matches!(
                        inner.kind(),
                        "field_identifier" | "identifier" | "qualified_identifier"
                    ) || walk(inner);
                }
                return false;
            }
            _ => {}
        }
        for i in 0..n.child_count() {
            if let Some(c) = n.child(i) {
                if walk(c) {
                    return true;
                }
            }
        }
        false
    }
    walk(node)
}

fn has_virtual_token(node: Node) -> bool {
    for child in node.children(&mut node.walk()) {
        match child.kind() {
            "virtual" => return true,
            "virtual_specifier" => {}
            _ => {}
        }
    }
    false
}

fn lower_struct_specifier(
    program: &mut Program,
    ctx: &LowerContext,
    source: &str,
    node: Node,
) -> String {
    let is_union = node.kind() == "union_specifier";
    let name_node = node.child_by_field_name("name");
    let mut name = name_node
        .map(|n| {
            let raw = node_text(source, &n).to_string();
            if n.kind() == "template_type" || raw.contains('<') {
                // `Box<int>` specializations register under the bare template
                // tag; members merge with the primary template's.
                strip_template_args(&normalize_qualified(&raw))
            } else if raw.contains("::") || raw.contains(char::is_whitespace) {
                normalize_qualified(&raw)
            } else {
                raw
            }
        })
        .unwrap_or_default();

    if name.is_empty() {
        program.anon_type_counter += 1;
        name = format!("anon_{}", program.anon_type_counter);
    }

    // Classes register under their fully qualified tag so type references,
    // owner-class derivation and member resolution all agree on one name.
    let reg_name = if node.kind() == "class_specifier" && !name.contains("::") {
        ctx.qualify(&name)
    } else {
        name.clone()
    };

    // C++: `class D : B, A { ... }` — record inheritance for virtual
    // dispatch expansion. Unqualified bases resolve against the current
    // namespace scope (usings ignored here; documented imprecision).
    if node.kind() == "class_specifier" {
        let derived = reg_name.clone();
        for child in node.children(&mut node.walk()) {
            if child.kind() != "base_class_clause" {
                continue;
            }
            for base in child.children(&mut child.walk()) {
                if !base.is_named() && base.kind() != "qualified_identifier" {
                    continue;
                }
                if matches!(
                    base.kind(),
                    "type_identifier"
                        | "qualified_identifier"
                        | "template_type"
                        | "namespace_identifier"
                ) {
                    let raw = normalize_qualified(node_text(source, &base));
                    let base = strip_template_args(&raw);
                    let base = if base.contains("::") {
                        base
                    } else {
                        ctx.qualify(&base)
                    };
                    program.add_inheritance(&derived, &base);
                }
            }
        }
    }

    let mut fields = Vec::new();
    if let Some(body) = node.child_by_field_name("body") {
        let mut cursor = body.walk();
        for child in body.children(&mut cursor) {
            if child.kind() == "field_declaration" && !member_decl_is_function(child) {
                if let Some((fname, field_type)) =
                    type_desc_from_field_declaration(program, ctx, source, child)
                {
                    if !fname.is_empty() {
                        fields.push((fname, field_type));
                    }
                }
            }
        }
    }

    // Classes always intern a layout entry (even when data fields are
    // absent) so type references, owner-class derivation and member-call
    // resolution can find them by tag.
    // C++ classes always intern a layout entry; C++ structs do too — a
    // method-only body (`struct Ctor { Ctor(); };`) must still resolve as
    // a class type for member-initializer and receiver inference.
    if !fields.is_empty() || node.kind() == "class_specifier" || ctx.is_cpp {
        if is_union {
            program.types.compute_union_layout(reg_name.clone(), fields);
        } else {
            program
                .types
                .compute_struct_layout(reg_name.clone(), fields);
        }
    }
    reg_name
}

/// Lower the member functions of a class body: prototypes first (so later
/// definitions in other TUs merge against them and virtuality is recorded),
/// then in-class definitions with the class context active.
fn lower_class_members(
    program: &mut Program,
    ctx: &mut LowerContext,
    source: &str,
    node: Node,
    cls_qual: &str,
) {
    let Some(body) = node.child_by_field_name("body") else {
        return;
    };
    let mut cursor = body.walk();
    let members: Vec<Node> = body
        .children(&mut cursor)
        .filter(|m| {
            matches!(
                m.kind(),
                "function_definition" | "field_declaration" | "declaration"
            )
        })
        .collect();
    for m in &members {
        if m.kind() == "field_declaration" && member_decl_is_function(*m) {
            register_member_prototype(program, ctx, source, *m, cls_qual);
        }
        // A ctor written `Cls(int);` inside the class parses as a plain
        // declaration wrapping a function_declarator.
        if m.kind() == "declaration" && member_decl_is_function(*m) {
            register_member_prototype(program, ctx, source, *m, cls_qual);
        }
    }
    let saved = ctx.class_ctx.clone();
    for m in &members {
        if m.kind() == "function_definition" {
            ctx.class_ctx = Some(ClassCtx {
                qual_name: cls_qual.to_string(),
            });
            lower_function(program, ctx, source, *m);
        }
    }
    ctx.class_ctx = saved;
}

/// The bare name a member declares: method identifier, ctor class-name, or
/// destructor spelling (`~Cls`).
fn member_short_name(source: &str, node: Node) -> Option<String> {
    fn walk(source: &str, n: Node) -> Option<String> {
        match n.kind() {
            "destructor_name" => return Some(normalize_qualified(node_text(source, &n))),
            "function_declarator" => {
                if let Some(inner) = n.child_by_field_name("declarator") {
                    return walk(source, inner);
                }
                return None;
            }
            "field_identifier" | "identifier" => {
                return Some(normalize_qualified(node_text(source, &n)));
            }
            _ => {}
        }
        for i in 0..n.child_count() {
            if let Some(c) = n.child(i) {
                if let Some(found) = walk(source, c) {
                    return Some(found);
                }
            }
        }
        None
    }
    walk(source, node)
}

fn register_member_prototype(
    program: &mut Program,
    ctx: &LowerContext,
    source: &str,
    node: Node,
    cls_qual: &str,
) {
    let Some(short) = member_short_name(source, node) else {
        return;
    };
    if short.is_empty() || short == "operator" {
        return;
    }
    let full_name = format!("{}::{}", cls_qual, short);
    let is_virtual = has_virtual_token(node);
    let provisional_id = program.symbols.alloc_fn_id();
    // Prototypes carry no parameter variables; they merge into their
    // definitions, which supply the real param list for arity filtering.
    let params: Vec<VarId> = Vec::new();
    let span = node_span(program, ctx, node);
    program.symbols.add_function(Function {
        id: provisional_id,
        name: full_name,
        linkage: if ctx.in_anonymous_namespace() {
            Linkage::Internal
        } else {
            Linkage::External
        },
        return_type: program.types.void(),
        params,
        locals: Vec::new(),
        span,
        file: ctx.current_file,
        is_defined: false,
        is_virtual,
        is_cpp: ctx.is_cpp,
    });
}

fn lower_function(program: &mut Program, ctx: &mut LowerContext, source: &str, node: Node) {
    let Some(decl) = node
        .child_by_field_name("declarator")
        .or_else(|| find_function_declarator(node))
    else {
        return;
    };
    let (raw_name, _) = parse_declarator_name(source, decl);
    if raw_name.is_empty() {
        return;
    }
    // Qualify: in-class definitions get the enclosing class prefix;
    // free functions get the namespace prefix (no-op for C — empty stack).
    // Out-of-class member spellings (`Shape::area`, `Cls::~Cls`) may omit
    // the namespace: prefix it unless the name already starts with one of
    // the enclosing namespaces (fully qualified spelling).
    let normalized_raw = normalize_qualified(&raw_name);
    let name = if let Some(cls) = &ctx.class_ctx {
        if normalized_raw.contains("::") {
            normalized_raw
        } else {
            format!("{}::{}", cls.qual_name, normalized_raw)
        }
    } else if normalized_raw.contains("::") {
        let first_seg = normalized_raw.split("::").next().unwrap_or("");
        let already_qualified =
            ctx.ns_stack.iter().flatten().any(|ns| ns == first_seg) || !ctx.is_cpp;
        if already_qualified {
            normalized_raw
        } else {
            ctx.qualify(&normalized_raw)
        }
    } else {
        ctx.qualify(&raw_name)
    };
    // Out-of-class member definitions (`void Cls::f() {}`, ctors, dtors):
    // recover the owning class from the longest `::`-prefix that names a
    // known class type.
    let eff_class: Option<String> = match &ctx.class_ctx {
        Some(c) => Some(c.qual_name.clone()),
        None => derive_owner_class(program, &name),
    };
    let ret_type = node
        .child_by_field_name("type")
        .map(|t| parse_type_node(program, ctx, source, t))
        .unwrap_or_else(|| program.types.int());
    let provisional_id = program.symbols.alloc_fn_id();
    let mut params = Vec::new();
    // Implicit `this` for member functions, ctors and dtors.
    if let Some(cls) = &eff_class {
        let this_type = program
            .types
            .intern(TypeDesc::Ptr(Box::new(TypeDesc::Struct {
                name: cls.clone(),
                fields: Vec::new(),
            })));
        let this_id = program.symbols.alloc_var_id();
        let span = node_span(program, ctx, node);
        program.symbols.add_variable(Variable {
            id: this_id,
            name: "this".to_string(),
            type_id: this_type,
            storage: StorageClass::Param,
            fn_id: Some(provisional_id),
            param_index: Some(0),
            span,
            is_pointer: true,
        });
        params.push(this_id);
    }
    if let Some(params_node) = find_params(decl) {
        for param in params_node.children(&mut params_node.walk()) {
            if param.kind() == "parameter_declaration" {
                if let Some(var) = lower_parameter(
                    program,
                    ctx,
                    source,
                    param,
                    provisional_id,
                    params.len() as u32,
                ) {
                    params.push(var);
                }
            }
        }
    }

    let is_static = declaration_is_static(source, node) || ctx.in_anonymous_namespace();
    let is_virtual = has_virtual_token(node);

    let span = node_span(program, ctx, node);
    let fn_id = program.symbols.add_function(Function {
        id: provisional_id,
        name: name.clone(),
        linkage: if is_static {
            Linkage::Internal
        } else {
            Linkage::External
        },
        return_type: ret_type,
        params: params.clone(),
        locals: Vec::new(),
        span,
        file: ctx.current_file,
        is_defined: true,
        is_virtual,
        is_cpp: ctx.is_cpp,
    });
    reassign_fn_id(program, provisional_id, fn_id);
    ctx.current_fn = Some(fn_id);
    ctx.locals.clear();
    for &param in &params {
        if let Some(v) = program.symbols.variable_by_id(param) {
            ctx.locals.insert(v.name.clone(), param);
        }
    }
    // Out-of-class definitions still resolve implicit `this->` members.
    let saved_class = ctx.class_ctx.clone();
    if let Some(cls) = &eff_class {
        let needs_set = match &ctx.class_ctx {
            Some(c) => c.qual_name != *cls,
            None => true,
        };
        if needs_set {
            ctx.class_ctx = Some(ClassCtx {
                qual_name: cls.clone(),
            });
        }
    }

    if let Some(body_node) = node.child_by_field_name("body") {
        walk_function_body(program, ctx, source, body_node, fn_id);
    }
    // Constructor-initializer lists are siblings of the body on
    // function_definition; they carry base/member ctor calls.
    if ctx.is_cpp {
        let init_lists: Vec<_> = node
            .children(&mut node.walk())
            .filter(|c| c.kind() == "field_initializer_list")
            .collect();
        for il in init_lists {
            walk_function_body(program, ctx, source, il, fn_id);
        }
    }

    ctx.current_fn = None;
    ctx.locals.clear();
    ctx.class_ctx = saved_class;
}

/// Longest `a::b::Cls` prefix of a qualified function name that resolves to
/// an interned class/struct tag.
fn derive_owner_class(program: &Program, qualified_name: &str) -> Option<String> {
    let segs: Vec<&str> = qualified_name.split("::").collect();
    if segs.len() < 2 {
        return None;
    }
    for split in (1..segs.len()).rev() {
        let candidate = segs[..split].join("::");
        if program
            .types
            .type_id_by_tag(&candidate, trace_ir::TypeKind::Struct)
            .is_some()
        {
            return Some(candidate);
        }
    }
    None
}

fn lower_parameter(
    program: &mut Program,
    ctx: &mut LowerContext,
    source: &str,
    node: Node,
    fn_id: FnId,
    index: u32,
) -> Option<VarId> {
    let decl = node.child_by_field_name("declarator")?;
    let (name, is_ptr) = parse_declarator_name(source, decl);
    if name.is_empty() {
        return None;
    }
    let base_desc = node
        .child_by_field_name("type")
        .map(|t| type_desc_from_node(program, ctx, source, t))
        .unwrap_or(TypeDesc::Int);
    let type_desc = if is_ptr {
        TypeDesc::Ptr(Box::new(base_desc))
    } else {
        base_desc
    };
    let type_id = program.types.intern(type_desc);
    let var_id = program.symbols.alloc_var_id();
    let span = node_span(program, ctx, node);
    program.symbols.add_variable(Variable {
        id: var_id,
        name: name.clone(),
        type_id,
        storage: StorageClass::Param,
        fn_id: Some(fn_id),
        param_index: Some(index),
        span,
        is_pointer: is_ptr,
    });
    register_local(ctx, name, var_id);
    Some(var_id)
}

fn lower_declaration(
    program: &mut Program,
    ctx: &mut LowerContext,
    source: &str,
    node: Node,
    storage_override: Option<StorageClass>,
) {
    let type_node = match node.child_by_field_name("type") {
        Some(t) => t,
        None => return,
    };
    let type_id = parse_type_node(program, ctx, source, type_node);
    let is_static = declaration_is_static(source, node);

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "init_declarator" => {
                let decl = child.child_by_field_name("declarator").unwrap_or(child);
                // `auto p = new T(...)`: recover the pointee class so member
                // calls through p resolve by static type.
                let mut eff_type_id = type_id;
                if type_node.kind() == "placeholder_type_specifier" {
                    if let Some(value) = child.child_by_field_name("value") {
                        if value.kind() == "new_expression" {
                            if let Some(cls) = new_expression_class(program, ctx, source, value) {
                                let inner = program.types.intern(TypeDesc::Struct {
                                    name: cls,
                                    fields: Vec::new(),
                                });
                                eff_type_id = program.types.intern(TypeDesc::Ptr(Box::new(
                                    program.types.get(inner).desc.clone(),
                                )));
                            }
                        }
                    }
                }
                lower_one_declarator(
                    program,
                    ctx,
                    source,
                    child,
                    decl,
                    eff_type_id,
                    is_static,
                    storage_override,
                    child.child_by_field_name("value"),
                );
            }
            "declarator" | "pointer_declarator" | "function_declarator" | "array_declarator" => {
                // A pointer-returning function declaration (`T *f(void);`) is a
                // `pointer_declarator` wrapping a `function_declarator`; it must
                // register a function, not a variable that shadows the name.
                if let Some((fdecl, ptr_depth)) = fn_decl_under_pointer(child) {
                    let mut ret = type_id;
                    for _ in 0..ptr_depth {
                        ret = program.types.intern(trace_ir::TypeDesc::Ptr(Box::new(
                            program.types.get(ret).desc.clone(),
                        )));
                    }
                    lower_function_decl(program, ctx, source, fdecl, ret, is_static);
                    continue;
                }
                lower_one_declarator(
                    program,
                    ctx,
                    source,
                    child,
                    child,
                    type_id,
                    is_static,
                    storage_override,
                    None,
                );
            }
            "identifier" => {
                let name = node_text(source, &child).to_string();
                if name.is_empty() {
                    continue;
                }
                let var_id = program.symbols.alloc_var_id();
                let span = node_span(program, ctx, child);
                program.symbols.add_variable(Variable {
                    id: var_id,
                    name: name.clone(),
                    type_id,
                    storage: storage_override.unwrap_or_else(|| storage_for(ctx, is_static)),
                    fn_id: ctx.current_fn,
                    param_index: None,
                    span,
                    is_pointer: false,
                });
                register_local(ctx, name, var_id);
            }
            _ => {}
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn lower_one_declarator(
    program: &mut Program,
    ctx: &mut LowerContext,
    source: &str,
    span_node: Node,
    decl: Node,
    type_id: trace_ir::TypeId,
    is_static: bool,
    storage_override: Option<StorageClass>,
    init_expr: Option<Node>,
) {
    if is_function_pointer_declarator(decl) {
        let (name, _is_ptr) = parse_declarator_name(source, decl);
        if name.is_empty() {
            return;
        }
        let var_id = program.symbols.alloc_var_id();
        let span = node_span(program, ctx, span_node);
        program.symbols.add_variable(Variable {
            id: var_id,
            name: name.clone(),
            type_id,
            storage: storage_override.unwrap_or_else(|| storage_for(ctx, is_static)),
            fn_id: ctx.current_fn,
            param_index: None,
            span,
            is_pointer: true,
        });
        register_local(ctx, name, var_id);
        if let Some(init) = init_expr {
            if init.kind() == "initializer_list"
                && (is_array_type(program, type_id) || declarator_is_array(decl))
            {
                lower_fn_ptr_array_init(program, ctx, source, var_id, init);
            }
            extract_flow_from_expr(program, ctx, source, init, Some(var_id));
        }
        return;
    }

    if decl.kind() == "function_declarator" && !is_function_pointer_declarator(decl) {
        lower_function_decl(program, ctx, source, decl, type_id, is_static);
        return;
    }

    let (name, is_ptr) = parse_declarator_name(source, decl);
    if name.is_empty() {
        return;
    }
    let var_id = program.symbols.alloc_var_id();
    let span = node_span(program, ctx, span_node);
    program.symbols.add_variable(Variable {
        id: var_id,
        name: name.clone(),
        type_id,
        storage: storage_override.unwrap_or_else(|| storage_for(ctx, is_static)),
        fn_id: ctx.current_fn,
        param_index: None,
        span,
        is_pointer: is_ptr,
    });
    register_local(ctx, name, var_id);
    // Constructor invocation spelled as a declaration: `Cls o(1, 2);`.
    // tree-sitter parks the argument list in init_declarator's `value`
    // field, so an argument_list "initializer" IS the ctor call.
    let ctor_args: Option<Node> = match init_expr {
        Some(n) if n.kind() == "argument_list" => Some(n),
        _ => span_node
            .children(&mut span_node.walk())
            .find(|c| c.kind() == "argument_list"),
    };
    if ctx.is_cpp
        && ctor_args.is_some()
        && span_node.kind() == "init_declarator"
        && ctx.current_fn.is_some()
    {
        if let Some(cls) = match program.types.get(type_id).desc.clone() {
            TypeDesc::Struct { name, .. } if !name.is_empty() && !name.starts_with("anon_") => {
                Some(name)
            }
            _ => None,
        } {
            let span = node_span(program, ctx, span_node);
            let (var_args, fn_args) = collect_call_args(program, ctx, source, ctor_args);
            emit_member_sites(
                program,
                ctx.current_fn.unwrap(),
                &cls,
                &trace_ir::MethodKind::Ctor,
                (var_args, fn_args),
                span,
            );
        }
    }
    if let Some(init) = init_expr {
        if init.kind() != "argument_list" {
            // A ctor argument list is not a value flowing into the object.
            if init.kind() == "initializer_list"
                && (is_array_type(program, type_id) || declarator_is_array(decl))
            {
                lower_fn_ptr_array_init(program, ctx, source, var_id, init);
            }
            extract_flow_from_expr(program, ctx, source, init, Some(var_id));
        }
    }
}

/// `ArrayFnMember` facts are only sound for array-typed tables (unknown-index
/// element access merges all members). Parking members of a plain struct
/// initializer into the variable node would let any field load observe every
/// member function regardless of field identity.
fn is_array_type(program: &Program, type_id: trace_ir::TypeId) -> bool {
    matches!(
        program.types.get(type_id).desc,
        trace_ir::TypeDesc::Array { .. }
    )
}

fn declarator_is_array(decl: Node) -> bool {
    if decl.kind() == "array_declarator" {
        return true;
    }
    let mut found = false;
    let mut cursor = decl.walk();
    for child in decl.children(&mut cursor) {
        if declarator_is_array(child) {
            found = true;
            break;
        }
    }
    found
}

fn lower_fn_ptr_array_init(
    program: &mut Program,
    ctx: &LowerContext,
    source: &str,
    array: VarId,
    init: Node,
) {
    let mut cursor = init.walk();
    for child in init.children(&mut cursor) {
        if matches!(child.kind(), "(" | ")" | ",") {
            continue;
        }
        // Arrays of structs: `{ {TYPE, Fn}, ... }` or `{ {.init = Fn}, ... }`.
        // Recurse so element expressions nested in inner lists are visited.
        if child.kind() == "initializer_list" {
            lower_fn_ptr_array_init(program, ctx, source, array, child);
            continue;
        }
        if child.kind() == "initializer_pair" || child.kind() == "designated_initializer" {
            if let Some(value) = init_pair_value_node(child) {
                // `[i] = { ... }` with a nested *positional* element list has
                // no field info — park members via ArrayFnMember (sound blob).
                // Lists carrying field designators are handled precisely by
                // `lower_designated_initializer` (via `extract_flow_from_expr`)
                // so members stay bound to their own field.
                if value.kind() == "initializer_list" {
                    if !list_has_field_designators(value) {
                        lower_fn_ptr_array_init(program, ctx, source, array, value);
                    }
                } else {
                    push_array_fn_member(program, ctx, source, array, value);
                }
            }
            continue;
        }
        push_array_fn_member(program, ctx, source, array, child);
    }
}

fn init_pair_value_node(node: Node) -> Option<Node> {
    let mut cursor = node.walk();
    node.children(&mut cursor)
        .filter(|c| c.is_named() && c.kind() != "field_designator")
        .last()
}

/// True when any direct `initializer_pair`/`designated_initializer` child of
/// this list carries a `.field =` designator (as opposed to purely positional
/// contents).
fn list_has_field_designators(list: Node) -> bool {
    let mut cursor = list.walk();
    for c in list.children(&mut cursor) {
        if c.kind() != "initializer_pair" && c.kind() != "designated_initializer" {
            continue;
        }
        let mut inner = c.walk();
        for g in c.children(&mut inner) {
            if g.kind() == "field_designator" {
                return true;
            }
        }
    }
    false
}

fn push_array_fn_member(
    program: &mut Program,
    ctx: &LowerContext,
    source: &str,
    array: VarId,
    elem: Node,
) {
    if let Some(callee) = resolve_call_fn_arg(program, ctx, source, elem) {
        program
            .flow
            .push(FlowConstraint::ArrayFnMember { array, callee });
    }
}

fn lower_function_decl(
    program: &mut Program,
    ctx: &mut LowerContext,
    source: &str,
    decl: Node,
    ret_type: trace_ir::TypeId,
    is_static: bool,
) {
    let (name, _) = parse_declarator_name(source, decl);
    if name.is_empty() {
        return;
    }
    let provisional_id = program.symbols.alloc_fn_id();
    let mut params = Vec::new();
    if let Some(params_node) = find_params(decl) {
        for param in params_node.children(&mut params_node.walk()) {
            if param.kind() == "parameter_declaration" {
                if let Some(var) = lower_parameter(
                    program,
                    ctx,
                    source,
                    param,
                    provisional_id,
                    params.len() as u32,
                ) {
                    params.push(var);
                }
            }
        }
    }
    let span = node_span(program, ctx, decl);
    let fn_id = program.symbols.add_function(Function {
        id: provisional_id,
        name,
        linkage: if is_static {
            Linkage::Internal
        } else {
            Linkage::External
        },
        return_type: ret_type,
        params,
        locals: Vec::new(),
        span,
        file: ctx.current_file,
        is_defined: false,
        is_virtual: false,
        is_cpp: ctx.is_cpp,
    });
    reassign_fn_id(program, provisional_id, fn_id);
}

fn reassign_fn_id(program: &mut Program, from: FnId, to: FnId) {
    if from == to {
        return;
    }
    let mut moved: Vec<VarId> = Vec::new();
    for var in &mut program.symbols.variables {
        if var.fn_id == Some(from) {
            var.fn_id = Some(to);
            moved.push(var.id);
        }
    }
    for cs in &mut program.symbols.call_sites {
        if cs.caller == from {
            cs.caller = to;
        }
    }
    // Re-pointed declarations supply the authoritative parameter list;
    // appending them onto the entry's existing params would double-count
    // prototype + definition parameters as distinct overload slots.
    if !moved.is_empty() {
        if let Some(func) = program.symbols.functions.iter_mut().find(|f| f.id == to) {
            func.params = moved;
        }
    }
}

fn walk_function_body(
    program: &mut Program,
    ctx: &mut LowerContext,
    source: &str,
    node: Node,
    caller: FnId,
) {
    match node.kind() {
        "declaration" => lower_declaration(program, ctx, source, node, None),
        "assignment_expression" => {
            extract_flow_from_expr(program, ctx, source, node, None);
        }
        "call_expression" => collect_call_at_node(program, ctx, source, node, caller),
        "return_statement" => collect_return_statement(program, ctx, source, node, caller),
        // C++ object lifecycle: constructor invocations and destructor runs.
        "new_expression" if ctx.is_cpp => {
            if let Some(cls) = new_expression_class(program, ctx, source, node) {
                let args = node
                    .children(&mut node.walk())
                    .find(|c| c.kind() == "argument_list");
                let span = node_span(program, ctx, node);
                let (var_args, fn_args) = collect_call_args(program, ctx, source, args);
                emit_member_sites(
                    program,
                    caller,
                    &cls,
                    &trace_ir::MethodKind::Ctor,
                    (var_args, fn_args),
                    span,
                );
            }
        }
        "delete_expression" if ctx.is_cpp => {
            let operand = node
                .children(&mut node.walk())
                .filter(|c| c.is_named())
                .last();
            if let Some(operand) = operand {
                if let Some(cls) = infer_static_class(program, ctx, source, operand) {
                    let span = node_span(program, ctx, node);
                    emit_member_sites(
                        program,
                        caller,
                        &cls,
                        &trace_ir::MethodKind::Dtor,
                        (Vec::new(), Vec::new()),
                        span,
                    );
                }
            }
        }
        "field_initializer_list" if ctx.is_cpp => {
            lower_field_initializer_list(program, ctx, source, node, caller);
        }
        _ => {}
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk_function_body(program, ctx, source, child, caller);
    }
}

fn collect_call_at_node(
    program: &mut Program,
    ctx: &mut LowerContext,
    source: &str,
    node: Node,
    caller: FnId,
) {
    let func = match node.child_by_field_name("function") {
        Some(f) => f,
        None => return,
    };
    let span = node_span(program, ctx, node);

    // ---- C++ member calls with statically-typed receivers ----
    // `recv.method(args)` / `p->method(args)` / explicit `x.~T()` /
    // virtual dispatch through base pointers. Receivers we cannot type
    // fall through to the generic indirect handling below (vtable-slot
    // style flow resolution), preserving soundness.
    if ctx.is_cpp && func.kind() == "field_expression" {
        let op_is_member_access = func
            .children(&mut func.walk())
            .any(|c| c.kind() == "." || c.kind() == "->");
        if op_is_member_access {
            if let Some(field) = func.child_by_field_name("field") {
                if matches!(
                    field.kind(),
                    "field_identifier" | "destructor_name" | "template_type"
                ) {
                    if let Some(recv) = func.child_by_field_name("argument") {
                        if let Some(cls) = infer_static_class(program, ctx, source, recv) {
                            let kind = if field.kind() == "destructor_name" {
                                trace_ir::MethodKind::Dtor
                            } else {
                                trace_ir::MethodKind::Named(strip_template_args(
                                    &normalize_qualified(node_text(source, &field)),
                                ))
                            };
                            let (var_args, fn_args) = collect_call_args(
                                program,
                                ctx,
                                source,
                                node.child_by_field_name("arguments"),
                            );
                            emit_member_sites(
                                program,
                                caller,
                                &cls,
                                &kind,
                                (var_args, fn_args),
                                span,
                            );
                            return;
                        }
                    }
                }
            }
        }
    }

    let (callee_name, mut is_direct, callee_var) =
        resolve_callee_with_loads(program, ctx, source, func);
    // Macro-expansion artifacts (stringified log fragments and similar
    // token soup) surface as call sites whose "callee" text embeds string
    // literals; real callees are plain identifiers or field paths. Note
    // that whitespace is legitimate here — preprocessed text keeps token
    // spacing (`tbl [ i ]->fn`) — so only quotes are rejected.
    if callee_name.contains('"') {
        return;
    }
    if !is_direct && callee_var.is_none() {
        is_direct = resolve_function_named(program, ctx, &callee_name).is_some();
    }
    if !is_direct && is_likely_macro_callee(&callee_name) {
        return;
    }
    let (var_args, fn_args) =
        collect_call_args(program, ctx, source, node.child_by_field_name("arguments"));

    // ---- Resolution ----
    // C preserves the exact legacy semantics: one scoped lookup, zero or
    // one target. C++ resolves over the candidate set with arity filtering
    // for overloads; an arity-filtered empty set falls back to every
    // candidate so varargs declarations keep their targets.
    let chosen: Vec<FnId> = if !ctx.is_cpp {
        program
            .symbols
            .resolve_function_in_scope(&callee_name, Some(ctx.current_file))
            .into_iter()
            .collect()
    } else if callee_var.is_none() {
        let candidates = program
            .symbols
            .resolve_function_candidates(&callee_name, Some(ctx.current_file));
        let argc = var_args.len() + fn_args.len();
        let by_arity: Vec<FnId> = candidates
            .iter()
            .copied()
            .filter(|&f| program.symbols.function(f).params.len() == argc)
            .collect();
        if by_arity.is_empty() {
            candidates
        } else {
            by_arity
        }
    } else {
        Vec::new()
    };

    match chosen.len() {
        0 => {
            let call_id = program.symbols.alloc_call_id();
            program.symbols.call_sites.push(CallSite {
                id: call_id,
                caller,
                callee_name,
                callee_var,
                callee_fn_id: None,
                var_args,
                fn_args,
                span,
                is_direct,
            });
        }
        1 => {
            let call_id = program.symbols.alloc_call_id();
            program.symbols.call_sites.push(CallSite {
                id: call_id,
                caller,
                callee_name,
                callee_var,
                callee_fn_id: Some(chosen[0]),
                var_args,
                fn_args,
                span,
                is_direct: true,
            });
        }
        n => {
            // Overload tie (same arity, types undecidable here): emit one
            // site per candidate — a bounded, explicit may-approximation.
            for t in chosen.iter().copied() {
                let call_id = program.symbols.alloc_call_id();
                program.symbols.call_sites.push(CallSite {
                    id: call_id,
                    caller,
                    callee_name: if n > 1 && t != chosen[0] {
                        format!("{}::{}", callee_name, program.symbols.function(t).id.0)
                    } else {
                        callee_name.clone()
                    },
                    callee_var,
                    callee_fn_id: Some(t),
                    var_args: var_args.clone(),
                    fn_args: fn_args.clone(),
                    span,
                    is_direct: true,
                });
            }
        }
    }
}

/// Collected call arguments: value args as `(index, var)` plus function
/// arguments (address-of-function) as `(index, fn)`.
type CallArgs = (Vec<(u32, VarId)>, Vec<(u32, FnId)>);

/// Collect call arguments once; shared by the member-call, overload and
/// legacy paths.
fn collect_call_args(
    program: &mut Program,
    ctx: &mut LowerContext,
    source: &str,
    args_node: Option<Node>,
) -> CallArgs {
    let mut var_args = Vec::new();
    let mut fn_args = Vec::new();
    let mut arg_index = 0u32;
    if let Some(args_node) = args_node {
        for arg in args_node.children(&mut args_node.walk()) {
            if arg.kind() != "(" && arg.kind() != ")" && arg.kind() != "," {
                if let Some(v) = resolve_expr_var(program, ctx, source, arg) {
                    // A field/subscript argument passes the *value* stored in
                    // that memory (e.g. `take(g_h.h, 0)` passes the fn-ptr in
                    // `g_h.h`). `resolve_expr_var` yields the base object, so
                    // materialize a load temp and pass that instead.
                    if matches!(arg.kind(), "field_expression" | "subscript_expression") {
                        let temp = alloc_ret_temp(program, ctx, arg);
                        if let Some(flow) = expr_to_rhs_flow(program, ctx, source, arg, temp) {
                            program.flow.push(flow);
                            var_args.push((arg_index, temp));
                            arg_index += 1;
                            continue;
                        }
                    }
                    var_args.push((arg_index, v));
                    arg_index += 1;
                } else if let Some(fn_id) = resolve_call_fn_arg(program, ctx, source, arg) {
                    fn_args.push((arg_index, fn_id));
                    arg_index += 1;
                }
            }
        }
    }
    (var_args, fn_args)
}

/// Emit call sites for `cls::member` — the override set across derived
/// classes, found by walking up the inheritance chain to the nearest
/// declaring class and expanding its subclasses.
fn emit_member_sites(
    program: &mut Program,
    caller: FnId,
    cls: &str,
    kind: &trace_ir::MethodKind,
    args: CallArgs,
    span: Span,
) {
    let (var_args, fn_args) = args;
    let targets = member_targets_upward(program, cls, kind);
    let display = kind.name_on(cls);
    if targets.is_empty() {
        // Unknown method: keep an unresolved site; the solver synthesizes
        // an external entry (mirrors plain-identifier C behavior).
        let call_id = program.symbols.alloc_call_id();
        program.symbols.call_sites.push(CallSite {
            id: call_id,
            caller,
            callee_name: display,
            callee_var: None,
            callee_fn_id: None,
            var_args,
            fn_args,
            span,
            is_direct: false,
        });
        return;
    }
    for t in targets {
        let (call_id, name) = {
            let id = program.symbols.alloc_call_id();
            let nm = program.symbols.function(t).name.clone();
            (id, nm)
        };
        program.symbols.call_sites.push(CallSite {
            id: call_id,
            caller,
            callee_name: name,
            callee_var: None,
            callee_fn_id: Some(t),
            var_args: var_args.clone(),
            fn_args: fn_args.clone(),
            span,
            is_direct: true,
        });
    }
}

/// Constructor member-initializer lists: `Derived() : Base(1, 2), sub_(3) {}`.
/// A name matching a direct base constructs that base; anything else
/// constructs the declared class of the data member.
fn lower_field_initializer_list(
    program: &mut Program,
    ctx: &mut LowerContext,
    source: &str,
    node: Node,
    caller: FnId,
) {
    let Some(cc) = ctx.class_ctx.clone() else {
        return;
    };
    let cls = cc.qual_name;
    let bases = program.bases_of(&cls);
    let cls_type = program
        .types
        .type_id_by_tag(&cls, trace_ir::TypeKind::Struct);
    for fi in node.children(&mut node.walk()) {
        if fi.kind() != "field_initializer" {
            continue;
        }
        let Some(name_node) = fi
            .children(&mut fi.walk())
            .find(|c| matches!(c.kind(), "field_identifier" | "identifier"))
        else {
            continue;
        };
        let fname = normalize_qualified(node_text(source, &name_node));
        let target_cls: Option<String> = bases
            .iter()
            .find(|b| last_segment_of(b) == fname)
            .cloned()
            .or_else(|| {
                let fid = cls_type?;
                let info = program.types.get(fid);
                let (_, fl) = info.layout.fields.iter().find(|(_, f)| f.name == fname)?;
                match program.types.get(fl.type_id).desc.clone() {
                    TypeDesc::Struct { name, .. } => Some(name),
                    _ => None,
                }
            });
        if let Some(target) = target_cls {
            let args = fi
                .children(&mut fi.walk())
                .find(|c| c.kind() == "argument_list");
            let span = node_span(program, ctx, fi);
            let (var_args, fn_args) = collect_call_args(program, ctx, source, args);
            emit_member_sites(
                program,
                caller,
                &target,
                &trace_ir::MethodKind::Ctor,
                (var_args, fn_args),
                span,
            );
        }
    }
}

fn last_segment_of(name: &str) -> &str {
    name.rsplit("::").next().unwrap_or(name)
}

/// Walk UP the inheritance chain from `cls` until some class declares the
/// member. Non-virtual declarations (and ctors) resolve to exactly the
/// declaring entries; `virtual` ones — and destructors, where
/// delete-through-base is the dominant pattern — expand downward through
/// the subclass closure as the dynamic-dispatch target set.
fn member_targets_upward(program: &Program, cls: &str, kind: &trace_ir::MethodKind) -> Vec<FnId> {
    let declared_on = |c: &str| -> Vec<FnId> { program.symbols.functions_named(&kind.name_on(c)) };
    let mut queue = std::collections::VecDeque::new();
    let mut seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    queue.push_back(cls.to_string());
    seen.insert(cls.to_string());
    while let Some(cur) = queue.pop_front() {
        let own = declared_on(&cur);
        if !own.is_empty() {
            let virtual_dispatch =
                kind.is_destructor() || own.iter().any(|t| program.symbols.function(*t).is_virtual);
            return if virtual_dispatch {
                program.method_targets(&cur, kind)
            } else {
                own
            };
        }
        for base in program.bases_of(&cur) {
            if seen.insert(base.clone()) {
                queue.push_back(base);
            }
        }
    }
    // No ancestor declares the member. A derived class may still define it
    // (the receiver's static type is just imprecise) — over-approximate by
    // searching the subclass closure.
    let down = program.method_targets(cls, kind);
    if !down.is_empty() {
        return down;
    }
    Vec::new()
}

/// Static class of a receiver expression, when inferable from declared
/// types (`this`, locals/globals, fields along typed chains, casts, news).
fn infer_static_class(
    program: &Program,
    ctx: &LowerContext,
    source: &str,
    node: Node,
) -> Option<String> {
    let node = peel_expression(node);
    match node.kind() {
        "this" => ctx.class_ctx.as_ref().map(|c| c.qual_name.clone()),
        "identifier" => {
            let v = lookup_var(ctx, program, node_text(source, &node))?;
            var_static_class(program, v)
        }
        "pointer_expression" => {
            let op = pointer_op(source, node);
            if op.as_deref() == Some("*") {
                let arg = node.named_child(0)?;
                return infer_static_class(program, ctx, source, arg);
            }
            None
        }
        "field_expression" => {
            let base = node.child_by_field_name("argument")?;
            let field = node.child_by_field_name("field")?;
            let base_cls = infer_static_class(program, ctx, source, base)?;
            let fid = program
                .types
                .type_id_by_tag(&base_cls, trace_ir::TypeKind::Struct)?;
            let info = program.types.get(fid);
            let fname = normalize_qualified(node_text(source, &field));
            let (_, fl) = info.layout.fields.iter().find(|(_, f)| f.name == fname)?;
            let inner = program.types.get(fl.type_id).desc.clone();
            match inner {
                TypeDesc::Struct { name, .. } => Some(name),
                TypeDesc::Ptr(boxed) => match *boxed {
                    TypeDesc::Struct { name, .. } => Some(name),
                    _ => None,
                },
                _ => None,
            }
        }
        "cast_expression" => {
            let type_node = node.child_by_field_name("type")?;
            let raw = normalize_qualified(node_text(source, &type_node));
            let stripped = strip_template_args(&raw);
            let qualified = if stripped.contains("::") {
                stripped
            } else {
                ctx.qualify(&stripped)
            };
            if program
                .types
                .type_id_by_tag(&qualified, trace_ir::TypeKind::Struct)
                .is_some()
            {
                Some(qualified)
            } else {
                None
            }
        }
        "new_expression" => new_expression_class(program, ctx, source, node),
        _ => None,
    }
}

fn var_static_class(program: &Program, v: VarId) -> Option<String> {
    let var = program.symbols.variable(v);
    let desc = program.types.get(var.type_id).desc.clone();
    match desc {
        TypeDesc::Struct { name, .. } => Some(name),
        TypeDesc::Ptr(boxed) => match *boxed {
            TypeDesc::Struct { name, .. } => Some(name),
            _ => None,
        },
        _ => None,
    }
}

/// The constructed class spelled in a `new T(...)` expression.
fn new_expression_class(
    program: &Program,
    ctx: &LowerContext,
    source: &str,
    node: Node,
) -> Option<String> {
    let _ = program;
    for child in node.children(&mut node.walk()) {
        match child.kind() {
            "qualified_identifier" | "type_identifier" | "template_type" => {
                let raw = normalize_qualified(node_text(source, &child));
                let stripped = strip_template_args(&raw);
                let qualified = if stripped.contains("::") {
                    stripped
                } else {
                    ctx.qualify(&stripped)
                };
                return Some(qualified);
            }
            _ => {}
        }
    }
    None
}

fn extract_flow_from_expr(
    program: &mut Program,
    ctx: &mut LowerContext,
    source: &str,
    node: Node,
    assign_target: Option<VarId>,
) {
    if node.kind() == "assignment_expression" {
        let lhs = peel_expression(
            node.child_by_field_name("left")
                .or_else(|| node.named_child(0))
                .unwrap(),
        );
        let rhs = node
            .child_by_field_name("right")
            .or_else(|| node.named_child(1))
            .unwrap();
        if is_deref_lhs(source, lhs) {
            if let Some(arg) = deref_operand(lhs) {
                if let Some(ptr) = resolve_lvalue_var(program, ctx, source, arg) {
                    if let Some(src) = expr_to_store_src(program, ctx, source, rhs) {
                        program.flow.push(FlowConstraint::Store { dst: ptr, src });
                    } else if rhs.kind() == "call_expression" {
                        if let Some(callee_name) = resolve_direct_call(program, ctx, source, rhs) {
                            let ret_temp = alloc_ret_temp(program, ctx, node);
                            program.flow.push(FlowConstraint::CallReturn {
                                dst: ret_temp,
                                callee_name,
                            });
                            program.flow.push(FlowConstraint::Store {
                                dst: ptr,
                                src: ret_temp,
                            });
                        }
                    }
                }
            }
        } else if lhs.kind() == "field_expression" {
            emit_field_store(program, ctx, source, lhs, rhs);
        } else if let Some(dst) = resolve_lvalue_var(program, ctx, source, lhs) {
            if let Some(flow) = expr_to_rhs_flow(program, ctx, source, rhs, dst) {
                program.flow.push(flow);
            }
        }
        return;
    }

    if node.kind() == "initializer_list" {
        if let Some(base) = assign_target {
            lower_initializer_list(program, ctx, source, node, base);
            return;
        }
    }

    if let Some(dst) = assign_target {
        if let Some(flow) = expr_to_rhs_flow(program, ctx, source, node, dst) {
            program.flow.push(flow);
        }
        return;
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        extract_flow_from_expr(program, ctx, source, child, None);
    }
}

fn lower_initializer_list(
    program: &mut Program,
    ctx: &mut LowerContext,
    source: &str,
    node: Node,
    base: VarId,
) {
    // Positional struct initializers (`static struct Ops o = { Fn };`):
    // map each bare value to its declared field by position and lower it
    // as a regular field store (function addresses included).
    let field_names = positional_struct_fields(program, base);
    let mut pos = 0usize;
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "designated_initializer" | "initializer_pair" => {
                lower_designated_initializer(program, ctx, source, child, base);
            }
            "(" | ")" | "," | ";" | "{" | "}" => {}
            _ => {
                if !field_names.is_empty() {
                    if let Some(fname) = field_names.get(pos).and_then(|f| f.clone()) {
                        if let Some(fid) = field_id_for(program, base, &fname) {
                            emit_field_value_store(
                                program,
                                ctx,
                                source,
                                child,
                                base,
                                &[fid],
                                child,
                            );
                        }
                    }
                }
                pos += 1;
            }
        }
    }
}

/// Declared field names of the struct type behind `base`, in order.
fn positional_struct_fields(program: &Program, base: VarId) -> Vec<Option<String>> {
    let ty = program.symbols.variable(base).type_id;
    let TypeDesc::Struct { name, .. } = program.types.get(ty).desc.clone() else {
        return Vec::new();
    };
    if name.is_empty() {
        return Vec::new();
    }
    let Some(tid) = program
        .types
        .type_id_by_tag(&name, trace_ir::TypeKind::Struct)
    else {
        return Vec::new();
    };
    program
        .types
        .get(tid)
        .layout
        .fields
        .iter()
        .map(|(_, f)| Some(f.name.clone()))
        .collect()
}

fn field_id_for(program: &Program, base: VarId, fname: &str) -> Option<trace_ir::FieldId> {
    let ty = program.symbols.variable(base).type_id;
    let TypeDesc::Struct { name, .. } = program.types.get(ty).desc.clone() else {
        return None;
    };
    let tid = program
        .types
        .type_id_by_tag(&name, trace_ir::TypeKind::Struct)?;
    program.types.field_id_by_name(tid, fname)
}

fn lower_designated_initializer(
    program: &mut Program,
    ctx: &mut LowerContext,
    source: &str,
    node: Node,
    base: VarId,
) {
    let mut field_names = Vec::new();
    let mut value = None;
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "field_designator" => {
                let mut inner = child.walk();
                for c in child.children(&mut inner) {
                    if c.kind() == "field_identifier" {
                        field_names.push(node_text(source, &c).to_string());
                    }
                }
            }
            // `[i]` selects an array element; element access is
            // index-insensitive in this IR, so the subscript itself carries no
            // information — just don't mistake it for the value.
            "subscript_designator" | "=" => {}
            _ if value.is_none() && child.is_named() && child.kind() != "field_designator" => {
                value = Some(child)
            }
            _ => {}
        }
    }
    let Some(value_node) = value else {
        return;
    };
    let mut type_id = match struct_type_for_var(program, base) {
        Some(t) => t,
        None => return,
    };
    if value_node.kind() == "initializer_list" {
        // `[i] = { .f = v }` or `.s = { .g = v }`: descend into the nested
        // list against the same base (array elements are index-insensitive),
        // chaining GEPs for any field designators seen so far.
        let mut current = base;
        for fname in &field_names {
            let Some(fid) = program.types.field_id_by_name(type_id, fname) else {
                return;
            };
            current = alloc_gep_temp(program, ctx, node, current, fid);
            type_id = program.types.get(type_id).layout.fields[&fid].type_id;
            type_id = peel_ptr_to_struct(program, type_id);
        }
        lower_initializer_list(program, ctx, source, value_node, current);
        return;
    }
    if field_names.is_empty() {
        // Designated form without a field designator (`[i] = v` on a plain
        // fn-ptr array): handled by `lower_fn_ptr_array_init`.
        return;
    }
    let mut field_ids = Vec::with_capacity(field_names.len());
    for fname in &field_names {
        let Some(fid) = program.types.field_id_by_name(type_id, fname) else {
            return;
        };
        field_ids.push(fid);
        type_id = program.types.get(type_id).layout.fields[&fid].type_id;
    }
    emit_field_value_store(program, ctx, source, node, base, &field_ids, value_node);
}

fn peel_expression(mut node: Node) -> Node {
    while node.kind() == "parenthesized_expression" {
        node = node.named_child(0).unwrap_or(node);
    }
    node
}

fn emit_field_store(
    program: &mut Program,
    ctx: &mut LowerContext,
    source: &str,
    lhs: Node,
    rhs: Node,
) {
    let Some((base, field_ids)) = decompose_field_path(program, ctx, source, lhs) else {
        return;
    };
    if field_ids.is_empty() {
        return;
    }
    emit_field_value_store(program, ctx, source, lhs, base, &field_ids, rhs);
}

fn emit_field_value_store(
    program: &mut Program,
    ctx: &mut LowerContext,
    source: &str,
    span_node: Node,
    base: VarId,
    field_ids: &[FieldId],
    value_node: Node,
) {
    let mut current = base;
    for (i, fid) in field_ids.iter().enumerate() {
        if i + 1 == field_ids.len() {
            let gep = alloc_gep_temp(program, ctx, span_node, current, *fid);
            if let Some(src) = expr_to_store_src(program, ctx, source, value_node) {
                program.flow.push(FlowConstraint::Store { dst: gep, src });
            } else if value_node.kind() == "identifier" {
                let name = node_text(source, &value_node);
                if let Some(callee) = resolve_function_named(program, ctx, name) {
                    let src_temp = alloc_ret_temp(program, ctx, span_node);
                    program.flow.push(FlowConstraint::AddrOfFn {
                        dst: src_temp,
                        callee,
                    });
                    program.flow.push(FlowConstraint::Store {
                        dst: gep,
                        src: src_temp,
                    });
                } else {
                    // Defined later in the unit (no forward declaration):
                    // defer until the whole symbol table is populated.
                    ctx.pending.borrow_mut().push(PendingFnRef::FieldStore {
                        dst: gep,
                        name: name.to_string(),
                        span: node_span(program, ctx, span_node),
                    });
                }
            } else {
                let ret_temp = alloc_ret_temp(program, ctx, span_node);
                let emitted = if value_node.kind() == "call_expression" {
                    if let Some(callee_name) = resolve_direct_call(program, ctx, source, value_node)
                    {
                        program.flow.push(FlowConstraint::CallReturn {
                            dst: ret_temp,
                            callee_name,
                        });
                        true
                    } else {
                        false
                    }
                } else {
                    expr_to_rhs_flow(program, ctx, source, value_node, ret_temp)
                        .map(|flow| {
                            program.flow.push(flow);
                        })
                        .is_some()
                };
                if emitted {
                    program.flow.push(FlowConstraint::Store {
                        dst: gep,
                        src: ret_temp,
                    });
                }
            }
        } else {
            current = alloc_gep_temp(program, ctx, span_node, current, *fid);
        }
    }
}

fn alloc_gep_temp(
    program: &mut Program,
    ctx: &LowerContext,
    span_node: Node,
    base: VarId,
    field: FieldId,
) -> VarId {
    let var_id = program.symbols.alloc_var_id();
    let span = node_span(program, ctx, span_node);
    program.symbols.add_variable(Variable {
        id: var_id,
        name: format!("_gep{}", var_id.0),
        type_id: program.types.int(),
        storage: StorageClass::Local,
        fn_id: ctx.current_fn,
        param_index: None,
        span,
        is_pointer: true,
    });
    program.flow.push(FlowConstraint::GepField {
        dst: var_id,
        base,
        field,
    });
    var_id
}

fn field_name_from_node(source: &str, node: Node) -> Option<String> {
    node.child_by_field_name("field")
        .map(|n| node_text(source, &n).to_string())
}

fn decompose_field_path(
    program: &mut Program,
    ctx: &LowerContext,
    source: &str,
    node: Node,
) -> Option<(VarId, Vec<FieldId>)> {
    let mut field_names = Vec::new();
    let mut cur = peel_expression(node);
    while cur.kind() == "field_expression" {
        field_names.push(field_name_from_node(source, cur)?);
        cur = cur.child_by_field_name("argument")?;
    }
    let base = resolve_lvalue_var(program, ctx, source, cur)?;
    field_names.reverse();

    let mut type_id = struct_type_for_var(program, base)?;
    let mut field_ids = Vec::new();
    for fname in &field_names {
        let fid = program.types.field_id_by_name(type_id, fname)?;
        field_ids.push(fid);
        let layout = program.types.get(type_id);
        type_id = layout.layout.fields.get(&fid)?.type_id;
        type_id = peel_ptr_to_struct(program, type_id);
    }
    Some((base, field_ids))
}

fn peel_ptr_to_struct(program: &mut Program, type_id: trace_ir::TypeId) -> trace_ir::TypeId {
    let inner = match &program.types.get(type_id).desc {
        TypeDesc::Ptr(inner) => Some((**inner).clone()),
        _ => None,
    };
    inner.map_or(type_id, |desc| program.types.intern(desc))
}

fn struct_type_for_var(program: &mut Program, var: VarId) -> Option<trace_ir::TypeId> {
    let mut type_id = variable_type_id(program, var)?;
    for _ in 0..4 {
        match &program.types.get(type_id).desc {
            TypeDesc::Ptr(inner) => {
                type_id = program.types.resolve_type_id(inner);
            }
            // Arrays of structs: field access via `arr[i].f` resolves
            // against the element type (index-insensitive over-approx).
            TypeDesc::Array { elem, .. } => {
                let inner = (**elem).clone();
                type_id = program.types.intern(inner);
            }
            TypeDesc::Struct { .. } | TypeDesc::Union { .. } => return Some(type_id),
            _ => return Some(type_id),
        }
    }
    Some(type_id)
}

fn variable_type_id(program: &Program, var: VarId) -> Option<trace_ir::TypeId> {
    program.symbols.variable_by_id(var).map(|v| v.type_id)
}

fn pointer_op(source: &str, node: Node) -> Option<String> {
    if node.kind() != "pointer_expression" {
        return None;
    }
    node.child_by_field_name("operator")
        .map(|n| node_text(source, &n).to_string())
        .or_else(|| node.child(0).map(|n| node_text(source, &n).to_string()))
}

fn pointer_arg(node: Node) -> Option<Node> {
    if node.kind() != "pointer_expression" {
        return None;
    }
    node.child_by_field_name("argument")
        .or_else(|| node.named_child(0))
}

fn is_deref_lhs(source: &str, node: Node) -> bool {
    pointer_op(source, node).as_deref() == Some("*")
}

fn deref_operand(node: Node) -> Option<Node> {
    pointer_arg(node)
}

/// Lower `&base.f1.f2` into a gep-temp chain so the resulting pointer
/// targets the field's own abstract location (with the field's type),
/// not the flattened outer instance. Returns the final temp var.
fn addr_of_field_path(
    program: &mut Program,
    ctx: &LowerContext,
    source: &str,
    arg: Node,
) -> Option<VarId> {
    let peeled = peel_expression(arg);
    if peeled.kind() != "field_expression" {
        return None;
    }
    let (base, field_ids) = decompose_field_path(program, ctx, source, peeled)?;
    let mut current = base;
    for fid in field_ids {
        current = alloc_gep_temp(program, ctx, peeled, current, fid);
    }
    Some(current)
}

fn expr_to_store_src(
    program: &mut Program,
    ctx: &mut LowerContext,
    source: &str,
    node: Node,
) -> Option<VarId> {
    match node.kind() {
        "pointer_expression" => {
            let op = pointer_op(source, node);
            let arg = pointer_arg(node)?;
            if op.as_deref() == Some("&") {
                return addr_of_field_path(program, ctx, source, arg)
                    .or_else(|| resolve_lvalue_var(program, ctx, source, arg));
            }
            None
        }
        "identifier" => resolve_lvalue_var(program, ctx, source, node),
        _ => resolve_expr_var(program, ctx, source, node),
    }
}

fn expr_to_rhs_flow(
    program: &mut Program,
    ctx: &LowerContext,
    source: &str,
    node: Node,
    dst: VarId,
) -> Option<FlowConstraint> {
    match node.kind() {
        "identifier" => {
            let name = node_text(source, &node);
            if let Some(callee) = program
                .symbols
                .resolve_function_in_scope(name, Some(ctx.current_file))
            {
                Some(FlowConstraint::AddrOfFn { dst, callee })
            } else if let Some(src) = lookup_var(ctx, program, name) {
                Some(FlowConstraint::Copy { dst, src })
            } else {
                // Might be a function defined later in the unit.
                ctx.pending.borrow_mut().push(PendingFnRef::RhsIdent {
                    dst,
                    name: name.to_string(),
                });
                None
            }
        }
        "pointer_expression" => {
            let op = pointer_op(source, node);
            let arg = pointer_arg(node)?;
            if op.as_deref() == Some("&") {
                if let Some(callee) = resolve_fn_ref(program, ctx, source, arg) {
                    Some(FlowConstraint::AddrOfFn { dst, callee })
                } else if let Some(gep) = addr_of_field_path(program, ctx, source, arg) {
                    // The gep temp's pts-to is the field's own location.
                    Some(FlowConstraint::Copy { dst, src: gep })
                } else if let Some(src) = resolve_lvalue_var(program, ctx, source, arg) {
                    Some(FlowConstraint::AddrOfVar { dst, src })
                } else {
                    if arg.kind() == "identifier" {
                        // Might be a function defined later in the unit.
                        ctx.pending.borrow_mut().push(PendingFnRef::AddrOfIdent {
                            dst,
                            name: node_text(source, &arg).to_string(),
                        });
                    }
                    None
                }
            } else if op.as_deref() == Some("*") {
                let ptr = resolve_lvalue_var(program, ctx, source, arg)?;
                Some(FlowConstraint::Load { dst, src: ptr })
            } else {
                None
            }
        }
        "cast_expression" => node
            .child_by_field_name("expression")
            .or_else(|| node.named_child(1))
            .and_then(|inner| expr_to_rhs_flow(program, ctx, source, inner, dst)),
        "parenthesized_expression" => node
            .named_child(0)
            .and_then(|inner| expr_to_rhs_flow(program, ctx, source, inner, dst)),
        "call_expression" => {
            if let Some(callee_name) = resolve_direct_call(program, ctx, source, node) {
                program
                    .flow
                    .push(FlowConstraint::CallReturn { dst, callee_name });
            }
            None
        }
        "field_expression" => {
            let (base, field_ids) = decompose_field_path(program, ctx, source, node)?;
            let mut current = base;
            for (i, fid) in field_ids.iter().enumerate() {
                if i + 1 == field_ids.len() {
                    let tmp = alloc_gep_temp(program, ctx, node, current, *fid);
                    return Some(FlowConstraint::Load { dst, src: tmp });
                }
                current = alloc_gep_temp(program, ctx, node, current, *fid);
            }
            None
        }
        _ => resolve_expr_var(program, ctx, source, node)
            .map(|src| FlowConstraint::Copy { dst, src }),
    }
}

fn collect_return_statement(
    program: &mut Program,
    ctx: &LowerContext,
    source: &str,
    node: Node,
    fn_id: FnId,
) {
    let value = node
        .child_by_field_name("value")
        .or_else(|| node.named_child(0));
    let Some(value) = value else {
        return;
    };
    if value.kind() == ";" {
        return;
    }
    collect_return_flow(program, ctx, source, value, fn_id);
}

fn collect_return_flow(
    program: &mut Program,
    ctx: &LowerContext,
    source: &str,
    node: Node,
    fn_id: FnId,
) {
    if let Some(flow) = return_flow_from_expr(program, ctx, source, node, fn_id) {
        program.fn_returns.entry(fn_id).or_default().push(flow);
    }
}

fn return_flow_from_expr(
    program: &mut Program,
    ctx: &LowerContext,
    source: &str,
    node: Node,
    fn_id: FnId,
) -> Option<ReturnFlow> {
    let node = peel_expression(node);
    match node.kind() {
        "pointer_expression" => {
            let op = pointer_op(source, node);
            let arg = pointer_arg(node)?;
            if op.as_deref() == Some("&") {
                if let Some(callee) = resolve_fn_ref(program, ctx, source, arg) {
                    return Some(ReturnFlow::AddrOfFn { callee });
                }
                // `&base.field` returns a pointer to the field subobject;
                // carry it as a Copy of the gep temp's pts-to.
                if let Some(gep) = addr_of_field_path(program, ctx, source, arg) {
                    return Some(ReturnFlow::Copy { src: gep });
                }
                if let Some(src) = resolve_lvalue_var(program, ctx, source, arg) {
                    return Some(ReturnFlow::AddrOfVar { src });
                }
                if arg.kind() == "identifier" {
                    ctx.pending.borrow_mut().push(PendingFnRef::ReturnAddrOf {
                        owner: fn_id,
                        name: node_text(source, &arg).to_string(),
                    });
                }
                return None;
            }
            None
        }
        "identifier" => {
            let name = node_text(source, &node);
            if resolve_function_named(program, ctx, name).is_some() {
                None
            } else if let Some(src) = lookup_var(ctx, program, name) {
                Some(ReturnFlow::Copy { src })
            } else {
                ctx.pending.borrow_mut().push(PendingFnRef::ReturnIdent {
                    owner: fn_id,
                    name: name.to_string(),
                });
                None
            }
        }
        "call_expression" => resolve_direct_call_name(source, node)
            .map(|callee_name| ReturnFlow::Call { callee_name }),
        "cast_expression" => node
            .child_by_field_name("expression")
            .or_else(|| node.named_child(1))
            .and_then(|inner| return_flow_from_expr(program, ctx, source, inner, fn_id)),
        "parenthesized_expression" => node
            .named_child(0)
            .and_then(|inner| return_flow_from_expr(program, ctx, source, inner, fn_id)),
        _ => None,
    }
}

fn resolve_direct_call_name(source: &str, node: Node) -> Option<String> {
    let func = node.child_by_field_name("function")?;
    let func = peel_expression(func);
    match func.kind() {
        "identifier" => Some(node_text(source, &func).to_string()),
        "pointer_expression" | "parenthesized_expression" => func
            .named_child(0)
            .and_then(|inner| resolve_direct_call_name(source, inner)),
        _ => None,
    }
}

fn resolve_direct_call(
    _program: &Program,
    _ctx: &LowerContext,
    source: &str,
    node: Node,
) -> Option<String> {
    resolve_direct_call_name(source, node)
}

fn alloc_ret_temp(program: &mut Program, ctx: &LowerContext, span_node: Node) -> VarId {
    let span = node_span(program, ctx, span_node);
    alloc_ret_temp_spanned(program, ctx, span)
}

fn alloc_ret_temp_spanned(program: &mut Program, ctx: &LowerContext, span: Span) -> VarId {
    let var_id = program.symbols.alloc_var_id();
    program.symbols.add_variable(Variable {
        id: var_id,
        name: format!("_ret{}", var_id.0),
        type_id: program.types.int(),
        storage: StorageClass::Local,
        fn_id: ctx.current_fn,
        param_index: None,
        span,
        is_pointer: true,
    });
    var_id
}

fn resolve_lvalue_var(
    program: &Program,
    ctx: &LowerContext,
    source: &str,
    node: Node,
) -> Option<VarId> {
    match node.kind() {
        "identifier" => {
            let name = node_text(source, &node);
            lookup_var(ctx, program, name)
        }
        "pointer_expression" => {
            let op = pointer_op(source, node);
            let arg = pointer_arg(node)?;
            if op.as_deref() == Some("*") {
                return resolve_lvalue_var(program, ctx, source, arg);
            }
            resolve_lvalue_var(program, ctx, source, arg)
        }
        "field_expression" | "subscript_expression" => node
            .child_by_field_name("argument")
            .and_then(|n| resolve_lvalue_var(program, ctx, source, n)),
        "parenthesized_expression" => node
            .named_child(0)
            .and_then(|n| resolve_lvalue_var(program, ctx, source, n)),
        "cast_expression" => node
            .child_by_field_name("expression")
            .or_else(|| node.named_child(1))
            .and_then(|n| resolve_lvalue_var(program, ctx, source, n)),
        _ => None,
    }
}

fn find_function_declarator(node: Node) -> Option<Node> {
    if node.kind() == "function_declarator" {
        return Some(node);
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if let Some(found) = find_function_declarator(child) {
            return Some(found);
        }
    }
    None
}

fn type_desc_from_field_declaration(
    program: &mut Program,
    ctx: &LowerContext,
    source: &str,
    node: Node,
) -> Option<(String, TypeDesc)> {
    let decl = node.child_by_field_name("declarator")?;
    let (fname, _) = parse_declarator_name(source, decl);
    if fname.is_empty() {
        return None;
    }
    let base = node
        .child_by_field_name("type")
        .map(|t| type_desc_from_node(program, ctx, source, t))
        .unwrap_or(TypeDesc::Int);
    let desc = if is_function_pointer_declarator(decl) {
        TypeDesc::FnPtr {
            ret: Box::new(base),
            params: Vec::new(),
        }
    } else if declarator_is_pointer(decl) {
        TypeDesc::Ptr(Box::new(base))
    } else {
        base
    };
    Some((fname, desc))
}

fn declarator_is_pointer(decl: Node) -> bool {
    match decl.kind() {
        "pointer_declarator" => true,
        "function_declarator" | "parenthesized_declarator" | "array_declarator" => decl
            .child_by_field_name("declarator")
            .is_some_and(declarator_is_pointer),
        _ => false,
    }
}

fn resolve_call_fn_arg(
    program: &Program,
    ctx: &LowerContext,
    source: &str,
    node: Node,
) -> Option<FnId> {
    if let Some(fn_id) = resolve_fn_ref(program, ctx, source, node) {
        return Some(fn_id);
    }
    if node.kind() == "pointer_expression" {
        if let Some(inner) = pointer_arg(node) {
            return resolve_call_fn_arg(program, ctx, source, inner);
        }
    }
    None
}

fn resolve_function_named(program: &Program, ctx: &LowerContext, name: &str) -> Option<FnId> {
    program
        .symbols
        .resolve_function_in_scope(name, Some(ctx.current_file))
        .or_else(|| program.symbols.resolve_function(name))
}

fn resolve_fn_ref(program: &Program, ctx: &LowerContext, source: &str, node: Node) -> Option<FnId> {
    if node.kind() == "identifier" {
        return resolve_function_named(program, ctx, node_text(source, &node));
    }
    None
}

fn resolve_callee_with_loads(
    program: &mut Program,
    ctx: &mut LowerContext,
    source: &str,
    node: Node,
) -> (String, bool, Option<VarId>) {
    let node = peel_expression(node);
    if node.kind() == "field_expression" {
        if let Some((base, field_ids)) = decompose_field_path(program, ctx, source, node) {
            let text = field_callee_text(source, node);
            if let Some(load_var) =
                emit_field_fn_ptr_load(program, ctx, source, node, base, &field_ids)
            {
                return (text, false, Some(load_var));
            }
        }
    }
    resolve_callee(program, ctx, source, node)
}

fn field_callee_text(source: &str, node: Node) -> String {
    let mut parts = Vec::new();
    let mut cur = peel_expression(node);
    while cur.kind() == "field_expression" {
        if let Some(field) = cur.child_by_field_name("field") {
            parts.push(node_text(source, &field).to_string());
        }
        cur = cur.child_by_field_name("argument").unwrap_or(cur);
    }
    parts.reverse();
    let base = node_text(source, &cur);
    if parts.is_empty() {
        base.to_string()
    } else {
        format!("{}->{}", base, parts.join("->"))
    }
}

fn emit_field_fn_ptr_load(
    program: &mut Program,
    ctx: &LowerContext,
    _source: &str,
    span_node: Node,
    base: VarId,
    field_ids: &[FieldId],
) -> Option<VarId> {
    if field_ids.is_empty() {
        return None;
    }
    let mut type_id = struct_type_for_var(program, base)?;
    let mut current = base;
    for (i, fid) in field_ids.iter().enumerate() {
        let gep = alloc_gep_temp(program, ctx, span_node, current, *fid);
        let field_type_id = program.types.get(type_id).layout.fields.get(fid)?.type_id;
        type_id = field_type_id;
        if i + 1 == field_ids.len() {
            let load_var = program.symbols.alloc_var_id();
            let span = node_span(program, ctx, span_node);
            program.symbols.add_variable(Variable {
                id: load_var,
                name: format!("_load{}", load_var.0),
                type_id: program.types.int(),
                storage: StorageClass::Local,
                fn_id: ctx.current_fn,
                param_index: None,
                span,
                is_pointer: true,
            });
            program.flow.push(FlowConstraint::Load {
                dst: load_var,
                src: gep,
            });
            return Some(load_var);
        }
        if matches!(program.types.get(field_type_id).desc, TypeDesc::Ptr(_)) {
            let load_var = program.symbols.alloc_var_id();
            let span = node_span(program, ctx, span_node);
            program.symbols.add_variable(Variable {
                id: load_var,
                name: format!("_load{}", load_var.0),
                type_id: field_type_id,
                storage: StorageClass::Local,
                fn_id: ctx.current_fn,
                param_index: None,
                span,
                is_pointer: true,
            });
            program.flow.push(FlowConstraint::Load {
                dst: load_var,
                src: gep,
            });
            current = load_var;
            type_id = program
                .types
                .resolve_type_id(match &program.types.get(field_type_id).desc {
                    TypeDesc::Ptr(inner) => inner,
                    _ => unreachable!(),
                });
        } else {
            current = gep;
        }
    }
    None
}

fn is_likely_macro_callee(name: &str) -> bool {
    if name.contains("->") || name.contains('.') || name.contains('(') {
        return false;
    }
    name.len() > 2
        && name
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

fn resolve_callee(
    program: &Program,
    ctx: &LowerContext,
    source: &str,
    node: Node,
) -> (String, bool, Option<VarId>) {
    let node = peel_expression(node);
    match node.kind() {
        "identifier" => {
            let name = node_text(source, &node).to_string();
            if let Some(v) = lookup_var(ctx, program, &name) {
                return (name, false, Some(v));
            }
            if resolve_function_named(program, ctx, &name).is_some() {
                return (name, true, None);
            }
            (name, false, None)
        }
        // C++: `ns::fn`, `Cls::static_fn` — normalized text resolves by name
        // in the caller; template spellings drop their argument list.
        "qualified_identifier" => (normalize_qualified(node_text(source, &node)), false, None),
        "template_function" => {
            let raw = node_text(source, &node);
            let name = strip_template_args(&normalize_qualified(raw));
            if let Some(v) = lookup_var(ctx, program, &name) {
                return (name, false, Some(v));
            }
            (name, true, None)
        }
        "pointer_expression" | "parenthesized_expression" => node
            .named_child(0)
            .map(|inner| resolve_callee(program, ctx, source, inner))
            .unwrap_or(("<indirect>".into(), false, None)),
        "field_expression" => {
            let field = node
                .child_by_field_name("field")
                .map(|n| node_text(source, &n).to_string())
                .unwrap_or_else(|| "field".into());
            let arg = node.child_by_field_name("argument").unwrap();
            if let Some(v) = resolve_lvalue_var(program, ctx, source, arg) {
                return (
                    format!("{}->{}", node_text(source, &arg), field),
                    false,
                    Some(v),
                );
            }
            (field, false, None)
        }
        "subscript_expression" => {
            let arr = node.child_by_field_name("argument").unwrap();
            if let Some(v) = resolve_lvalue_var(program, ctx, source, arr) {
                return (format!("{}[...]", node_text(source, &arr)), false, Some(v));
            }
            ("<indirect>".into(), false, None)
        }
        _ => (node_text(source, &node).to_string(), false, None),
    }
}

fn resolve_expr_var(
    program: &Program,
    ctx: &LowerContext,
    source: &str,
    node: Node,
) -> Option<VarId> {
    match node.kind() {
        "identifier" => {
            let name = node_text(source, &node);
            lookup_var(ctx, program, name)
        }
        "pointer_expression" => {
            let op = pointer_op(source, node);
            let arg = pointer_arg(node)?;
            if op.as_deref() == Some("&") {
                return resolve_lvalue_var(program, ctx, source, arg);
            }
            resolve_expr_var(program, ctx, source, arg)
        }
        "field_expression" | "subscript_expression" => node
            .child_by_field_name("argument")
            .and_then(|n| resolve_expr_var(program, ctx, source, n)),
        "parenthesized_expression" => node
            .named_child(0)
            .and_then(|n| resolve_expr_var(program, ctx, source, n)),
        _ => None,
    }
}

fn lookup_var(ctx: &LowerContext, program: &Program, name: &str) -> Option<VarId> {
    if ctx.current_fn.is_some() {
        if let Some(&id) = ctx.locals.get(name) {
            return Some(id);
        }
    }
    if let Some(&id) = program.symbols.global_by_name.get(name) {
        return Some(id);
    }
    program
        .symbols
        .variables
        .iter()
        .find(|v| {
            v.name == name
                && match v.storage {
                    StorageClass::FileStatic => v.span.file == ctx.current_file,
                    StorageClass::FnStatic => v.fn_id == ctx.current_fn,
                    _ => false,
                }
        })
        .map(|v| v.id)
}

fn declaration_is_static(_source: &str, node: Node) -> bool {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() != "storage_class_specifier" {
            continue;
        }
        let mut inner = child.walk();
        for token in child.children(&mut inner) {
            if token.kind() == "static" {
                return true;
            }
        }
    }
    false
}

fn is_function_pointer_declarator(decl: Node) -> bool {
    if decl.kind() != "function_declarator" {
        return false;
    }
    matches!(
        decl.child_by_field_name("declarator").map(|n| n.kind()),
        Some("parenthesized_declarator") | Some("pointer_declarator")
    )
}

/// If `decl` denotes a function whose declarator chain starts with one or more
/// pointer levels (`T *f(...)` / `T **f(...)`), return the innermost
/// non-fn-ptr `function_declarator` and the number of pointer levels.
/// Variables (plain pointers, arrays, fn-ptr vars) yield `None`.
fn fn_decl_under_pointer(decl: Node) -> Option<(Node, usize)> {
    let mut cur = decl;
    let mut depth = 0usize;
    loop {
        match cur.kind() {
            "pointer_declarator" => {
                depth += 1;
                cur = cur
                    .child_by_field_name("declarator")
                    .or_else(|| cur.named_child(0))?;
            }
            "parenthesized_declarator" => {
                cur = cur.named_child(0)?;
            }
            "function_declarator" => {
                if is_function_pointer_declarator(cur) {
                    return None;
                }
                return Some((cur, depth));
            }
            _ => return None,
        }
    }
}

fn storage_for(ctx: &LowerContext, is_static: bool) -> StorageClass {
    if ctx.current_fn.is_some() {
        if is_static {
            StorageClass::FnStatic
        } else {
            StorageClass::Local
        }
    } else if is_static {
        StorageClass::FileStatic
    } else {
        StorageClass::Global
    }
}

fn type_desc_from_node(
    program: &mut Program,
    ctx: &LowerContext,
    source: &str,
    node: Node,
) -> TypeDesc {
    if node.kind() == "struct_specifier" || node.kind() == "union_specifier" {
        let name = lower_struct_specifier(program, ctx, source, node);
        if node.kind() == "union_specifier" {
            return TypeDesc::Union {
                name,
                fields: Vec::new(),
            };
        }
        return TypeDesc::Struct {
            name,
            fields: Vec::new(),
        };
    }
    if node.kind() == "class_specifier" {
        let name = lower_struct_specifier(program, ctx, source, node);
        return TypeDesc::Struct {
            name,
            fields: Vec::new(),
        };
    }
    if matches!(
        node.kind(),
        "qualified_identifier" | "type_identifier" | "template_type" | "placeholder_type_specifier"
    ) {
        let raw = normalize_qualified(node_text(source, &node));
        if node.kind() == "placeholder_type_specifier" {
            // `auto`: unknown until the initializer is examined; callers
            // refine new-expression initializers separately.
            return TypeDesc::Unknown;
        }
        let stripped = strip_template_args(&raw);
        let tag_hit = program
            .types
            .type_id_by_tag(&ctx.qualify(&stripped), trace_ir::TypeKind::Struct);
        let looks_class = stripped.contains("::")
            || stripped != raw // had template args stripped
            || tag_hit.is_some();
        if !looks_class {
            // Plain C typedef aliases keep the legacy path below.
        } else {
            let qualified = if stripped.contains("::") {
                stripped
            } else {
                ctx.qualify(&stripped)
            };
            return TypeDesc::Struct {
                name: qualified,
                fields: Vec::new(),
            };
        }
    }
    let text = node_text(source, &node);
    if text.contains("union") {
        TypeDesc::Union {
            name: extract_tag_name(source, &node, "union"),
            fields: Vec::new(),
        }
    } else if text.contains("struct") {
        TypeDesc::Struct {
            name: extract_tag_name(source, &node, "struct"),
            fields: Vec::new(),
        }
    } else if text.contains("char") {
        TypeDesc::Char
    } else if text.contains("void") {
        TypeDesc::Void
    } else {
        // Bare identifiers may be typedef aliases (`fn_t`, `SHandle`);
        // resolving them keeps pointer-ness that would otherwise degrade
        // to `Int` and mislead downstream type checks.
        let alias = text.trim();
        if !alias.contains(char::is_whitespace) && !alias.is_empty() {
            if let Some(desc) = program.types.resolve_alias(alias) {
                return desc.clone();
            }
        }
        TypeDesc::Int
    }
}

/// Resolve a `typedef X *Name;` / `typedef void (*Name)(...);` underlying
/// descriptor by walking the declarator chain for pointer/function/array
/// nesting. Only the shape matters for analysis purposes.
fn typedef_underlying_desc(
    program: &mut Program,
    ctx: &LowerContext,
    source: &str,
    node: Node,
) -> Option<TypeDesc> {
    let type_node = node.child_by_field_name("type")?;
    let decl_node = node.child_by_field_name("declarator")?;
    let base = type_desc_from_node(program, ctx, source, type_node);
    Some(walk_declarator_shape(decl_node, base))
}

fn walk_declarator_shape(node: Node, base: TypeDesc) -> TypeDesc {
    match node.kind() {
        "pointer_declarator" => {
            let inner = node
                .child_by_field_name("declarator")
                .map(|n| walk_declarator_shape(n, base.clone()))
                .unwrap_or(base);
            TypeDesc::Ptr(Box::new(inner))
        }
        "array_declarator" => {
            let inner = node
                .child_by_field_name("declarator")
                .map(|n| walk_declarator_shape(n, base.clone()))
                .unwrap_or(base);
            TypeDesc::Array {
                elem: Box::new(inner),
                size: None,
            }
        }
        "function_declarator" => {
            let inner = node.child_by_field_name("declarator");
            // `typedef void (*Name)(...)`: the pointer sits INSIDE the
            // parenthesized declarator, so it binds to the identifier first
            // and the function suffix applies outside it — the alias is
            // pointer-to-function, not function-returning-pointer. A plain
            // `typedef int f_t(int)` stays a bare FnPtr.
            let ptr_wrapped = inner.and_then(peel_paren_declarator).and_then(|n| {
                if n.kind() == "pointer_declarator" {
                    n.child_by_field_name("declarator")
                } else {
                    None
                }
            });
            if let Some(under) = ptr_wrapped {
                let ret = walk_declarator_shape(under, base);
                return TypeDesc::Ptr(Box::new(TypeDesc::FnPtr {
                    ret: Box::new(ret),
                    params: Vec::new(),
                }));
            }
            let ret = inner
                .map(|n| walk_declarator_shape(n, base.clone()))
                .unwrap_or(base);
            TypeDesc::FnPtr {
                ret: Box::new(ret),
                params: Vec::new(),
            }
        }
        "parenthesized_declarator" => node
            .named_child(0)
            .map(|n| walk_declarator_shape(n, base.clone()))
            .unwrap_or(base),
        _ => base,
    }
}

fn peel_paren_declarator(node: Node) -> Option<Node> {
    match node.kind() {
        "parenthesized_declarator" => node.named_child(0),
        _ => Some(node),
    }
}

fn parse_type_node(
    program: &mut Program,
    ctx: &LowerContext,
    source: &str,
    node: Node,
) -> trace_ir::TypeId {
    let desc = type_desc_from_node(program, ctx, source, node);
    program.types.intern(desc)
}

fn extract_tag_name(source: &str, node: &Node, keyword: &str) -> String {
    let text = node_text(source, node);
    if let Some(rest) = text.split(keyword).nth(1) {
        rest.trim()
            .trim_start_matches(" {")
            .split(|c: char| !c.is_alphanumeric() && c != '_')
            .next()
            .unwrap_or("anon")
            .to_string()
    } else {
        "anon".into()
    }
}

fn parse_declarator_name(source: &str, node: Node) -> (String, bool) {
    match node.kind() {
        "identifier" => (node_text(source, &node).to_string(), false),
        "pointer_declarator" => {
            if let Some(inner) = node.child_by_field_name("declarator") {
                let (name, _) = parse_declarator_name(source, inner);
                (name, true)
            } else {
                (String::new(), true)
            }
        }
        // C++ reference parameters alias their argument; treat them like
        // pointers so stores through them land on the caller's memory.
        "reference_declarator" => {
            if let Some(inner) = node.child_by_field_name("declarator") {
                let (name, _) = parse_declarator_name(source, inner);
                (name, true)
            } else {
                (String::new(), true)
            }
        }
        "qualified_identifier" => (normalize_qualified(node_text(source, &node)), false),
        // `~Name` spans two preprocessor tokens joined by whitespace in
        // expansion output; collapse it so protos and defs share one name.
        "destructor_name" => (normalize_qualified(node_text(source, &node)), false),
        "function_declarator" => {
            if let Some(inner) = node.child_by_field_name("declarator") {
                parse_declarator_name(source, inner)
            } else {
                (String::new(), false)
            }
        }
        "parenthesized_declarator" => node
            .named_child(0)
            .map(|n| parse_declarator_name(source, n))
            .unwrap_or((String::new(), false)),
        "array_declarator" => node
            .child_by_field_name("declarator")
            .map(|n| parse_declarator_name(source, n))
            .unwrap_or((String::new(), false)),
        _ => (node_text(source, &node).to_string(), false),
    }
}

/// Collapse preprocessor-introduced whitespace inside a qualified name and
/// strip balanced `<...>` argument spans per segment:
/// `outer :: inner :: Box < int >` → `outer::inner::Box`,
/// `clampT < double >` → `clampT`.
fn normalize_qualified(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut angle_depth = 0i32;
    for ch in text.chars() {
        if angle_depth > 0 {
            match ch {
                '<' => angle_depth += 1,
                '>' => angle_depth -= 1,
                _ => {}
            }
            continue;
        }
        match ch {
            '<' => angle_depth += 1,
            c if c.is_whitespace() => {}
            _ => out.push(ch),
        }
    }
    out
}

/// Strip a trailing balanced `<...>` argument list from a type/function
/// spelling: `clampT<double>` → `clampT`, `Box<int>` → `Box`.
fn strip_template_args(text: &str) -> String {
    let trimmed = text.trim_end();
    if !trimmed.ends_with('>') {
        return trimmed.to_string();
    }
    let bytes = trimmed.as_bytes();
    let mut depth = 0i32;
    let mut end = None;
    for (i, &b) in bytes.iter().enumerate().rev() {
        match b {
            b'>' => {
                if depth == 0 {
                    end = Some(i);
                }
                depth += 1;
            }
            b'<' => {
                depth -= 1;
                if depth == 0 && end.is_some() {
                    return trimmed[..i].trim_end().to_string();
                }
            }
            _ => {}
        }
    }
    trimmed.to_string()
}

fn find_params(decl: Node) -> Option<Node> {
    if decl.kind() == "function_declarator" {
        return decl.child_by_field_name("parameters");
    }
    for i in 0..decl.child_count() {
        if let Some(child) = decl.child(i) {
            if let Some(p) = find_params(child) {
                return Some(p);
            }
        }
    }
    None
}

fn node_span(program: &mut Program, ctx: &LowerContext, node: Node) -> Span {
    if let Some(line_map) = &ctx.line_map {
        if let Some(entry) = line_map.lookup(node.start_byte()) {
            let origin = line_map.path_of(entry);
            // Always report original-file coordinates. Code from an
            // `#include`d file is attributed to its original header;
            // TU-local code keeps the primary file but gets its original
            // (pre-expansion) line/col, so reported lines match what a
            // user sees in their editor (AGENTS.md LineMap invariant).
            let fid = if origin != ctx.primary_path {
                program.symbols.add_file_interned(origin.to_path_buf())
            } else {
                ctx.current_file
            };
            return Span::new(fid, entry.line, entry.col);
        }
    }
    let line = node.start_position().row as u32 + 1;
    let col = node.start_position().column as u32 + 1;
    Span::new(ctx.current_file, line, col)
}
