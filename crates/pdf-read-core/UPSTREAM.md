# Upstream source

This crate is derived from `pdf_oxide`:

- Repository: <https://github.com/yfedoseev/pdf_oxide>
- Upstream release: `v0.3.77`
- Upstream commit: `10b87f153200cd5c4d4a4defee471757091e6559`
- Source extraction date: 2026-08-09
- Selected license: Apache License 2.0

The crate is maintained as a selective source derivative, not as a dependency
on the complete upstream package. `repos/pdf_oxide` is used only as the local,
git-ignored reference checkout while preparing and comparing patches.

## Scope

Retained capabilities are PDF parsing and xref recovery, stream decoding,
read-side encryption, text and Markdown extraction, Tagged PDF reading order,
per-page text and visual assessment, and the `tiny-skia` renderer with required
image, colour, and font decoding.

Writer, editor, redaction, compliance, Office conversion, OCR and ML,
signatures, bindings, CLI and server targets, batch and parallel APIs,
benchmarks, `source_bytes`, and all mutation or serialization APIs are removed.
The heuristic page classifier is also removed: it inferred document origin
(`Scanned` / `BornDigital`) and materialised image streams to do so, which put
pixel decoding on the cheap text path and gave vector pages, raster scans, and
garbled text layers the same value.

`rendering` is a default feature, so `tiny-skia`, `fontdb`, `harfrust`,
`hayro-*`, and `fast_image_resize` are always compiled in. "The text path does
not start the renderer" is therefore a runtime property, enforced by counters,
not a claim about binary size.

## Local module map

The retained upstream responsibilities are mapped to local modules as follows:

- `document.rs` remains the aggregate facade and state owner. Its
  `document/*` modules group opening/encryption, object loading, page trees,
  structure and ActualText ordering, span normalization, column/block ordering,
  text/word/line/vector/region extraction, fonts, images, tables, and embedded
  files without changing the public `PdfDocument` surface.
- `content/parser/*`, `object/*`, `xref/*`, and `xref_reconstruction/*`
  retain parsing, object decoding, cross-reference loading, and bounded recovery.
- `extractors/text/*`, `pipeline/converters/markdown/*`, and
  `structure/spatial_table_detector/*` retain text execution, Markdown
  conversion, and spatial table detection.
- `rendering/page_renderer/*`, `rendering/resolution/*`, and
  `rendering/separation_renderer/*` retain page operator execution, colour and
  ICC resolution, image/form/shading rendering, overprint, soft masks, and
  separation output.
- `fonts/font_dict/*` and the other `fonts/*` modules retain font parsing,
  mapping, and fallback. The five oversized Adobe glyph/CID mapping files are
  immutable data exceptions; their lookup semantics remain static.

The local split is structural: upstream methods keep their names and can be
matched to these responsibility modules when importing a future upstream fix.

## Edition decision

`pdf-read-core` remains on Rust edition 2021. Moving this selective upstream
derivative to edition 2024 is not required by the responsibility split and must
be evaluated and verified as a separate Cargo edition migration.

## Patch ledger

