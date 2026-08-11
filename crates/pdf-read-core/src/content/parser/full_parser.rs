use super::*;

pub(super) fn parse_content_stream_uncounted(data: &[u8]) -> Result<Vec<Operator>> {
    let estimated_capacity = data.len() / 20;
    let mut operators = Vec::with_capacity(estimated_capacity.min(100_000));
    let mut input = data;
    let mut consecutive_errors: usize = 0;

    // Parse until we consume all input
    while !input.is_empty() {
        // Skip whitespace
        if let Ok((rest, _)) = multispace0::<&[u8], nom::error::Error<&[u8]>>.parse(input) {
            input = rest;
        }

        // Check if we're done
        if input.is_empty() {
            break;
        }

        // Parse one operator with its operands
        match parse_operator_with_operands(input) {
            Ok((rest, op)) => {
                operators.push(op);
                input = rest;
                consecutive_errors = 0;

                check_operator_budget(operators.len())?;

                if operators.len() >= effective_max_operators() {
                    push_operator_cap_warning();
                    break;
                }
            }
            Err(_) => {
                consecutive_errors += 1;
                if consecutive_errors >= MAX_CONSECUTIVE_ERRORS {
                    log::warn!(
                        "Content stream had {} consecutive parse errors, bailing out ({} bytes remaining)",
                        MAX_CONSECUTIVE_ERRORS,
                        input.len()
                    );
                    break;
                }
                // If we can't parse, skip the problematic byte and continue
                // This makes us more resilient to malformed streams
                if input.len() > 1 {
                    input = &input[1..];
                } else {
                    break;
                }
            }
        }
    }

    Ok(operators)
}

/// Parse a single operator with its operands.
///
/// Returns the remaining input and the parsed operator.
///
/// Uses `SmallVec<[Object; 6]>` for the operand buffer to avoid heap
/// allocation for the common case (most PDF operators have 0-6 operands).
/// Only spills to the heap for rare operators with more than 6 operands.
pub(super) fn parse_operator_with_operands(input: &[u8]) -> IResult<&[u8], Operator> {
    // Collect operands until we hit an operator name.
    // SmallVec<[Object; 6]>: stack-allocated for <= 6 operands (covers all
    // standard PDF operators: cm/Tm need 6, most need 0-4). Only spills to
    // heap for pathological content (e.g., deeply nested arrays in Other).
    let mut operands: SmallVec<[Object; 6]> = SmallVec::new();
    let mut remaining = input;

    loop {
        // Skip whitespace
        let (inp, _) = multispace0.parse(remaining)?;
        remaining = inp;

        if remaining.is_empty() {
            return Err(nom::Err::Error(nom::error::Error::new(
                remaining,
                nom::error::ErrorKind::Eof,
            )));
        }

        // Check if this looks like an operator name (alphabetic characters)
        // Operators are typically 1-3 letter keywords
        if is_operator_start(remaining[0]) {
            let (rest, op_name) = parse_operator_name(remaining)?;

            // Special handling for inline images (BI...ID...EI sequence)
            if op_name == "BI" {
                // Parse inline image: BI <dict entries> ID <binary data> EI
                return parse_inline_image(rest);
            }

            let op = build_operator(op_name, operands);
            return Ok((rest, op));
        }

        // Otherwise, try to parse an operand (PDF object)
        let (inp, obj) = parse_object(remaining)?;
        operands.push(obj);
        remaining = inp;
    }
}

/// Check if a byte could start an operator name.
///
/// Operators start with alphabetic characters or special characters like ' or "
pub(super) fn is_operator_start(byte: u8) -> bool {
    byte.is_ascii_alphabetic() || byte == b'\'' || byte == b'"' || byte == b'*'
}

/// Parse an operator name from the input.
///
/// Operator names are typically 1-3 letter alphabetic sequences, but can include:
/// - Single quote (') for the Quote operator
/// - Double quote (") for the DoubleQuote operator
/// - Star (*) for T* operator
pub(super) fn parse_operator_name(input: &[u8]) -> IResult<&[u8], &str> {
    let (input, name_bytes) =
        take_while1(|c: u8| c.is_ascii_alphanumeric() || c == b'\'' || c == b'"' || c == b'*')
            .parse(input)?;

    let name = std::str::from_utf8(name_bytes)
        .map_err(|_| nom::Err::Error(nom::error::Error::new(input, nom::error::ErrorKind::Char)))?;

    Ok((input, name))
}

