//! Version-controlled PDF corpus.
//!
//! Every fixture is synthesised in code so the bytes are reviewable in the diff and
//! reproducible in CI. `local/` and `repos/` are git-ignored and must never back a CI
//! assertion; this layer is the one that guards hard-limit defaults and negative cases.

use std::io::Write as _;

/// How the cross-reference section and `startxref` pointer are emitted.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum XrefStyle {
    Valid,
    /// `startxref` points at an offset that holds no cross-reference section.
    UnparsableStartxref,
    /// A syntactically well-formed section that declares zero entries.
    EmptyTable,
}

pub struct PdfBuilder {
    bodies: Vec<Option<Vec<u8>>>,
    info: Option<String>,
}

impl PdfBuilder {
    pub fn new() -> Self {
        Self {
            bodies: Vec::new(),
            info: None,
        }
    }

    pub fn reserve(&mut self) -> usize {
        self.bodies.push(None);
        self.bodies.len()
    }

    pub fn define(&mut self, id: usize, body: impl AsRef<[u8]>) {
        self.bodies[id - 1] = Some(body.as_ref().to_vec());
    }

    pub fn add(&mut self, body: impl AsRef<[u8]>) -> usize {
        let id = self.reserve();
        self.define(id, body);
        id
    }

    pub fn define_stream(&mut self, id: usize, dict_entries: &str, data: &[u8]) {
        let mut body =
            format!("<< {dict_entries} /Length {} >>\nstream\n", data.len()).into_bytes();
        body.extend_from_slice(data);
        body.extend_from_slice(b"\nendstream");
        self.define(id, body);
    }

    pub fn add_stream(&mut self, dict_entries: &str, data: &[u8]) -> usize {
        let id = self.reserve();
        self.define_stream(id, dict_entries, data);
        id
    }

    pub fn set_info(&mut self, entries: &str) {
        self.info = Some(entries.to_owned());
    }

    pub fn build(&self, root: usize) -> Vec<u8> {
        self.build_with(root, XrefStyle::Valid)
    }

    pub fn build_with(&self, root: usize, style: XrefStyle) -> Vec<u8> {
        let mut pdf = b"%PDF-1.7\n".to_vec();
        let mut offsets = vec![0_usize; self.bodies.len() + 1];
        let mut bodies = self.bodies.clone();
        let info_id = self.info.as_ref().map(|entries| {
            bodies.push(Some(format!("<< {entries} >>").into_bytes()));
            offsets.push(0);
            bodies.len()
        });

        for (index, body) in bodies.iter().enumerate() {
            let id = index + 1;
            let body = body
                .as_ref()
                .unwrap_or_else(|| panic!("corpus object {id} was reserved but never defined"));
            offsets[id] = pdf.len();
            pdf.extend_from_slice(format!("{id} 0 obj\n").as_bytes());
            pdf.extend_from_slice(body);
            pdf.extend_from_slice(b"\nendobj\n");
        }

        let size = bodies.len() + 1;
        let xref_offset = pdf.len();
        match style {
            XrefStyle::Valid | XrefStyle::UnparsableStartxref => {
                pdf.extend_from_slice(format!("xref\n0 {size}\n").as_bytes());
                pdf.extend_from_slice(b"0000000000 65535 f \n");
                for offset in offsets.iter().skip(1) {
                    pdf.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
                }
            }
            XrefStyle::EmptyTable => pdf.extend_from_slice(b"xref\n0 0\n"),
        }

        pdf.extend_from_slice(format!("trailer\n<< /Size {size} /Root {root} 0 R").as_bytes());
        if let Some(info_id) = info_id {
            pdf.extend_from_slice(format!(" /Info {info_id} 0 R").as_bytes());
        }
        pdf.extend_from_slice(b" >>\nstartxref\n");
        let pointer = match style {
            // Far past EOF: the reader finds no `xref` keyword and must fall back.
            XrefStyle::UnparsableStartxref => xref_offset + 4096,
            XrefStyle::Valid | XrefStyle::EmptyTable => xref_offset,
        };
        pdf.extend_from_slice(format!("{pointer}\n%%EOF\n").as_bytes());
        pdf
    }
}

const HELVETICA: &str =
    "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>";

