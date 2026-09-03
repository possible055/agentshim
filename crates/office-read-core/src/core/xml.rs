use std::borrow::Cow;

use quick_xml::events::BytesStart;

use super::error::{Error, Result};

/// Get a required attribute value, returning Error::MissingAttribute if absent.
pub fn required_attr<'a>(event: &'a BytesStart, key: &[u8]) -> Result<Cow<'a, [u8]>> {
    match event.try_get_attribute(key)? {
        Some(attr) => Ok(attr.value),
        None => Err(Error::MissingAttribute {
            element: String::from_utf8_lossy(event.local_name().as_ref()).into_owned(),
            attr: String::from_utf8_lossy(key).into_owned(),
        }),
    }
}

/// Get a required attribute as a UTF-8 string.
pub fn required_attr_str<'a>(event: &'a BytesStart, key: &[u8]) -> Result<Cow<'a, str>> {
    let value = required_attr(event, key)?;
    match value {
        Cow::Borrowed(b) => Ok(Cow::Borrowed(std::str::from_utf8(b)?)),
        Cow::Owned(v) => Ok(Cow::Owned(
            String::from_utf8(v).map_err(|e| e.utf8_error())?,
        )),
    }
}

/// Get an optional attribute value.
pub fn optional_attr<'a>(event: &'a BytesStart, key: &[u8]) -> Result<Option<Cow<'a, [u8]>>> {
    Ok(event.try_get_attribute(key)?.map(|a| a.value))
}

/// Get an optional attribute as a UTF-8 string.
pub fn optional_attr_str<'a>(event: &'a BytesStart, key: &[u8]) -> Result<Option<Cow<'a, str>>> {
    match optional_attr(event, key)? {
        Some(Cow::Borrowed(b)) => Ok(Some(Cow::Borrowed(std::str::from_utf8(b)?))),
        Some(Cow::Owned(v)) => Ok(Some(Cow::Owned(
            String::from_utf8(v).map_err(|e| e.utf8_error())?,
        ))),
        None => Ok(None),
    }
}

/// Get an optional prefixed attribute by local name, trying all namespace prefixes.
/// For example, `optional_prefixed_attr_str(e, b"id")` matches `r:id`, `d3p1:id`, etc.
/// Falls back to unprefixed `id` if no prefixed match is found.
pub fn optional_prefixed_attr_str<'a>(
    event: &'a BytesStart,
    local_name: &[u8],
) -> Result<Option<Cow<'a, str>>> {
    let mut unprefixed = None;
    for attr in event.attributes().flatten() {
        let key = attr.key.as_ref();
        if let Some(pos) = key.iter().position(|&b| b == b':') {
            if &key[pos + 1..] == local_name {
                return Ok(Some(Cow::Owned(unescape_attr_value(&attr)?)));
            }
        } else if key == local_name {
            unprefixed = Some(Cow::Owned(unescape_attr_value(&attr)?));
        }
    }
    Ok(unprefixed)
}

/// Parse an OOXML boolean toggle element.
///
/// Bare element (`<b/>`) = true, `val="0"` / `val="false"` / `val="off"` = false.
/// The `attr_name` is typically `b"w:val"` (WML) or `b"val"` (SML/DrawingML).
pub fn parse_toggle(e: &BytesStart, attr_name: &[u8]) -> bool {
    match optional_attr_str(e, attr_name) {
        Ok(Some(ref val)) => !matches!(val.as_ref(), "0" | "false" | "off"),
        _ => true,
    }
}

// ===========================================================================
// Fast Reader utilities (no namespace resolution — for hot-path parsing)
// ===========================================================================

/// Decode and unescape a `BytesText` event into an owned string.
///
/// quick-xml 0.40 removed `BytesText::unescape()` in favor of explicit
/// `decode()` followed by `escape::unescape()`. This helper preserves
/// the old single-call ergonomics so the parsers don't have to repeat
/// the two-step dance. `EncodingError` and `EscapeError` go through
/// `quick_xml::Error` to reach our `core::Error`.
pub fn unescape_text(e: &quick_xml::events::BytesText<'_>) -> Result<String> {
    let decoded = e.decode().map_err(quick_xml::Error::from)?;
    let unescaped = quick_xml::escape::unescape(&decoded).map_err(quick_xml::Error::from)?;
    Ok(unescaped.into_owned())
}

/// Decode and unescape an `Attribute` value into an owned string.
///
/// quick-xml 0.40 deprecated `Attribute::unescape_value()`, and under the
/// `encoding` feature the method is `cfg`-compiled out entirely (only
/// `decode_and_unescape_value(decoder)` remains). Feature unification can
/// turn `encoding` on transitively (e.g. via `calamine`), so relying on
/// `unescape_value()` makes the build fragile — it fails to compile the
/// moment any crate in the tree enables quick-xml's `encoding` feature.
///
/// OOXML documents are always UTF-8, so we decode the raw attribute bytes
/// as UTF-8 and unescape XML entities (`&amp;`, `&lt;`, …) explicitly. This
/// mirrors `unescape_text` above and is independent of the `encoding` feature.
///
/// One deliberate difference from `unescape_value()`: that method additionally
/// applied XML attribute-value whitespace normalization (a *literal* tab/CR/LF
/// inside a value collapses to a space), which `escape::unescape` does not do.
/// This never affects real OOXML — attribute values do not contain literal
/// control whitespace, and character references (`&#9;`, `&#10;`) are unescaped
/// identically either way.
pub fn unescape_attr_value(attr: &quick_xml::events::attributes::Attribute<'_>) -> Result<String> {
    let decoded = std::str::from_utf8(&attr.value)?;
    let unescaped = quick_xml::escape::unescape(decoded).map_err(quick_xml::Error::from)?;
    Ok(unescaped.into_owned())
}

