use std::{
    fs::File,
    io::Write,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use agentshim_office_read::{
    CancelSignal, OfficeFormat, OfficeLogicalCursor, OfficeReadDocument, OfficeReadError,
    OfficeReadLimits,
};

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(name)
}

fn read_all(name: &str, hint: OfficeFormat) -> (OfficeFormat, String) {
    let mut document = OfficeReadDocument::from_file(
        File::open(fixture(name)).expect("fixture"),
        hint,
        OfficeReadLimits::within(64 * 1024 * 1024),
        CancelSignal::never(),
    )
    .expect("parse fixture");
    let format = document.format();
    let mut markdown = String::new();
    let mut cursor: Option<OfficeLogicalCursor> = None;
    loop {
        let chunk = document
            .markdown_chunk(cursor.as_ref(), 23)
            .expect("read chunk");
        markdown.push_str(&chunk.markdown);
        cursor = chunk.next;
        if cursor.is_none() {
            break;
        }
    }
    (format, markdown)
}

#[test]
fn six_formats_match_the_tracked_characterization() {
    let cases = [
        ("sample.docx", OfficeFormat::Docx, "Office read fixture"),
        ("sample.xlsx", OfficeFormat::Xlsx, "Alpha"),
        ("sample.pptx", OfficeFormat::Pptx, "Slide body"),
        ("sample.doc", OfficeFormat::Doc, "Office read fixture"),
        ("sample.xls", OfficeFormat::Xls, "Alpha"),
        ("sample.ppt", OfficeFormat::Ppt, "Slide body"),
    ];
    for (name, format, expected) in cases {
        let (actual_format, markdown) = read_all(name, format);
        assert_eq!(actual_format, format, "{name}");
        assert!(markdown.contains(expected), "{name}: {markdown}");
    }
    let (_, pptx) = read_all("sample.pptx", OfficeFormat::Pptx);
    assert!(pptx.contains("Speaker notes"));
    let (_, ppt) = read_all("sample.ppt", OfficeFormat::Ppt);
    assert!(ppt.contains("Speaker notes"));
}

#[test]
fn internal_format_overrides_the_extension_hint() {
    let (format, markdown) = read_all("sample.docx", OfficeFormat::Doc);
    assert_eq!(format, OfficeFormat::Docx);
    assert!(markdown.contains("Office read fixture"));
}

#[test]
fn input_budget_is_enforced_before_parsing() {
    let error = OfficeReadDocument::from_file(
        File::open(fixture("sample.docx")).expect("fixture"),
        OfficeFormat::Docx,
        OfficeReadLimits::within(1_024),
        CancelSignal::never(),
    )
    .err()
    .expect("fixture exceeds tiny budget");
    assert!(matches!(error, OfficeReadError::ResourceLimit { .. }));
}

#[test]
fn cancellation_stops_before_package_expansion() {
    let cancelled = Arc::new(AtomicBool::new(true));
    let signal = Arc::clone(&cancelled);
    let error = OfficeReadDocument::from_file(
        File::open(fixture("sample.docx")).expect("fixture"),
        OfficeFormat::Docx,
        OfficeReadLimits::within(64 * 1024 * 1024),
        CancelSignal::new(move || signal.load(Ordering::Relaxed)),
    )
    .err()
    .expect("cancelled");
    assert!(matches!(error, OfficeReadError::Cancelled));
}

#[test]
fn oversized_compressed_main_part_hits_the_part_budget() {
    let directory = tempfile::tempdir().expect("fixture directory");
    let path = directory.path().join("large.docx");
    let file = File::create(&path).expect("fixture");
    let mut archive = zip::ZipWriter::new(file);
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    archive
        .start_file("[Content_Types].xml", options)
        .expect("content types");
    archive
        .write_all(br#"<Types><Override ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml" PartName="/word/document.xml"/></Types>"#)
        .expect("content types");
    archive
        .start_file("_rels/.rels", options)
        .expect("relationships");
    archive
        .write_all(br#"<Relationships><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/></Relationships>"#)
        .expect("relationships");
    archive
        .start_file("word/document.xml", options)
        .expect("document");
    archive
        .write_all(&vec![b'a'; 2 * 1024 * 1024])
        .expect("document");
    archive.finish().expect("finish fixture");

    let error = OfficeReadDocument::from_file(
        File::open(path).expect("fixture"),
        OfficeFormat::Docx,
        OfficeReadLimits::within(4 * 1024 * 1024),
        CancelSignal::never(),
    )
    .err()
    .expect("part limit");
    assert!(matches!(error, OfficeReadError::ResourceLimit { .. }));
}
