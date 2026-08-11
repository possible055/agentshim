use super::*;

// ── Helper function tests ───────────────────────────────────────

#[test]
fn test_is_operator_start() {
    assert!(is_operator_start(b'B'));
    assert!(is_operator_start(b'a'));
    assert!(is_operator_start(b'\''));
    assert!(is_operator_start(b'"'));
    assert!(is_operator_start(b'*'));
    assert!(!is_operator_start(b'0'));
    assert!(!is_operator_start(b' '));
    assert!(!is_operator_start(b'('));
    assert!(!is_operator_start(b'/'));
}

#[test]
fn test_is_whitespace() {
    // NUL (0x00) is one of the six PDF white-space chars (ISO 32000-1:2008 §7.2, Table 1).
    assert!(is_whitespace(0x00));
    assert!(is_whitespace(b' '));
    assert!(is_whitespace(b'\t'));
    assert!(is_whitespace(b'\r'));
    assert!(is_whitespace(b'\n'));
    assert!(is_whitespace(0x0C));
    assert!(!is_whitespace(b'A'));
    assert!(!is_whitespace(b'0'));
}

#[test]
fn test_is_whitespace_or_delimiter() {
    assert!(is_whitespace_or_delimiter(b' '));
    assert!(is_whitespace_or_delimiter(b'('));
    assert!(is_whitespace_or_delimiter(b')'));
    assert!(is_whitespace_or_delimiter(b'<'));
    assert!(is_whitespace_or_delimiter(b'>'));
    assert!(is_whitespace_or_delimiter(b'['));
    assert!(is_whitespace_or_delimiter(b']'));
    assert!(is_whitespace_or_delimiter(b'{'));
    assert!(is_whitespace_or_delimiter(b'}'));
    assert!(is_whitespace_or_delimiter(b'/'));
    assert!(is_whitespace_or_delimiter(b'%'));
    assert!(!is_whitespace_or_delimiter(b'A'));
    assert!(!is_whitespace_or_delimiter(b'0'));
}

#[test]
fn test_parse_float_fast() {
    assert_eq!(parse_float_fast(b"123"), Some((123.0, 3)));
    assert_eq!(parse_float_fast(b"-45"), Some((-45.0, 3)));
    assert_eq!(parse_float_fast(b"+10"), Some((10.0, 3)));
    assert_eq!(parse_float_fast(b"0.5"), Some((0.5, 3)));
    assert_eq!(parse_float_fast(b".25"), Some((0.25, 3)));
    assert_eq!(parse_float_fast(b"-0.001"), Some((-0.001, 6)));
    assert_eq!(parse_float_fast(b"0"), Some((0.0, 1)));
    // No digits at all
    assert_eq!(parse_float_fast(b"abc"), None);
    assert_eq!(parse_float_fast(b""), None);
    // Sign only
    assert_eq!(parse_float_fast(b"-"), None);
    assert_eq!(parse_float_fast(b"+"), None);
}

#[test]
fn test_parse_literal_string_fast_simple() {
    let data = b"(Hello)";
    let result = parse_literal_string_fast(data, 0);
    assert!(result.is_some());
    let (bytes, end) = result.unwrap();
    assert_eq!(bytes, b"Hello");
    assert_eq!(end, 7);
}

#[test]
fn test_parse_literal_string_fast_with_escapes() {
    let data = b"(Hello\\nWorld)";
    let result = parse_literal_string_fast(data, 0);
    assert!(result.is_some());
    let (bytes, _end) = result.unwrap();
    // Should decode \n to newline
    assert!(bytes.contains(&b'\n'));
}

#[test]
fn test_parse_literal_string_fast_nested_parens() {
    let data = b"(Hello (World))";
    let result = parse_literal_string_fast(data, 0);
    assert!(result.is_some());
    let (bytes, _) = result.unwrap();
    assert_eq!(bytes, b"Hello (World)");
}

