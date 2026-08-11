use super::*;

/// Expand a Unicode ligature character code to its ASCII equivalent.
///
/// This function handles the Unicode ligature character codes (U+FB00 to U+FB04)
/// and expands them to their multi-character ASCII equivalents.
///
/// This is the u16 character code variant, used in the character mapping priority chain
/// where character codes come as u16 values directly from the PDF.
///
/// # Arguments
///
/// * `char_code` - The character code (as u16) to potentially expand
///
/// # Returns
///
/// The expanded string if `char_code` is a ligature, None otherwise.
///
/// # Examples
/// Look up a character in the Adobe Symbol font encoding.
///
/// This function implements the Symbol font encoding table as defined in
/// PDF Specification Appendix D.4 (ISO 32000-1:2008, pages 996-997).
///
/// Symbol font is used extensively in mathematical and scientific documents
/// for Greek letters, mathematical operators, and special symbols.
///
/// # Arguments
///
/// * `code` - The character code (0-255)
///
/// # Returns
///
/// The corresponding Unicode character, or None if not in the encoding.
///
/// # References
///
/// - PDF 32000-1:2008, Appendix D.4 - Symbol Encoding
///
/// # Examples
///
/// ```ignore
/// # use pdf_oxide::fonts::font_dict::symbol_encoding_lookup;
/// assert_eq!(symbol_encoding_lookup(0x72), Some('ρ')); // rho
/// assert_eq!(symbol_encoding_lookup(0x61), Some('α')); // alpha
/// assert_eq!(symbol_encoding_lookup(0xF2), Some('∫')); // integral
/// ```ignore
pub(super) fn symbol_encoding_lookup(code: u8) -> Option<char> {
    match code {
        // Greek lowercase letters
        0x61 => Some('α'), // alpha
        0x62 => Some('β'), // beta
        0x63 => Some('χ'), // chi
        0x64 => Some('δ'), // delta
        0x65 => Some('ε'), // epsilon
        0x66 => Some('φ'), // phi
        0x67 => Some('γ'), // gamma
        0x68 => Some('η'), // eta
        0x69 => Some('ι'), // iota
        0x6A => Some('ϕ'), // phi1 (variant)
        0x6B => Some('κ'), // kappa
        0x6C => Some('λ'), // lambda
        0x6D => Some('μ'), // mu
        0x6E => Some('ν'), // nu
        0x6F => Some('ο'), // omicron
        0x70 => Some('π'), // pi
        0x71 => Some('θ'), // theta
        0x72 => Some('ρ'), // rho ← THE IMPORTANT ONE for Pearson's ρ!
        0x73 => Some('σ'), // sigma
        0x74 => Some('τ'), // tau
        0x75 => Some('υ'), // upsilon
        0x76 => Some('ϖ'), // omega1 (variant pi)
        0x77 => Some('ω'), // omega
        0x78 => Some('ξ'), // xi
        0x79 => Some('ψ'), // psi
        0x7A => Some('ζ'), // zeta

        // Greek uppercase letters
        0x41 => Some('Α'), // Alpha
        0x42 => Some('Β'), // Beta
        0x43 => Some('Χ'), // Chi
        0x44 => Some('Δ'), // Delta
        0x45 => Some('Ε'), // Epsilon
        0x46 => Some('Φ'), // Phi
        0x47 => Some('Γ'), // Gamma
        0x48 => Some('Η'), // Eta
        0x49 => Some('Ι'), // Iota
        0x4B => Some('Κ'), // Kappa
        0x4C => Some('Λ'), // Lambda
        0x4D => Some('Μ'), // Mu
        0x4E => Some('Ν'), // Nu
        0x4F => Some('Ο'), // Omicron
        0x50 => Some('Π'), // Pi
        0x51 => Some('Θ'), // Theta
        0x52 => Some('Ρ'), // Rho
        0x53 => Some('Σ'), // Sigma
        0x54 => Some('Τ'), // Tau
        0x55 => Some('Υ'), // Upsilon
        0x57 => Some('Ω'), // Omega
        0x58 => Some('Ξ'), // Xi
        0x59 => Some('Ψ'), // Psi
        0x5A => Some('Ζ'), // Zeta

        // Mathematical operators
        0xB1 => Some('±'), // plusminus
        0xB4 => Some('÷'), // divide
        0xB5 => Some('∞'), // infinity
        0xB6 => Some('∂'), // partialdiff
        0xB7 => Some('•'), // bullet
        0xB9 => Some('≠'), // notequal
        0xBA => Some('≡'), // equivalence
        0xBB => Some('≈'), // approxequal
        0xBC => Some('…'), // ellipsis
        0xBE => Some('⊥'), // perpendicular
        0xBF => Some('⊙'), // circleplus

        0xD0 => Some('°'), // degree
        0xD1 => Some('∇'), // gradient (nabla)
        0xD2 => Some('¬'), // logicalnot
        0xD3 => Some('∧'), // logicaland
        0xD4 => Some('∨'), // logicalor
        0xD5 => Some('∏'), // product ← Product symbol!
        0xD6 => Some('√'), // radical ← Square root!
        0xD7 => Some('⋅'), // dotmath
        0xD8 => Some('⊕'), // circleplus
        0xD9 => Some('⊗'), // circletimes

        0xDA => Some('∈'), // element
        0xDB => Some('∉'), // notelement
        0xDC => Some('∠'), // angle
        0xDD => Some('∇'), // gradient
        0xDE => Some('®'), // registered
        0xDF => Some('©'), // copyright
        0xE0 => Some('™'), // trademark

        0xE1 => Some('∑'), // summation ← Summation symbol!
        0xE2 => Some('⊂'), // propersubset
        0xE3 => Some('⊃'), // propersuperset
        0xE4 => Some('⊆'), // reflexsubset
        0xE5 => Some('⊇'), // reflexsuperset
        0xE6 => Some('∪'), // union
        0xE7 => Some('∩'), // intersection
        0xE8 => Some('∀'), // universal
        0xE9 => Some('∃'), // existential
        0xEA => Some('¬'), // logicalnot

        0xF1 => Some('〈'), // angleleft
        0xF2 => Some('∫'),  // integral ← Integral symbol!
        0xF3 => Some('⌠'),  // integraltp
        0xF4 => Some('⌡'),  // integralbt
        0xF5 => Some('⊓'),  // square intersection
        0xF6 => Some('⊔'),  // square union
        0xF7 => Some('〉'), // angleright

        // Basic punctuation and symbols (overlap with ASCII)
        0x20 => Some(' '), // space
        0x21 => Some('!'), // exclam
        0x22 => Some('∀'), // universal (sometimes mapped here)
        0x23 => Some('#'), // numbersign
        0x24 => Some('∃'), // existential (sometimes mapped here)
        0x25 => Some('%'), // percent
        0x26 => Some('&'), // ampersand
        0x27 => Some('∋'), // suchthat
        0x28 => Some('('), // parenleft
        0x29 => Some(')'), // parenright
        0x2A => Some('∗'), // asteriskmath
        0x2B => Some('+'), // plus
        0x2C => Some(','), // comma
        0x2D => Some('−'), // minus
        0x2E => Some('.'), // period
        0x2F => Some('/'), // slash

        // Digits 0-9 (0x30-0x39) map to themselves
        0x30..=0x39 => Some(code as char),

        0x3A => Some(':'), // colon
        0x3B => Some(';'), // semicolon
        0x3C => Some('<'), // less
        0x3D => Some('='), // equal
        0x3E => Some('>'), // greater
        0x3F => Some('?'), // question

        0x40 => Some('≅'), // congruent

        // Brackets and arrows
        0x5B => Some('['), // bracketleft
        0x5C => Some('∴'), // therefore
        0x5D => Some(']'), // bracketright
        0x5E => Some('⊥'), // perpendicular
        0x5F => Some('_'), // underscore

        0x7B => Some('{'), // braceleft
        0x7C => Some('|'), // bar
        0x7D => Some('}'), // braceright
        0x7E => Some('∼'), // similar

        // Math operators previously missing from the Adobe Symbol set (Annex D.5).
        0xA3 => Some('\u{2264}'), // ≤ lessequal    (octal 243)
        0xA5 => Some('\u{221E}'), // ∞ infinity     (octal 245)
        0xB3 => Some('\u{2265}'), // ≥ greaterequal (octal 263)

        _ => None,
    }
}

