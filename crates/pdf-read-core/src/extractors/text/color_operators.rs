use super::*;

impl<'doc> TextExtractor<'doc> {
    pub(super) fn execute_color_operator(&mut self, op: Operator) -> Result<()> {
        match op {
            Operator::SetFillRgb { r, g, b } => {
                // rg operator implicitly sets DeviceRGB — a process color.
                self.inside_excluded_ink = false;
                self.state_stack.current_mut().fill_color_rgb = (r, g, b);
            }
            Operator::SetStrokeRgb { r, g, b } => {
                self.state_stack.current_mut().stroke_color_rgb = (r, g, b);
            }
            Operator::SetFillGray { gray } => {
                // g operator implicitly sets DeviceGray — a process color,
                // so clear any active ink exclusion.
                self.inside_excluded_ink = false;
                self.state_stack.current_mut().fill_color_rgb = (gray, gray, gray);
            }
            Operator::SetStrokeGray { gray } => {
                self.state_stack.current_mut().stroke_color_rgb = (gray, gray, gray);
            }
            Operator::SetFillCmyk { c, m, y, k } => {
                // k operator implicitly sets DeviceCMYK — a process color.
                self.inside_excluded_ink = false;
                let state = self.state_stack.current_mut();
                state.fill_color_cmyk = Some((c, m, y, k));
                state.fill_color_rgb = cmyk_to_rgb(c, m, y, k);
            }
            Operator::SetStrokeCmyk { c, m, y, k } => {
                let state = self.state_stack.current_mut();
                state.stroke_color_cmyk = Some((c, m, y, k));
                state.stroke_color_rgb = cmyk_to_rgb(c, m, y, k);
            }

            // Color space operators
            Operator::SetFillColorSpace { name } => {
                // Check for excluded ink before mutating state (needs &self)
                let ink_excluded = self.is_excluded_ink_color_space(&name);
                self.inside_excluded_ink = ink_excluded;
                if ink_excluded {
                    log::debug!(
                        "Fill color space {:?} matches excluded ink, suppressing text",
                        name
                    );
                }

                let state = self.state_stack.current_mut();
                state.fill_color_space = name.clone();
                // Reset color when changing color space
                state.fill_color_rgb = (0.0, 0.0, 0.0);
                state.fill_color_cmyk = None;
            }
            Operator::SetStrokeColorSpace { name } => {
                let state = self.state_stack.current_mut();
                state.stroke_color_space = name.clone();
                // Reset color when changing color space
                state.stroke_color_rgb = (0.0, 0.0, 0.0);
                state.stroke_color_cmyk = None;
            }
            Operator::SetFillColor { components } => {
                // Set fill color using components in current fill color space
                let state = self.state_stack.current_mut();
                match state.fill_color_space.as_str() {
                    "DeviceGray" | "CalGray" if components.len() == 1 => {
                        let gray = components[0];
                        state.fill_color_rgb = (gray, gray, gray);
                    }
                    "DeviceRGB" | "CalRGB" if components.len() == 3 => {
                        state.fill_color_rgb = (components[0], components[1], components[2]);
                    }
                    "Lab" if components.len() == 3 => {
                        // CIE L*a*b* color space
                        // For now, treat as RGB (proper conversion requires whitepoint)
                        // L* is lightness (0-100), a* and b* are color opponents
                        // Simplified conversion: normalize and treat as RGB
                        let l = components[0] / 100.0;
                        state.fill_color_rgb = (l, l, l); // Simplified grayscale approximation
                        log::debug!(
                            "Lab color space simplified to grayscale (full conversion not yet implemented)"
                        );
                    }
                    "DeviceCMYK" if components.len() == 4 => {
                        state.fill_color_cmyk =
                            Some((components[0], components[1], components[2], components[3]));
                        state.fill_color_rgb =
                            cmyk_to_rgb(components[0], components[1], components[2], components[3]);
                    }
                    "ICCBased" => {
                        // ICC profile-based color space
                        // For now, assume RGB and use components directly
                        if components.len() == 3 {
                            state.fill_color_rgb = (components[0], components[1], components[2]);
                        } else if components.len() == 1 {
                            let gray = components[0];
                            state.fill_color_rgb = (gray, gray, gray);
                        } else if components.len() == 4 {
                            // Treat as CMYK
                            state.fill_color_cmyk =
                                Some((components[0], components[1], components[2], components[3]));
                            state.fill_color_rgb = cmyk_to_rgb(
                                components[0],
                                components[1],
                                components[2],
                                components[3],
                            );
                        }
                        log::debug!(
                            "ICCBased color space using simplified conversion (ICC profile not processed)"
                        );
                    }
                    "Separation" if components.len() == 1 => {
                        // Separation color space (spot color)
                        // Component is tint value (0.0 = no ink, 1.0 = full ink)
                        // For now, treat as grayscale
                        let tint = components[0];
                        let gray = 1.0 - tint; // Inverted (0 tint = white, 1 tint = black)
                        state.fill_color_rgb = (gray, gray, gray);
                        log::debug!("Separation color space simplified to grayscale");
                    }
                    "DeviceN" if !components.is_empty() => {
                        // DeviceN color space (multiple colorants)
                        // For now, use simplified conversion
                        if components.len() == 4 {
                            state.fill_color_cmyk =
                                Some((components[0], components[1], components[2], components[3]));
                            state.fill_color_rgb = cmyk_to_rgb(
                                components[0],
                                components[1],
                                components[2],
                                components[3],
                            );
                        } else {
                            // Use first component as grayscale
                            let gray = 1.0 - components[0];
                            state.fill_color_rgb = (gray, gray, gray);
                        }
                        log::debug!("DeviceN color space using simplified conversion");
                    }
                    _ => {
                        // Named color space reference (e.g. "Cs1") or unknown —
                        // fall back by component count to avoid warn spam.
                        match components.len() {
                            1 => {
                                let gray = components[0];
                                state.fill_color_rgb = (gray, gray, gray);
                            }
                            3 => {
                                state.fill_color_rgb =
                                    (components[0], components[1], components[2]);
                            }
                            4 => {
                                state.fill_color_cmyk = Some((
                                    components[0],
                                    components[1],
                                    components[2],
                                    components[3],
                                ));
                                state.fill_color_rgb = cmyk_to_rgb(
                                    components[0],
                                    components[1],
                                    components[2],
                                    components[3],
                                );
                            }
                            _ => {}
                        }
                        log::debug!(
                            "Unknown fill color space {:?} with {} components; \
                             applied component-count fallback",
                            state.fill_color_space,
                            components.len()
                        );
                    }
                }
            }
            Operator::SetStrokeColor { components } => {
                // Set stroke color using components in current stroke color space
                let state = self.state_stack.current_mut();
                match state.stroke_color_space.as_str() {
                    "DeviceGray" | "CalGray" if components.len() == 1 => {
                        let gray = components[0];
                        state.stroke_color_rgb = (gray, gray, gray);
                    }
                    "DeviceRGB" | "CalRGB" if components.len() == 3 => {
                        state.stroke_color_rgb = (components[0], components[1], components[2]);
                    }
                    "Lab" if components.len() == 3 => {
                        let l = components[0] / 100.0;
                        state.stroke_color_rgb = (l, l, l);
                        log::debug!("Lab stroke color space simplified to grayscale");
                    }
                    "DeviceCMYK" if components.len() == 4 => {
                        state.stroke_color_cmyk =
                            Some((components[0], components[1], components[2], components[3]));
                        state.stroke_color_rgb =
                            cmyk_to_rgb(components[0], components[1], components[2], components[3]);
                    }
                    "ICCBased" => {
                        if components.len() == 3 {
                            state.stroke_color_rgb = (components[0], components[1], components[2]);
                        } else if components.len() == 1 {
                            let gray = components[0];
                            state.stroke_color_rgb = (gray, gray, gray);
                        } else if components.len() == 4 {
                            state.stroke_color_cmyk =
                                Some((components[0], components[1], components[2], components[3]));
                            state.stroke_color_rgb = cmyk_to_rgb(
                                components[0],
                                components[1],
                                components[2],
                                components[3],
                            );
                        }
                        log::debug!("ICCBased stroke color using simplified conversion");
                    }
                    "Separation" if components.len() == 1 => {
                        let tint = components[0];
                        let gray = 1.0 - tint;
                        state.stroke_color_rgb = (gray, gray, gray);
                        log::debug!("Separation stroke color simplified to grayscale");
                    }
                    "DeviceN" if !components.is_empty() => {
                        if components.len() == 4 {
                            state.stroke_color_cmyk =
                                Some((components[0], components[1], components[2], components[3]));
                            state.stroke_color_rgb = cmyk_to_rgb(
                                components[0],
                                components[1],
                                components[2],
                                components[3],
                            );
                        } else {
                            let gray = 1.0 - components[0];
                            state.stroke_color_rgb = (gray, gray, gray);
                        }
                        log::debug!("DeviceN stroke color using simplified conversion");
                    }
                    _ => {
                        match components.len() {
                            1 => {
                                let gray = components[0];
                                state.stroke_color_rgb = (gray, gray, gray);
                            }
                            3 => {
                                state.stroke_color_rgb =
                                    (components[0], components[1], components[2]);
                            }
                            4 => {
                                state.stroke_color_cmyk = Some((
                                    components[0],
                                    components[1],
                                    components[2],
                                    components[3],
                                ));
                                state.stroke_color_rgb = cmyk_to_rgb(
                                    components[0],
                                    components[1],
                                    components[2],
                                    components[3],
                                );
                            }
                            _ => {}
                        }
                        log::debug!(
                            "Unknown stroke color space {:?} with {} components; \
                             applied component-count fallback",
                            state.stroke_color_space,
                            components.len()
                        );
                    }
                }
            }
            Operator::SetFillColorN { components, name } => {
                // Like SetFillColor, but also supports pattern color spaces
                if name.is_some() {
                    // Pattern color space - for now, just log and ignore
                    log::debug!("Pattern fill color not yet supported: {:?}", name);
                } else {
                    // Same logic as SetFillColor - supports all color spaces
                    let state = self.state_stack.current_mut();
                    match state.fill_color_space.as_str() {
                        "DeviceGray" | "CalGray" if components.len() == 1 => {
                            let gray = components[0];
                            state.fill_color_rgb = (gray, gray, gray);
                        }
                        "DeviceRGB" | "CalRGB" if components.len() == 3 => {
                            state.fill_color_rgb = (components[0], components[1], components[2]);
                        }
                        "Lab" if components.len() == 3 => {
                            let l = components[0] / 100.0;
                            state.fill_color_rgb = (l, l, l);
                        }
                        "DeviceCMYK" if components.len() == 4 => {
                            state.fill_color_cmyk =
                                Some((components[0], components[1], components[2], components[3]));
                            state.fill_color_rgb = cmyk_to_rgb(
                                components[0],
                                components[1],
                                components[2],
                                components[3],
                            );
                        }
                        "ICCBased" => {
                            if components.len() == 3 {
                                state.fill_color_rgb =
                                    (components[0], components[1], components[2]);
                            } else if components.len() == 1 {
                                let gray = components[0];
                                state.fill_color_rgb = (gray, gray, gray);
                            } else if components.len() == 4 {
                                state.fill_color_cmyk = Some((
                                    components[0],
                                    components[1],
                                    components[2],
                                    components[3],
                                ));
                                state.fill_color_rgb = cmyk_to_rgb(
                                    components[0],
                                    components[1],
                                    components[2],
                                    components[3],
                                );
                            }
                        }
                        "Separation" if components.len() == 1 => {
                            let tint = components[0];
                            let gray = 1.0 - tint;
                            state.fill_color_rgb = (gray, gray, gray);
                        }
                        "DeviceN" if !components.is_empty() => {
                            if components.len() == 4 {
                                state.fill_color_cmyk = Some((
                                    components[0],
                                    components[1],
                                    components[2],
                                    components[3],
                                ));
                                state.fill_color_rgb = cmyk_to_rgb(
                                    components[0],
                                    components[1],
                                    components[2],
                                    components[3],
                                );
                            } else {
                                let gray = 1.0 - components[0];
                                state.fill_color_rgb = (gray, gray, gray);
                            }
                        }
                        _ => {
                            match components.len() {
                                1 => {
                                    let gray = components[0];
                                    state.fill_color_rgb = (gray, gray, gray);
                                }
                                3 => {
                                    state.fill_color_rgb =
                                        (components[0], components[1], components[2]);
                                }
                                4 => {
                                    state.fill_color_cmyk = Some((
                                        components[0],
                                        components[1],
                                        components[2],
                                        components[3],
                                    ));
                                    state.fill_color_rgb = cmyk_to_rgb(
                                        components[0],
                                        components[1],
                                        components[2],
                                        components[3],
                                    );
                                }
                                _ => {}
                            }
                            log::debug!(
                                "Unknown fill color space {:?} with {} components; \
                                 applied component-count fallback",
                                state.fill_color_space,
                                components.len()
                            );
                        }
                    }
                }
            }
            Operator::SetStrokeColorN { components, name } => {
                // Like SetStrokeColor, but also supports pattern color spaces
                if name.is_some() {
                    // Pattern color space - for now, just log and ignore
                    log::debug!("Pattern stroke color not yet supported: {:?}", name);
                } else {
                    // Same logic as SetStrokeColor - supports all color spaces
                    let state = self.state_stack.current_mut();
                    match state.stroke_color_space.as_str() {
                        "DeviceGray" | "CalGray" if components.len() == 1 => {
                            let gray = components[0];
                            state.stroke_color_rgb = (gray, gray, gray);
                        }
                        "DeviceRGB" | "CalRGB" if components.len() == 3 => {
                            state.stroke_color_rgb = (components[0], components[1], components[2]);
                        }
                        "Lab" if components.len() == 3 => {
                            let l = components[0] / 100.0;
                            state.stroke_color_rgb = (l, l, l);
                        }
                        "DeviceCMYK" if components.len() == 4 => {
                            state.stroke_color_cmyk =
                                Some((components[0], components[1], components[2], components[3]));
                            state.stroke_color_rgb = cmyk_to_rgb(
                                components[0],
                                components[1],
                                components[2],
                                components[3],
                            );
                        }
                        "ICCBased" => {
                            if components.len() == 3 {
                                state.stroke_color_rgb =
                                    (components[0], components[1], components[2]);
                            } else if components.len() == 1 {
                                let gray = components[0];
                                state.stroke_color_rgb = (gray, gray, gray);
                            } else if components.len() == 4 {
                                state.stroke_color_cmyk = Some((
                                    components[0],
                                    components[1],
                                    components[2],
                                    components[3],
                                ));
                                state.stroke_color_rgb = cmyk_to_rgb(
                                    components[0],
                                    components[1],
                                    components[2],
                                    components[3],
                                );
                            }
                        }
                        "Separation" if components.len() == 1 => {
                            let tint = components[0];
                            let gray = 1.0 - tint;
                            state.stroke_color_rgb = (gray, gray, gray);
                        }
                        "DeviceN" if !components.is_empty() => {
                            if components.len() == 4 {
                                state.stroke_color_cmyk = Some((
                                    components[0],
                                    components[1],
                                    components[2],
                                    components[3],
                                ));
                                state.stroke_color_rgb = cmyk_to_rgb(
                                    components[0],
                                    components[1],
                                    components[2],
                                    components[3],
                                );
                            } else {
                                let gray = 1.0 - components[0];
                                state.stroke_color_rgb = (gray, gray, gray);
                            }
                        }
                        _ => {
                            match components.len() {
                                1 => {
                                    let gray = components[0];
                                    state.stroke_color_rgb = (gray, gray, gray);
                                }
                                3 => {
                                    state.stroke_color_rgb =
                                        (components[0], components[1], components[2]);
                                }
                                4 => {
                                    state.stroke_color_cmyk = Some((
                                        components[0],
                                        components[1],
                                        components[2],
                                        components[3],
                                    ));
                                    state.stroke_color_rgb = cmyk_to_rgb(
                                        components[0],
                                        components[1],
                                        components[2],
                                        components[3],
                                    );
                                }
                                _ => {}
                            }
                            log::debug!(
                                "Unknown stroke color space {:?} with {} components; \
                                 applied component-count fallback",
                                state.stroke_color_space,
                                components.len()
                            );
                        }
                    }
                }
            }

            // Line style operators
            _ => unreachable!("non-color operator delegated to color handler"),
        }
        Ok(())
    }
}
