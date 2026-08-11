# Upstream source

This crate is a selective derivative of Gigatoken:

- Repository: <https://github.com/marcelroed/gigatoken>
- Upstream version: `0.10.0`
- Upstream commit: `fac0114b37120ec8a76362e9ee8e1c742aaafaef`
- Source extraction date: 2026-08-11
- License: MIT
- Reference checkout: `repos/gigatoken` (git-ignored; never used by production builds)

The embedded `o200k_base.tiktoken` data comes from
<https://openaipublic.blob.core.windows.net/encodings/o200k_base.tiktoken>.
Its size is 3,613,922 bytes and its SHA-256 is
`446a9538cb6c348e3516120d7c08b09f57c36495e2acfffe59a5bf8b0cfb1a2d`.
The independent development oracle is Python `tiktoken==0.12.0`; it is not a
Cargo dependency or a production runtime component.

## Scope

Retained semantics are the `.tiktoken` dense-rank loader, Gigatoken's
rank-to-pair reconstruction, byte-level BPE merge ordering, and the scalar
`o200k_base` ordinary-text pretokenizer. The public surface only loads the
fixed model, forks a bounded counter, counts ordinary tokens up to a limit,
and reports resource metrics.

Python and NumPy bindings, Arrow, Parquet, file and compressed sources,
network loading, SentencePiece, normalization, training, CLI, Rayon worker
pools, decoding, added-token handling, full token-ID results, and all other
pretokenizers are omitted. Stable Unicode general-category tables replace the
upstream ICU-built runtime tables. Stable scalar code replaces the upstream
nightly/SIMD mask scanner; production does not use nightly features.

## Local module map

The retained upstream responsibilities are mapped to local modules as follows:

- `lib.rs` is the narrow public facade for the fixed o200k prototype, bounded
  counters, cancellation-aware counting, and resource metrics.
- `model.rs` embeds and verifies the pinned dense ranks, reconstructs the
  rank-to-pair merge graph, and builds the byte-rank table.
- `pretokenizer.rs` implements the stable scalar ordinary-text boundaries and
  Unicode classification that replace upstream ICU/nightly dispatch.
- `counter.rs` owns count-only byte-pair merging, early exit, cancellation
  checkpoints, bounded scratch, and the fixed/long pretoken caches.

The split follows the retained upstream algorithm stages; omitted bindings,
sources, token-ID materialization, and training modules have no local analogue.

## Phase 0 baseline

- codexshim commit: `ab710b43b4e2061b043bebe4e005c0dc3528890b`
- Codex client commit: `bbcf5e10fb493f0f46e4155455e1b5c4f69cfe63`
- Original planned 246,384-byte / 78,909-token artifact: not present in the
  tracked tree, `local/`, or any local reference checkout.
- Replacement reproducible corpus: repeat
  `tests/fixtures/replacement_large_corpus_seed.txt` and truncate to 246,384
  UTF-8 bytes; `tiktoken==0.12.0` ordinary count: 57,475. The pinned fixture
  decouples the oracle from later source-file module moves.
- Client payload contract: structured content takes precedence over MCP text;
  otherwise text-only MCP content is JSON-serialized; image results become
  content items. Codex prepends `Wall time: … seconds\nOutput:`. The server
  reserves 128 tokens for that client-owned wrapper.

## Patch ledger

| Date | Upstream basis | Change |
|---|---|---|
| 2026-08-11 | `fac0114b37120ec8a76362e9ee8e1c742aaafaef` | Fixed source, license, ranks URL/hash, Codex client payload contract, and differential oracle. |
| 2026-08-11 | `fac0114b37120ec8a76362e9ee8e1c742aaafaef` | Retained dense rank loading, merge reconstruction, ordinary o200k scalar pretokenization, and byte-level BPE count semantics. |
| 2026-08-11 | `fac0114b37120ec8a76362e9ee8e1c742aaafaef` | Removed all non-o200k source closures and production dependencies; replaced ICU/nightly SIMD dispatch with stable Unicode classification and scalar traversal. |
| 2026-08-11 | `fac0114b37120ec8a76362e9ee8e1c742aaafaef` | Replaced caller-visible token IDs and token arenas with early-exit count-only processing, cancellation checkpoints, fixed short-cache slots, bounded long-cache storage, and bounded retained scratch. |
| 2026-08-11 | `fac0114b37120ec8a76362e9ee8e1c742aaafaef` | Pinned the 246,384-byte replacement corpus seed under `tests/fixtures` so responsibility-driven source moves cannot alter the 57,475-token differential oracle. |
| 2026-08-11 | `fac0114b37120ec8a76362e9ee8e1c742aaafaef` | Declared the embedded ranks binary and the replacement seed LF-only in `.gitattributes`, preserving their pinned bytes in Windows checkouts. |

Future upstream changes must add a ledger row with the source commit, local
adaptation, and differential verification. Do not replace this source tree
wholesale or depend on an unpinned Git branch.