/// Look up a character in the Adobe ZapfDingbats font encoding.
///
/// This function implements a subset of the ZapfDingbats font encoding table
/// as defined in PDF Specification Appendix D.5 (ISO 32000-1:2008, page 998).
///
/// ZapfDingbats font is used for ornamental symbols, arrows, and decorative characters.
///
/// # Arguments
///
/// * `code` - The character code (0-255)
///
/// # Returns
///
/// The corresponding Unicode character, or None if not in the encoding.
///
/// # References
///
/// - PDF 32000-1:2008, Appendix D.5 - ZapfDingbats Encoding
pub(super) fn zapf_dingbats_encoding_lookup(code: u8) -> Option<char> {
    match code {
        0x20 => Some(' '), // space
        0x21 => Some('✁'), // scissors
        0x22 => Some('✂'), // scissors (filled)
        0x23 => Some('✃'), // scissors (outline)
        0x24 => Some('✄'), // scissors (small)
        0x25 => Some('☎'), // telephone
        0x26 => Some('✆'), // telephone (filled)
        0x27 => Some('✇'), // tape drive
        0x28 => Some('✈'), // airplane
        0x29 => Some('✉'), // envelope
        0x2A => Some('☛'), // hand pointing right
        0x2B => Some('☞'), // hand pointing right (filled)
        0x2C => Some('✌'), // victory hand
        0x2D => Some('✍'), // writing hand
        0x2E => Some('✎'), // pencil
        0x2F => Some('✏'), // pencil (filled)

        0x30 => Some('✐'), // pen nib
        0x31 => Some('✑'), // pen nib (filled)
        0x32 => Some('✒'), // pen nib (outline)
        0x33 => Some('✓'), // checkmark
        0x34 => Some('✔'), // checkmark (bold)
        0x35 => Some('✕'), // multiplication X
        0x36 => Some('✖'), // multiplication X (heavy)
        0x37 => Some('✗'), // ballot X
        0x38 => Some('✘'), // ballot X (heavy)
        0x39 => Some('✙'), // outlined Greek cross
        0x3A => Some('✚'), // heavy Greek cross
        0x3B => Some('✛'), // open center cross
        0x3C => Some('✜'), // heavy open center cross
        0x3D => Some('✝'), // Latin cross
        0x3E => Some('✞'), // Latin cross (shadowed)
        0x3F => Some('✟'), // Latin cross (outline)

        // Common symbols
        0x40 => Some('✠'), // Maltese cross
        0x41 => Some('✡'), // Star of David
        0x42 => Some('✢'), // four teardrop-spoked asterisk
        0x43 => Some('✣'), // four balloon-spoked asterisk
        0x44 => Some('✤'), // heavy four balloon-spoked asterisk
        0x45 => Some('✥'), // four club-spoked asterisk
        0x46 => Some('✦'), // black four pointed star
        0x47 => Some('✧'), // white four pointed star
        0x48 => Some('★'), // black star
        0x49 => Some('✩'), // outlined black star
        0x4A => Some('✪'), // circled white star
        0x4B => Some('✫'), // circled black star
        0x4C => Some('✬'), // shadowed white star
        0x4D => Some('✭'), // heavy asterisk
        0x4E => Some('✮'), // eight spoke asterisk
        0x4F => Some('✯'), // eight pointed black star

        // More ornaments
        0x50 => Some('✰'), // eight pointed pinwheel star
        0x51 => Some('✱'), // heavy eight pointed pinwheel star
        0x52 => Some('✲'), // eight pointed star
        0x53 => Some('✳'), // eight pointed star (outlined)
        0x54 => Some('✴'), // eight pointed star (heavy)
        0x55 => Some('✵'), // six pointed black star
        0x56 => Some('✶'), // six pointed star
        0x57 => Some('✷'), // eight pointed star (black)
        0x58 => Some('✸'), // heavy eight pointed star
        0x59 => Some('✹'), // twelve pointed black star
        0x5A => Some('✺'), // sixteen pointed star
        0x5B => Some('✻'), // teardrop-spoked asterisk
        0x5C => Some('✼'), // open center teardrop-spoked asterisk
        0x5D => Some('✽'), // heavy teardrop-spoked asterisk
        0x5E => Some('✾'), // six petalled black and white florette
        0x5F => Some('✿'), // black florette

        // Geometric shapes
        0x60 => Some('❀'), // white florette
        0x61 => Some('❁'), // eight petalled outlined black florette
        0x62 => Some('❂'), // circled open center eight pointed star
        0x63 => Some('❃'), // heavy teardrop-spoked pinwheel asterisk
        0x64 => Some('❄'), // snowflake
        0x65 => Some('❅'), // tight trifoliate snowflake
        0x66 => Some('❆'), // heavy chevron snowflake
        0x67 => Some('❇'), // sparkle
        0x68 => Some('❈'), // heavy sparkle
        0x69 => Some('❉'), // balloon-spoked asterisk
        0x6A => Some('❊'), // eight teardrop-spoked propeller asterisk
        0x6B => Some('❋'), // heavy eight teardrop-spoked propeller asterisk

        // Arrows
        0x6C => Some('●'), // black circle
        0x6D => Some('○'), // white circle
        0x6E => Some('❍'), // shadowed white circle
        0x6F => Some('■'), // black square
        0x70 => Some('□'), // white square
        0x71 => Some('▢'), // white square with rounded corners
        0x72 => Some('▣'), // white square containing black small square
        0x73 => Some('▤'), // square with horizontal fill
        0x74 => Some('▥'), // square with vertical fill
        0x75 => Some('▦'), // square with orthogonal crosshatch fill
        0x76 => Some('▧'), // square with upper left to lower right fill
        0x77 => Some('▨'), // square with upper right to lower left fill
        0x78 => Some('▩'), // square with diagonal crosshatch fill
        0x79 => Some('▪'), // black small square
        0x7A => Some('▫'), // white small square

        // Circled digits (Annex D.6, octal 254–323), previously dropped. Codes
        // are the spec's octal CODE in hex; each range is contiguous in Unicode.
        0xAC..=0xB5 => char::from_u32(0x2460 + (code as u32 - 0xAC)), // ① ⑩  a120 a129
        0xB6..=0xBF => char::from_u32(0x2776 + (code as u32 - 0xB6)), // ❶ ❿  a130 a139
        0xC0..=0xC9 => char::from_u32(0x2780 + (code as u32 - 0xC0)), // ➀ ➉  a140 a149
        0xCA..=0xD3 => char::from_u32(0x278A + (code as u32 - 0xCA)), // ➊ ➓  a150 a159

        // Arrows (Annex D.6, octal 324–376): four singletons, then two runs.
        0xD4 => Some('\u{2794}'), // ➔ a160  heavy wide-headed rightwards arrow
        0xD5 => Some('\u{2192}'), // → a161  rightwards arrow
        0xD6 => Some('\u{2194}'), // ↔ a163  left right arrow
        0xD7 => Some('\u{2195}'), // ↕ a164  up down arrow
        0xD8..=0xEF => char::from_u32(0x2798 + (code as u32 - 0xD8)), // ➘ ➯  a196…a182
        0xF1..=0xFE => char::from_u32(0x27B1 + (code as u32 - 0xF1)), // ➱ ➾  a201…a191

        _ => None,
    }
}

