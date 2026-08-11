use super::*;

// 3.14 is a common PDF test value, not trying to use PI constant
#[allow(clippy::approx_constant)]
fn _allow_approx_const() {}

// ========================================================================
// Basic Token Tests
// ========================================================================

#[test]
fn test_parse_positive_integer() {
    let result = token(b"42");
    assert_eq!(result, Ok((&b""[..], Token::Integer(42))));
}

#[test]
fn test_parse_negative_integer() {
    let result = token(b"-123");
    assert_eq!(result, Ok((&b""[..], Token::Integer(-123))));
}

#[test]
fn test_parse_zero() {
    let result = token(b"0");
    assert_eq!(result, Ok((&b""[..], Token::Integer(0))));
}

#[test]
#[allow(clippy::approx_constant)]
fn test_parse_positive_real() {
    let result = token(b"3.14");
    assert_eq!(result, Ok((&b""[..], Token::Real(3.14))));
}

#[test]
fn test_parse_negative_real() {
    let result = token(b"-2.5");
    assert_eq!(result, Ok((&b""[..], Token::Real(-2.5))));
}

#[test]
fn test_parse_real_starting_with_dot() {
    let result = token(b".5");
    assert_eq!(result, Ok((&b""[..], Token::Real(0.5))));
}

#[test]
fn test_parse_real_ending_with_dot() {
    let result = token(b"5.");
    assert_eq!(result, Ok((&b""[..], Token::Real(5.0))));
}

#[test]
fn test_parse_negative_real_starting_with_dot() {
    let result = token(b"-.002");
    assert_eq!(result, Ok((&b""[..], Token::Real(-0.002))));
}

/// A real literal with enough digits to overflow `f64` (well past the
/// PDF 32000-1 Annex C.2 implementation limit of ±3.403×10^38) must
/// clamp to a finite value instead of becoming `Infinity` — an infinite
/// coordinate can poison downstream arithmetic into NaN.
#[test]
fn test_parse_oversized_real_clamps_to_finite() {
    let huge = "9".repeat(400) + ".0";
    let (rest, tok) = token(huge.as_bytes()).unwrap();
    assert!(rest.is_empty());
    match tok {
        Token::Real(n) => assert!(n.is_finite(), "expected a finite clamp, got {n}"),
        other => panic!("expected Token::Real, got {other:?}"),
    }
}

#[test]
fn test_parse_oversized_negative_real_clamps_to_finite_negative() {
    let huge = "-".to_string() + &"9".repeat(400) + ".0";
    let (rest, tok) = token(huge.as_bytes()).unwrap();
    assert!(rest.is_empty());
    match tok {
        Token::Real(n) => {
            assert!(
                n.is_finite() && n < 0.0,
                "expected a finite negative clamp, got {n}"
            )
        }
        other => panic!("expected Token::Real, got {other:?}"),
    }
}

// ========================================================================
// String Tests
// ========================================================================

#[test]
fn test_parse_literal_string() {
    let result = token(b"(Hello)");
    assert_eq!(result, Ok((&b""[..], Token::LiteralString(b"Hello"))));
}

#[test]
fn test_parse_literal_string_with_spaces() {
    let result = token(b"(Hello World)");
    assert_eq!(result, Ok((&b""[..], Token::LiteralString(b"Hello World"))));
}

#[test]
fn test_parse_literal_string_with_nested_parens() {
    let result = token(b"(Hello (nested) World)");
    assert_eq!(
        result,
        Ok((&b""[..], Token::LiteralString(b"Hello (nested) World")))
    );
}

#[test]
fn test_parse_literal_string_with_escape() {
    let result = token(b"(Line1\\nLine2)");
    assert_eq!(
        result,
        Ok((&b""[..], Token::LiteralString(b"Line1\\nLine2")))
    );
}

#[test]
fn test_parse_literal_string_with_escaped_paren() {
    let result = token(b"(Open \\( Close \\))");
    assert_eq!(
        result,
        Ok((&b""[..], Token::LiteralString(b"Open \\( Close \\)")))
    );
}

#[test]
fn test_parse_empty_literal_string() {
    let result = token(b"()");
    assert_eq!(result, Ok((&b""[..], Token::LiteralString(b""))));
}

#[test]
fn test_parse_hex_string() {
    let result = token(b"<48656C6C6F>");
    assert_eq!(result, Ok((&b""[..], Token::HexString(b"48656C6C6F"))));
}

