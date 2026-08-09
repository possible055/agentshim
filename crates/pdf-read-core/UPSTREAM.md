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
page classification, and the `tiny-skia` renderer with required image, colour,
and font decoding.

Writer, editor, redaction, compliance, Office conversion, OCR and ML,
signatures, bindings, CLI and server targets, batch and parallel APIs,
benchmarks, `source_bytes`, and all mutation or serialization APIs are removed.

## Patch ledger

| Date | Upstream basis | Change |
|---|---|---|
| 2026-08-09 | `10b87f153200cd5c4d4a4defee471757091e6559` | Established provenance and characterization baseline. |
| 2026-08-09 | `10b87f153200cd5c4d4a4defee471757091e6559` | Added the file-backed reader and narrow `PdfReadDocument` facade; removed `source_bytes` and the path-based public entry points. |
| 2026-08-09 | `10b87f153200cd5c4d4a4defee471757091e6559` | Removed writer, editor, redaction, compliance, Office, OCR/ML, signature, binding, server, CLI, batch, parallel, and benchmark source closures and dependencies. |
| 2026-08-09 | `10b87f153200cd5c4d4a4defee471757091e6559` | Split classification, Markdown, and rendering adapters from `document.rs`; removed image-to-disk and embedded-image conversion APIs. |
| 2026-08-09 | `10b87f153200cd5c4d4a4defee471757091e6559` | Replaced the renderer's self-referential font-face cache with scoped parsing, eliminating the retained crate's unsafe code. |

Future upstream fixes must be recorded here with their source commit, affected
modules, local adaptation, and verification results. Do not replace the source
tree wholesale.
