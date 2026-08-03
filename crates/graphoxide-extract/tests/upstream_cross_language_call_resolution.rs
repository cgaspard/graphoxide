//! One-to-one executable port of pinned upstream
//! `tests/test_cross_language_call_resolution.py`.

use graphoxide_core::{Confidence, Extraction};
use graphoxide_extract::extract_files;
use std::{collections::HashMap, fs, path::PathBuf};
use tempfile::TempDir;

struct Fixture {
    root: TempDir,
}

impl Fixture {
    fn new() -> Self {
        Self {
            root: TempDir::new().expect("cross-language fixture"),
        }
    }

    fn write(&self, relative: &str, source: &str) -> PathBuf {
        let path = self.root.path().join(relative);
        fs::create_dir_all(path.parent().expect("fixture parent")).expect("create fixture parent");
        fs::write(&path, source).expect("write fixture");
        path
    }

    fn extract(&self, files: &[PathBuf]) -> Extraction {
        let chunks = extract_files(files, Some(self.root.path()), true)
            .expect("extract cross-language fixture")
            .extractions;
        Extraction {
            nodes: chunks
                .iter()
                .flat_map(|chunk| chunk.nodes.iter().cloned())
                .collect(),
            edges: chunks
                .iter()
                .flat_map(|chunk| chunk.edges.iter().cloned())
                .collect(),
            hyperedges: chunks
                .iter()
                .flat_map(|chunk| chunk.hyperedges.iter().cloned())
                .collect(),
        }
    }
}

fn resolved_calls(result: &Extraction) -> Vec<(&str, &str, &str, Confidence)> {
    let labels: HashMap<_, _> = result
        .nodes
        .iter()
        .map(|node| (node.id.as_str(), node.label.as_str()))
        .collect();
    result
        .edges
        .iter()
        .filter(|edge| matches!(edge.relation.as_str(), "calls" | "indirect_call"))
        .filter(|edge| {
            !edge
                .extra
                .get("unresolved_call")
                .and_then(|value| value.as_bool())
                .unwrap_or(false)
        })
        .map(|edge| {
            (
                labels.get(edge.true_source()).copied().unwrap_or(""),
                labels.get(edge.true_target()).copied().unwrap_or(""),
                edge.relation.as_str(),
                edge.confidence,
            )
        })
        .collect()
}

#[test]
fn test_tsx_callback_does_not_bind_to_kotlin_method() {
    let fixture = Fixture::new();
    let tsx = fixture.write(
        "web/Upcoming.tsx",
        "declare function register(cb: () => void): void;\nexport function UpcomingPanel() {\n  register(refreshHeading);\n  return null;\n}\n",
    );
    let kotlin = fixture.write(
        "android/HeadingSensorBridge.kt",
        "class HeadingSensorBridge {\n    fun refreshHeading() {\n        println(\"native sensor\")\n    }\n}\n",
    );
    let result = fixture.extract(&[tsx, kotlin]);
    let calls = resolved_calls(&result);
    assert!(
        calls
            .iter()
            .all(|(_, target, _, _)| !target.contains("refreshHeading")),
        "cross-language callback bound to Kotlin: {calls:?}"
    );
}

#[test]
fn test_python_call_does_not_bind_to_kotlin_function() {
    let fixture = Fixture::new();
    let python = fixture.write(
        "py/worker.py",
        "def process():\n    return refreshHeading()\n",
    );
    let kotlin = fixture.write(
        "android/HeadingSensorBridge.kt",
        "class HeadingSensorBridge {\n    fun refreshHeading() {\n        println(\"native sensor\")\n    }\n}\n",
    );
    let result = fixture.extract(&[python, kotlin]);
    let calls = resolved_calls(&result);
    assert!(
        calls
            .iter()
            .all(|(_, target, _, _)| !target.contains("refreshHeading")),
        "Python call bound to Kotlin: {calls:?}"
    );
}

#[test]
fn test_same_language_callback_still_resolves() {
    let fixture = Fixture::new();
    let caller = fixture.write(
        "a.ts",
        "import { refreshHeading } from \"./b\";\ndeclare function register(cb: () => void): void;\nexport function run() { register(refreshHeading); }\n",
    );
    let target = fixture.write("b.ts", "export function refreshHeading(): void {}\n");
    let result = fixture.extract(&[caller, target]);
    let calls = resolved_calls(&result);
    assert!(
        calls.iter().any(|(_, target, relation, confidence)| {
            target.contains("refreshHeading")
                && *relation == "indirect_call"
                && *confidence == Confidence::Inferred
        }),
        "same-language callback did not resolve: {calls:?}"
    );
}

#[test]
fn test_jvm_interop_kotlin_call_to_java_still_resolves() {
    let fixture = Fixture::new();
    let java = fixture.write(
        "Alarm.java",
        "public class Alarm {\n    public static void ring() { System.out.println(\"ring\"); }\n}\n",
    );
    let kotlin = fixture.write("Scheduler.kt", "fun schedule() {\n    ring()\n}\n");
    let result = fixture.extract(&[java, kotlin]);
    let calls = resolved_calls(&result);
    assert!(
        calls
            .iter()
            .any(|(_, target, _, _)| target.contains("ring")),
        "Kotlin call did not resolve to Java: {calls:?}"
    );
}