#[test]
fn test_parse_literal_string_fast_octal_escape() {
    let data = b"(\\101)"; // \101 = 'A' in octal
    let result = parse_literal_string_fast(data, 0);
    assert!(result.is_some());
    let (bytes, _) = result.unwrap();
    assert_eq!(bytes, b"A");
}

#[test]
fn test_parse_literal_string_fast_all_escapes() {
    let data = b"(\\n\\r\\t\\b\\f\\\\\\(\\))";
    let result = parse_literal_string_fast(data, 0);
    assert!(result.is_some());
    let (bytes, _) = result.unwrap();
    assert_eq!(bytes, &[b'\n', b'\r', b'\t', 0x08, 0x0C, b'\\', b'(', b')']);
}

#[test]
fn test_parse_literal_string_fast_line_continuation_cr() {
    // Backslash-CR should be ignored (line continuation)
    let data = b"(AB\\\rCD)";
    let result = parse_literal_string_fast(data, 0);
    assert!(result.is_some());
    let (bytes, _) = result.unwrap();
    assert_eq!(bytes, b"ABCD");
}

#[test]
fn test_parse_literal_string_fast_line_continuation_lf() {
    // Backslash-LF should be ignored (line continuation)
    let data = b"(AB\\\nCD)";
    let result = parse_literal_string_fast(data, 0);
    assert!(result.is_some());
    let (bytes, _) = result.unwrap();
    assert_eq!(bytes, b"ABCD");
}

#[test]
fn test_parse_literal_string_fast_line_continuation_crlf() {
    // Backslash-CRLF should be ignored (line continuation)
    let data = b"(AB\\\r\nCD)";
    let result = parse_literal_string_fast(data, 0);
    assert!(result.is_some());
    let (bytes, _) = result.unwrap();
    assert_eq!(bytes, b"ABCD");
}

#[test]
fn test_parse_literal_string_fast_unterminated() {
    let data = b"(Hello";
    let result = parse_literal_string_fast(data, 0);
    assert!(result.is_none());
}

#[test]
fn test_parse_hex_string_fast_basic() {
    let data = b"<48656C6C6F>";
    let result = parse_hex_string_fast(data, 0);
    assert!(result.is_some());
    let (bytes, end) = result.unwrap();
    assert_eq!(bytes, b"Hello");
    assert_eq!(end, 12);
}

#[test]
fn test_parse_hex_string_fast_with_whitespace() {
    let data = b"<48 65 6C 6C 6F>";
    let result = parse_hex_string_fast(data, 0);
    assert!(result.is_some());
    let (bytes, _) = result.unwrap();
    assert_eq!(bytes, b"Hello");
}

#[test]
fn test_parse_hex_string_fast_odd_nibbles() {
    let data = b"<ABC>";
    let result = parse_hex_string_fast(data, 0);
    assert!(result.is_some());
    let (bytes, _) = result.unwrap();
    assert_eq!(bytes.len(), 2);
    assert_eq!(bytes[0], 0xAB);
    assert_eq!(bytes[1], 0xC0);
}

#[test]
fn test_parse_hex_string_fast_empty() {
    let data = b"<>";
    let result = parse_hex_string_fast(data, 0);
    assert!(result.is_some());
    let (bytes, _) = result.unwrap();
    assert!(bytes.is_empty());
}

#[test]
fn test_parse_hex_string_fast_unterminated() {
    let data = b"<4865";
    let result = parse_hex_string_fast(data, 0);
    assert!(result.is_none());
}

#[test]
fn test_hex_nibble() {
    assert_eq!(hex_nibble(b'0'), Some(0));
    assert_eq!(hex_nibble(b'9'), Some(9));
    assert_eq!(hex_nibble(b'a'), Some(10));
    assert_eq!(hex_nibble(b'f'), Some(15));
    assert_eq!(hex_nibble(b'A'), Some(10));
    assert_eq!(hex_nibble(b'F'), Some(15));
    assert_eq!(hex_nibble(b'G'), None);
    assert_eq!(hex_nibble(b' '), None);
}

