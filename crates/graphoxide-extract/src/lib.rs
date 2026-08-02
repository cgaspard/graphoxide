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