struct PageSpec {
    /// Pages that show text get `/F1` bound to the shared Helvetica object.
    uses_font: bool,
    content: Vec<u8>,
    media_box: (u32, u32),
}

fn document(pages: Vec<PageSpec>, style: XrefStyle) -> Vec<u8> {
    let mut builder = PdfBuilder::new();
    let catalog = builder.reserve();
    let page_tree = builder.reserve();
    let kids: Vec<usize> = pages.iter().map(|_| builder.reserve()).collect();
    let font = builder.add(HELVETICA);

    builder.define(
        catalog,
        format!("<< /Type /Catalog /Pages {page_tree} 0 R >>"),
    );
    let kid_refs = kids
        .iter()
        .map(|id| format!("{id} 0 R"))
        .collect::<Vec<_>>()
        .join(" ");
    builder.define(
        page_tree,
        format!(
            "<< /Type /Pages /Kids [{kid_refs}] /Count {} >>",
            pages.len()
        ),
    );

    for (page, id) in pages.iter().zip(kids) {
        let content = builder.add_stream("", &page.content);
        let (width, height) = page.media_box;
        let resources = if page.uses_font {
            format!("/Font << /F1 {font} 0 R >>")
        } else {
            String::new()
        };
        builder.define(
            id,
            format!(
                "<< /Type /Page /Parent {page_tree} 0 R /MediaBox [0 0 {width} {height}] \
                 /Resources << {resources} >> /Contents {content} 0 R >>"
            ),
        );
    }

    builder.build_with(catalog, style)
}

fn text_page_content(lines: &[(&str, u32, u32, u32)]) -> Vec<u8> {
    let mut content = b"BT\n".to_vec();
    for (text, size, x, y) in lines {
        let escaped = text
            .replace('\\', r"\\")
            .replace('(', r"\(")
            .replace(')', r"\)");
        content.extend_from_slice(
            format!("/F1 {size} Tf\n1 0 0 1 {x} {y} Tm\n({escaped}) Tj\n").as_bytes(),
        );
    }
    content.extend_from_slice(b"ET");
    content
}

/// Twelve pages of clean authored text. Exercises the default first-batch page cap
/// (10) and the explicit-range cap (20) from the tool layer.
pub const BORN_DIGITAL_PAGE_COUNT: usize = 12;

pub fn born_digital_text() -> Vec<u8> {
    let pages = (1..=BORN_DIGITAL_PAGE_COUNT)
        .map(|number| {
            let heading = format!("Section {number}");
            let body = format!("Body paragraph for section {number}.");
            PageSpec {
                uses_font: true,
                content: text_page_content(&[
                    (heading.as_str(), 18, 20, 250),
                    (body.as_str(), 11, 20, 220),
                ]),
                media_box: (300, 300),
            }
        })
        .collect();
    document(pages, XrefStyle::Valid)
}

/// A right-aligned multi-column table: the layout and table-detection path most at
/// risk of ordering or floating-point nondeterminism.
pub fn table_document() -> Vec<u8> {
    let headers = ["Year", "Revenue", "Cost", "Net Income"];
    let rows = [
        ["2021", "365,817", "212,981", "94,680"],
        ["2022", "394,328", "223,546", "99,803"],
        ["2023", "383,285", "214,137", "96,995"],
        ["2024", "391,035", "210,352", "93,736"],
    ];
    let columns = [60_u32, 200, 320, 450];

    let mut content = b"BT\n".to_vec();
    content.extend_from_slice(b"/F1 14 Tf\n1 0 0 1 60 700 Tm\n(Synthetic fiscal summary) Tj\n");
    content.extend_from_slice(b"/F1 10 Tf\n");
    for (text, x) in headers.iter().zip(columns) {
        content.extend_from_slice(format!("1 0 0 1 {x} 650 Tm\n({text}) Tj\n").as_bytes());
    }
    for (index, row) in rows.iter().enumerate() {
        let y = 620 - (index as u32 * 24);
        for (text, x) in row.iter().zip(columns) {
            content.extend_from_slice(format!("1 0 0 1 {x} {y} Tm\n({text}) Tj\n").as_bytes());
        }
    }
    content.extend_from_slice(b"ET");

    document(
        vec![PageSpec {
            uses_font: true,
            content,
            media_box: (612, 792),
        }],
        XrefStyle::Valid,
    )
}