#[test]
fn test_parse_name_fast() {
    let data = b"/Font1 12";
    let (name, end) = parse_name_fast(data, 0);
    assert_eq!(name, "Font1");
    assert_eq!(end, 6);
}

#[test]
fn test_parse_name_fast_empty() {
    let data = b"/ next";
    let (name, end) = parse_name_fast(data, 0);
    assert_eq!(name, "");
    assert_eq!(end, 1);
}

#[test]
fn test_parse_tj_array_fast_basic() {
    let data = b"[(AB) -100 (CD)]";
    let result = parse_tj_array_fast(data, 0);
    assert!(result.is_some());
    let (elements, end) = result.unwrap();
    assert_eq!(elements.len(), 3);
    assert_eq!(end, 16);
    assert!(matches!(&elements[0], TextElement::String(s) if s == b"AB"));
    assert!(matches!(elements[1], TextElement::Offset(f) if (f + 100.0).abs() < 0.01));
    assert!(matches!(&elements[2], TextElement::String(s) if s == b"CD"));
}

#[test]
fn test_parse_tj_array_fast_with_hex() {
    let data = b"[<4142> 50 <4344>]";
    let result = parse_tj_array_fast(data, 0);
    assert!(result.is_some());
    let (elements, _) = result.unwrap();
    assert_eq!(elements.len(), 3);
}

#[test]
fn test_parse_tj_array_fast_empty() {
    let data = b"[]";
    let result = parse_tj_array_fast(data, 0);
    assert!(result.is_some());
    let (elements, _) = result.unwrap();
    assert!(elements.is_empty());
}

#[test]
fn test_parse_tj_array_fast_unterminated() {
    let data = b"[(AB) -100";
    let result = parse_tj_array_fast(data, 0);
    assert!(result.is_none());
}

#[test]
fn test_parse_six_floats_valid() {
    let data = b"1 0 0 1 72 700";
    let result = parse_six_floats(data);
    assert!(result.is_some());
    let (a, b, c, d, e, f) = result.unwrap();
    assert_eq!(a, 1.0);
    assert_eq!(b, 0.0);
    assert_eq!(c, 0.0);
    assert_eq!(d, 1.0);
    assert_eq!(e, 72.0);
    assert_eq!(f, 700.0);
}

#[test]
fn test_parse_six_floats_too_few() {
    let data = b"1 0 0";
    let result = parse_six_floats(data);
    assert!(result.is_none());
}

#[test]
fn test_parse_six_floats_with_negatives() {
    let data = b"-1 0.5 0 -0.5 -72 700.5";
    let result = parse_six_floats(data);
    assert!(result.is_some());
    let (a, _b, _c, d, e, f) = result.unwrap();
    assert_eq!(a, -1.0);
    assert_eq!(d, -0.5);
    assert_eq!(e, -72.0);
    assert!((f - 700.5).abs() < 0.01);
}

#[test]
fn test_parse_six_floats_invalid() {
    let data = b"not numbers";
    let result = parse_six_floats(data);
    assert!(result.is_none());
}

// ── scan_to_et tests ────────────────────────────────────────────

#[test]
fn test_scan_to_et_basic() {
    let data = b"/F1 12 Tf (Hello) Tj ET";
    let result = scan_to_et(data);
    assert!(result.is_some());
    // Should return data after ET
    let remaining = result.unwrap();
    assert!(remaining.is_empty() || remaining[0] != b'E');
}

#[test]
fn test_scan_to_et_with_string_containing_et() {
    // "ET" inside a string should not be matched
    let data = b"(text ET here) Tj ET";
    let result = scan_to_et(data);
    assert!(result.is_some());
}

#[test]
fn test_scan_to_et_no_et() {
    let data = b"/F1 12 Tf (Hello) Tj";
    let result = scan_to_et(data);
    assert!(result.is_none());
}

#[test]
fn test_scan_to_et_in_hex_string() {
    // ET inside a hex string should not be matched
    let data = b"<4554> Tj ET";
    let result = scan_to_et(data);
    assert!(result.is_some());
}
