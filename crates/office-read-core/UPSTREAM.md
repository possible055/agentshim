# Upstream source record

- Repository: `https://github.com/yfedoseev/office_oxide`
- Release: `v0.1.8`
- Commit: `744b25be7f79ad333ffe68a11b2a39856846cdf3`
- Retrieved: 2026-08-26
- Selected license: Apache License 2.0
- Local package: `agentshim-office-read`

The production build uses only the source under this crate. `repos/office_oxide`
is a comparison checkout and is excluded from the Cargo workspace.

## Retained module map

| Local module | Upstream responsibility | Local scope |
|---|---|---|
| `core` | OPC, content types, relationships, XML, themes and units | Read-only OPC/XML with bounded ZIP reads |
| `cfb` | Compound Binary File reader | Read-only sector and stream traversal |
| `docx` | WordprocessingML parser and Markdown | Structure, text, hyperlinks and alt text metadata |
| `xlsx` | SpreadsheetML parser and Markdown | Sheets, cells, formats, chart text and drawing text shapes |
| `pptx` | PresentationML parser and Markdown | Slides, shapes, tables, notes and alt text metadata |
| `doc` | Word Binary parser | FIB, piece table and visible text |
| `xls` | BIFF parser | Workbook records, shared strings and bounded sparse rows |
| `ppt` | PowerPoint Binary parser | Persist directory, records, slides, notes and visible text |

## Removed responsibility groups

- CLI, MCP server, FFI, Python, WASM and other language bindings.
- Create, edit, write, save and conversion APIs.
- Document IR, HTML and JSON conversion surfaces.
- Raw image payload extraction and embedded font payload loading.
- Rayon dispatch, mmap paths and parser-owned thread creation.
- Examples, writer tests, binding tests and upstream benchmark artifacts.

## Patch ledger

| Local change | Upstream basis | Reason |
|---|---|---|
| Narrow `OfficeReadDocument` facade | v0.1.8 format readers | Accept capability-derived `File` only and expose Markdown chunks only |
| Bounded ZIP entry reader | `core/opc.rs` | Check declared and actual size, stream decompression, reject CRC errors |
| Unified budget and cancellation checks | OPC, XML and CFB allocation loops | Bound untrusted expansion and stop cooperatively |
| Raw media/font loading removed | DOCX/XLSX/PPTX and DOC/XLS/PPT readers | Markdown needs metadata and alt text, not payload bytes |
| XLS sparse row construction | `xls/workbook.rs` | Avoid maximum-coordinate rectangular allocation |
| Strict legacy empty-success paths | DOC/XLS/PPT readers | Missing required structures are invalid or unsupported, not empty success |
| Serial slide/sheet parsing | PPTX/XLSX readers | Keep work inside the caller-managed structured-document worker |