/// Every glyph is its own positioned show operator, so word clustering degrades and the
/// text-quality gate sees a fragmented layer.
pub fn garbled_text_layer() -> Vec<u8> {
    let sample = "Lorem ipsum dolor sit amet consectetur adipiscing elit sed do eiusmod";
    let mut content = b"BT\n/F1 11 Tf\n".to_vec();
    let mut x = 20_u32;
    let mut y = 260_u32;
    for character in sample.chars().filter(|c| !c.is_whitespace()) {
        content.extend_from_slice(format!("1 0 0 1 {x} {y} Tm\n({character}) Tj\n").as_bytes());
        x += 17;
        if x > 260 {
            x = 20;
            y -= 20;
        }
    }
    content.extend_from_slice(b"ET");

    document(
        vec![PageSpec {
            uses_font: true,
            content,
            media_box: (300, 300),
        }],
        XrefStyle::Valid,
    )
}

/// A full-page raster with an invisible (`Tr 3`) text layer over it: the OCR sidecar
/// shape. Text is present but was never meant to be seen.
pub fn hidden_text_layer() -> Vec<u8> {
    let mut builder = PdfBuilder::new();
    let catalog = builder.reserve();
    let page_tree = builder.reserve();
    let page = builder.reserve();
    let font = builder.add(HELVETICA);

    let pixels = vec![0xC0_u8; 200 * 200 * 3];
    let image = builder.add_stream(
        "/Type /XObject /Subtype /Image /Width 200 /Height 200 /ColorSpace /DeviceRGB \
         /BitsPerComponent 8",
        &pixels,
    );

    let mut content = b"q 300 0 0 300 0 0 cm /Im0 Do Q\n".to_vec();
    content.extend_from_slice(b"BT\n3 Tr\n/F1 11 Tf\n1 0 0 1 20 250 Tm\n");
    content.extend_from_slice(b"(Recognised text behind the scan) Tj\nET");
    let content = builder.add_stream("", &content);

    builder.define(
        catalog,
        format!("<< /Type /Catalog /Pages {page_tree} 0 R >>"),
    );
    builder.define(
        page_tree,
        format!("<< /Type /Pages /Kids [{page} 0 R] /Count 1 >>"),
    );
    builder.define(
        page,
        format!(
            "<< /Type /Page /Parent {page_tree} 0 R /MediaBox [0 0 300 300] /Resources \
             << /Font << /F1 {font} 0 R >> /XObject << /Im0 {image} 0 R >> >> \
             /Contents {content} 0 R >>"
        ),
    );
    builder.build(catalog)
}

/// One full-page raster and no text at all.
pub fn full_page_image() -> Vec<u8> {
    let mut builder = PdfBuilder::new();
    let catalog = builder.reserve();
    let page_tree = builder.reserve();
    let page = builder.reserve();

    let pixels = vec![0x80_u8; 400 * 200 * 3];
    let image = builder.add_stream(
        "/Type /XObject /Subtype /Image /Width 400 /Height 200 /ColorSpace /DeviceRGB \
         /BitsPerComponent 8",
        &pixels,
    );
    let content = builder.add_stream("", b"q 400 0 0 200 0 0 cm /Im0 Do Q");

    builder.define(
        catalog,
        format!("<< /Type /Catalog /Pages {page_tree} 0 R >>"),
    );
    builder.define(
        page_tree,
        format!("<< /Type /Pages /Kids [{page} 0 R] /Count 1 >>"),
    );
    builder.define(
        page,
        format!(
            "<< /Type /Page /Parent {page_tree} 0 R /MediaBox [0 0 400 200] /Resources \
             << /XObject << /Im0 {image} 0 R >> >> /Contents {content} 0 R >>"
        ),
    );
    builder.build(catalog)
}

/// Interleaves every routing-relevant page shape in one document so partial-success
/// behaviour has a single fixture to assert against.
pub const MIXED_TEXT_PAGE: usize = 0;
pub const MIXED_IMAGE_PAGE: usize = 1;
pub const MIXED_VECTOR_PAGE: usize = 2;
pub const MIXED_BLANK_PAGE: usize = 3;
pub const MIXED_TEXT_OVER_IMAGE_PAGE: usize = 4;
pub const MIXED_PAGE_COUNT: usize = 5;