/// Build an operator from its name and operands.
///
/// This function converts the raw operator name and operands into a strongly-typed
/// Operator enum variant. It handles type conversions and validates operand counts.
///
/// Accepts `SmallVec<[Object; 6]>` to avoid heap allocation for the common case
/// (most PDF operators have 0-6 operands). The operands are consumed and dropped
/// after extraction.
pub(super) fn build_operator(name: &str, operands: SmallVec<[Object; 6]>) -> Operator {
    match name {
        // Text positioning
        "Td" => {
            let tx = get_number(&operands, 0).unwrap_or(0.0);
            let ty = get_number(&operands, 1).unwrap_or(0.0);
            Operator::Td { tx, ty }
        }
        "TD" => {
            let tx = get_number(&operands, 0).unwrap_or(0.0);
            let ty = get_number(&operands, 1).unwrap_or(0.0);
            Operator::TD { tx, ty }
        }
        "Tm" => {
            let a = get_number(&operands, 0).unwrap_or(1.0);
            let b = get_number(&operands, 1).unwrap_or(0.0);
            let c = get_number(&operands, 2).unwrap_or(0.0);
            let d = get_number(&operands, 3).unwrap_or(1.0);
            let e = get_number(&operands, 4).unwrap_or(0.0);
            let f = get_number(&operands, 5).unwrap_or(0.0);
            Operator::Tm { a, b, c, d, e, f }
        }
        "T*" => Operator::TStar,

        // Text showing
        "Tj" => {
            let text = get_string(&operands, 0).unwrap_or_default();
            Operator::Tj { text }
        }
        "TJ" => {
            let elements = if let Some(array) = get_array(&operands, 0) {
                array
                    .iter()
                    .filter_map(|obj| match obj {
                        Object::String(s) => Some(TextElement::String(s.clone())),
                        Object::Integer(i) => Some(TextElement::Offset(*i as f32)),
                        Object::Real(r) => Some(TextElement::Offset(*r as f32)),
                        _ => None,
                    })
                    .collect()
            } else {
                Vec::new()
            };
            Operator::TJ { array: elements }
        }
        "'" => {
            let text = get_string(&operands, 0).unwrap_or_default();
            Operator::Quote { text }
        }
        "\"" => {
            let word_space = get_number(&operands, 0).unwrap_or(0.0);
            let char_space = get_number(&operands, 1).unwrap_or(0.0);
            let text = get_string(&operands, 2).unwrap_or_default();
            Operator::DoubleQuote {
                word_space,
                char_space,
                text,
            }
        }

        // Text state
        "Tc" => {
            let char_space = get_number(&operands, 0).unwrap_or(0.0);
            Operator::Tc { char_space }
        }
        "Tw" => {
            let word_space = get_number(&operands, 0).unwrap_or(0.0);
            Operator::Tw { word_space }
        }
        "Tz" => {
            let scale = get_number(&operands, 0).unwrap_or(100.0);
            Operator::Tz { scale }
        }
        "TL" => {
            let leading = get_number(&operands, 0).unwrap_or(0.0);
            Operator::TL { leading }
        }
        "Tf" => {
            let font = get_name(&operands, 0).unwrap_or("").to_string();
            let size = get_number(&operands, 1).unwrap_or(12.0);
            Operator::Tf { font, size }
        }
        "Tr" => {
            let render = get_integer(&operands, 0).unwrap_or(0) as u8;
            Operator::Tr { render }
        }
        "Ts" => {
            let rise = get_number(&operands, 0).unwrap_or(0.0);
            Operator::Ts { rise }
        }

        // Graphics state
        "q" => Operator::SaveState,
        "Q" => Operator::RestoreState,
        "cm" => {
            let a = get_number(&operands, 0).unwrap_or(1.0);
            let b = get_number(&operands, 1).unwrap_or(0.0);
            let c = get_number(&operands, 2).unwrap_or(0.0);
            let d = get_number(&operands, 3).unwrap_or(1.0);
            let e = get_number(&operands, 4).unwrap_or(0.0);
            let f = get_number(&operands, 5).unwrap_or(0.0);
            Operator::Cm { a, b, c, d, e, f }
        }

        // Color
        "rg" => {
            let r = get_number(&operands, 0).unwrap_or(0.0);
            let g = get_number(&operands, 1).unwrap_or(0.0);
            let b = get_number(&operands, 2).unwrap_or(0.0);
            Operator::SetFillRgb { r, g, b }
        }
        "RG" => {
            let r = get_number(&operands, 0).unwrap_or(0.0);
            let g = get_number(&operands, 1).unwrap_or(0.0);
            let b = get_number(&operands, 2).unwrap_or(0.0);
            Operator::SetStrokeRgb { r, g, b }
        }
        "g" => {
            let gray = get_number(&operands, 0).unwrap_or(0.0);
            Operator::SetFillGray { gray }
        }
        "G" => {
            let gray = get_number(&operands, 0).unwrap_or(0.0);
            Operator::SetStrokeGray { gray }
        }
        "k" => {
            // Set CMYK fill color
            let c = get_number(&operands, 0).unwrap_or(0.0);
            let m = get_number(&operands, 1).unwrap_or(0.0);
            let y = get_number(&operands, 2).unwrap_or(0.0);
            let k = get_number(&operands, 3).unwrap_or(0.0);
            Operator::SetFillCmyk { c, m, y, k }
        }
        "K" => {
            // Set CMYK stroke color
            let c = get_number(&operands, 0).unwrap_or(0.0);
            let m = get_number(&operands, 1).unwrap_or(0.0);
            let y = get_number(&operands, 2).unwrap_or(0.0);
            let k = get_number(&operands, 3).unwrap_or(0.0);
            Operator::SetStrokeCmyk { c, m, y, k }
        }

        // Color space operators
        "cs" => {
            // Set fill color space: name cs
            let name = get_name(&operands, 0).unwrap_or("DeviceGray").to_string();
            Operator::SetFillColorSpace { name }
        }
        "CS" => {
            // Set stroke color space: name CS
            let name = get_name(&operands, 0).unwrap_or("DeviceGray").to_string();
            Operator::SetStrokeColorSpace { name }
        }
        "sc" => {
            // Set fill color: c1 c2 ... cn sc
            // Number of components depends on current color space
            let components: Vec<f32> = operands
                .iter()
                .filter_map(|obj| match obj {
                    Object::Real(r) => Some(*r as f32),
                    Object::Integer(i) => Some(*i as f32),
                    _ => None,
                })
                .collect();
            Operator::SetFillColor { components }
        }
        "SC" => {
            // Set stroke color: c1 c2 ... cn SC
            let components: Vec<f32> = operands
                .iter()
                .filter_map(|obj| match obj {
                    Object::Real(r) => Some(*r as f32),
                    Object::Integer(i) => Some(*i as f32),
                    _ => None,
                })
                .collect();
            Operator::SetStrokeColor { components }
        }
        "scn" => {
            // Set fill color with pattern support: c1 c2 ... cn [name] scn
            // Last operand may be a name for pattern color spaces
            let name = if let Some(Object::Name(n)) = operands.last() {
                Some(n.clone())
            } else {
                None
            };
            let components: Vec<f32> = operands
                .iter()
                .filter_map(|obj| match obj {
                    Object::Real(r) => Some(*r as f32),
                    Object::Integer(i) => Some(*i as f32),
                    Object::Name(_) => None, // Skip pattern name
                    _ => None,
                })
                .collect();
            Operator::SetFillColorN {
                components,
                name: name.map(Box::new),
            }
        }
        "SCN" => {
            // Set stroke color with pattern support: c1 c2 ... cn [name] SCN
            let name = if let Some(Object::Name(n)) = operands.last() {
                Some(n.clone())
            } else {
                None
            };
            let components: Vec<f32> = operands
                .iter()
                .filter_map(|obj| match obj {
                    Object::Real(r) => Some(*r as f32),
                    Object::Integer(i) => Some(*i as f32),
                    Object::Name(_) => None, // Skip pattern name
                    _ => None,
                })
                .collect();
            Operator::SetStrokeColorN {
                components,
                name: name.map(Box::new),
            }
        }

        // Text object
        "BT" => Operator::BeginText,
        "ET" => Operator::EndText,

        // XObject
        "Do" => {
            // Per ISO 32000-1:2008 §7.8.2, operands "shall immediately precede"
            // their operator and none "shall be left over" once it executes.
            // Do takes exactly one operand (the XObject name); if stray operands
            // accumulated ahead of it (e.g. a dropped/malformed `cm` before this
            // `Do`), the name is still the one immediately preceding the operator,
            // i.e. the LAST element, not necessarily the first.
            let name = get_name(&operands, operands.len().saturating_sub(1))
                .unwrap_or("")
                .to_string();
            Operator::Do { name }
        }

        // Path construction
        "m" => {
            let x = get_number(&operands, 0).unwrap_or(0.0);
            let y = get_number(&operands, 1).unwrap_or(0.0);
            Operator::MoveTo { x, y }
        }
        "l" => {
            let x = get_number(&operands, 0).unwrap_or(0.0);
            let y = get_number(&operands, 1).unwrap_or(0.0);
            Operator::LineTo { x, y }
        }
        "c" => {
            // Cubic Bézier curve
            let x1 = get_number(&operands, 0).unwrap_or(0.0);
            let y1 = get_number(&operands, 1).unwrap_or(0.0);
            let x2 = get_number(&operands, 2).unwrap_or(0.0);
            let y2 = get_number(&operands, 3).unwrap_or(0.0);
            let x3 = get_number(&operands, 4).unwrap_or(0.0);
            let y3 = get_number(&operands, 5).unwrap_or(0.0);
            Operator::CurveTo {
                x1,
                y1,
                x2,
                y2,
                x3,
                y3,
            }
        }
        "v" => {
            // Bézier curve (first control point = current point)
            let x2 = get_number(&operands, 0).unwrap_or(0.0);
            let y2 = get_number(&operands, 1).unwrap_or(0.0);
            let x3 = get_number(&operands, 2).unwrap_or(0.0);
            let y3 = get_number(&operands, 3).unwrap_or(0.0);
            Operator::CurveToV { x2, y2, x3, y3 }
        }
        "y" => {
            // Bézier curve (second control point = end point)
            let x1 = get_number(&operands, 0).unwrap_or(0.0);
            let y1 = get_number(&operands, 1).unwrap_or(0.0);
            let x3 = get_number(&operands, 2).unwrap_or(0.0);
            let y3 = get_number(&operands, 3).unwrap_or(0.0);
            Operator::CurveToY { x1, y1, x3, y3 }
        }
        "h" => Operator::ClosePath,
        "re" => {
            let x = get_number(&operands, 0).unwrap_or(0.0);
            let y = get_number(&operands, 1).unwrap_or(0.0);
            let width = get_number(&operands, 2).unwrap_or(0.0);
            let height = get_number(&operands, 3).unwrap_or(0.0);
            Operator::Rectangle {
                x,
                y,
                width,
                height,
            }
        }
        "S" => Operator::Stroke,
        "f" | "F" => Operator::Fill, // "F" is obsolete equivalent of "f" (nonzero winding fill)
        "f*" => Operator::FillEvenOdd,
        "b" => Operator::CloseFillStroke,
        "b*" => Operator::CloseFillStrokeEvenOdd,
        "B" => Operator::FillStroke,
        "B*" => Operator::FillStrokeEvenOdd,
        "n" => Operator::EndPath,
        "W" => Operator::ClipNonZero,
        "W*" => Operator::ClipEvenOdd,

        // Graphics state operators
        "w" => {
            let width = get_number(&operands, 0).unwrap_or(1.0);
            Operator::SetLineWidth { width }
        }
        "d" => {
            // d operator: array phase
            // Example: [3 2] 0 d means 3 on, 2 off, starting at phase 0
            let array = if let Some(Object::Array(arr)) = operands.first() {
                arr.iter()
                    .filter_map(|obj| match obj {
                        Object::Integer(i) => Some(*i as f32),
                        Object::Real(r) => Some(*r as f32),
                        _ => None,
                    })
                    .collect()
            } else {
                Vec::new()
            };
            let phase = get_number(&operands, 1).unwrap_or(0.0);
            Operator::SetDash { array, phase }
        }
        "J" => {
            // J operator: integer J
            // 0=butt cap, 1=round cap, 2=projecting square cap
            let cap_style = get_integer(&operands, 0).unwrap_or(0) as u8;
            Operator::SetLineCap { cap_style }
        }
        "j" => {
            // j operator: integer j
            // 0=miter join, 1=round join, 2=bevel join
            let join_style = get_integer(&operands, 0).unwrap_or(0) as u8;
            Operator::SetLineJoin { join_style }
        }
        "M" => {
            // M operator: number M
            // Miter limit (ratio of miter length to line width)
            let limit = get_number(&operands, 0).unwrap_or(10.0);
            Operator::SetMiterLimit { limit }
        }
        "ri" => {
            // ri operator: name ri
            // Rendering intent: /AbsoluteColorimetric, /RelativeColorimetric, /Saturation, or /Perceptual
            let intent = get_name(&operands, 0)
                .unwrap_or("RelativeColorimetric")
                .to_string();
            Operator::SetRenderingIntent { intent }
        }
        "i" => {
            // i operator: number i
            // Flatness tolerance (0-100)
            let tolerance = get_number(&operands, 0).unwrap_or(1.0);
            Operator::SetFlatness { tolerance }
        }
        "gs" => {
            // gs operator: name gs
            // Set extended graphics state from resource dictionary
            let dict_name = get_name(&operands, 0).unwrap_or("").to_string();
            Operator::SetExtGState { dict_name }
        }
        "sh" => {
            // sh operator: name sh
            // Paint shading pattern (gradient)
            let name = get_name(&operands, 0).unwrap_or("").to_string();
            Operator::PaintShading { name }
        }

        // Marked content operators (for tagged PDF structure)
        // PDF Spec: ISO 32000-1:2008, Section 14.6
        "BMC" => {
            // Begin marked content: tag BMC
            let tag = get_name(&operands, 0).unwrap_or("").to_string();
            Operator::BeginMarkedContent { tag }
        }
        "BDC" => {
            // Begin marked content with properties: tag properties BDC
            // properties can be a dictionary or a name (reference to /Properties resource)
            let tag = get_name(&operands, 0).unwrap_or("").to_string();
            let properties = Box::new(operands.get(1).cloned().unwrap_or(Object::Null));
            Operator::BeginMarkedContentDict { tag, properties }
        }
        "EMC" => {
            // End marked content: EMC (no operands)
            Operator::EndMarkedContent
        }

        // Unknown operator — convert SmallVec to Vec for the boxed storage.
        // This path is rare (only for unrecognized operators), so the
        // conversion cost is negligible.
        _ => Operator::Other {
            name: name.to_string(),
            operands: Box::new(operands.into_vec()),
        },
    }
}

// Helper functions to extract operands

pub(super) fn get_number(operands: &[Object], index: usize) -> Option<f32> {
    operands.get(index).and_then(|obj| match obj {
        Object::Integer(i) => Some(*i as f32),
        Object::Real(r) => Some(*r as f32),
        _ => None,
    })
}

pub(super) fn get_integer(operands: &[Object], index: usize) -> Option<i64> {
    operands.get(index).and_then(|obj| obj.as_integer())
}

pub(super) fn get_string(operands: &[Object], index: usize) -> Option<Vec<u8>> {
    operands
        .get(index)
        .and_then(|obj| obj.as_string().map(|s| s.to_vec()))
}

pub(super) fn get_name(operands: &[Object], index: usize) -> Option<&str> {
    operands.get(index).and_then(|obj| obj.as_name())
}

pub(super) fn get_array(operands: &[Object], index: usize) -> Option<&Vec<Object>> {
    operands.get(index).and_then(|obj| obj.as_array())
}
