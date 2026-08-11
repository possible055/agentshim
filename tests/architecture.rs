use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

const MAX_HANDWRITTEN_RUST_LINES: usize = 800;

struct Exception {
    path: &'static str,
    reason: &'static str,
}

const OVERSIZED_EXCEPTIONS: &[Exception] = &[
    Exception {
        path: "crates/pdf-read-core/src/annotation_types.rs",
        reason: "Phase 5 annotation type split pending",
    },
    Exception {
        path: "crates/pdf-read-core/src/annotations.rs",
        reason: "Phase 5 annotation processing split pending",
    },
    Exception {
        path: "crates/pdf-read-core/src/color/mod.rs",
        reason: "Phase 5 color responsibility split pending",
    },
    Exception {
        path: "crates/pdf-read-core/src/content/graphics_state.rs",
        reason: "Phase 5 graphics state split pending",
    },
    Exception {
        path: "crates/pdf-read-core/src/content/operators.rs",
        reason: "Phase 5 content operator split pending",
    },
    Exception {
        path: "crates/pdf-read-core/src/content/parser.rs",
        reason: "Phase 5 content parser split pending",
    },
    Exception {
        path: "crates/pdf-read-core/src/decoders/ccitt.rs",
        reason: "Phase 5 CCITT decoder test split pending",
    },
    Exception {
        path: "crates/pdf-read-core/src/document.rs",
        reason: "Phase 5 document aggregate split pending",
    },
    Exception {
        path: "crates/pdf-read-core/src/elements/path.rs",
        reason: "Phase 5 path element split pending",
    },
    Exception {
        path: "crates/pdf-read-core/src/extractors/forms.rs",
        reason: "Phase 5 form extractor split pending",
    },
    Exception {
        path: "crates/pdf-read-core/src/extractors/gap_statistics.rs",
        reason: "Phase 5 gap statistics split pending",
    },
    Exception {
        path: "crates/pdf-read-core/src/extractors/images.rs",
        reason: "Phase 5 image extractor split pending",
    },
    Exception {
        path: "crates/pdf-read-core/src/extractors/paths.rs",
        reason: "Phase 5 path extractor split pending",
    },
    Exception {
        path: "crates/pdf-read-core/src/extractors/text.rs",
        reason: "Phase 5 text extractor split pending",
    },
    Exception {
        path: "crates/pdf-read-core/src/fonts/adobe_glyph_list.rs",
        reason: "immutable Adobe glyph mapping data with pinned upstream provenance",
    },
    Exception {
        path: "crates/pdf-read-core/src/fonts/cff_encoding.rs",
        reason: "Phase 5 CFF data and lookup split pending",
    },
    Exception {
        path: "crates/pdf-read-core/src/fonts/character_mapper.rs",
        reason: "Phase 5 character mapper split pending",
    },
    Exception {
        path: "crates/pdf-read-core/src/fonts/cid_mappings/adobe_cns1.rs",
        reason: "immutable Adobe CNS1 mapping data with pinned upstream provenance",
    },
    Exception {
        path: "crates/pdf-read-core/src/fonts/cid_mappings/adobe_gb1.rs",
        reason: "immutable Adobe GB1 mapping data with pinned upstream provenance",
    },
    Exception {
        path: "crates/pdf-read-core/src/fonts/cid_mappings/adobe_japan1.rs",
        reason: "immutable Adobe Japan1 mapping data with pinned upstream provenance",
    },
    Exception {
        path: "crates/pdf-read-core/src/fonts/cid_mappings/adobe_korea1.rs",
        reason: "immutable Adobe Korea1 mapping data with pinned upstream provenance",
    },
    Exception {
        path: "crates/pdf-read-core/src/fonts/cmap.rs",
        reason: "Phase 5 CMap split pending",
    },
    Exception {
        path: "crates/pdf-read-core/src/fonts/font_dict.rs",
        reason: "Phase 5 font dictionary aggregate split pending",
    },
    Exception {
        path: "crates/pdf-read-core/src/fonts/truetype_cmap.rs",
        reason: "Phase 5 TrueType CMap split pending",
    },
    Exception {
        path: "crates/pdf-read-core/src/functions/mod.rs",
        reason: "Phase 5 PDF function split pending",
    },
    Exception {
        path: "crates/pdf-read-core/src/layout/text_block.rs",
        reason: "Phase 5 text block split pending",
    },
    Exception {
        path: "crates/pdf-read-core/src/lexer.rs",
        reason: "Phase 5 lexer test split pending",
    },
    Exception {
        path: "crates/pdf-read-core/src/object.rs",
        reason: "Phase 5 object type split pending",
    },
    Exception {
        path: "crates/pdf-read-core/src/parser.rs",
        reason: "Phase 5 object parser split pending",
    },
    Exception {
        path: "crates/pdf-read-core/src/pipeline/config.rs",
        reason: "Phase 5 pipeline configuration split pending",
    },
    Exception {
        path: "crates/pdf-read-core/src/pipeline/converters/markdown.rs",
        reason: "Phase 5 markdown converter split pending",
    },
    Exception {
        path: "crates/pdf-read-core/src/pipeline/reading_order/xycut.rs",
        reason: "Phase 5 XY-cut split pending",
    },
    Exception {
        path: "crates/pdf-read-core/src/rendering/mesh_shading.rs",
        reason: "Phase 5 mesh shading split pending",
    },
    Exception {
        path: "crates/pdf-read-core/src/rendering/page_renderer.rs",
        reason: "Phase 5 page renderer aggregate split pending",
    },
    Exception {
        path: "crates/pdf-read-core/src/rendering/resolution/color.rs",
        reason: "Phase 5 color resolution split pending",
    },
    Exception {
        path: "crates/pdf-read-core/src/rendering/resolution/separation_backend.rs",
        reason: "Phase 5 separation backend split pending",
    },
    Exception {
        path: "crates/pdf-read-core/src/rendering/separation_renderer.rs",
        reason: "Phase 5 separation renderer split pending",
    },
    Exception {
        path: "crates/pdf-read-core/src/rendering/sidecar.rs",
        reason: "Phase 5 rendering sidecar split pending",
    },
    Exception {
        path: "crates/pdf-read-core/src/rendering/text_rasterizer.rs",
        reason: "Phase 5 text rasterizer split pending",
    },
    Exception {
        path: "crates/pdf-read-core/src/structure/parser.rs",
        reason: "Phase 5 structure parser split pending",
    },
    Exception {
        path: "crates/pdf-read-core/src/structure/spatial_table_detector.rs",
        reason: "Phase 5 spatial table detector split pending",
    },
    Exception {
        path: "crates/pdf-read-core/src/structure/table_extractor.rs",
        reason: "Phase 5 table extractor split pending",
    },
    Exception {
        path: "crates/pdf-read-core/src/structure/traversal.rs",
        reason: "Phase 5 structure traversal split pending",
    },
    Exception {
        path: "crates/pdf-read-core/src/text/bidi.rs",
        reason: "Phase 5 bidirectional text split pending",
    },
    Exception {
        path: "crates/pdf-read-core/src/text/word_boundary.rs",
        reason: "Phase 5 word boundary split pending",
    },
    Exception {
        path: "crates/pdf-read-core/src/xref.rs",
        reason: "Phase 5 xref split pending",
    },
    Exception {
        path: "crates/pdf-read-core/src/xref_reconstruction.rs",
        reason: "Phase 5 xref reconstruction split pending",
    },
];
const TEXTUAL_INCLUDE_EXCEPTIONS: &[Exception] = &[];