| Date | Upstream basis | Change |
|---|---|---|
| 2026-08-09 | `10b87f153200cd5c4d4a4defee471757091e6559` | Established provenance and characterization baseline. |
| 2026-08-09 | `10b87f153200cd5c4d4a4defee471757091e6559` | Added the file-backed reader and narrow `PdfReadDocument` facade; removed `source_bytes` and the path-based public entry points. |
| 2026-08-09 | `10b87f153200cd5c4d4a4defee471757091e6559` | Removed writer, editor, redaction, compliance, Office, OCR/ML, signature, binding, server, CLI, batch, parallel, and benchmark source closures and dependencies. |
| 2026-08-09 | `10b87f153200cd5c4d4a4defee471757091e6559` | Split classification, Markdown, and rendering adapters from `document.rs`; removed image-to-disk and embedded-image conversion APIs. |
| 2026-08-09 | `10b87f153200cd5c4d4a4defee471757091e6559` | Replaced the renderer's self-referential font-face cache with scoped parsing, eliminating the retained crate's unsafe code. |
| 2026-08-10 | `10b87f153200cd5c4d4a4defee471757091e6559` | Added `metrics`: per-call observation counters for decoded streams, object cache, content operators, render pixels, PNG bytes, and font-database loads. |
| 2026-08-10 | `10b87f153200cd5c4d4a4defee471757091e6559` | Added `budget`: a per-call resource budget checked before each expandable allocation, plus a cancellation signal carried through long loops. |
| 2026-08-10 | `10b87f153200cd5c4d4a4defee471757091e6559` | Removed the `PDF_OXIDE_MAX_DECOMPRESS_MB` environment override; the Flate ceiling now comes from the call budget. Bounded the Brotli reader, which previously used an unbounded `read_to_end`. |
| 2026-08-10 | `10b87f153200cd5c4d4a4defee471757091e6559` | Rewrote `reconstruct_xref()` to scan in overlapping bounded windows and to parse trailers from bounded windows, removing the last path that buffered a second complete copy of the source. |
| 2026-08-10 | `10b87f153200cd5c4d4a4defee471757091e6559` | Stopped degrading resource-limit and cancellation errors to "empty page" in the content-stream and span paths; a budget refusal now ends the call instead of being reported as a blank page. |
| 2026-08-10 | `10b87f153200cd5c4d4a4defee471757091e6559` | Lowered the default `object_cache_bytes` from 64 MiB to 16 MiB and added budget-driven eviction. |
| 2026-08-10 | `10b87f153200cd5c4d4a4defee471757091e6559` | Added `page_to_markdown_chunk` and `release_page_scratch` for bounded single-page continuation and forward-only page walks. |
| 2026-08-10 | `10b87f153200cd5c4d4a4defee471757091e6559` | Added `assess_page_text` / `assess_page_visual`, which read operators and resource dictionaries only, and removed `classify_page` and `PageClass` along with the `extract_images()` call they made on the text path. |
| 2026-08-10 | `10b87f153200cd5c4d4a4defee471757091e6559` | Added pre-allocation validation of declared image geometry (pixel count, edge length, bit depth) in `extract_image_from_xobject`. |
| 2026-08-10 | `10b87f153200cd5c4d4a4defee471757091e6559` | Bounded the text extractor's span accumulation with a per-page ceiling derived from the call reservation, checked on a stride inside the content-stream loop. The layout stages downstream each hold a copy of the span vector, so the page's real cost is a multiple of a count no single allocation reveals. |
| 2026-08-10 | `10b87f153200cd5c4d4a4defee471757091e6559` | Added a cancellation checkpoint to the same stride. The content-stream loop was the one long loop with no checkpoint in it, so a cancelled or timed-out call could not stop until the whole stream had executed. |
| 2026-08-10 | `10b87f153200cd5c4d4a4defee471757091e6559` | Bounded the per-stream operator vector and the fast pre-scan's region index, both of which were proportional to attacker-controlled input and built whole before use. The pre-scan declines rather than truncates, leaving the caller on the bounded full parser. |
| 2026-08-10 | `10b87f153200cd5c4d4a4defee471757091e6559` | Made `call_total_bytes` an enforced ceiling rather than a declared one: live bytes are now tracked per category and every check tests the running total, which the stream ceiling also consults. |
| 2026-08-10 | `10b87f153200cd5c4d4a4defee471757091e6559` | Removed the unused `StreamReservation` and the `live_stream_bytes` budget it was supposed to feed. With nothing incrementing it the ceiling could never fire; the decoded-XObject reuse cache replaced it as the stream allocation that genuinely accumulates. |
| 2026-08-10 | `10b87f153200cd5c4d4a4defee471757091e6559` | Brought the decoded-XObject reuse cache under the call budget. Its 50 MiB ceiling was hardcoded at four sites and consulted no budget, so a call could hold that much beyond everything the budget accounted for. |
| 2026-08-10 | `10b87f153200cd5c4d4a4defee471757091e6559` | Derived every ceiling from the call reservation via `text_within` / `image_within`, so a configured reservation configures enforcement instead of only bookkeeping. |
| 2026-08-10 | `10b87f153200cd5c4d4a4defee471757091e6559` | Added `LimitScope` to `Error::ResourceLimit`, distinguishing a spent call budget from a page too dense to deliver so one such page no longer discards the pages around it. |
| 2026-08-10 | `10b87f153200cd5c4d4a4defee471757091e6559` | Re-enabled `dead_code` and `clippy::all` on the files added by this work; the crate-wide allow exists for retained upstream source and was silently covering them too. |
| 2026-08-10 | `10b87f153200cd5c4d4a4defee471757091e6559` | Removed the always-`None` `image_area_hint` field from `PageVisualAssessment`. |
| 2026-08-10 | `10b87f153200cd5c4d4a4defee471757091e6559` | Rejoined words split across lines by a typesetter's hyphen. Upstream suppressed the space at a soft-hyphen line break but kept the hyphen, so `implementa-` + `tion` reached callers as `implementa-tion`; on academic PDFs this affected several words per page. Added `SpaceSource::SoftHyphen` so the merge site can identify the case, and drop the hyphen only between two lowercase letters and never after a compound prefix — capitalised compounds, number ranges, headings, and terms such as `pre-training` and `self-attention` keep theirs. The prefix list is taken from the dead `text::hyphenation` module, whose own rule defaults the other way (both-lowercase means compound) and was never wired into any extraction path. A lowercase compound outside that list breaking at its own hyphen (`state-` + `of-the-art`) is still rejoined wrongly; that is undecidable without a lexicon and costs a hyphen rather than a word. Verified against the full 2757-test suite plus a new boundary test. |
| 2026-08-11 | `10b87f153200cd5c4d4a4defee471757091e6559` | Reorganized retained parser, object, xref, document, text, table, font, colour, image, and renderer code into responsibility modules. Public facades, output bytes, resource limits, and algorithms are unchanged; direct PDF unit tests and the architecture limit protect the split. |

Future upstream fixes must be recorded here with their source commit, affected
modules, local adaptation, and verification results. Do not replace the source
tree wholesale.
