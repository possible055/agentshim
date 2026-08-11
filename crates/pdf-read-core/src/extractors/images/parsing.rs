use super::*;

/// Parse a ColorSpace name from a PDF object.
pub fn parse_color_space(obj: &crate::object::Object) -> Result<ColorSpace> {
    use crate::object::Object;

    match obj {
        Object::Name(name) => match name.as_str() {
            "DeviceRGB" => Ok(ColorSpace::DeviceRGB),
            "DeviceGray" => Ok(ColorSpace::DeviceGray),
            "DeviceCMYK" => Ok(ColorSpace::DeviceCMYK),
            "Pattern" => Ok(ColorSpace::Pattern),
            other => Err(Error::Image(format!("Unsupported color space: {}", other))),
        },
        Object::Array(arr) if !arr.is_empty() => {
            if let Some(name) = arr[0].as_name() {
                match name {
                    "Indexed" => Ok(ColorSpace::Indexed),
                    "CalGray" => Ok(ColorSpace::CalGray),
                    "CalRGB" => Ok(ColorSpace::CalRGB),
                    "Lab" => Ok(ColorSpace::Lab),
                    "ICCBased" => {
                        let num_components = if arr.len() > 1 {
                            if let Some(stream_dict) = arr[1].as_dict() {
                                stream_dict
                                    .get("N")
                                    .and_then(|obj| match obj {
                                        Object::Integer(n) => Some(*n as usize),
                                        _ => None,
                                    })
                                    .unwrap_or(3)
                            } else {
                                3
                            }
                        } else {
                            3
                        };
                        Ok(ColorSpace::ICCBased(num_components))
                    }
                    "Separation" => Ok(ColorSpace::Separation),
                    "DeviceN" => Ok(ColorSpace::DeviceN),
                    "Pattern" => Ok(ColorSpace::Pattern),
                    other => Err(Error::Image(format!(
                        "Unsupported array color space: {}",
                        other
                    ))),
                }
            } else {
                Err(Error::Image(
                    "Color space array must start with a name".to_string(),
                ))
            }
        }
        _ => Err(Error::Image(format!(
            "Invalid color space object: {:?}",
            obj
        ))),
    }
}

/// True when a 1-bit image's `/Decode` array is `[1 0]` (inverted) rather
/// than the DeviceGray default `[0 1]`. ISO 32000-1:2008 8.9.5.2 Table 90:
/// for a 1-bit component the default Decode maps sample 0 -> black, 1 ->
/// white; `[1 0]` reverses that (0 -> white, 1 -> black). Absent, malformed,
/// or non-inverted arrays are treated as the default (no inversion).
pub(super) fn decode_array_inverts_1bpc(decode: Option<&crate::object::Object>) -> bool {
    let arr = match decode.and_then(|o| o.as_array()) {
        Some(a) if a.len() == 2 => a,
        _ => return false,
    };
    let as_num =
        |o: &crate::object::Object| o.as_integer().map(|i| i as f64).or_else(|| o.as_real());
    matches!((as_num(&arr[0]), as_num(&arr[1])), (Some(lo), Some(hi)) if lo > hi)
}