pub fn mixed_document() -> Vec<u8> {
    let mut builder = PdfBuilder::new();
    let catalog = builder.reserve();
    let page_tree = builder.reserve();
    let pages: Vec<usize> = (0..MIXED_PAGE_COUNT).map(|_| builder.reserve()).collect();
    let font = builder.add(HELVETICA);

    let pixels = vec![0x60_u8; 100 * 100 * 3];
    let image = builder.add_stream(
        "/Type /XObject /Subtype /Image /Width 100 /Height 100 /ColorSpace /DeviceRGB \
         /BitsPerComponent 8",
        &pixels,
    );

    let contents = [
        text_page_content(&[
            ("Readable heading", 18, 20, 250),
            ("Readable body text.", 11, 20, 220),
        ]),
        b"q 300 0 0 300 0 0 cm /Im0 Do Q".to_vec(),
        b"0 0 1 rg\n40 40 220 220 re\nf\n0.8 0 0 RG\n4 w\n60 60 m 240 240 l\nS".to_vec(),
        b"q Q".to_vec(),
        {
            let mut content = b"q 300 0 0 150 0 0 cm /Im0 Do Q\n".to_vec();
            content.extend_from_slice(
                b"BT\n/F1 12 Tf\n1 0 0 1 20 250 Tm\n(Caption above the figure) Tj\nET",
            );
            content
        },
    ];

    builder.define(
        catalog,
        format!("<< /Type /Catalog /Pages {page_tree} 0 R >>"),
    );
    let kid_refs = pages
        .iter()
        .map(|id| format!("{id} 0 R"))
        .collect::<Vec<_>>()
        .join(" ");
    builder.define(
        page_tree,
        format!("<< /Type /Pages /Kids [{kid_refs}] /Count {MIXED_PAGE_COUNT} >>"),
    );
    for (index, (id, content)) in pages.iter().zip(contents).enumerate() {
        let content = builder.add_stream("", &content);
        let resources = if index == MIXED_VECTOR_PAGE || index == MIXED_BLANK_PAGE {
            String::new()
        } else {
            format!("/Font << /F1 {font} 0 R >> /XObject << /Im0 {image} 0 R >>")
        };
        builder.define(
            *id,
            format!(
                "<< /Type /Page /Parent {page_tree} 0 R /MediaBox [0 0 300 300] \
                 /Resources << {resources} >> /Contents {content} 0 R >>"
            ),
        );
    }
    builder.build(catalog)
}

/// Filled and stroked paths only: no text and no image XObject. Must not be reported as
/// a blank page.
pub fn vector_graphics() -> Vec<u8> {
    document(
        vec![PageSpec {
            uses_font: false,
            content: b"0 0 1 rg\n40 40 220 220 re\nf\n0.8 0 0 RG\n4 w\n60 60 m 240 240 l\nS"
                .to_vec(),
            media_box: (300, 300),
        }],
        XrefStyle::Valid,
    )
}

/// No text, no paint operations, no images.
pub fn blank_page() -> Vec<u8> {
    document(
        vec![PageSpec {
            uses_font: false,
            content: b"q Q".to_vec(),
            media_box: (300, 300),
        }],
        XrefStyle::Valid,
    )
}

/// Broken-xref trigger one: `startxref` points past the section, so parsing fails.
pub fn broken_xref_unparsable() -> Vec<u8> {
    document(
        vec![PageSpec {
            uses_font: true,
            content: text_page_content(&[("Recovered after xref damage", 14, 20, 250)]),
            media_box: (300, 300),
        }],
        XrefStyle::UnparsableStartxref,
    )
}

/// Broken-xref trigger two: the section parses cleanly but declares zero entries.
pub fn broken_xref_empty_table() -> Vec<u8> {
    document(
        vec![PageSpec {
            uses_font: true,
            content: text_page_content(&[("Recovered from an empty table", 14, 20, 250)]),
            media_box: (300, 300),
        }],
        XrefStyle::EmptyTable,
    )
}

