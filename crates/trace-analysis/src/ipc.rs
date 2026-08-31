//! IPC proxy/stub bridge detection.
//!
//! OpenHarmony services communicate over Binder IPC: a proxy method calls
//! `remote->SendRequest(...)` and a stub dispatches in `OnRemoteRequest`.
//! When both proxy and stub live under the analyzed root, we can connect them
//! with a synthetic call edge so the call graph has no gap at the IPC
//! boundary.
//!
//! Detection is purely name-based — no control-flow / opcode analysis:
//!
//! - A **stub** class is identified by its name (`*Stub*`).
//! - A **proxy** class is identified by its name (`*Proxy*`, `*Client*`) plus
//!   the presence of a `SendRequest`-family call in the method body.
//! - Bridges pair proxy methods to stub handlers by interface class name +
//!   method name correspondence (e.g. `FooProxy::Bar` → `FooStub::Bar`).

use rustc_hash::FxHashMap;
use trace_ir::{FnId, IpcBridge, Program};

/// `(qualified_class_name, handler FnIds)` for each detected stub class.
type StubClasses = Vec<(String, Vec<FnId>)>;
/// `(qualified_class_name, simple_method_name, method_fn)` for each
/// IPC-sending proxy method.
type ProxyMethods = Vec<(String, String, FnId)>;

/// Detect proxy/stub IPC bridges in a post-merge program.
///
/// Pure: reads `program` and returns the matched bridges. Runs after merge,
/// during PAG build. No `Program` mutation required.
pub fn detect_ipc_pairs(program: &Program) -> Vec<IpcBridge> {
    let (stubs, proxies) = scan(program);

    let mut stub_index: FxHashMap<String, &Vec<FnId>> = FxHashMap::default();
    for (class, handlers) in &stubs {
        stub_index.insert(class.clone(), handlers);
    }

    let mut bridges: Vec<IpcBridge> = Vec::new();
    for (proxy_class, method, proxy_method) in &proxies {
        let stub_class = derive_stub_class(proxy_class);
        let Some(handlers) = stub_index.get(&stub_class) else {
            continue;
        };
        let Some(handler) = find_handler(program, handlers, method) else {
            continue;
        };
        bridges.push(IpcBridge {
            proxy_method: *proxy_method,
            stub_handler: handler,
            descriptor: String::new(),
        });
    }
    bridges
}

/// Returns the stub classes and the IPC-sending proxy methods collected from
/// a post-merge program.
fn scan(program: &Program) -> (StubClasses, ProxyMethods) {
    // Index all defined C++ methods by their qualified class.
    // (qualified_class → (simple_method_name, FnId)).
    let mut methods_by_class: FxHashMap<String, Vec<(String, FnId)>> = FxHashMap::default();
    for f in &program.symbols.functions {
        if !f.is_defined || !f.is_cpp {
            continue;
        }
        let Some((class, method)) = split_qualified(&f.name) else {
            continue;
        };
        methods_by_class.entry(class).or_default().push((method, f.id));
    }

    let mut stubs: StubClasses = Vec::new();
    let mut proxies: ProxyMethods = Vec::new();
    let mut seen_stub = std::collections::HashSet::new();

    for (class, methods) in &methods_by_class {
        if is_stub_class(class) {
            // A stub class: handlers are its methods that are not the
            // dispatcher/descriptor boilerplate.
            let handlers: Vec<FnId> = methods
                .iter()
                .filter(|(m, _)| !is_stub_entry(m) && !is_boilerplate(m))
                .map(|(_, id)| *id)
                .collect();
            if !handlers.is_empty() && seen_stub.insert(class.clone()) {
                stubs.push((class.clone(), handlers));
            }
        } else if is_proxy_class(class) {
            // A proxy class: its methods that call SendRequest are IPC sends.
            for (method, id) in methods {
                if method_has_send_request(program, *id) {
                    proxies.push((class.clone(), method.clone(), *id));
                }
            }
        }
    }

    (stubs, proxies)
}

/// Derive the matching stub class name from a proxy/client class name.
/// `FooProxy` → `FooStub`, `FooClient` → `FooStub`.
fn derive_stub_class(proxy_class: &str) -> String {
    if let Some(base) = proxy_class.strip_suffix("Proxy") {
        return format!("{base}Stub");
    }
    if let Some(base) = proxy_class.strip_suffix("Client") {
        return format!("{base}Stub");
    }
    proxy_class.to_string()
}

/// Find a stub handler matching a proxy method name. Tries, in order:
/// exact name, a `Handle` prefix variant, then a `Stub` suffix variant
/// (the marshalling shim name used by some IDL generators).
fn find_handler(
    program: &Program,
    handlers: &[FnId],
    method_name: &str,
) -> Option<FnId> {
    let by_name = |name: &str| -> Option<FnId> {
        handlers
            .iter()
            .copied()
            .find(|&id| program.symbols.function(id).name.ends_with(&format!("::{name}")))
    };
    by_name(method_name)
        .or_else(|| by_name(&format!("Handle{method_name}")))
        .or_else(|| by_name(&format!("{method_name}Stub")))
}

fn is_stub_class(class: &str) -> bool {
    class.ends_with("Stub")
}

fn is_stub_entry(method: &str) -> bool {
    method == "OnRemoteRequest"
}

fn is_boilerplate(method: &str) -> bool {
    method == "GetDescriptor" || method.starts_with('~')
}

fn is_proxy_class(class: &str) -> bool {
    class.ends_with("Proxy") || class.ends_with("Client")
}

fn method_has_send_request(program: &Program, fn_id: FnId) -> bool {
    program
        .symbols
        .call_sites
        .iter()
        .any(|cs| cs.caller == fn_id && cs.callee_name.contains("SendRequest"))
}

/// Split a qualified C++ function name into `(class, method)`.
/// Returns `None` for plain/non-member functions and destructors.
fn split_qualified(name: &str) -> Option<(String, String)> {
    let mut parts: Vec<&str> = name.split("::").collect();
    if parts.len() < 2 {
        return None;
    }
    let method = parts.pop().unwrap().to_string();
    if method.starts_with('~') {
        return None;
    }
    let class = parts.join("::");
    Some((class, method))
}