#[test]
fn test_parse_hex_string_with_whitespace() {
    let result = token(b"<48 65 6C 6C 6F>");
    assert_eq!(result, Ok((&b""[..], Token::HexString(b"48 65 6C 6C 6F"))));
}

#[test]
fn test_parse_empty_hex_string() {
    let result = token(b"<>");
    assert_eq!(result, Ok((&b""[..], Token::HexString(b""))));
}

// ========================================================================
// Name Tests
// ========================================================================

#[test]
fn test_parse_name() {
    let result = token(b"/Type");
    assert_eq!(result, Ok((&b""[..], Token::Name("Type".to_string()))));
}

#[test]
fn test_parse_name_with_special_chars() {
    let result = token(b"/A;Name_With-Various***Characters");
    assert_eq!(
        result,
        Ok((
            &b""[..],
            Token::Name("A;Name_With-Various***Characters".to_string())
        ))
    );
}

#[test]
fn test_parse_empty_name() {
    // Empty name is technically invalid per spec but we accept in lenient mode
    let result = token(b"/ ");
    assert_eq!(result, Ok((&b" "[..], Token::Name("".to_string()))));
}

#[test]
fn test_parse_name_with_hex_escape() {
    // /A#20B should decode to "A B"
    let result = token(b"/A#20B");
    assert_eq!(result, Ok((&b""[..], Token::Name("A B".to_string()))));
}

#[test]
fn test_parse_name_with_multiple_hex_escapes() {
    // /A#20B#23C should decode to "A B#C"
    let result = token(b"/A#20B#23C");
    assert_eq!(result, Ok((&b""[..], Token::Name("A B#C".to_string()))));
}

#[test]
fn test_parse_name_with_invalid_hex_escape() {
    // /A#ZZ has invalid hex - should keep # literal
    let result = token(b"/A#ZZ");
    assert_eq!(result, Ok((&b""[..], Token::Name("A#ZZ".to_string()))));
}

#[test]
fn test_decode_name_escapes_directly() {
    // Test the decoder function directly
    assert_eq!(decode_name_escapes("Type"), "Type");
    assert_eq!(decode_name_escapes("A#20B"), "A B");
    assert_eq!(decode_name_escapes("A#20B#23C"), "A B#C");
    assert_eq!(decode_name_escapes("A#"), "A#"); // Invalid - # at end
    assert_eq!(decode_name_escapes("A#2"), "A#2"); // Invalid - only 1 digit
    assert_eq!(decode_name_escapes("A#ZZ"), "A#ZZ"); // Invalid hex
}

// ========================================================================
// Keyword Tests
// ========================================================================

#[test]
fn test_parse_true() {
    let result = token(b"true");
    assert_eq!(result, Ok((&b""[..], Token::True)));
}

#[test]
fn test_parse_false() {
    let result = token(b"false");
    assert_eq!(result, Ok((&b""[..], Token::False)));
}

#[test]
fn test_parse_null() {
    let result = token(b"null");
    assert_eq!(result, Ok((&b""[..], Token::Null)));
}

#[test]
fn test_parse_array_start() {
    let result = token(b"[");
    assert_eq!(result, Ok((&b""[..], Token::ArrayStart)));
}

#[test]
fn test_parse_array_end() {
    let result = token(b"]");
    assert_eq!(result, Ok((&b""[..], Token::ArrayEnd)));
}

#[test]
fn test_parse_dict_start() {
    let result = token(b"<<");
    assert_eq!(result, Ok((&b""[..], Token::DictStart)));
}

#[test]
fn test_parse_dict_end() {
    let result = token(b">>");
    assert_eq!(result, Ok((&b""[..], Token::DictEnd)));
}

#[test]
fn test_parse_obj_start() {
    let result = token(b"obj");
    assert_eq!(result, Ok((&b""[..], Token::ObjStart)));
}

#[test]
fn test_parse_obj_end() {
    let result = token(b"endobj");
    assert_eq!(result, Ok((&b""[..], Token::ObjEnd)));
}

#[test]
fn test_parse_stream_start() {
    let result = token(b"stream");
    assert_eq!(result, Ok((&b""[..], Token::StreamStart)));
}

#[test]
fn test_parse_stream_end() {
    let result = token(b"endstream");
    assert_eq!(result, Ok((&b""[..], Token::StreamEnd)));
}

#[test]
fn test_parse_reference_marker() {
    let result = token(b"R");
    assert_eq!(result, Ok((&b""[..], Token::R)));
}

// ========================================================================
// Whitespace and Comment Tests
// ========================================================================

