use std::{fs::File, path::PathBuf};

use codexshim_pdf_read::{MarkdownOptions, ParserLimits, PdfReadDocument};

const CASES: [(&str, &str); 2] = [
    ("hello_structure.pdf", "# Hello World\n"),
    (
        "multi_column_table.pdf",
        "**Apple Inc. summary (synthetic, $ millions)**\n\n\
The table below summarises fiscal-year totals. Each cell is written with a separate text-show operator and right-aligned, which is the common layout pattern that triggers word concatenation and column alignment failures in PDF parsers.\n\n\
| Year | Revenue | Cost | Net Income |\n\
|---|---|---|---|\n\
| 2021 | 365,817 | 212,981 | 94,680 |\n\
| 2022 | 394,328 | 223,546 | 99,803 |\n\
| 2023 | 383,285 | 214,137 | 96,995 |\n\
| 2024 | 391,035 | 210,352 | 93,736 |\n\n\
*Note: this fixture is generated, not from any real filing.*\n",
    ),
];

#[test]
#[ignore = "requires PDF_OXIDE_CORPUS pointing to the pinned upstream tests/fixtures directory"]
fn matches_upstream_markdown_for_representative_corpus() {
    let root = std::env::var_os("PDF_OXIDE_CORPUS")
        .map(PathBuf::from)
        .expect("set PDF_OXIDE_CORPUS to the pinned upstream tests/fixtures directory");

    for (name, expected) in CASES {
        let file = File::open(root.join(name)).expect("open upstream corpus fixture");
        let document =
            PdfReadDocument::from_file(file, ParserLimits::default()).expect("open PDF fixture");
        let markdown = document
            .page_to_markdown(0, &MarkdownOptions::default())
            .expect("extract fixture Markdown");
        assert_eq!(markdown, expected, "upstream Markdown mismatch for {name}");
    }
}
