//! Bounded semantic-input slicing with lossless source-file identity.

use std::path::{Path, PathBuf};

pub const FILE_CHAR_CAP: usize = 20_000;
pub const PER_FILE_OVERHEAD_CHARS: usize = 160;
pub const CHARS_PER_TOKEN: usize = 4;
pub const IMAGE_TOKEN_ESTIMATE: usize = 1_600;
pub const MAX_IMAGES_PER_CHUNK: usize = 20;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileSlice {
    pub path: PathBuf,
    pub start: usize,
    pub end: usize,
    pub index: usize,
    pub total: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileUnit {
    Path(PathBuf),
    Slice(FileSlice),
}

pub fn slice_boundaries(text: &str, max_chars: usize) -> Vec<(usize, usize)> {
    if text.is_empty() {
        return vec![(0, 0)];
    }
    let max_chars = max_chars.max(1);
    let mut boundaries = Vec::new();
    let mut start = 0;
    while start < text.len() {
        let mut hard_end = (start + max_chars).min(text.len());
        while hard_end > start && !text.is_char_boundary(hard_end) {
            hard_end -= 1;
        }
        if hard_end == start {
            hard_end = text[start..]
                .char_indices()
                .nth(1)
                .map_or(text.len(), |(offset, _)| start + offset);
        }
        let end = if hard_end == text.len() {
            hard_end
        } else {
            let window = &text[start..hard_end];
            window
                .rfind("\n#")
                .map(|offset| start + offset + 1)
                .filter(|split| *split > start)
                .or_else(|| {
                    window
                        .rfind("\n\n")
                        .map(|offset| start + offset + 2)
                        .filter(|split| *split > start)
                })
                .or_else(|| {
                    window
                        .rfind('\n')
                        .map(|offset| start + offset + 1)
                        .filter(|split| *split > start)
                })
                .unwrap_or(hard_end)
        };
        boundaries.push((start, end));
        start = end;
    }
    boundaries
}

pub fn is_splittable_text(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            matches!(
                extension.to_ascii_lowercase().as_str(),
                "md" | "markdown" | "txt" | "rst" | "adoc" | "asciidoc"
            )
        })
}

pub fn expand_oversized_files(paths: &[PathBuf], max_chars: usize) -> Vec<FileUnit> {
    let mut units = Vec::new();
    for path in paths {
        if !is_splittable_text(path) {
            units.push(FileUnit::Path(path.clone()));
            continue;
        }
        let Ok(text) = std::fs::read_to_string(path) else {
            units.push(FileUnit::Path(path.clone()));
            continue;
        };
        if text.len() <= max_chars {
            units.push(FileUnit::Path(path.clone()));
            continue;
        }
        let boundaries = slice_boundaries(&text, max_chars);
        let total = boundaries.len();
        units.extend(
            boundaries
                .into_iter()
                .enumerate()
                .map(|(index, (start, end))| {
                    FileUnit::Slice(FileSlice {
                        path: path.clone(),
                        start,
                        end,
                        index,
                        total,
                    })
                }),
        );
    }
    units
}

pub fn read_slice_text(slice: &FileSlice) -> std::io::Result<String> {
    let text = std::fs::read_to_string(&slice.path)?;
    if slice.start > slice.end
        || slice.end > text.len()
        || !text.is_char_boundary(slice.start)
        || !text.is_char_boundary(slice.end)
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "file slice is outside UTF-8 text boundaries",
        ));
    }
    Ok(text[slice.start..slice.end].to_owned())
}

pub fn unit_path(unit: &FileUnit) -> &Path {
    match unit {
        FileUnit::Path(path) => path,
        FileUnit::Slice(slice) => &slice.path,
    }
}

pub fn estimate_file_tokens(unit: &FileUnit) -> usize {
    if matches!(unit, FileUnit::Path(path) if is_vision_image(path)) {
        return IMAGE_TOKEN_ESTIMATE;
    }
    let chars = match unit {
        FileUnit::Slice(slice) => slice.end.saturating_sub(slice.start).min(FILE_CHAR_CAP),
        FileUnit::Path(path) => std::fs::metadata(path)
            .map(|metadata| (metadata.len() as usize).min(FILE_CHAR_CAP))
            .unwrap_or(0),
    };
    if chars == 0 {
        return 0;
    }
    (chars + PER_FILE_OVERHEAD_CHARS) / CHARS_PER_TOKEN
}

