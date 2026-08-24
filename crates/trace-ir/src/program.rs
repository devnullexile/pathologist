use crate::flow::ReturnFlow;
use crate::symbol::SymbolTable;
use crate::types::TypeTable;
use crate::{CallSiteId, FileId, FnId};
use indexmap::IndexMap;
use rustc_hash::FxHashMap;
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiagnosticSeverity {
    Error,
    Warning,
    Info,
}

/// How to spell a C++ member function across a class hierarchy.
///
/// Constructors and destructors change spelling per class (`Derived::Derived`,
/// `Derived::~Derived`) while ordinary methods keep their name, so expansion
/// over an override set needs this distinction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MethodKind {
    Named(String),
    Ctor,
    Dtor,
}

impl MethodKind {
    /// Whether this kind participates in dynamic dispatch by default.
    pub fn is_destructor(&self) -> bool {
        matches!(self, MethodKind::Dtor)
    }

    /// The member's full name as spelled on class `cls`.
    pub fn name_on(&self, cls: &str) -> String {
        let last = cls.rsplit("::").next().unwrap_or(cls);
        match self {
            MethodKind::Named(m) => format!("{}::{}", cls, m),
            MethodKind::Ctor => format!("{}::{}", cls, last),
            MethodKind::Dtor => format!("{}::~{}", cls, last),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Diagnostic {
    pub severity: DiagnosticSeverity,
    pub file: Option<crate::FileId>,
    pub line: u32,
    pub message: String,
    pub stage: String,
}

/// Cross-unit deduplication state used by the merge stage: entities whose
/// origin (header file + position) was already merged map to the first copy.
#[derive(Debug, Clone, Default)]
pub struct MergeDedup {
    pub fn_keys: FxHashMap<(FileId, String, u32), FnId>,
    pub site_keys: FxHashMap<(FileId, u32, u32, String), CallSiteId>,
}

#[derive(Debug, Clone, Default)]
pub struct Program {
    pub root: PathBuf,
    pub types: TypeTable,
    pub symbols: SymbolTable,
    pub flow: Vec<crate::FlowConstraint>,
    /// Per-function return-value summaries collected during lowering.
    pub fn_returns: IndexMap<FnId, Vec<ReturnFlow>>,
    pub diagnostics: Vec<Diagnostic>,
    pub include_paths: Vec<PathBuf>,
    /// `#include` dependency edges (dependent → included), project-local only.
    pub include_deps: Vec<(PathBuf, PathBuf)>,
    pub defines: IndexMap<String, String>,
    pub anon_type_counter: u32,
    pub dedup: MergeDedup,
    /// C++ class-inheritance facts: `(derived, base)` qualified names.
    /// Names are the fully qualified spellings used for functions/types
    /// (`ns::Cls`). Populated at lowering, consumed post-merge by virtual
    /// dispatch expansion.
    pub inheritance: Vec<(String, String)>,
}

impl Program {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            ..Default::default()
        }
    }

    /// Record a `(derived, base)` edge once.
    pub fn add_inheritance(&mut self, derived: &str, base: &str) {
        if derived.is_empty() || base.is_empty() {
            return;
        }
        let edge = (derived.to_string(), base.to_string());
        if !self.inheritance.contains(&edge) {
            self.inheritance.push(edge);
        }
    }

    /// Direct base classes of `cls`.
    pub fn bases_of(&self, cls: &str) -> Vec<String> {
        self.inheritance
            .iter()
            .filter(|(d, _)| d == cls)
            .map(|(_, b)| b.clone())
            .collect()
    }

    /// `root` plus every class transitively deriving from it (BFS).
    pub fn subclass_closure(&self, root: &str) -> Vec<String> {
        let mut out = vec![root.to_string()];
        let mut i = 0;
        while i < out.len() {
            let cur = out[i].clone();
            for (derived, base) in &self.inheritance {
                if base == &cur && !out.iter().any(|c| c == derived) {
                    out.push(derived.clone());
                }
            }
            i += 1;
        }
        out
    }

    /// Every declared member matching `kind` on `cls` or any of its
    /// subclasses — the virtual override set. Order: root first, then
    /// subclasses in discovery order.
    pub fn method_targets(&self, cls: &str, kind: &MethodKind) -> Vec<FnId> {
        let mut out = Vec::new();
        for c in self.subclass_closure(cls) {
            let full = kind.name_on(&c);
            for id in self.symbols.functions_named(&full) {
                if !out.contains(&id) {
                    out.push(id);
                }
            }
        }
        out
    }

    pub fn add_diagnostic(&mut self, diag: Diagnostic) {
        self.diagnostics.push(diag);
    }
}
