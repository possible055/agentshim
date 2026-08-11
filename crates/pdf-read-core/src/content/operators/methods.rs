use super::*;

impl Operator {
    /// Returns `true` if this operator sets a colour or colour-space parameter.
    ///
    /// Used by the Type 3 `d1` glyph path: per ISO 32000-1:2008 §9.6.5.2, the
    /// glyph description of a `d1` glyph is a stencil that is painted with the
    /// current fill colour, and any colour operators inside it shall be
    /// ignored. The renderer skips these operators while a `d1` colour lock is
    /// in effect.
    pub fn is_color_setting(&self) -> bool {
        matches!(
            self,
            Operator::SetFillRgb { .. }
                | Operator::SetStrokeRgb { .. }
                | Operator::SetFillGray { .. }
                | Operator::SetStrokeGray { .. }
                | Operator::SetFillCmyk { .. }
                | Operator::SetStrokeCmyk { .. }
                | Operator::SetFillColorSpace { .. }
                | Operator::SetStrokeColorSpace { .. }
                | Operator::SetFillColor { .. }
                | Operator::SetStrokeColor { .. }
                | Operator::SetFillColorN { .. }
                | Operator::SetStrokeColorN { .. }
        )
    }

    /// Validate operand count and types according to PDF spec Table A.1.
    ///
    /// PDF Spec: ISO 32000-1:2008, Appendix A - Table A.1 - PDF content stream operators
    ///
    /// This method checks that operators have the correct number and types of operands.
    /// Only call this in strict mode for spec compliance validation.
    ///
    /// # Arguments
    ///
    /// * `operands` - The operands provided for this operator
    ///
    /// # Returns
    ///
    /// Ok(()) if operands are valid, Err with descriptive message if invalid
    ///
    /// # Example
    ///
    /// ```ignore
    /// use pdf_oxide::content::operators::Operator;
    /// use pdf_oxide::object::Object;
    ///
    /// let op = Operator::MoveTo { x: 10.0, y: 20.0 };
    /// let operands = vec![Object::Integer(10), Object::Integer(20)];
    /// assert!(op.validate_operands(&operands).is_ok());
    /// ```
    pub fn validate_operands_for_raw_operator(
        operator_name: &str,
        operands: &[Object],
    ) -> crate::error::Result<()> {
        use crate::error::Error;

        // Validate operand count according to PDF Spec Table A.1
        match operator_name {
            // Path construction operators - PDF Spec Section 8.5.2
            "m" => {
                // moveto: x y m
                if operands.len() != 2 {
                    return Err(Error::InvalidPdf(format!(
                        "Operator 'm' (moveto) requires 2 operands (x, y), got {}",
                        operands.len()
                    )));
                }
            }
            "l" => {
                // lineto: x y l
                if operands.len() != 2 {
                    return Err(Error::InvalidPdf(format!(
                        "Operator 'l' (lineto) requires 2 operands (x, y), got {}",
                        operands.len()
                    )));
                }
            }
            "c" => {
                // curveto: x1 y1 x2 y2 x3 y3 c
                if operands.len() != 6 {
                    return Err(Error::InvalidPdf(format!(
                        "Operator 'c' (curveto) requires 6 operands (x1, y1, x2, y2, x3, y3), got {}",
                        operands.len()
                    )));
                }
            }
            "v" => {
                // curveto (v variant): x2 y2 x3 y3 v
                if operands.len() != 4 {
                    return Err(Error::InvalidPdf(format!(
                        "Operator 'v' (curveto) requires 4 operands (x2, y2, x3, y3), got {}",
                        operands.len()
                    )));
                }
            }
            "y" => {
                // curveto (y variant): x1 y1 x3 y3 y
                if operands.len() != 4 {
                    return Err(Error::InvalidPdf(format!(
                        "Operator 'y' (curveto) requires 4 operands (x1, y1, x3, y3), got {}",
                        operands.len()
                    )));
                }
            }
            "h" => {
                // closepath: h (no operands)
                if !operands.is_empty() {
                    return Err(Error::InvalidPdf(format!(
                        "Operator 'h' (closepath) requires 0 operands, got {}",
                        operands.len()
                    )));
                }
            }
            "re" => {
                // rectangle: x y width height re
                if operands.len() != 4 {
                    return Err(Error::InvalidPdf(format!(
                        "Operator 're' (rectangle) requires 4 operands (x, y, width, height), got {}",
                        operands.len()
                    )));
                }
            }

            // Text positioning operators - PDF Spec Section 9.4.2
            "Td" => {
                // Move text position: tx ty Td
                if operands.len() != 2 {
                    return Err(Error::InvalidPdf(format!(
                        "Operator 'Td' requires 2 operands (tx, ty), got {}",
                        operands.len()
                    )));
                }
            }
            "TD" => {
                // Move text position and set leading: tx ty TD
                if operands.len() != 2 {
                    return Err(Error::InvalidPdf(format!(
                        "Operator 'TD' requires 2 operands (tx, ty), got {}",
                        operands.len()
                    )));
                }
            }
            "Tm" => {
                // Set text matrix: a b c d e f Tm
                if operands.len() != 6 {
                    return Err(Error::InvalidPdf(format!(
                        "Operator 'Tm' requires 6 operands (a, b, c, d, e, f), got {}",
                        operands.len()
                    )));
                }
            }
            "T*" => {
                // Move to next line: T* (no operands)
                if !operands.is_empty() {
                    return Err(Error::InvalidPdf(format!(
                        "Operator 'T*' requires 0 operands, got {}",
                        operands.len()
                    )));
                }
            }

            // Text showing operators - PDF Spec Section 9.4.3
            "Tj" => {
                // Show text: string Tj
                if operands.len() != 1 {
                    return Err(Error::InvalidPdf(format!(
                        "Operator 'Tj' requires 1 operand (string), got {}",
                        operands.len()
                    )));
                }
            }
            "TJ" => {
                // Show text with positioning: array TJ
                if operands.len() != 1 {
                    return Err(Error::InvalidPdf(format!(
                        "Operator 'TJ' requires 1 operand (array), got {}",
                        operands.len()
                    )));
                }
            }
            "'" => {
                // Move to next line and show text: string '
                if operands.len() != 1 {
                    return Err(Error::InvalidPdf(format!(
                        "Operator ''' requires 1 operand (string), got {}",
                        operands.len()
                    )));
                }
            }
            "\"" => {
                // Set spacing and show text: aw ac string "
                if operands.len() != 3 {
                    return Err(Error::InvalidPdf(format!(
                        "Operator '\"' requires 3 operands (word_space, char_space, string), got {}",
                        operands.len()
                    )));
                }
            }

            // Text state operators - PDF Spec Section 9.3
            "Tc" => {
                // Set character spacing: charSpace Tc
                if operands.len() != 1 {
                    return Err(Error::InvalidPdf(format!(
                        "Operator 'Tc' requires 1 operand (char_space), got {}",
                        operands.len()
                    )));
                }
            }
            "Tw" => {
                // Set word spacing: wordSpace Tw
                if operands.len() != 1 {
                    return Err(Error::InvalidPdf(format!(
                        "Operator 'Tw' requires 1 operand (word_space), got {}",
                        operands.len()
                    )));
                }
            }
            "Tz" => {
                // Set horizontal scaling: scale Tz
                if operands.len() != 1 {
                    return Err(Error::InvalidPdf(format!(
                        "Operator 'Tz' requires 1 operand (scale), got {}",
                        operands.len()
                    )));
                }
            }
            "TL" => {
                // Set text leading: leading TL
                if operands.len() != 1 {
                    return Err(Error::InvalidPdf(format!(
                        "Operator 'TL' requires 1 operand (leading), got {}",
                        operands.len()
                    )));
                }
            }
            "Tf" => {
                // Set font: font size Tf
                if operands.len() != 2 {
                    return Err(Error::InvalidPdf(format!(
                        "Operator 'Tf' requires 2 operands (font, size), got {}",
                        operands.len()
                    )));
                }
            }
            "Tr" => {
                // Set text rendering mode: render Tr
                if operands.len() != 1 {
                    return Err(Error::InvalidPdf(format!(
                        "Operator 'Tr' requires 1 operand (render), got {}",
                        operands.len()
                    )));
                }
            }
            "Ts" => {
                // Set text rise: rise Ts
                if operands.len() != 1 {
                    return Err(Error::InvalidPdf(format!(
                        "Operator 'Ts' requires 1 operand (rise), got {}",
                        operands.len()
                    )));
                }
            }

            // Graphics state operators
            "q" | "Q" => {
                // Save/restore graphics state: q, Q (no operands)
                if !operands.is_empty() {
                    return Err(Error::InvalidPdf(format!(
                        "Operator '{}' requires 0 operands, got {}",
                        operator_name,
                        operands.len()
                    )));
                }
            }
            "cm" => {
                // Modify CTM: a b c d e f cm
                if operands.len() != 6 {
                    return Err(Error::InvalidPdf(format!(
                        "Operator 'cm' requires 6 operands (a, b, c, d, e, f), got {}",
                        operands.len()
                    )));
                }
            }

            // Color operators - PDF Spec Section 8.6.8
            "rg" => {
                // Set RGB fill color: r g b rg
                if operands.len() != 3 {
                    return Err(Error::InvalidPdf(format!(
                        "Operator 'rg' requires 3 operands (r, g, b), got {}",
                        operands.len()
                    )));
                }
            }
            "RG" => {
                // Set RGB stroke color: r g b RG
                if operands.len() != 3 {
                    return Err(Error::InvalidPdf(format!(
                        "Operator 'RG' requires 3 operands (r, g, b), got {}",
                        operands.len()
                    )));
                }
            }
            "g" => {
                // Set gray fill color: gray g
                if operands.len() != 1 {
                    return Err(Error::InvalidPdf(format!(
                        "Operator 'g' requires 1 operand (gray), got {}",
                        operands.len()
                    )));
                }
            }
            "G" => {
                // Set gray stroke color: gray G
                if operands.len() != 1 {
                    return Err(Error::InvalidPdf(format!(
                        "Operator 'G' requires 1 operand (gray), got {}",
                        operands.len()
                    )));
                }
            }
            "k" => {
                // Set CMYK fill color: c m y k k
                if operands.len() != 4 {
                    return Err(Error::InvalidPdf(format!(
                        "Operator 'k' requires 4 operands (c, m, y, k), got {}",
                        operands.len()
                    )));
                }
            }
            "K" => {
                // Set CMYK stroke color: c m y k K
                if operands.len() != 4 {
                    return Err(Error::InvalidPdf(format!(
                        "Operator 'K' requires 4 operands (c, m, y, k), got {}",
                        operands.len()
                    )));
                }
            }

            // Text object operators - PDF Spec Section 9.4
            "BT" | "ET" => {
                // Begin/end text: BT, ET (no operands)
                if !operands.is_empty() {
                    return Err(Error::InvalidPdf(format!(
                        "Operator '{}' requires 0 operands, got {}",
                        operator_name,
                        operands.len()
                    )));
                }
            }

            // XObject operator - PDF Spec Section 8.8
            "Do" => {
                // Paint XObject: name Do
                if operands.len() != 1 {
                    return Err(Error::InvalidPdf(format!(
                        "Operator 'Do' requires 1 operand (name), got {}",
                        operands.len()
                    )));
                }
            }

            // Other operators we don't validate yet
            _ => {
                // No validation for unknown operators (lenient behavior)
                log::debug!(
                    "No operand validation for operator '{}' (not implemented yet)",
                    operator_name
                );
            }
        }

        Ok(())
    }
}
