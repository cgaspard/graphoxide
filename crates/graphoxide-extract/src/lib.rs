//! File detection and per-file extraction.
//!
//! Port of upstream `detect.py`, `extract.py`, `extractors/*`, `cache.py`,
//! `manifest.py`. The pipeline stage contract is unchanged:
//!
//! ```text
//! collect_files(root) -> Vec<PathBuf>
//! extract(path)       -> Extraction { nodes, edges }
//! ```
//!
//! Extraction runs in-process on a rayon pool (upstream used a subprocess
//! pool to dodge the GIL — unnecessary here, and one of the main speed wins).

pub mod cache;
pub mod detect;
pub mod engine;
mod fallback;
pub mod languages;
pub mod resolution;

pub use detect::collect_files;
pub use engine::extract;

/// Collect and extract a project in parallel, storing repo-relative paths.
pub fn extract_project(root: &std::path::Path) -> anyhow::Result<Vec<graphoxide_core::Extraction>> {
    extract_project_with_options(root, false)
}

/// Extract a project, optionally bypassing the AST cache for a true full scan.
pub fn extract_project_with_options(
    root: &std::path::Path,
    force: bool,
) -> anyhow::Result<Vec<graphoxide_core::Extraction>> {
    use md5::Digest as _;
    use rayon::prelude::*;
    let files = collect_files(root)?;
    let rows: anyhow::Result<Vec<_>> = files
        .par_iter()
        .map(|path| {
            let relative = path
                .strip_prefix(root)
                .unwrap_or(path)
                .to_string_lossy()
                .replace('\\', "/");
            let bytes = std::fs::read(path)?;
            let extraction = if !force {
                cache::ast_cache_get(root, &relative, &bytes)
            } else {
                None
            };
            let extraction = if let Some(cached) = extraction {
                cached
            } else {
                let extracted = engine::extract_as(path, &relative)?;
                cache::ast_cache_put(root, &relative, &bytes, &extracted)?;
                extracted
            };
            let metadata = std::fs::metadata(path)?;
            let mtime = metadata
                .modified()?
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs_f64();
            let hash = format!("{:x}", md5::Md5::digest(&bytes));
            Ok((relative, extraction, mtime, hash))
        })
        .collect();
    let rows = rows?;
    let previous = cache::load_manifest(root);
    let manifest = rows
        .iter()
        .map(|(relative, _, mtime, hash)| {
            let semantic_hash = previous
                .get(relative)
                .filter(|entry| entry.ast_hash == *hash)
                .map(|entry| entry.semantic_hash.clone())
                .unwrap_or_default();
            (
                relative.clone(),
                cache::ManifestEntry {
                    mtime: *mtime,
                    ast_hash: hash.clone(),
                    semantic_hash,
                },
            )
        })
        .collect();
    cache::save_manifest(root, &manifest)?;
    let mut extractions: Vec<_> = rows
        .into_iter()
        .map(|(_, extraction, _, _)| extraction)
        .collect();
    resolution::resolve(&mut extractions);
    Ok(extractions)
}

#[cfg(test)]
mod tests {
    use graphoxide_core::make_id;
    use std::{
        fs,
        path::{Path, PathBuf},
        sync::atomic::{AtomicU64, Ordering},
    };

    static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(0);