/// Create a plain Reader (no namespace resolution) configured for OOXML parsing.
/// Use this for format-specific hot paths (worksheets, slides, document body)
/// where all elements are in a single known namespace.
pub fn make_fast_reader(xml: &[u8]) -> quick_xml::Reader<&[u8]> {
    let mut reader = quick_xml::Reader::from_reader(xml);
    let config = reader.config_mut();
    config.trim_text(true);
    config.check_end_names = false;
    config.check_comments = false;
    reader
}

/// Read text content between start and end tags using fast Reader.
pub fn read_text_content_fast(reader: &mut quick_xml::Reader<&[u8]>) -> Result<String> {
    use quick_xml::events::Event;
    let mut text = String::new();
    let mut depth = 1u32;
    loop {
        if crate::budget::is_cancelled() {
            return Err(crate::core::Error::Cancelled);
        }
        match reader.read_event()? {
            Event::Text(e) => {
                text.push_str(&unescape_text(&e)?);
            }
            Event::CData(e) => {
                text.push_str(&String::from_utf8_lossy(&e));
            }
            Event::Start(_) => depth += 1,
            Event::End(_) => {
                depth -= 1;
                if depth == 0 {
                    break;
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(text)
}

/// Skip over the current element and all its children using fast Reader.
pub fn skip_element_fast(reader: &mut quick_xml::Reader<&[u8]>) -> Result<()> {
    use quick_xml::events::Event;
    let mut depth = 1u32;
    loop {
        if crate::budget::is_cancelled() {
            return Err(crate::core::Error::Cancelled);
        }
        match reader.read_event()? {
            Event::Start(_) => depth += 1,
            Event::End(_) => {
                depth -= 1;
                if depth == 0 {
                    break;
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(())
}

/// Transcode XML bytes to UTF-8 if the XML declaration specifies a non-UTF-8 encoding.
/// Returns `None` if the data is already UTF-8 (the common case), or `Some(transcoded)`
/// if transcoding was needed. Callers should use the returned buffer for parsing.
pub fn ensure_utf8(data: &[u8]) -> Option<Vec<u8>> {
    // Quick check: if it's valid UTF-8 already, skip everything
    if std::str::from_utf8(data).is_ok() {
        return None;
    }

    // Look for encoding="..." in the first 200 bytes of the XML declaration
    let header = &data[..data.len().min(200)];
    let header_str = String::from_utf8_lossy(header);

    let encoding_name = if let Some(pos) = header_str.find("encoding=") {
        let rest = &header_str[pos + 9..];
        let quote = rest.as_bytes().first().copied().unwrap_or(b'"');
        if quote == b'"' || quote == b'\'' {
            let inner = &rest[1..];
            inner.split(quote as char).next().unwrap_or("utf-8")
        } else {
            return None;
        }
    } else {
        // No encoding declaration, try ISO-8859-1 as fallback for non-UTF-8
        "iso-8859-1"
    };

    let encoding = encoding_rs::Encoding::for_label(encoding_name.as_bytes())?;
    if encoding == encoding_rs::UTF_8 {
        return None;
    }

    let (result, _, had_errors) = encoding.decode(data);
    if had_errors {
        return None;
    }

    // Replace the encoding declaration with utf-8 so the XML parser doesn't complain
    let mut utf8 = result.into_owned().into_bytes();
    if let Some(pos) = utf8
        .windows(9)
        .position(|w| w.eq_ignore_ascii_case(b"encoding="))
    {
        let rest = &utf8[pos + 9..];
        if let Some(&quote) = rest.first() {
            if quote == b'"' || quote == b'\'' {
                if let Some(end) = rest[1..].iter().position(|&b| b == quote) {
                    let start = pos + 10;
                    let end = start + end;
                    utf8.splice(start..end, b"utf-8".iter().copied());
                }
            }
        }
    }

    Some(utf8)
}

#[cfg(any())]
mod attr_tests {
    use super::unescape_attr_value;
    use quick_xml::events::BytesStart;

    /// Parse `<e {attrs}>` and unescape the value of attribute `key`.
    fn attr_value(attrs: &str, key: &str) -> String {
        let start = BytesStart::from_content(format!("e {attrs}"), 1);
        let attr = start
            .attributes()
            .map(|a| a.unwrap())
            .find(|a| a.key.as_ref() == key.as_bytes())
            .expect("attribute present");
        unescape_attr_value(&attr).unwrap()
    }

    #[test]
    fn unescapes_predefined_and_numeric_entities() {
        assert_eq!(
            attr_value(r#"v="a &amp; b &lt;x&gt; &#65;""#, "v"),
            "a & b <x> A"
        );
    }

    #[test]
    fn passes_plain_value_through_unchanged() {
        assert_eq!(attr_value(r#"r:id="rId7""#, "r:id"), "rId7");
    }

    #[test]
    fn unescapes_ampersand_in_hyperlink_target() {
        assert_eq!(
            attr_value(r#"Target="https://x/?a=1&amp;b=2""#, "Target"),
            "https://x/?a=1&b=2"
        );
    }
}