#[test]
fn handwritten_rust_files_respect_the_architecture_constraints() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let files = rust_files(&root);
    let oversized = exception_map(OVERSIZED_EXCEPTIONS);
    let textual_includes = exception_map(TEXTUAL_INCLUDE_EXCEPTIONS);
    let mut unexpected_oversized = Vec::new();
    let mut stale_oversized = oversized.clone();
    let mut unexpected_includes = Vec::new();
    let mut stale_includes = textual_includes.clone();

    for file in files {
        let relative = relative_path(&root, &file);
        let source = fs::read_to_string(&file)
            .unwrap_or_else(|error| panic!("failed to read {relative}: {error}"));
        let line_count = source.lines().count();

        if line_count > MAX_HANDWRITTEN_RUST_LINES {
            if oversized.contains_key(relative.as_str()) {
                stale_oversized.remove(relative.as_str());
            } else {
                unexpected_oversized.push(format!("{relative}: {line_count} lines"));
            }
        }

        if contains_textual_include(&source) {
            if textual_includes.contains_key(relative.as_str()) {
                stale_includes.remove(relative.as_str());
            } else {
                unexpected_includes.push(relative);
            }
        }
    }

    assert_architecture_result(
        "oversized handwritten Rust files",
        &unexpected_oversized,
        &stale_oversized,
    );
    assert_architecture_result(
        "handwritten textual include! macros",
        &unexpected_includes,
        &stale_includes,
    );
}

fn rust_files(root: &Path) -> Vec<PathBuf> {
    let mut pending = vec![root.to_path_buf()];
    let mut files = Vec::new();

    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(&directory)
            .unwrap_or_else(|error| panic!("failed to read {}: {error}", directory.display()))
        {
            let entry = entry.unwrap_or_else(|error| {
                panic!(
                    "failed to read an entry in {}: {error}",
                    directory.display()
                )
            });
            let file_type = entry.file_type().unwrap_or_else(|error| {
                panic!("failed to inspect {}: {error}", entry.path().display())
            });
            let path = entry.path();

            if file_type.is_dir() {
                if !ignored_directory(&path) {
                    pending.push(path);
                }
            } else if file_type.is_file() && path.extension().is_some_and(|ext| ext == "rs") {
                files.push(path);
            }
        }
    }

    files.sort();
    files
}