    struct Fixture {
        root: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "graphoxide-injected-calls-{}-{}",
                std::process::id(),
                NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&root).expect("create extraction fixture");
            Self { root }
        }

        fn write(&self, name: &str, contents: &str) -> PathBuf {
            let path = self.root.join(name);
            fs::write(&path, contents).expect("write extraction fixture");
            path
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.root).expect("remove extraction fixture");
        }
    }

    fn extract(path: &Path, source_file: &str) -> graphoxide_core::Extraction {
        super::engine::extract_as(path, source_file).expect("extract fixture file")
    }

    fn definition_labels(extraction: &graphoxide_core::Extraction) -> Vec<&str> {
        let mut labels: Vec<_> = extraction
            .nodes
            .iter()
            .filter(|node| {
                matches!(
                    node.extra.get("type").and_then(|value| value.as_str()),
                    Some("class" | "function")
                )
            })
            .map(|node| node.label.as_str())
            .collect();
        labels.sort_unstable();
        labels
    }

    fn assert_definition(extraction: &graphoxide_core::Extraction, id: &str, kind: &str) {
        let node = extraction
            .nodes
            .iter()
            .find(|node| node.id == id)
            .unwrap_or_else(|| panic!("missing {kind} node {id}"));
        assert_eq!(
            node.extra.get("type").and_then(|value| value.as_str()),
            Some(kind),
            "node {id} should be a {kind}"
        );
    }

    fn assert_export_status(extraction: &graphoxide_core::Extraction, id: &str, exported: bool) {
        let node = extraction
            .nodes
            .iter()
            .find(|node| node.id == id)
            .unwrap_or_else(|| panic!("missing node {id}"));
        assert_eq!(
            node.extra
                .get("exported")
                .and_then(|value| value.as_bool())
                .unwrap_or(false),
            exported,
            "unexpected export status for node {id}"
        );
    }

    fn assert_single_edge(
        extraction: &graphoxide_core::Extraction,
        source: &str,
        target: &str,
        relation: &str,
    ) {
        let count = extraction
            .edges
            .iter()
            .filter(|edge| {
                edge.relation == relation
                    && edge.true_source() == source
                    && edge.true_target() == target
            })
            .count();
        assert_eq!(
            count, 1,
            "expected one {relation} edge from {source} to {target}"
        );
    }

    #[test]
    fn javascript_extracts_exported_and_variable_bound_declarations() {
        let fixture = Fixture::new();
        let javascript = fixture.write(
            "demo.js",
            r#"
function bareFn() {}
async function bareAsyncFn() {}
const bareArrow = () => {};
const bareAsyncArrow = async () => {};
const bareFnExpr = function () {};
class BareClass { bareMethod() {} }

export function expFn() {}
export async function expAsyncFn() {}
export const expArrow = () => {};
export class ExpClass { expMethod() {} }
export default function defFn() {}
"#,
        );
        let extraction = extract(&javascript, "demo.js");

        let definitions = definition_labels(&extraction);
        assert_eq!(definitions.len(), 13);
        for label in [
            "BareClass",
            "ExpClass",
            "bareArrow()",
            "bareAsyncArrow()",
            "bareAsyncFn()",
            "bareFn()",
            "bareFnExpr()",
            "defFn()",
            "expArrow()",
            "expAsyncFn()",
            "expFn()",
        ] {
            assert!(definitions.contains(&label), "missing definition {label}");
        }

        let file = make_id(&["demo"]);
        for (name, kind) in [
            ("bareFn", "function"),
            ("bareAsyncFn", "function"),
            ("bareArrow", "function"),
            ("bareAsyncArrow", "function"),
            ("bareFnExpr", "function"),
            ("BareClass", "class"),
            ("expFn", "function"),
            ("expAsyncFn", "function"),
            ("expArrow", "function"),
            ("ExpClass", "class"),
            ("defFn", "function"),
        ] {
            let id = make_id(&["demo", name]);
            assert_definition(&extraction, &id, kind);
            assert_single_edge(&extraction, &file, &id, "contains");
        }

        for id in [
            make_id(&["demo", "expFn"]),
            make_id(&["demo", "expAsyncFn"]),
            make_id(&["demo", "expArrow"]),
            make_id(&["demo", "ExpClass"]),
            make_id(&["demo", "defFn"]),
        ] {
            assert_export_status(&extraction, &id, true);
        }
        assert_export_status(&extraction, &make_id(&["demo", "bareFn"]), false);
        assert_export_status(&extraction, &make_id(&["demo", "bareArrow"]), false);

        for (class, method) in [("BareClass", "bareMethod"), ("ExpClass", "expMethod")] {
            let class = make_id(&["demo", class]);
            let method = make_id(&[&class, method]);
            assert_definition(&extraction, &method, "function");
            assert_single_edge(&extraction, &class, &method, "method");
        }
    }

    #[test]
    fn javascript_variable_binding_names_own_their_calls() {
        let fixture = Fixture::new();
        let javascript = fixture.write(
            "calls.js",
            r#"
function helper() {}
export const publicName = function internalName() { helper(); };
"#,
        );
        let extraction = extract(&javascript, "calls.js");

        assert_eq!(
            definition_labels(&extraction),
            vec!["helper()", "publicName()"]
        );
        let public_name = make_id(&["calls", "publicName"]);
        assert_export_status(&extraction, &public_name, true);
        assert!(extraction
            .nodes
            .iter()
            .all(|node| node.id != make_id(&["calls", "internalName"])));
        assert_single_edge(
            &extraction,
            &public_name,
            &make_id(&["calls", "helper"]),
            "calls",
        );
    }

    #[test]
    fn typescript_extracts_exported_variable_bound_functions() {
        let fixture = Fixture::new();
        let typescript = fixture.write(
            "demo.ts",
            r#"
function helper(): void {}
export const typedArrow = async (): Promise<void> => { helper(); };
export const typedFnExpr = function (): void { helper(); };
export class Service {}
"#,
        );
        let extraction = extract(&typescript, "demo.ts");

        assert_eq!(
            definition_labels(&extraction),
            vec!["Service", "helper()", "typedArrow()", "typedFnExpr()"]
        );
        for id in [
            make_id(&["demo", "typedArrow"]),
            make_id(&["demo", "typedFnExpr"]),
            make_id(&["demo", "Service"]),
        ] {
            assert_export_status(&extraction, &id, true);
        }
        assert_export_status(&extraction, &make_id(&["demo", "helper"]), false);

        let helper = make_id(&["demo", "helper"]);
        for caller in ["typedArrow", "typedFnExpr"] {
            assert_single_edge(&extraction, &make_id(&["demo", caller]), &helper, "calls");
        }
    }

    #[test]
    fn python_injected_fields_resolve_to_their_typed_methods() {
        let fixture = Fixture::new();
        let ports = fixture.write(
            "ports.py",
            r#"
class InventoryRepository:
    def reserve(self, items): ...
    def release(self, items): ...

class PaymentGateway:
    def charge(self, order_id): ...

class DemoPaymentGateway:
    def charge(self, order_id): ...

class OrderRepository:
    def save(self, order): ...

class InMemoryOrderRepository:
    def save(self, order): ...

class NotificationService:
    def send_confirmation(self, order): ...
"#,
        );
        let checkout_file = fixture.write(
            "checkout.py",
            r#"
from ports import InventoryRepository, NotificationService, OrderRepository, PaymentGateway

class CheckoutService:
    def __init__(
        self,
        inventory: InventoryRepository,
        payments: PaymentGateway,
        orders: OrderRepository,
        notifications: NotificationService,
    ):
        self.inventory = inventory
        self.payments = payments
        self.orders = orders
        self.notifications = notifications

    def checkout(self, order):
        self.inventory.reserve(order.items)
        self.payments.charge(order.order_id)
        self.inventory.release(order.items)
        self.orders.save(order)
        self.notifications.send_confirmation(order)
"#,
        );
        let mut extractions = vec![
            extract(&ports, "ports.py"),
            extract(&checkout_file, "checkout.py"),
        ];
        super::resolution::resolve(&mut extractions);

        let checkout = make_id(&["checkout", "CheckoutService", "checkout"]);
        let expected = [
            (
                make_id(&["ports", "InventoryRepository", "reserve"]),
                "InventoryRepository",
            ),
            (
                make_id(&["ports", "InventoryRepository", "release"]),
                "InventoryRepository",
            ),
            (
                make_id(&["ports", "PaymentGateway", "charge"]),
                "PaymentGateway",
            ),
            (
                make_id(&["ports", "OrderRepository", "save"]),
                "OrderRepository",
            ),
            (
                make_id(&["ports", "NotificationService", "send_confirmation"]),
                "NotificationService",
            ),
        ];

        for (target, receiver_type) in expected {
            let edge = extractions
                .iter()
                .flat_map(|extraction| &extraction.edges)
                .find(|edge| {
                    edge.relation == "calls"
                        && edge.true_source() == checkout
                        && edge.true_target() == target
                })
                .unwrap_or_else(|| panic!("missing injected call from {checkout} to {target}"));
            assert_eq!(
                edge.extra
                    .get("receiver_type")
                    .and_then(|value| value.as_str()),
                Some(receiver_type)
            );
        }
    }
}
