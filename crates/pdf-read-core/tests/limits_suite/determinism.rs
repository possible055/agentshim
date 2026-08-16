use std::io::{Seek, SeekFrom, Write};
use std::process::Command;

use agentshim_pdf_read::{MarkdownOptions, ParserLimits, PdfReadDocument};

use super::corpus;

const REPEATS: usize = 64;
const DIGEST_MARKER: &str = "pdf-markdown-digest ";

fn fixtures() -> Vec<(&'static str, Vec<u8>)> {
    vec![
        ("born_digital_text", corpus::born_digital_text()),
        ("flate_compressed_text", corpus::flate_compressed_text()),
        ("table_document", corpus::table_document()),
        ("garbled_text_layer", corpus::garbled_text_layer()),
        ("hidden_text_layer", corpus::hidden_text_layer()),
        ("full_page_image", corpus::full_page_image()),
        ("mixed_document", corpus::mixed_document()),
        ("vector_graphics", corpus::vector_graphics()),
        ("blank_page", corpus::blank_page()),
        ("broken_xref_unparsable", corpus::broken_xref_unparsable()),
        ("broken_xref_empty_table", corpus::broken_xref_empty_table()),
        ("oversized_media_box", corpus::oversized_media_box()),
    ]
}

fn open(bytes: &[u8]) -> PdfReadDocument {
    let mut file = tempfile::tempfile().expect("temporary corpus file");
    file.write_all(bytes).expect("write corpus fixture");
    file.seek(SeekFrom::Start(0))
        .expect("rewind corpus fixture");
    PdfReadDocument::from_file(file, ParserLimits::default()).expect("open corpus fixture")
}

fn hex(value: &str) -> String {
    value.bytes().map(|byte| format!("{byte:02x}")).collect()
}

/// One `name page hex` line per extracted page, in a fixed order.
fn digest_lines() -> Vec<String> {
    let mut lines = Vec::new();
    for (name, bytes) in fixtures() {
        let document = open(&bytes);
        let pages = document.page_count().expect("page count");
        for page in 0..pages {
            let markdown = document
                .page_to_markdown(page, &MarkdownOptions::default())
                .expect("extract Markdown");
            lines.push(format!("{name} {page} {}", hex(&markdown)));
        }
    }
    lines
}

#[test]
fn markdown_is_byte_identical_across_repeated_calls() {
    for (name, bytes) in fixtures() {
        let document = open(&bytes);
        let pages = document.page_count().expect("page count");
        for page in 0..pages {
            let first = document
                .page_to_markdown(page, &MarkdownOptions::default())
                .expect("extract Markdown");
            for attempt in 1..REPEATS {
                let again = document
                    .page_to_markdown(page, &MarkdownOptions::default())
                    .expect("extract Markdown");
                assert_eq!(
                    again, first,
                    "{name} page {page} changed on repeat {attempt}"
                );
            }
        }
    }
}

/// Reopening the document defeats any per-document cache, so this covers the cold path
/// that a fresh `read` call actually takes.
#[test]
fn markdown_is_byte_identical_across_fresh_documents() {
    for (name, bytes) in fixtures() {
        let pages = open(&bytes).page_count().expect("page count");
        for page in 0..pages {
            let first = open(&bytes)
                .page_to_markdown(page, &MarkdownOptions::default())
                .expect("extract Markdown");
            for attempt in 1..REPEATS {
                let again = open(&bytes)
                    .page_to_markdown(page, &MarkdownOptions::default())
                    .expect("extract Markdown");
                assert_eq!(
                    again, first,
                    "{name} page {page} changed on reopen {attempt}"
                );
            }
        }
    }
}

/// Prints the digest of every corpus page. Doubles as the child process for
/// [`markdown_is_byte_identical_across_processes`].
#[test]
fn markdown_digest_child_fixture() {
    let lines = digest_lines();
    assert!(!lines.is_empty(), "corpus produced no pages");
    for line in lines {
        println!("{DIGEST_MARKER}{line}");
    }
}

/// Address-space layout, hash seeds, and allocator state differ between processes; a
/// digest that only matches within one process would not support cross-call resume.
#[test]
fn markdown_is_byte_identical_across_processes() {
    let executable = std::env::current_exe().expect("integration test executable");
    let output = Command::new(executable)
        .args([
            "--exact",
            "determinism::markdown_digest_child_fixture",
            "--nocapture",
        ])
        .output()
        .expect("spawn determinism child process");
    assert!(
        output.status.success(),
        "determinism child failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout).expect("child stdout is UTF-8");
    let child: Vec<&str> = stdout
        .lines()
        .filter_map(|line| line.strip_prefix(DIGEST_MARKER))
        .collect();
    let parent = digest_lines();

    assert_eq!(
        child.len(),
        parent.len(),
        "child reported {} digests, parent computed {}",
        child.len(),
        parent.len()
    );
    for (child_line, parent_line) in child.iter().zip(&parent) {
        assert_eq!(
            *child_line, parent_line,
            "Markdown differs between processes"
        );
    }
}