/// Look up a character in PDFDocEncoding.
///
/// PDFDocEncoding is a superset of ISO Latin-1 used as the default encoding
/// for PDF text strings and metadata (bookmarks, annotations, document info).
///
/// Codes 0-127 are identical to ASCII.
/// Codes 128-159 have special mappings (different from ISO Latin-1).
/// Codes 160-255 are identical to ISO Latin-1.
///
/// # PDF Spec Reference
///
/// ISO 32000-1:2008, Appendix D.2, Table D.2, page 994
///
/// # Arguments
///
/// * `code` - The byte code to look up (0-255)
///
/// # Returns
///
/// The Unicode character for this code, or None for undefined codes
pub fn pdfdoc_encoding_lookup(code: u8) -> Option<char> {
    match code {
        // ASCII range (0-127)
        0x00..=0x7F => Some(code as char),

        // PDFDocEncoding special range (128-159)
        0x80 => Some('•'),        // bullet
        0x81 => Some('†'),        // dagger
        0x82 => Some('‡'),        // daggerdbl
        0x83 => Some('…'),        // ellipsis
        0x84 => Some('—'),        // emdash
        0x85 => Some('–'),        // endash
        0x86 => Some('ƒ'),        // florin
        0x87 => Some('⁄'),        // fraction
        0x88 => Some('‹'),        // guilsinglleft
        0x89 => Some('›'),        // guilsinglright
        0x8A => Some('−'),        // minus (different from hyphen!)
        0x8B => Some('‰'),        // perthousand
        0x8C => Some('„'),        // quotedblbase
        0x8D => Some('"'),        // quotedblleft
        0x8E => Some('"'),        // quotedblright
        0x8F => Some('\u{2018}'), // quoteleft (left single quotation mark)
        0x90 => Some('\u{2019}'), // quoteright (right single quotation mark)
        0x91 => Some('‚'),        // quotesinglbase
        0x92 => Some('™'),        // trademark
        0x93 => Some('ﬁ'),        // fi ligature
        0x94 => Some('ﬂ'),        // fl ligature
        0x95 => Some('Ł'),        // Lslash
        0x96 => Some('Œ'),        // OE
        0x97 => Some('Š'),        // Scaron
        0x98 => Some('Ÿ'),        // Ydieresis
        0x99 => Some('Ž'),        // Zcaron
        0x9A => Some('ı'),        // dotlessi
        0x9B => Some('ł'),        // lslash
        0x9C => Some('œ'),        // oe
        0x9D => Some('š'),        // scaron
        0x9E => Some('ž'),        // zcaron
        0x9F => None,             // undefined

        // ISO Latin-1 range (160-255) - direct mapping
        0xA0..=0xFF => Some(code as char),
    }
}
