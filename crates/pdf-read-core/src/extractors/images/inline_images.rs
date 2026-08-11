use super::*;

/// Expand an inline image's abbreviated dictionary (ISO 32000-1 §8.9.7 Table 91)
/// into the equivalent image XObject dictionary.
///
/// As well as expanding the abbreviations (`/W` → `/Width`, …) this supplies the
/// `/Subtype /Image` that an inline image never carries: §8.9.7 says an inline
/// image's dictionary holds "a subset of the entries in the image dictionary",
/// with the subtype implied by the `BI` operator rather than written out. Callers
/// hand the result to the image-XObject decoder, which *requires* `/Subtype` — so
/// without this the decoder rejects every inline image with "XObject missing
/// /Subtype", and the callers, which use `if let Ok(..)`, drop them SILENTLY.
pub fn expand_inline_image_dict(
    dict: std::collections::HashMap<String, crate::object::Object>,
) -> std::collections::HashMap<String, crate::object::Object> {
    use std::collections::HashMap;
    let mut expanded = HashMap::new();
    for (key, value) in dict {
        let expanded_key = match key.as_str() {
            "W" => "Width",
            "H" => "Height",
            "CS" => "ColorSpace",
            "BPC" => "BitsPerComponent",
            "F" => "Filter",
            "DP" => "DecodeParms",
            "IM" => "ImageMask",
            "I" => "Interpolate",
            "D" => "Decode",
            "Intent" => "Intent",
            _ => &key,
        };
        // §8.9.7 Table 92: inline images abbreviate the VALUES too, not just the
        // keys - `/CS /RGB`, `/F /Fl`. Expanding only the keys leaves the decoder
        // looking at a colour space called "RGB", which it does not know.
        let value = match expanded_key {
            "ColorSpace" => expand_inline_abbrev(value, colorspace_abbrev),
            "Filter" => expand_inline_abbrev(value, filter_abbrev),
            _ => value,
        };
        expanded.insert(expanded_key.to_string(), value);
    }
    // §8.9.7: the subtype is implied by `BI`, never written in the dictionary.
    // The image-XObject decoder requires it, so supply it here. Do not clobber a
    // dictionary that somehow carries one already.
    expanded
        .entry("Subtype".to_string())
        .or_insert_with(|| crate::object::Object::Name("Image".to_string()));
    expanded
}

/// §8.9.7 Table 92 colour-space abbreviations. An unabbreviated name (or a name
/// we do not recognise, e.g. a `/Resources /ColorSpace` entry like `/CS0`) passes
/// through untouched.
fn colorspace_abbrev(name: &str) -> Option<&'static str> {
    match name {
        "G" => Some("DeviceGray"),
        "RGB" => Some("DeviceRGB"),
        "CMYK" => Some("DeviceCMYK"),
        "I" => Some("Indexed"),
        _ => None,
    }
}

/// §8.9.7 Table 92 filter abbreviations.
fn filter_abbrev(name: &str) -> Option<&'static str> {
    match name {
        "AHx" => Some("ASCIIHexDecode"),
        "A85" => Some("ASCII85Decode"),
        "LZW" => Some("LZWDecode"),
        "Fl" => Some("FlateDecode"),
        "RL" => Some("RunLengthDecode"),
        "CCF" => Some("CCITTFaxDecode"),
        "DCT" => Some("DCTDecode"),
        _ => None,
    }
}

/// Rewrite abbreviated names in an inline-image value. Handles a bare name, and
/// an array (a filter chain, or an `[/I /RGB 255 <lookup>]` indexed space, whose
/// BASE name is itself abbreviated).
fn expand_inline_abbrev(
    value: crate::object::Object,
    map: fn(&str) -> Option<&'static str>,
) -> crate::object::Object {
    use crate::object::Object;
    match value {
        Object::Name(n) => match map(&n) {
            Some(full) => Object::Name(full.to_string()),
            None => Object::Name(n),
        },
        Object::Array(items) => Object::Array(
            items
                .into_iter()
                .map(|item| expand_inline_abbrev(item, map))
                .collect(),
        ),
        other => other,
    }
}
