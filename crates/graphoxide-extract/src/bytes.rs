//! Portable byte-oriented helpers for the extractor hot path.
//!
//! This module is the only extractor-local boundary that selects byte-search,
//! UTF-8 validation, and integrity implementations.  The selected crates use
//! portable runtime dispatch where the host supports it and retain scalar
//! fallbacks.  Callers pass borrowed source bytes, so none of these helpers
//! need to materialize a second source-sized allocation.

use memchr::{memchr_iter, memmem};

/// Validate UTF-8 on borrowed source bytes.
///
/// `simdutf8::basic` is the fast valid-input path.  On malformed input we
/// deliberately re-run the standard-library validator so callers retain its
/// established `valid_up_to` and `error_len` diagnostics.  This makes the
/// accelerated and scalar paths externally indistinguishable.
pub(crate) fn validate_utf8(bytes: &[u8]) -> Result<&str, std::str::Utf8Error> {
    match simdutf8::basic::from_utf8(bytes) {
        Ok(text) => Ok(text),
        Err(_) => std::str::from_utf8(bytes),
    }
}

/// Find a byte substring using `memchr`'s portable SIMD implementations.
///
/// This intentionally preserves `memmem::find` semantics for an empty needle.
pub(crate) fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    memmem::find(haystack, needle)
}

/// Return whether a non-empty byte substring occurs in `haystack`.
pub(crate) fn contains_nonempty_subslice(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty() && find_subslice(haystack, needle).is_some()
}

/// Return the one-based line containing `offset` with the historic prefix
/// semantics: a newline byte itself belongs to its preceding line.
///
/// This is preferable to constructing [`LineIndex`] for a single lookup.
pub(crate) fn line_number(source: &[u8], offset: usize) -> usize {
    memchr_iter(b'\n', &source[..offset.min(source.len())]).count() + 1
}

/// Return an integrity CRC without allocating or copying `bytes`.
pub(crate) fn crc32(bytes: &[u8]) -> u32 {
    crc32fast::hash(bytes)
}

/// Return the BLAKE3 content digest without allocating or copying `bytes`.
///
/// BLAKE3 selects its portable accelerated implementation internally; callers
/// must not enable its Rayon helpers because extraction workers are already
/// fixed-owner CPU tasks.
pub(crate) fn blake3_digest(bytes: &[u8]) -> [u8; blake3::OUT_LEN] {
    *blake3::hash(bytes).as_bytes()
}

/// A compact index of line starts for repeated source-location lookups.
///
/// `memchr` uses the best available implementation for the current CPU while
/// retaining a portable fallback.  Most admitted source files are below 4 GiB,
/// so offsets use `u32` rather than pointer-width `usize`, reducing line-index
/// cache footprint by half on 64-bit hosts.  The wide representation keeps
/// behavior correct for larger borrowed inputs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LineIndex {
    Compact(Vec<u32>),
    Wide(Vec<usize>),
}

impl LineIndex {
    pub(crate) fn new(source: &[u8]) -> Self {
        // Count with the same SIMD-aware iterator before reserving.  This
        // avoids Vec growth/reallocation for newline-dense generated files.
        let line_count = memchr_iter(b'\n', source).count() + 1;
        if u32::try_from(source.len()).is_ok() {
            let mut starts = Vec::with_capacity(line_count);
            starts.push(0);
            starts.extend(memchr_iter(b'\n', source).map(|offset| {
                u32::try_from(offset + 1).expect("source length was checked as u32")
            }));
            Self::Compact(starts)
        } else {
            let mut starts = Vec::with_capacity(line_count);
            starts.push(0);
            starts.extend(memchr_iter(b'\n', source).map(|offset| offset + 1));
            Self::Wide(starts)
        }
    }

    /// Return the one-based line number containing `offset`.
    ///
    /// Offsets at a newline belong to the preceding line, matching the former
    /// `text[..offset].bytes().filter(...)` implementation.  Offsets past EOF
    /// are intentionally treated as EOF, which makes diagnostic callers safe.
    pub(crate) fn line_of(&self, offset: usize) -> usize {
        match self {
            Self::Compact(starts) => u32::try_from(offset).map_or_else(
                |_| starts.len(),
                |offset| starts.partition_point(|start| *start <= offset),
            ),
            Self::Wide(starts) => starts.partition_point(|start| *start <= offset),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        blake3_digest, contains_nonempty_subslice, crc32, find_subslice, line_number,
        validate_utf8, LineIndex,
    };

    fn scalar_line_of(source: &[u8], offset: usize) -> usize {
        source[..offset.min(source.len())]
            .iter()
            .filter(|&&byte| byte == b'\n')
            .count()
            + 1
    }

    fn scalar_find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        if needle.is_empty() {
            return Some(0);
        }
        haystack
            .windows(needle.len())
            .position(|window| window == needle)
    }

    #[test]
    fn line_index_matches_scalar_prefix_semantics() {
        for source in [
            b"".as_slice(),
            b"one".as_slice(),
            b"one\ntwo\nthree".as_slice(),
            b"\n\n\n".as_slice(),
            b"mixed\r\nline\nend".as_slice(),
        ] {
            let index = LineIndex::new(source);
            assert!(matches!(index, LineIndex::Compact(_)));
            for offset in 0..=source.len().saturating_add(2) {
                assert_eq!(index.line_of(offset), scalar_line_of(source, offset));
                assert_eq!(line_number(source, offset), scalar_line_of(source, offset));
            }
        }
        assert_eq!(LineIndex::new(b"one\ntwo\nthree").line_of(usize::MAX), 3);
    }

    #[test]
    fn portable_utf8_validation_matches_scalar_diagnostics() {
        let inputs = [
            b"plain ASCII".as_slice(),
            "emoji: 🦀".as_bytes(),
            &[0xff, b'a'],
            &[b'a', 0xe2, 0x82],
            &[b'a', 0xf0, 0x80, 0x80, 0x80],
        ];
        for bytes in inputs {
            let accelerated = validate_utf8(bytes)
                .map(str::to_owned)
                .map_err(|error| (error.valid_up_to(), error.error_len()));
            let scalar = std::str::from_utf8(bytes)
                .map(str::to_owned)
                .map_err(|error| (error.valid_up_to(), error.error_len()));
            assert_eq!(accelerated, scalar, "input: {bytes:?}");
        }
    }

    #[test]
    fn portable_search_matches_scalar_search() {
        let haystack = b"Graphoxide graphs graph-shaped source";
        for needle in [
            b"".as_slice(),
            b"Graph".as_slice(),
            b"graph".as_slice(),
            b"source".as_slice(),
            b"missing".as_slice(),
        ] {
            assert_eq!(
                find_subslice(haystack, needle),
                scalar_find(haystack, needle)
            );
            assert_eq!(
                contains_nonempty_subslice(haystack, needle),
                !needle.is_empty() && scalar_find(haystack, needle).is_some()
            );
        }
    }

    #[test]
    fn portable_integrity_outputs_are_stable() {
        assert_eq!(crc32(b"123456789"), 0xcbf4_3926);
        assert_eq!(
            blake3_digest(b""),
            [
                0xaf, 0x13, 0x49, 0xb9, 0xf5, 0xf9, 0xa1, 0xa6, 0xa0, 0x40, 0x4d, 0xea, 0x36, 0xdc,
                0xc9, 0x49, 0x9b, 0xcb, 0x25, 0xc9, 0xad, 0xc1, 0x12, 0xb7, 0xcc, 0x9a, 0x93, 0xca,
                0xe4, 0x1f, 0x32, 0x62,
            ]
        );
    }
}