/// Real producers Flate nearly every content stream, so the corpus needs one ordinary
/// compressed document alongside the raw ones.
pub fn flate_compressed_text() -> Vec<u8> {
    let mut builder = PdfBuilder::new();
    let catalog = builder.reserve();
    let page_tree = builder.reserve();
    let page = builder.reserve();
    let font = builder.add(HELVETICA);

    let plain = text_page_content(&[
        ("Compressed heading", 18, 20, 250),
        ("Compressed body text.", 11, 20, 220),
    ]);
    let content = builder.add_stream("/Filter /FlateDecode", &deflate_bytes(&plain));

    builder.define(
        catalog,
        format!("<< /Type /Catalog /Pages {page_tree} 0 R >>"),
    );
    builder.define(
        page_tree,
        format!("<< /Type /Pages /Kids [{page} 0 R] /Count 1 >>"),
    );
    builder.define(
        page,
        format!(
            "<< /Type /Page /Parent {page_tree} 0 R /MediaBox [0 0 300 300] /Resources \
             << /Font << /F1 {font} 0 R >> >> /Contents {content} 0 R >>"
        ),
    );
    builder.build(catalog)
}

fn deflate_bytes(data: &[u8]) -> Vec<u8> {
    let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(data).expect("deflate fixture");
    encoder.finish().expect("finish deflate fixture")
}

/// Nominal decompressed size of the Flate negatives. Above both per-stream caps (16 MiB
/// text, 32 MiB image) and below the 256 MiB no-budget backstop, so one fixture covers
/// both the refusal under a call budget and the permissive library default.
pub const FLATE_BOMB_DECODED_BYTES: usize = 48 * 1024 * 1024;

fn deflate_filler(total: usize, byte: u8) -> Vec<u8> {
    let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    let chunk = vec![byte; 64 * 1024];
    let mut written = 0;
    while written < total {
        let take = chunk.len().min(total - written);
        encoder.write_all(&chunk[..take]).expect("deflate fixture");
        written += take;
    }
    encoder.finish().expect("finish deflate fixture")
}

/// A page whose content stream inflates to [`FLATE_BOMB_DECODED_BYTES`].
pub fn flate_bomb_content_stream() -> Vec<u8> {
    let mut builder = PdfBuilder::new();
    let catalog = builder.reserve();
    let page_tree = builder.reserve();
    let page = builder.reserve();

    let font = builder.add(HELVETICA);
    let bomb = deflate_filler(FLATE_BOMB_DECODED_BYTES, b' ');
    let content = builder.add_stream("/Filter /FlateDecode", &bomb);

    builder.define(
        catalog,
        format!("<< /Type /Catalog /Pages {page_tree} 0 R >>"),
    );
    builder.define(
        page_tree,
        format!("<< /Type /Pages /Kids [{page} 0 R] /Count 1 >>"),
    );
    // The page advertises a font so the text extractor actually processes the stream. A
    // resourceless page short-circuits as "cannot contain text", which would make the
    // fixture prove nothing about the decode path.
    builder.define(
        page,
        format!(
            "<< /Type /Page /Parent {page_tree} 0 R /MediaBox [0 0 300 300] /Resources \
             << /Font << /F1 {font} 0 R >> >> /Contents {content} 0 R >>"
        ),
    );
    builder.build(catalog)
}

/// An image XObject whose Flate stream inflates to [`FLATE_BOMB_DECODED_BYTES`].
pub fn flate_bomb_image() -> Vec<u8> {
    let mut builder = PdfBuilder::new();
    let catalog = builder.reserve();
    let page_tree = builder.reserve();
    let page = builder.reserve();

    let side = 4096_usize;
    let bomb = deflate_filler(FLATE_BOMB_DECODED_BYTES, 0x40);
    let image = builder.add_stream(
        &format!(
            "/Type /XObject /Subtype /Image /Width {side} /Height {} /ColorSpace /DeviceRGB \
             /BitsPerComponent 8 /Filter /FlateDecode",
            FLATE_BOMB_DECODED_BYTES / (side * 3)
        ),
        &bomb,
    );
    let content = builder.add_stream("", b"q 300 0 0 300 0 0 cm /Im0 Do Q");

    builder.define(
        catalog,
        format!("<< /Type /Catalog /Pages {page_tree} 0 R >>"),
    );
    builder.define(
        page_tree,
        format!("<< /Type /Pages /Kids [{page} 0 R] /Count 1 >>"),
    );
    builder.define(
        page,
        format!(
            "<< /Type /Page /Parent {page_tree} 0 R /MediaBox [0 0 300 300] /Resources \
             << /XObject << /Im0 {image} 0 R >> >> /Contents {content} 0 R >>"
        ),
    );
    builder.build(catalog)
}

