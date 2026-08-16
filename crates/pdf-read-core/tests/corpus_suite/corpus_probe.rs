use std::io::{Seek, SeekFrom, Write};

use agentshim_pdf_read::{MarkdownOptions, ParserLimits, PdfReadDocument, RenderLimits};

use super::corpus;

fn open(bytes: &[u8]) -> Result<PdfReadDocument, String> {
    let mut file = tempfile::tempfile().unwrap();
    file.write_all(bytes).unwrap();
    file.seek(SeekFrom::Start(0)).unwrap();
    PdfReadDocument::from_file(file, ParserLimits::default()).map_err(|error| format!("{error:?}"))
}

fn probe(name: &str, bytes: &[u8], render: bool) {
    println!("=== {name} ({} bytes) ===", bytes.len());
    let document = match open(bytes) {
        Ok(document) => document,
        Err(error) => {
            println!("  open error: {error}");
            return;
        }
    };
    let count = match document.page_count() {
        Ok(count) => count,
        Err(error) => {
            println!("  page_count error: {error:?}");
            return;
        }
    };
    println!("  pages: {count}");
    for page in 0..count.min(5) {
        match document.page_to_markdown(page, &MarkdownOptions::default()) {
            Ok(markdown) => println!("  [{page}] markdown {:?}", markdown),
            Err(error) => println!("  [{page}] markdown error: {error:?}"),
        }
        match document.assess_page_text(page) {
            Ok(text) => println!("  [{page}] text {text:?}"),
            Err(error) => println!("  [{page}] text error: {error:?}"),
        }
        match document.assess_page_visual(page) {
            Ok(visual) => println!("  [{page}] visual {visual:?}"),
            Err(error) => println!("  [{page}] visual error: {error:?}"),
        }
        match document.page_info(page) {
            Ok(info) => println!("  [{page}] info {info:?}"),
            Err(error) => println!("  [{page}] info error: {error:?}"),
        }
        if render {
            match document.render_page_fit(page, RenderLimits::default()) {
                Ok(rendered) => println!(
                    "  [{page}] render {}x{} png {} bytes",
                    rendered.width_pixels,
                    rendered.height_pixels,
                    rendered.png.len()
                ),
                Err(error) => println!("  [{page}] render error: {error:?}"),
            }
        }
    }
}

#[test]
#[ignore = "developer probe: prints current corpus behaviour for pinning expectations"]
fn probe_corpus() {
    probe("born_digital_text", &corpus::born_digital_text(), false);
    probe("table_document", &corpus::table_document(), false);
    probe("garbled_text_layer", &corpus::garbled_text_layer(), false);
    probe("hidden_text_layer", &corpus::hidden_text_layer(), false);
    probe("full_page_image", &corpus::full_page_image(), true);
    probe("mixed_document", &corpus::mixed_document(), false);
    probe("vector_graphics", &corpus::vector_graphics(), true);
    probe("blank_page", &corpus::blank_page(), true);
    probe(
        "broken_xref_unparsable",
        &corpus::broken_xref_unparsable(),
        false,
    );
    probe(
        "broken_xref_empty_table",
        &corpus::broken_xref_empty_table(),
        false,
    );
    probe(
        "oversized_image_dimensions",
        &corpus::oversized_image_dimensions(),
        true,
    );
    probe("oversized_media_box", &corpus::oversized_media_box(), true);
}

#[test]
#[ignore = "developer probe: inflates 48 MiB Flate negatives"]
fn probe_flate_bombs() {
    probe(
        "flate_bomb_content_stream",
        &corpus::flate_bomb_content_stream(),
        false,
    );
    probe("flate_bomb_image", &corpus::flate_bomb_image(), false);
}
