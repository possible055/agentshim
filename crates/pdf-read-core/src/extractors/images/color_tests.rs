use super::*;

#[cfg(test)]
mod inline_image_dict_tests {
    use super::super::*;
    use crate::object::Object;
    use std::collections::HashMap;

    fn dict(pairs: &[(&str, Object)]) -> HashMap<String, Object> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect()
    }

    /// The decoder requires `/Subtype`, and an inline image never carries one
    /// (§8.9.7 - it is implied by `BI`). Without this every inline image was
    /// rejected with "XObject missing /Subtype" and silently dropped.
    #[test]
    fn supplies_the_implied_image_subtype() {
        let out = expand_inline_image_dict(dict(&[
            ("W", Object::Integer(26)),
            ("H", Object::Integer(1)),
        ]));
        assert_eq!(out.get("Subtype"), Some(&Object::Name("Image".to_string())));
        assert_eq!(out.get("Width"), Some(&Object::Integer(26)));
        assert_eq!(out.get("Height"), Some(&Object::Integer(1)));
    }

    /// §8.9.7 Table 92: inline images abbreviate the VALUES too. Expanding only
    /// the keys left the decoder looking at a colour space named "RGB".
    #[test]
    fn expands_abbreviated_colour_space_and_filter_values() {
        let out = expand_inline_image_dict(dict(&[
            ("CS", Object::Name("RGB".to_string())),
            ("F", Object::Name("Fl".to_string())),
        ]));
        assert_eq!(
            out.get("ColorSpace"),
            Some(&Object::Name("DeviceRGB".to_string()))
        );
        assert_eq!(
            out.get("Filter"),
            Some(&Object::Name("FlateDecode".to_string()))
        );
    }

    #[test]
    fn expands_every_table_92_abbreviation() {
        for (abbr, full) in [
            ("G", "DeviceGray"),
            ("RGB", "DeviceRGB"),
            ("CMYK", "DeviceCMYK"),
            ("I", "Indexed"),
        ] {
            let out = expand_inline_image_dict(dict(&[("CS", Object::Name(abbr.to_string()))]));
            assert_eq!(
                out.get("ColorSpace"),
                Some(&Object::Name(full.to_string())),
                "colour space /{abbr}"
            );
        }
        for (abbr, full) in [
            ("AHx", "ASCIIHexDecode"),
            ("A85", "ASCII85Decode"),
            ("LZW", "LZWDecode"),
            ("Fl", "FlateDecode"),
            ("RL", "RunLengthDecode"),
            ("CCF", "CCITTFaxDecode"),
            ("DCT", "DCTDecode"),
        ] {
            let out = expand_inline_image_dict(dict(&[("F", Object::Name(abbr.to_string()))]));
            assert_eq!(
                out.get("Filter"),
                Some(&Object::Name(full.to_string())),
                "filter /{abbr}"
            );
        }
    }

    /// A filter CHAIN, and an indexed space whose base name is itself abbreviated.
    #[test]
    fn expands_abbreviations_inside_arrays() {
        let out = expand_inline_image_dict(dict(&[
            (
                "F",
                Object::Array(vec![
                    Object::Name("A85".to_string()),
                    Object::Name("Fl".to_string()),
                ]),
            ),
            (
                "CS",
                Object::Array(vec![
                    Object::Name("I".to_string()),
                    Object::Name("RGB".to_string()),
                    Object::Integer(255),
                ]),
            ),
        ]));
        assert_eq!(
            out.get("Filter"),
            Some(&Object::Array(vec![
                Object::Name("ASCII85Decode".to_string()),
                Object::Name("FlateDecode".to_string()),
            ]))
        );
        assert_eq!(
            out.get("ColorSpace"),
            Some(&Object::Array(vec![
                Object::Name("Indexed".to_string()),
                Object::Name("DeviceRGB".to_string()),
                Object::Integer(255),
            ]))
        );
    }

    /// A name we do not recognise (a `/Resources /ColorSpace` entry like `/CS0`,
    /// or an already-full name) must pass through untouched.
    #[test]
    fn leaves_unabbreviated_and_named_spaces_alone() {
        let out = expand_inline_image_dict(dict(&[("CS", Object::Name("CS0".to_string()))]));
        assert_eq!(
            out.get("ColorSpace"),
            Some(&Object::Name("CS0".to_string()))
        );
        let out = expand_inline_image_dict(dict(&[("CS", Object::Name("DeviceGray".to_string()))]));
        assert_eq!(
            out.get("ColorSpace"),
            Some(&Object::Name("DeviceGray".to_string()))
        );
    }
}

