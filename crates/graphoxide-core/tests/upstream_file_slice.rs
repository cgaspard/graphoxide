use graphoxide_core::{
    bisect_slice, estimate_file_tokens, expand_oversized_files, is_splittable_text,
    pack_chunks_by_tokens, partition_semantic_files, read_files_prompt, read_slice_text,
    slice_boundaries, unit_path, FileSlice, FileUnit,
};
use std::path::PathBuf;

fn write(path: &std::path::Path, text: &str) -> PathBuf {
    std::fs::write(path, text).expect("write file-slice fixture");
    path.to_owned()
}

#[test]
fn test_slice_boundaries_small_text_is_one_range() {
    let text = "short doc";
    assert_eq!(slice_boundaries(text, 100), [(0, text.len())]);
}

#[test]
fn test_slice_boundaries_full_coverage_and_bounds() {
    let text = ("# Heading\n\n".to_owned() + &"lorem ipsum ".repeat(40) + "\n\n").repeat(20);
    for max_chars in [50, 100, 500, 1000] {
        let bounds = slice_boundaries(&text, max_chars);
        assert_eq!(bounds[0].0, 0);
        assert_eq!(bounds.last().unwrap().1, text.len());
        assert!(bounds.windows(2).all(|pair| pair[0].1 == pair[1].0));
        assert_eq!(
            bounds
                .iter()
                .map(|(start, end)| &text[*start..*end])
                .collect::<String>(),
            text
        );
        assert!(bounds.iter().all(|(start, end)| end - start <= max_chars));
    }
}

#[test]
fn test_slice_boundaries_single_huge_line_still_progresses() {
    let text = "x".repeat(5000);
    let bounds = slice_boundaries(&text, 1000);
    assert_eq!(
        bounds
            .iter()
            .map(|(start, end)| &text[*start..*end])
            .collect::<String>(),
        text
    );
    assert!(bounds.iter().all(|(start, end)| end - start <= 1000));
}

#[test]
fn test_slice_boundaries_prefers_heading_boundary() {
    let a = "# A\n".to_owned() + &"a".repeat(30) + "\n";
    let b = "# B\n".to_owned() + &"b".repeat(30) + "\n";
    let text = format!("{a}{b}");
    let bounds = slice_boundaries(&text, a.len() + 5);
    assert_eq!(&text[bounds[1].0..bounds[1].0 + 3], "# B");
}

#[test]
fn test_expand_small_file_stays_whole() {
    let directory = tempfile::tempdir().unwrap();
    let file = write(&directory.path().join("small.md"), "# Tiny\n\nhi\n");
    assert_eq!(
        expand_oversized_files(std::slice::from_ref(&file), 1000),
        [FileUnit::Path(file)]
    );
}

#[test]
fn test_expand_oversized_markdown_is_sliced_with_full_coverage() {
    let directory = tempfile::tempdir().unwrap();
    let text = ("# Section\n\n".to_owned() + &"word ".repeat(200) + "\n\n").repeat(30);
    let file = write(&directory.path().join("big.md"), &text);
    let units = expand_oversized_files(std::slice::from_ref(&file), 2000);
    let slices: Vec<_> = units
        .iter()
        .filter_map(|unit| match unit {
            FileUnit::Slice(slice) => Some(slice),
            FileUnit::Path(_) => None,
        })
        .collect();
    assert!(slices.len() >= 2);
    assert_eq!(slices.len(), units.len());
    assert_eq!(
        slices
            .iter()
            .map(|slice| read_slice_text(slice).unwrap())
            .collect::<String>(),
        text
    );
    assert!(slices.iter().all(|slice| slice.end - slice.start <= 2000));
    assert!(slices.iter().all(|slice| slice.path == file));
    assert_eq!(slices[0].total, slices.len());
}