/// Estimate a file with a caller-provided tokenizer.
///
/// Graphify uses this path when `tiktoken` is available and explicitly treats
/// special-token spellings as ordinary source text. Keeping the tokenizer
/// injectable gives native backends the same behavior without coupling the
/// core crate to a particular tokenization library.
pub fn estimate_file_tokens_with<F>(unit: &FileUnit, tokenizer: F) -> usize
where
    F: FnOnce(&str) -> usize,
{
    if matches!(unit, FileUnit::Path(path) if is_vision_image(path)) {
        return IMAGE_TOKEN_ESTIMATE;
    }
    let text = match unit {
        FileUnit::Slice(slice) => read_slice_text(slice).ok(),
        FileUnit::Path(path) => std::fs::read(path)
            .ok()
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned()),
    };
    let Some(text) = text else {
        return estimate_file_tokens(unit);
    };
    let capped = if text.chars().count() > FILE_CHAR_CAP {
        let end = text
            .char_indices()
            .nth(FILE_CHAR_CAP)
            .map_or(text.len(), |(index, _)| index);
        &text[..end]
    } else {
        &text
    };
    tokenizer(capped) + (PER_FILE_OVERHEAD_CHARS / CHARS_PER_TOKEN)
}

pub fn is_vision_image(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            matches!(
                extension.to_ascii_lowercase().as_str(),
                "png" | "jpg" | "jpeg" | "gif" | "webp"
            )
        })
}

pub fn partition_semantic_files(units: &[FileUnit]) -> (Vec<FileUnit>, Vec<PathBuf>) {
    let mut text = Vec::new();
    let mut images = Vec::new();
    for unit in units {
        let path = unit_path(unit);
        let image = path
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| {
                matches!(
                    extension.to_ascii_lowercase().as_str(),
                    "png" | "jpg" | "jpeg" | "gif" | "webp"
                )
            });
        if image && matches!(unit, FileUnit::Path(_)) {
            images.push(path.to_owned());
        } else {
            text.push(unit.clone());
        }
    }
    (text, images)
}

pub fn pack_chunks_by_tokens(units: &[FileUnit], token_budget: usize) -> Vec<Vec<FileUnit>> {
    try_pack_chunks_by_tokens(units, token_budget).unwrap_or_default()
}

pub fn try_pack_chunks_by_tokens(
    units: &[FileUnit],
    token_budget: usize,
) -> anyhow::Result<Vec<Vec<FileUnit>>> {
    anyhow::ensure!(
        token_budget > 0,
        "token_budget must be positive, got {token_budget}"
    );
    let mut by_directory = std::collections::BTreeMap::<PathBuf, Vec<FileUnit>>::new();
    for unit in units {
        by_directory
            .entry(
                unit_path(unit)
                    .parent()
                    .unwrap_or_else(|| Path::new(""))
                    .to_path_buf(),
            )
            .or_default()
            .push(unit.clone());
    }
    let mut chunks = Vec::new();
    let mut current = Vec::new();
    let mut tokens: usize = 0;
    let mut images = 0;
    for units in by_directory.into_values() {
        for unit in units {
            let unit_tokens = estimate_file_tokens(&unit);
            let image = matches!(&unit, FileUnit::Path(path) if is_vision_image(path));
            if !current.is_empty()
                && (tokens.saturating_add(unit_tokens) > token_budget
                    || (image && images >= MAX_IMAGES_PER_CHUNK))
            {
                chunks.push(std::mem::take(&mut current));
                tokens = 0;
                images = 0;
            }
            current.push(unit);
            tokens = tokens.saturating_add(unit_tokens);
            images += usize::from(image);
        }
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    Ok(chunks)
}

pub fn read_files_prompt(units: &[FileUnit], root: &Path) -> String {
    let mut blocks = Vec::new();
    for unit in units {
        let path = unit_path(unit);
        let relative = path.strip_prefix(root).unwrap_or(path);
        let text = match unit {
            FileUnit::Path(path) => std::fs::read_to_string(path).unwrap_or_default(),
            FileUnit::Slice(slice) => read_slice_text(slice).unwrap_or_default(),
        };
        blocks.push(format!(
            "<untrusted_source path=\"{}\">\n{}\n</untrusted_source>",
            relative.to_string_lossy().replace('\\', "/"),
            text
        ));
    }
    blocks.join("\n")
}

pub fn bisect_slice(slice: &FileSlice) -> Option<(FileSlice, FileSlice)> {
    if slice.end.saturating_sub(slice.start) <= 1 {
        return None;
    }
    let text = std::fs::read_to_string(&slice.path).ok()?;
    if slice.end > text.len() || slice.start >= slice.end {
        return None;
    }
    let midpoint = slice.start + (slice.end - slice.start) / 2;
    let before = text[slice.start..midpoint]
        .rfind('\n')
        .map(|offset| slice.start + offset + 1);
    let after = text[midpoint..slice.end]
        .find('\n')
        .map(|offset| midpoint + offset + 1);
    let split = before
        .filter(|split| *split > slice.start)
        .or(after)
        .unwrap_or(midpoint);
    if split <= slice.start || split >= slice.end {
        return None;
    }
    Some((
        FileSlice {
            path: slice.path.clone(),
            start: slice.start,
            end: split,
            index: 0,
            total: 2,
        },
        FileSlice {
            path: slice.path.clone(),
            start: split,
            end: slice.end,
            index: 1,
            total: 2,
        },
    ))
}
