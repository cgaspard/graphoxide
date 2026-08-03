//! Shared test-path and ambiguous-symbol tie breaking heuristics.

use std::{
    cmp::Reverse,
    collections::{HashMap, HashSet},
};

pub fn is_test_path(path: &str) -> bool {
    if path.is_empty() {
        return false;
    }
    let normalized = path.replace('\\', "/");
    let mut segments: Vec<_> = normalized
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect();
    if segments.iter().any(|segment| {
        matches!(
            segment.to_ascii_lowercase().as_str(),
            "tests" | "test" | "spec" | "specs" | "__tests__"
        )
    }) {
        return true;
    }
    let Some(filename) = segments.pop() else {
        return false;
    };
    let lower = filename.to_ascii_lowercase();
    lower.starts_with("test_")
        || lower.contains("_test.")
        || lower.contains(".test.")
        || lower.contains(".spec.")
        || lower.contains("_spec.")
        || lower.ends_with(".tests.ps1")
        || filename.ends_with("Test.java")
        || filename.ends_with("Tests.java")
        || filename.ends_with("Tests.cs")
}

fn normalized_parts(path: &str) -> Vec<&str> {
    path.split(['/', '\\'])
        .filter(|part| !part.is_empty())
        .collect()
}

fn proximity_winner<'a>(
    call_site: &str,
    candidates: &[&'a str],
    files: &HashMap<&str, &str>,
) -> Option<&'a str> {
    if call_site.is_empty() {
        return None;
    }
    let call_normalized = call_site.replace('\\', "/");
    let same_file: Vec<_> = candidates
        .iter()
        .copied()
        .filter(|candidate| {
            files.get(candidate).unwrap_or(&"").replace('\\', "/") == call_normalized
        })
        .collect();
    if same_file.len() == 1 {
        return same_file.first().copied();
    }
    if same_file.len() > 1 {
        return None;
    }
    let call_parts = normalized_parts(&call_normalized);
    let call_directory = &call_parts[..call_parts.len().saturating_sub(1)];
    let same_directory: Vec<_> = candidates
        .iter()
        .copied()
        .filter(|candidate| {
            let parts = normalized_parts(files.get(candidate).copied().unwrap_or(""));
            parts[..parts.len().saturating_sub(1)] == *call_directory
        })
        .collect();
    if same_directory.len() == 1 {
        return same_directory.first().copied();
    }
    if same_directory.len() > 1 {
        return None;
    }
    let mut scored: Vec<_> = candidates
        .iter()
        .copied()
        .map(|candidate| {
            let parts = normalized_parts(files.get(candidate).copied().unwrap_or(""));
            let directory = &parts[..parts.len().saturating_sub(1)];
            let score = call_directory
                .iter()
                .zip(directory)
                .take_while(|(left, right)| left == right)
                .count();
            (candidate, score)
        })
        .collect();
    scored.sort_by_key(|right| Reverse(right.1));
    let best = scored.first()?.1;
    (best > 0 && scored.iter().filter(|(_, score)| *score == best).count() == 1)
        .then(|| scored[0].0)
}

pub fn disambiguate_ambiguous_candidates<'a>(
    candidates: &[&'a str],
    candidate_files: &HashMap<&str, &str>,
    call_site_file: &str,
) -> Option<&'a str> {
    match candidates {
        [] => return None,
        [only] => return Some(*only),
        _ => {}
    }
    let test_candidates: Vec<_> = candidates
        .iter()
        .copied()
        .filter(|candidate| is_test_path(candidate_files.get(candidate).copied().unwrap_or("")))
        .collect();
    let test_set: HashSet<_> = test_candidates.iter().copied().collect();
    let non_test: Vec<_> = candidates
        .iter()
        .copied()
        .filter(|candidate| !test_set.contains(candidate))
        .collect();
    let survivors = if is_test_path(call_site_file) {
        let normalized = call_site_file.replace('\\', "/");
        let local: Vec<_> = test_candidates
            .iter()
            .copied()
            .filter(|candidate| {
                candidate_files
                    .get(candidate)
                    .copied()
                    .unwrap_or("")
                    .replace('\\', "/")
                    == normalized
            })
            .collect();
        if local.len() == 1 {
            return local.first().copied();
        }
        if test_candidates.is_empty() {
            if non_test.is_empty() {
                candidates.to_vec()
            } else {
                non_test
            }
        } else {
            test_candidates
        }
    } else {
        non_test
    };
    if survivors.len() == 1 {
        return survivors.first().copied();
    }
    if survivors.is_empty() {
        return None;
    }
    proximity_winner(call_site_file, &survivors, candidate_files)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_test_path_positive() {
        for path in [
            "tests/foo.py",
            "src/tests/foo.py",
            "test/foo.go",
            "spec/foo.rb",
            "specs/foo.rb",
            "app/__tests__/foo.js",
            "a/b/TESTS/foo.py",
            "src/test_service.py",
            "pkg/service_test.go",
            "src/service.test.ts",
            "src/service.spec.ts",
            "src/service_spec.rb",
            "ps/Module.Tests.ps1",
            "java/FooTest.java",
            "java/FooTests.java",
            "cs/FooTests.cs",
            "src\\tests\\foo.py",
            "src\\service_test.py",
        ] {
            assert!(is_test_path(path), "{path}");
        }
    }

    #[test]
    fn test_is_test_path_negative() {
        for path in [
            "",
            "latest.py",
            "contest.py",
            "src/contest.py",
            "src/greatest/x.py",
            "src/service.py",
            "lib/helper.go",
            "src/attestation.py",
            "src/testimony.py",
            "src/contest/x.py",
            "src/greatest.cs",
            "src/protest.java",
            "config/manifest.json",
        ] {
            assert!(!is_test_path(path), "{path}");
        }
    }

    #[test]
    fn test_disambiguate_drops_test_candidate_for_nontest_call_site() {
        let files = HashMap::from([("src", "src/service.py"), ("mock", "tests/test_service.py")]);
        assert_eq!(
            disambiguate_ambiguous_candidates(&["src", "mock"], &files, "src/caller.py"),
            Some("src")
        );
    }

    #[test]
    fn test_disambiguate_bails_on_two_nontest_candidates() {
        let files = HashMap::from([("a", "alpha/a.py"), ("b", "beta/b.py")]);
        assert_eq!(
            disambiguate_ambiguous_candidates(&["a", "b"], &files, "pkg/caller.py"),
            None
        );
    }

    #[test]
    fn test_disambiguate_test_call_site_prefers_test_local() {
        let files = HashMap::from([
            ("src", "src/service.py"),
            ("local", "tests/test_service.py"),
        ]);
        assert_eq!(
            disambiguate_ambiguous_candidates(&["src", "local"], &files, "tests/test_service.py"),
            Some("local")
        );
    }

    #[test]
    fn test_disambiguate_path_proximity_same_dir() {
        let files = HashMap::from([("near", "pkg/a/service.py"), ("far", "pkg/b/service.py")]);
        assert_eq!(
            disambiguate_ambiguous_candidates(&["near", "far"], &files, "pkg/a/caller.py"),
            Some("near")
        );
    }
}