#[test]
fn test_skip_leading_whitespace() {
    let result = token(b"  \n\t42");
    assert_eq!(result, Ok((&b""[..], Token::Integer(42))));
}

#[test]
fn test_skip_comment() {
    let result = token(b"% This is a comment\n42");
    assert_eq!(result, Ok((&b""[..], Token::Integer(42))));
}

#[test]
fn test_skip_multiple_comments() {
    let result = token(b"% Comment 1\n% Comment 2\n42");
    assert_eq!(result, Ok((&b""[..], Token::Integer(42))));
}

#[test]
fn test_skip_mixed_whitespace_and_comments() {
    let result = token(b"  % Comment\n  \t% Another\n  42");
    assert_eq!(result, Ok((&b""[..], Token::Integer(42))));
}

// ========================================================================
// Edge Cases
// ========================================================================

#[test]
fn test_multiple_tokens() {
    let input = b"42 /Type (Hello) true";
    let (input, tok1) = token(input).unwrap();
    assert_eq!(tok1, Token::Integer(42));

    let (input, tok2) = token(input).unwrap();
    assert_eq!(tok2, Token::Name("Type".to_string()));

    let (input, tok3) = token(input).unwrap();
    assert_eq!(tok3, Token::LiteralString(b"Hello"));

    let (input, tok4) = token(input).unwrap();
    assert_eq!(tok4, Token::True);
    assert_eq!(input, &b""[..]);
}

#[test]
fn test_tokens_function() {
    let input = b"42 /Type (Hello) true";
    let (remaining, toks) = tokens(input).unwrap();
    assert_eq!(remaining, &b""[..]);
    assert_eq!(toks.len(), 4);
    assert_eq!(toks[0], Token::Integer(42));
    assert_eq!(toks[1], Token::Name("Type".to_string()));
    assert_eq!(toks[2], Token::LiteralString(b"Hello"));
    assert_eq!(toks[3], Token::True);
}

#[test]
fn test_dict_vs_hex_string() {
    // << should parse as dict start, not hex string
    let result = token(b"<<");
    assert_eq!(result, Ok((&b""[..], Token::DictStart)));

    // < should parse as hex string
    let result = token(b"<ABC>");
    assert_eq!(result, Ok((&b""[..], Token::HexString(b"ABC"))));
}

#[test]
fn test_complex_pdf_snippet() {
    // Realistic PDF snippet
    let input = b"1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj";
    let (input, tok1) = token(input).unwrap();
    assert_eq!(tok1, Token::Integer(1));

    let (input, tok2) = token(input).unwrap();
    assert_eq!(tok2, Token::Integer(0));

    let (input, tok3) = token(input).unwrap();
    assert_eq!(tok3, Token::ObjStart);

    let (input, tok4) = token(input).unwrap();
    assert_eq!(tok4, Token::DictStart);

    let (input, tok5) = token(input).unwrap();
    assert_eq!(tok5, Token::Name("Type".to_string()));

    let (input, tok6) = token(input).unwrap();
    assert_eq!(tok6, Token::Name("Catalog".to_string()));

    let (input, tok7) = token(input).unwrap();
    assert_eq!(tok7, Token::Name("Pages".to_string()));

    let (input, tok8) = token(input).unwrap();
    assert_eq!(tok8, Token::Integer(2));

    let (input, tok9) = token(input).unwrap();
    assert_eq!(tok9, Token::Integer(0));

    let (input, tok10) = token(input).unwrap();
    assert_eq!(tok10, Token::R);

    let (input, tok11) = token(input).unwrap();
    assert_eq!(tok11, Token::DictEnd);

    let (input, tok12) = token(input).unwrap();
    assert_eq!(tok12, Token::ObjEnd);

    assert_eq!(input, &b""[..]);
}

#[test]
fn test_real_vs_integer_distinction() {
    // These should parse as integers
    assert!(matches!(token(b"0").unwrap().1, Token::Integer(0)));
    assert!(matches!(token(b"42").unwrap().1, Token::Integer(42)));
    assert!(matches!(token(b"-123").unwrap().1, Token::Integer(-123)));

    // These should parse as reals
    assert!(matches!(token(b"0.0").unwrap().1, Token::Real(_)));
    assert!(matches!(token(b"3.14").unwrap().1, Token::Real(_)));
    assert!(matches!(token(b".5").unwrap().1, Token::Real(_)));
    assert!(matches!(token(b"5.").unwrap().1, Token::Real(_)));
}