#[cfg(test)]
mod indexed_tests {
    use super::super::*;

    #[test]
    fn expand_indexed_rgb_8bpc() {
        // 2x2 image, 4 palette entries, each RGB
        let palette = vec![
            0, 0, 0, // index 0 black
            255, 0, 0, // index 1 red
            0, 255, 0, // index 2 green
            0, 0, 255, // index 3 blue
        ];
        let raw = vec![0, 1, 2, 3];
        let out = expand_indexed_to_rgb(&raw, &palette, PixelFormat::RGB, 2, 2, 8).unwrap();
        assert_eq!(out, vec![0, 0, 0, 255, 0, 0, 0, 255, 0, 0, 0, 255]);
    }

    #[test]
    fn expand_indexed_gray_base_to_rgb() {
        // Base color space is Grayscale, palette is 1 byte per entry
        let palette = vec![10, 128, 255];
        let raw = vec![0, 1, 2];
        let out = expand_indexed_to_rgb(&raw, &palette, PixelFormat::Grayscale, 3, 1, 8).unwrap();
        assert_eq!(out, vec![10, 10, 10, 128, 128, 128, 255, 255, 255]);
    }

    #[test]
    fn expand_indexed_out_of_range_index() {
        // Palette only has 2 entries but raw has index 5 → zeroed
        let palette = vec![10, 20, 30, 40, 50, 60];
        let raw = vec![0, 5];
        let out = expand_indexed_to_rgb(&raw, &palette, PixelFormat::RGB, 2, 1, 8).unwrap();
        assert_eq!(out, vec![10, 20, 30, 0, 0, 0]);
    }

    #[test]
    fn resolve_indexed_palette_truncates_to_hival() {
        use crate::object::Object;
        // [/Indexed /DeviceRGB 1 <inline palette>] — hival = 1, so 2 entries * 3 = 6 bytes.
        // Provide an oversized 12-byte palette; the extra 6 bytes must be dropped so
        // that indices > hival cannot pick up stray lookup data.
        let cs = Object::Array(vec![
            Object::Name("Indexed".to_string()),
            Object::Name("DeviceRGB".to_string()),
            Object::Integer(1),
            Object::String(vec![
                10, 20, 30, // entry 0
                40, 50, 60, // entry 1
                70, 80, 90, // stray — beyond hival
                100, 110, 120,
            ]),
        ]);
        let ir = resolve_indexed_palette(None, &cs).unwrap().unwrap();
        assert_eq!(ir.base_fmt, PixelFormat::RGB);
        assert_eq!(ir.palette, vec![10, 20, 30, 40, 50, 60]);
        assert!(
            ir.base_profile.is_none(),
            "DeviceRGB base has no ICC profile"
        );
        let (fmt, palette) = (ir.base_fmt, ir.palette);

        // Index 2 (> hival) must now be treated as out-of-range → black pixel.
        let raw = vec![0, 1, 2];
        let out = expand_indexed_to_rgb(&raw, &palette, fmt, 3, 1, 8).unwrap();
        assert_eq!(out, vec![10, 20, 30, 40, 50, 60, 0, 0, 0]);
    }

    #[test]
    fn expand_indexed_cmyk_base_matches_cmyk_to_rgb() {
        // Palette has a single CMYK entry; expansion must match the shared helper.
        let palette = vec![64, 128, 192, 32];
        let raw = vec![0];
        let out = expand_indexed_to_rgb(&raw, &palette, PixelFormat::CMYK, 1, 1, 8).unwrap();
        let expected = cmyk_pixel_to_rgb(64, 128, 192, 32);
        assert_eq!(out, expected.to_vec());
    }

    #[test]
    fn cmyk_pixel_uses_process_inks_not_additive() {
        // DeviceCMYK images convert via the process-ink corners (matching the
        // text/vector paths), NOT the naive additive `1 - min(1, C+K)`. 100% K is
        // the K ink #231F20 (not #000000); process cyan is #00ADEF (not #00FFFF).
        assert_eq!(cmyk_pixel_to_rgb(0, 0, 0, 255), [0x23, 0x1F, 0x20]);
        assert_eq!(cmyk_pixel_to_rgb(255, 0, 0, 0), [0x00, 0xAD, 0xEF]);
        // No ink at all is the paper.
        assert_eq!(cmyk_pixel_to_rgb(0, 0, 0, 0), [255, 255, 255]);
    }