/// Declared pixel dimensions that would need roughly 30 GB of RGB surface, backed by a
/// few bytes of sample data.
pub fn oversized_image_dimensions() -> Vec<u8> {
    let mut builder = PdfBuilder::new();
    let catalog = builder.reserve();
    let page_tree = builder.reserve();
    let page = builder.reserve();

    let image = builder.add_stream(
        "/Type /XObject /Subtype /Image /Width 100000 /Height 100000 /ColorSpace /DeviceRGB \
         /BitsPerComponent 8",
        &[0x11, 0x22, 0x33],
    );
    let content = builder.add_stream("", b"q 300 0 0 300 0 0 cm /Im0 Do Q");

    builder.define(
        catalog,
        format!("<< /Type /Catalog /Pages {page_tree} 0 R >>"),
    );
    builder.define(
        page_tree,
        format!("<< /Type /Pages /Kids [{page} 0 R] /Count 1 >>"),
    );
    builder.define(
        page,
        format!(
            "<< /Type /Page /Parent {page_tree} 0 R /MediaBox [0 0 300 300] /Resources \
             << /XObject << /Im0 {image} 0 R >> >> /Contents {content} 0 R >>"
        ),
    );
    builder.build(catalog)
}

/// A page whose MediaBox is large enough that a naive DPI scale would blow the render
/// surface budget.
pub fn oversized_media_box() -> Vec<u8> {
    document(
        vec![PageSpec {
            uses_font: true,
            content: text_page_content(&[("Wide canvas", 14, 20, 250)]),
            media_box: (14400, 14400),
        }],
        XrefStyle::Valid,
    )
}

/// Text-showing operations on a page dense enough to pass the default span ceiling.
pub const DENSE_PAGE_FITTING_OPERATIONS: usize = 5_000;
/// Text-showing operations on a page dense enough to exceed it.
pub const DENSE_PAGE_REFUSED_OPERATIONS: usize = 40_000;

/// One page carrying `operations` positioned text runs on an ordinary grid.
///
/// Nothing here is malformed or compressed suspiciously — this is the shape a large
/// table, a map label layer, or an OCR text layer takes. It is the input class the
/// per-stream byte ceilings do not describe: every individual allocation is small, and
/// the page is expensive because there are so many of them.
fn dense_page_content(operations: usize) -> Vec<u8> {
    let mut content = Vec::new();
    for index in 0..operations {
        let column = index % 10;
        let row = index / 10;
        let x = 20 + column as u32 * 58;
        let y = 780 - (row % 130) as u32 * 6;
        content.extend_from_slice(
            format!("BT\n/F1 5 Tf\n1 0 0 1 {x} {y} Tm\n(cell {index}) Tj\nET\n").as_bytes(),
        );
    }
    content
}

pub fn dense_text_page(operations: usize) -> Vec<u8> {
    document(
        vec![PageSpec {
            uses_font: true,
            content: dense_page_content(operations),
            media_box: (612, 792),
        }],
        XrefStyle::Valid,
    )
}

/// Zero-based index of the page that exceeds the span ceiling in [`mixed_density`].
pub const MIXED_DENSITY_REFUSED_PAGE: usize = 1;
pub const MIXED_DENSITY_PAGE_COUNT: usize = 3;

/// An ordinary page, then one too dense to deliver, then another ordinary page.
///
/// The middle page is the whole point: refusing it must not cost the caller the two
/// either side of it.
pub fn mixed_density() -> Vec<u8> {
    document(
        vec![
            PageSpec {
                uses_font: true,
                content: text_page_content(&[("First page body text", 12, 20, 700)]),
                media_box: (612, 792),
            },
            PageSpec {
                uses_font: true,
                content: dense_page_content(DENSE_PAGE_REFUSED_OPERATIONS),
                media_box: (612, 792),
            },
            PageSpec {
                uses_font: true,
                content: text_page_content(&[("Third page body text", 12, 20, 700)]),
                media_box: (612, 792),
            },
        ],
        XrefStyle::Valid,
    )
}
