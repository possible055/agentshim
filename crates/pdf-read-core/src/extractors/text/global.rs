use super::*;

/// Global flag controlling whether glyph-decode sites emit `U+FFFD`
/// (REPLACEMENT CHARACTER) into `extract_text` / `extract_words` /
/// `extract_spans` output.
///
/// The historical default is to silently drop `U+FFFD` chars, which
/// is preserved here for back-compat. Setting `true` makes the
/// high-level accessors consistent with `extract_chars` (which
/// always preserves FFFD) so callers can detect unmapped-glyph
/// pages without diffing the two accessors' outputs.
///
/// `Ordering::Relaxed` is sufficient because every read is gated on
/// `Acquire`-style writes from the setter, and the flag is a single
/// boolean with no other state dependencies.
static PRESERVE_UNMAPPED_GLYPHS: AtomicBool = AtomicBool::new(false);

/// Set the global U+FFFD preservation flag. When `true`, the high-level
/// text accessors (`extract_text` / `extract_words` / `extract_spans`)
/// emit U+FFFD chars for glyphs that map to the REPLACEMENT
/// CHARACTER, matching the behaviour of `extract_chars` which has
/// always preserved them. Returns the previous flag value.
///
/// Resolves the filter divergence where the high-level accessors
/// silently drop FFFD while `extract_chars` keeps them, producing
/// empty `extract_text` output on pages whose visible glyphs all
/// map to FFFD (e.g. the MSAM10 math-symbol font).
///
/// The default is `false` to preserve historical fixture output
/// byte-identical for the no-FFFD-glyph case; downstream callers
/// that want to surface unmapped glyphs to the user opt in by
/// setting `true`.
pub fn set_preserve_unmapped_glyphs(preserve: bool) -> bool {
    PRESERVE_UNMAPPED_GLYPHS.swap(preserve, Ordering::SeqCst)
}

/// True if the high-level accessors should preserve `U+FFFD` glyphs.
#[inline]
pub(crate) fn preserve_unmapped_glyphs() -> bool {
    PRESERVE_UNMAPPED_GLYPHS.load(Ordering::Relaxed)
}