    #[test]
    fn expand_indexed_1bpc_with_row_padding() {
        // 2-entry palette, 5x2 image at 1 bpc. 5 bits → 1 byte per row (3 bits padding).
        // Row 0 indices: 0,1,0,1,0 → top nibble 01010xxx = 0x50
        // Row 1 indices: 1,1,0,0,1 → top nibble 11001xxx = 0xC8
        let palette = vec![10, 20, 30, 200, 210, 220];
        let raw = vec![0x50, 0xC8];
        let out = expand_indexed_to_rgb(&raw, &palette, PixelFormat::RGB, 5, 2, 1).unwrap();
        assert_eq!(
            out,
            vec![
                10, 20, 30, 200, 210, 220, 10, 20, 30, 200, 210, 220, 10, 20, 30, // row 0
                200, 210, 220, 200, 210, 220, 10, 20, 30, 10, 20, 30, 200, 210, 220, // row 1
            ]
        );
    }

    #[test]
    fn expand_indexed_2bpc_with_row_padding() {
        // 4-entry palette, 3x1 image at 2 bpc. 6 bits → 1 byte per row (2 bits padding).
        // indices 0,1,2 → 00 01 10 xx → 0x18
        let palette = vec![
            0, 0, 0, // 0
            10, 20, 30, // 1
            40, 50, 60, // 2
            70, 80, 90, // 3
        ];
        let raw = vec![0x18];
        let out = expand_indexed_to_rgb(&raw, &palette, PixelFormat::RGB, 3, 1, 2).unwrap();
        assert_eq!(out, vec![0, 0, 0, 10, 20, 30, 40, 50, 60]);
    }

    #[test]
    fn expand_indexed_4bpc_packs_two_per_byte() {
        // 4x1 image, 4bpc: 2 indices per byte, high nibble first
        let palette = vec![
            0, 0, 0, // 0
            10, 20, 30, // 1
            40, 50, 60, // 2
            70, 80, 90, // 3
        ];
        // indices: 0,1,2,3 → packed: 0x01, 0x23
        let raw = vec![0x01, 0x23];
        let out = expand_indexed_to_rgb(&raw, &palette, PixelFormat::RGB, 4, 1, 4).unwrap();
        assert_eq!(out, vec![0, 0, 0, 10, 20, 30, 40, 50, 60, 70, 80, 90]);
    }

    // ---- DoS / hardening guards for #324 ----