fn ignored_directory(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| matches!(name, ".git" | "local" | "repos" | "target"))
}

fn relative_path(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

fn exception_map(exceptions: &[Exception]) -> BTreeMap<&str, &str> {
    exceptions
        .iter()
        .map(|exception| {
            assert!(
                !exception.reason.trim().is_empty(),
                "{} needs a reason",
                exception.path
            );
            (exception.path, exception.reason)
        })
        .collect()
}

fn assert_architecture_result(
    description: &str,
    unexpected: &[String],
    stale: &BTreeMap<&str, &str>,
) {
    assert!(
        unexpected.is_empty() && stale.is_empty(),
        "{description}\nunexpected:\n{}\nstale exceptions:\n{}",
        unexpected.join("\n"),
        stale.keys().copied().collect::<Vec<_>>().join("\n")
    );
}

fn contains_textual_include(source: &str) -> bool {
    let bytes = source.as_bytes();
    let mut index = 0;

    while index < bytes.len() {
        if bytes[index..].starts_with(b"//") {
            index = skip_line_comment(bytes, index + 2);
        } else if bytes[index..].starts_with(b"/*") {
            index = skip_block_comment(bytes, index + 2);
        } else if let Some((hashes, quote)) = raw_string_start(bytes, index) {
            index = skip_raw_string(bytes, quote + 1, hashes);
        } else if bytes[index] == b'"' {
            index = skip_quoted(bytes, index + 1, b'"');
        } else if bytes[index] == b'\'' && is_char_literal(bytes, index) {
            index = skip_quoted(bytes, index + 1, b'\'');
        } else if is_identifier_start(bytes[index]) {
            let start = index;
            index += 1;
            while index < bytes.len() && is_identifier_continue(bytes[index]) {
                index += 1;
            }
            if &bytes[start..index] == b"include" {
                while index < bytes.len() && bytes[index].is_ascii_whitespace() {
                    index += 1;
                }
                if bytes.get(index) == Some(&b'!') {
                    return true;
                }
            }
        } else {
            index += 1;
        }
    }

    false
}

fn skip_line_comment(bytes: &[u8], mut index: usize) -> usize {
    while index < bytes.len() && bytes[index] != b'\n' {
        index += 1;
    }
    index
}

fn skip_block_comment(bytes: &[u8], mut index: usize) -> usize {
    let mut depth = 1usize;
    while index < bytes.len() && depth > 0 {
        if bytes[index..].starts_with(b"/*") {
            depth += 1;
            index += 2;
        } else if bytes[index..].starts_with(b"*/") {
            depth -= 1;
            index += 2;
        } else {
            index += 1;
        }
    }
    index
}

fn raw_string_start(bytes: &[u8], index: usize) -> Option<(usize, usize)> {
    if bytes.get(index) != Some(&b'r') {
        return None;
    }
    let mut cursor = index + 1;
    while bytes.get(cursor) == Some(&b'#') {
        cursor += 1;
    }
    (bytes.get(cursor) == Some(&b'"')).then_some((cursor - index - 1, cursor))
}

fn skip_raw_string(bytes: &[u8], mut index: usize, hashes: usize) -> usize {
    while index < bytes.len() {
        if bytes[index] == b'"'
            && bytes
                .get(index + 1..index + 1 + hashes)
                .is_some_and(|suffix| suffix.iter().all(|byte| *byte == b'#'))
        {
            return index + 1 + hashes;
        }
        index += 1;
    }
    index
}

fn skip_quoted(bytes: &[u8], mut index: usize, quote: u8) -> usize {
    while index < bytes.len() {
        if bytes[index] == b'\\' {
            index = (index + 2).min(bytes.len());
        } else if bytes[index] == quote {
            return index + 1;
        } else {
            index += 1;
        }
    }
    index
}

fn is_char_literal(bytes: &[u8], index: usize) -> bool {
    let Some(&first) = bytes.get(index + 1) else {
        return false;
    };
    if first == b'\\' {
        bytes[index + 2..].iter().take(8).any(|byte| *byte == b'\'')
    } else {
        bytes.get(index + 2) == Some(&b'\'')
    }
}

fn is_identifier_start(byte: u8) -> bool {
    byte == b'_' || byte.is_ascii_alphabetic()
}

fn is_identifier_continue(byte: u8) -> bool {
    is_identifier_start(byte) || byte.is_ascii_digit()
}