#[test]
fn test_expand_does_not_slice_code_even_when_oversized() {
    let directory = tempfile::tempdir().unwrap();
    let file = write(&directory.path().join("mod.py"), &"x = 1\n".repeat(6000));
    assert!(!is_splittable_text(&file));
    assert_eq!(
        expand_oversized_files(std::slice::from_ref(&file), 2000),
        [FileUnit::Path(file)]
    );
}

#[test]
fn test_expand_unreadable_file_passes_through() {
    let directory = tempfile::tempdir().unwrap();
    let file = directory.path().join("nope.md");
    assert_eq!(
        expand_oversized_files(std::slice::from_ref(&file), 10),
        [FileUnit::Path(file)]
    );
}

#[test]
fn test_read_files_keys_every_slice_to_parent_path() {
    let directory = tempfile::tempdir().unwrap();
    let text = ("# H\n\n".to_owned() + &"lorem ".repeat(300) + "\n\n").repeat(20);
    let file = write(&directory.path().join("doc.md"), &text);
    let units = expand_oversized_files(std::slice::from_ref(&file), 2000);
    assert!(units.len() >= 2);
    let prompt = read_files_prompt(&units, directory.path());
    assert_eq!(
        prompt.matches("<untrusted_source path=\"doc.md\"").count(),
        units.len()
    );
}

#[test]
fn test_unit_path_resolves_slice_and_path() {
    let file = PathBuf::from("a.md");
    let slice = FileUnit::Slice(FileSlice {
        path: file.clone(),
        start: 0,
        end: 5,
        index: 0,
        total: 1,
    });
    assert_eq!(unit_path(&slice), file);
    assert_eq!(unit_path(&FileUnit::Path(file.clone())), file);
}

#[test]
fn test_estimate_tokens_for_slice_scales_with_range() {
    let file = PathBuf::from("a.md");
    let small = FileUnit::Slice(FileSlice {
        path: file.clone(),
        start: 0,
        end: 100,
        index: 0,
        total: 2,
    });
    let big = FileUnit::Slice(FileSlice {
        path: file,
        start: 0,
        end: 8000,
        index: 1,
        total: 2,
    });
    assert!(estimate_file_tokens(&small) < estimate_file_tokens(&big));
}

#[test]
fn test_partition_keeps_slices_as_text() {
    let slice = FileUnit::Slice(FileSlice {
        path: "a.md".into(),
        start: 0,
        end: 5,
        index: 0,
        total: 1,
    });
    let image = FileUnit::Path("pic.png".into());
    let (text, images) = partition_semantic_files(&[slice.clone(), image]);
    assert!(text.contains(&slice));
    assert_eq!(images, [PathBuf::from("pic.png")]);
}

#[test]
fn test_pack_chunks_handles_slices() {
    let directory = tempfile::tempdir().unwrap();
    let text = ("# H\n\n".to_owned() + &"word ".repeat(300) + "\n\n").repeat(20);
    let file = write(&directory.path().join("big.md"), &text);
    let units = expand_oversized_files(std::slice::from_ref(&file), 2000);
    let chunks = pack_chunks_by_tokens(&units, 2000);
    assert_eq!(chunks.iter().map(Vec::len).sum::<usize>(), units.len());
}

#[test]
fn test_bisect_slice_splits_at_newline() {
    let directory = tempfile::tempdir().unwrap();
    let file = write(&directory.path().join("a.md"), &"alpha\n".repeat(100));
    let original = FileSlice {
        path: file,
        start: 0,
        end: 600,
        index: 0,
        total: 1,
    };
    let (left, right) = bisect_slice(&original).expect("bisect file slice");
    assert_eq!(left.start, original.start);
    assert_eq!(right.end, original.end);
    assert_eq!(left.end, right.start);
    assert!(original.start < left.end && left.end < original.end);
}

#[test]
fn test_bisect_slice_returns_none_for_tiny() {
    let directory = tempfile::tempdir().unwrap();
    let file = write(&directory.path().join("a.md"), "ab");
    assert!(bisect_slice(&FileSlice {
        path: file,
        start: 0,
        end: 1,
        index: 0,
        total: 1,
    })
    .is_none());
}