    #[test]
    fn expand_indexed_rejects_overflow_dimensions() {
        // Dimensions that overflow usize when computing w * h * 3. Previously
        // Vec::with_capacity(w*h*3) would panic or reserve absurd amounts.
        let palette = vec![0, 0, 0, 255, 0, 0];
        let raw = vec![0, 1];
        let huge = u32::MAX / 2;
        let result = expand_indexed_to_rgb(&raw, &palette, PixelFormat::RGB, huge, huge, 8);
        assert!(result.is_err(), "overflow dimensions must be rejected");
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("overflow") || err.contains("exceeds"),
            "expected overflow/limit error, got: {err}"
        );
    }

    #[test]
    fn expand_indexed_rejects_truncated_stream() {
        // 10x10 8bpc image requires 100 index bytes. Supplying 10 used to
        // silently zero-pad the remaining rows; now it's an error.
        let palette = vec![10, 20, 30, 40, 50, 60];
        let raw = vec![0; 10];
        let result = expand_indexed_to_rgb(&raw, &palette, PixelFormat::RGB, 10, 10, 8);
        assert!(result.is_err(), "truncated stream must be rejected");
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("truncated"),
            "expected truncated error, got: {err}"
        );
    }

    #[test]
    fn expand_indexed_rejects_output_over_cap() {
        // 12 000 × 12 000 × 3 = 432 MB > 256 MB guard. The MAX_INDEXED_OUTPUT_BYTES
        // check fires before we inspect `raw.len()`, so the test doesn't need to
        // allocate a 144 MB stream — an empty buffer is enough to prove the cap
        // rejects the request.
        let palette = vec![0, 0, 0];
        let raw: Vec<u8> = Vec::new();
        let result = expand_indexed_to_rgb(&raw, &palette, PixelFormat::RGB, 12_000, 12_000, 8);
        assert!(result.is_err(), "oversized output must be rejected");
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("guard limit") || err.contains("exceeds"),
            "expected output-size guard error, got: {err}"
        );
    }

    // ---- #338: bpc validation per ISO 32000-2 §8.9.5.1 ----

    #[test]
    fn expand_indexed_rejects_bpc_zero() {
        // bpc = 0 used to be coerced to 1 by `bpc.max(1)`, silently
        // accepting a malformed PDF. Now it must be rejected.
        let palette = vec![0, 0, 0, 255, 0, 0];
        let raw = vec![0xFF];
        let result = expand_indexed_to_rgb(&raw, &palette, PixelFormat::RGB, 1, 1, 0);
        assert!(result.is_err(), "bpc=0 must be rejected");
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("BitsPerComponent") || err.contains("bpc"),
            "expected bpc error, got: {err}"
        );
    }

    #[test]
    fn expand_indexed_rejects_unsupported_bpc() {
        // 3, 5, 6, 7, 9, 12, 16, … are all invalid for Indexed. Previously
        // the `_ => 0` arm in `read_index` silently mapped every pixel to
        // palette entry 0, returning a solid-color image. Now they're
        // rejected up front.
        let palette = vec![0, 0, 0, 255, 0, 0];
        let raw = vec![0xFF];
        for bpc in [3u8, 5, 6, 7, 9, 12, 16] {
            let result = expand_indexed_to_rgb(&raw, &palette, PixelFormat::RGB, 1, 1, bpc);
            assert!(result.is_err(), "bpc={bpc} must be rejected");
        }
    }

    #[test]
    fn expand_indexed_accepts_all_spec_bpc_values() {
        // Sanity: 1, 2, 4, 8 must still all work.
        let palette = vec![0, 0, 0, 255, 0, 0, 10, 20, 30, 40, 50, 60];
        let raw = vec![0xFF];
        for bpc in [1u8, 2, 4, 8] {
            let result = expand_indexed_to_rgb(&raw, &palette, PixelFormat::RGB, 1, 1, bpc);
            assert!(result.is_ok(), "bpc={bpc} must be accepted, got {result:?}");
        }
    }

    // Regression test for #336. Per ISO 32000-1 §8.6.6.3, the lookup element of
    // `[/Indexed base hival lookup]` must be either a byte string or a stream.
    // Historical behaviour when it was neither: `resolve_indexed_palette` returned
    // `Ok(None)` and `extract_image_from_xobject` silently fell back to treating
    // the raw 1-byte/pixel index stream as 3-byte/pixel RGB, producing the
    // misleading "Invalid RGB image dimensions" error. The fix returns an
    // explicit `Error::Image("Unable to resolve Indexed color space palette")`.
    #[test]
    fn resolve_indexed_palette_array_lookup_returns_none() {
        use crate::object::Object;
        let cs = Object::Array(vec![
            Object::Name("Indexed".to_string()),
            Object::Name("DeviceRGB".to_string()),
            Object::Integer(1),
            // Lookup as Array-of-Array (not String or Stream) — unresolvable.
            Object::Array(vec![
                Object::Array(vec![
                    Object::Integer(0),
                    Object::Integer(0),
                    Object::Integer(0),
                ]),
                Object::Array(vec![
                    Object::Integer(255),
                    Object::Integer(255),
                    Object::Integer(255),
                ]),
            ]),
        ]);
        assert!(resolve_indexed_palette(None, &cs).unwrap().is_none());
    }

    #[test]
    fn extract_image_errors_when_indexed_lookup_is_array() {
        use crate::object::Object;
        use std::collections::HashMap;

        let mut dict = HashMap::new();
        dict.insert("Subtype".to_string(), Object::Name("Image".to_string()));
        dict.insert("Width".to_string(), Object::Integer(2));
        dict.insert("Height".to_string(), Object::Integer(1));
        dict.insert("BitsPerComponent".to_string(), Object::Integer(8));
        dict.insert(
            "ColorSpace".to_string(),
            Object::Array(vec![
                Object::Name("Indexed".to_string()),
                Object::Name("DeviceRGB".to_string()),
                Object::Integer(1),
                Object::Array(vec![
                    Object::Integer(0),
                    Object::Integer(0),
                    Object::Integer(0),
                ]),
            ]),
        );
        let xobject = Object::Stream {
            dict,
            data: bytes::Bytes::from_static(&[0, 1]),
        };

        let err = extract_image_from_xobject(None, &xobject, None, None)
            .expect_err("Indexed with Array lookup must error");
        let msg = format!("{err:?}");
        assert!(
            msg.contains("Unable to resolve Indexed color space palette"),
            "error message should identify palette-resolution failure, got: {msg}"
        );
        assert!(
            !msg.contains("Invalid RGB image dimensions"),
            "must not fall through to misleading RGB-dimension error, got: {msg}"
        );
    }

    // #337 Lab→XYZ→sRGB conversion tests

    #[test]
    fn lab_pixel_mid_gray() {
        // Lab(50, 0, 0) = perceptual mid-gray → sRGB ~(119, 119, 119).
        // Byte encoding: L=128, a=128, b=128.
        let d65: [f64; 3] = [0.9505, 1.0, 1.0890];
        let [r, g, b] = super::lab_pixel_to_rgb(128, 128, 128, d65);
        for (label, v, expected) in [("R", r, 119), ("G", g, 119), ("B", b, 119)] {
            let diff = (v as i32 - expected).abs();
            assert!(
                diff <= 3,
                "Lab(50,0,0) {label}: expected ~{expected}, got {v} (Δ={diff})"
            );
        }
    }

    #[test]
    fn lab_pixel_white() {
        // Lab(100, 0, 0) = white → sRGB ~(255, 255, 255).
        // Byte encoding: L=255, a=128, b=128.
        let d65: [f64; 3] = [0.9505, 1.0, 1.0890];
        let [r, g, b] = super::lab_pixel_to_rgb(255, 128, 128, d65);
        for (label, v) in [("R", r), ("G", g), ("B", b)] {
            assert!(v >= 250, "Lab(100,0,0) {label}: expected ~255, got {v}");
        }
    }

    #[test]
    fn lab_pixel_black() {
        // Lab(0, 0, 0) = black → sRGB ~(0, 0, 0).
        // Byte encoding: L=0, a=128, b=128.
        let d65: [f64; 3] = [0.9505, 1.0, 1.0890];
        let [r, g, b] = super::lab_pixel_to_rgb(0, 128, 128, d65);
        for (label, v) in [("R", r), ("G", g), ("B", b)] {
            assert!(v <= 5, "Lab(0,0,0) {label}: expected ~0, got {v}");
        }
    }

    #[test]
    fn lab_pixel_red_tint() {
        // Lab(50, 80, 0) has a strong red-magenta tint.
        // Byte encoding: L=128, a=208 (128+80), b=128.
        let d65: [f64; 3] = [0.9505, 1.0, 1.0890];
        let [r, g, b] = super::lab_pixel_to_rgb(128, 208, 128, d65);
        assert!(r > g + 50, "Lab(50,80,0) should have R >> G: R={r}, G={g}");
        assert!(r > b, "Lab(50,80,0) should have R > B: R={r}, B={b}");
    }

    #[test]
    fn lab_palette_round_trip() {
        // 3-entry Lab palette → RGB palette should have 9 bytes.
        let d65: [f64; 3] = [0.9505, 1.0, 1.0890];
        let palette: Vec<u8> = vec![
            0, 128, 128, // black
            128, 128, 128, // mid-gray
            255, 128, 128, // white
        ];
        let rgb = super::lab_palette_to_rgb(&palette, d65);
        assert_eq!(rgb.len(), 9, "3 Lab entries → 9 RGB bytes");
        // Black entry: all near 0
        assert!(rgb[0] <= 5 && rgb[1] <= 5 && rgb[2] <= 5);
        // White entry: all near 255
        assert!(rgb[6] >= 250 && rgb[7] >= 250 && rgb[8] >= 250);
    }

    #[test]
    fn extract_lab_whitepoint_d65() {
        use crate::object::Object;
        let cs = Object::Array(vec![
            Object::Name("Lab".to_string()),
            Object::Dictionary({
                let mut d = std::collections::HashMap::new();
                d.insert(
                    "WhitePoint".to_string(),
                    Object::Array(vec![
                        Object::Real(0.9505),
                        Object::Real(1.0),
                        Object::Real(1.0890),
                    ]),
                );
                d
            }),
        ]);
        let wp = super::extract_lab_whitepoint(&cs);
        assert!((wp[0] - 0.9505).abs() < 1e-6);
        assert!((wp[1] - 1.0).abs() < 1e-6);
        assert!((wp[2] - 1.0890).abs() < 1e-6);
    }

    #[test]
    fn extract_lab_whitepoint_missing_falls_back_to_d65() {
        use crate::object::Object;
        let cs = Object::Name("Lab".to_string());
        let wp = super::extract_lab_whitepoint(&cs);
        assert!((wp[0] - 0.9505).abs() < 1e-6);
    }
}
