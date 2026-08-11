use super::*;

#[test]
fn test_encode_pdf_text_string_ascii_unchanged() {
    let bytes = encode_pdf_text_string("Hello, world!");
    assert_eq!(bytes, b"Hello, world!");
}

#[test]
fn test_encode_pdf_text_string_latin1_direct_byte() {
    // é = U+00E9 → byte 0xE9 in PDFDocEncoding (same as Latin-1)
    let bytes = encode_pdf_text_string("é");
    assert_eq!(bytes, vec![0xE9_u8]);
}

#[test]
fn test_encode_pdf_text_string_portuguese_accents() {
    // "Lógico" from issue #402
    let bytes = encode_pdf_text_string("Lógico");
    // L=0x4C  ó=0xF3  g=0x67  i=0x69  c=0x63  o=0x6F
    assert_eq!(bytes, vec![0x4C, 0xF3, 0x67, 0x69, 0x63, 0x6F]);
}

#[test]
fn test_encode_pdf_text_string_all_latin1_supplement() {
    // Verify every character U+0080–U+00FF maps to its code-point byte
    for cp in 0x80_u32..=0xFF {
        let ch = char::from_u32(cp).unwrap();
        let s: String = ch.into();
        let bytes = encode_pdf_text_string(&s);
        assert_eq!(bytes, vec![cp as u8], "failed for U+{:04X}", cp);
    }
}

#[test]
fn test_encode_pdf_text_string_cjk_uses_utf16be_with_bom() {
    // 中 = U+4E2D
    let bytes = encode_pdf_text_string("中");
    // BOM 0xFE 0xFF, then U+4E2D as big-endian: 0x4E 0x2D
    assert_eq!(bytes, vec![0xFE, 0xFF, 0x4E, 0x2D]);
}

#[test]
fn test_encode_pdf_text_string_mixed_triggers_utf16be() {
    // If any char is above U+00FF the whole string goes UTF-16BE
    let bytes = encode_pdf_text_string("aé中");
    assert_eq!(&bytes[..2], &[0xFE, 0xFF], "must start with BOM");
    // a=0x0061, é=0x00E9, 中=0x4E2D
    let expected = vec![0xFE, 0xFF, 0x00, 0x61, 0x00, 0xE9, 0x4E, 0x2D];
    assert_eq!(bytes, expected);
}

#[test]
fn test_encode_pdf_text_string_supplementary_plane_surrogate_pair() {
    // 𝄞 (MUSICAL SYMBOL G CLEF) = U+1D11E, encoded in UTF-16 as surrogate pair
    let bytes = encode_pdf_text_string("𝄞");
    assert_eq!(&bytes[..2], &[0xFE, 0xFF], "must start with BOM");
    // UTF-16BE surrogate pair for U+1D11E: 0xD834 0xDD1E
    assert_eq!(bytes, vec![0xFE, 0xFF, 0xD8, 0x34, 0xDD, 0x1E]);
}

#[test]
fn test_object_text_string_accepts_str() {
    match Object::text_string("hello") {
        Object::String(b) => assert_eq!(b, b"hello"),
        _ => panic!("expected String"),
    }
}

#[test]
fn test_object_text_string_accepts_owned_string() {
    let s = String::from("Ångström");
    match Object::text_string(s) {
        Object::String(b) => {
            // Å=0xC5  n=0x6E  g=0x67  s=0x73  t=0x74  r=0x72  ö=0xF6  m=0x6D
            assert_eq!(b, vec![0xC5, 0x6E, 0x67, 0x73, 0x74, 0x72, 0xF6, 0x6D]);
        }
        _ => panic!("expected String"),
    }
}

#[test]
fn test_object_text_string_empty() {
    match Object::text_string("") {
        Object::String(b) => assert!(b.is_empty()),
        _ => panic!("expected String"),
    }
}
