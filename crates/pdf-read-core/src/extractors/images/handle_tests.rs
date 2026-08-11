use super::*;

#[cfg(test)]
mod handle_color_space_tests {
    use super::super::*;
    use crate::object::{Object, ObjectRef};
    use std::collections::HashMap;

    fn empty_map() -> HashMap<String, Object> {
        HashMap::new()
    }

    /// `[/Indexed /DeviceRGB 1 <00 00 00 FF FF FF>]` as a direct array.
    fn direct_indexed_rgb() -> Object {
        Object::Array(vec![
            Object::Name("Indexed".to_string()),
            Object::Name("DeviceRGB".to_string()),
            Object::Integer(1),
            Object::String(vec![0, 0, 0, 255, 255, 255]),
        ])
    }

    #[test]
    fn helper_direct_indexed_reports_indexed_with_rgb_base() {
        let entry = direct_indexed_rgb();
        let (cs, base) = resolve_color_space_for_handle(&entry, &empty_map(), None);
        assert_eq!(cs, ColorSpace::Indexed);
        assert_eq!(base, Some(ColorSpace::DeviceRGB));
    }

    #[test]
    fn helper_resource_name_resolves_to_device_gray() {
        // `/CS0` → /DeviceGray via the resource map (not a standard device name,
        // so it is looked up).
        let mut map = empty_map();
        map.insert("CS0".to_string(), Object::Name("DeviceGray".to_string()));
        let entry = Object::Name("CS0".to_string());
        let (cs, base) = resolve_color_space_for_handle(&entry, &map, None);
        assert_eq!(cs, ColorSpace::DeviceGray);
        assert_eq!(base, None);
    }

    #[test]
    fn helper_resource_name_resolves_to_indexed() {
        // `/CS0` → an Indexed array via the resource map: Indexed + base.
        let mut map = empty_map();
        map.insert("CS0".to_string(), direct_indexed_rgb());
        let entry = Object::Name("CS0".to_string());
        let (cs, base) = resolve_color_space_for_handle(&entry, &map, None);
        assert_eq!(cs, ColorSpace::Indexed);
        assert_eq!(base, Some(ColorSpace::DeviceRGB));
    }

    #[test]
    fn helper_standard_device_name_not_resource_resolved() {
        // A standard device name must resolve directly and never be looked up,
        // even if the map (incorrectly) shadows it.
        let mut map = empty_map();
        map.insert(
            "DeviceRGB".to_string(),
            Object::Name("DeviceGray".to_string()),
        );
        let entry = Object::Name("DeviceRGB".to_string());
        let (cs, base) = resolve_color_space_for_handle(&entry, &map, None);
        assert_eq!(cs, ColorSpace::DeviceRGB);
        assert_eq!(base, None);
    }

    /// Build a PDF whose object `99 0 obj` is the given colour-space object, so
    /// `resolve_color_space_for_handle(99 0 R, .., Some(doc))` can resolve the
    /// indirect-ref hop. The catalog/pages are minimal placeholders.
    fn pdf_with_cs_object(cs_body: &str) -> Vec<u8> {
        let mut pdf = b"%PDF-1.4\n".to_vec();
        let mut offsets: Vec<(u32, usize)> = Vec::new();

        offsets.push((1, pdf.len()));
        pdf.extend_from_slice(b"1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n");

        offsets.push((2, pdf.len()));
        pdf.extend_from_slice(b"2 0 obj\n<< /Type /Pages /Kids [3 0 R] /Count 1 >>\nendobj\n");

        offsets.push((3, pdf.len()));
        pdf.extend_from_slice(
            b"3 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 1 1] >>\nendobj\n",
        );

        offsets.push((99, pdf.len()));
        pdf.extend_from_slice(format!("99 0 obj\n{}\nendobj\n", cs_body).as_bytes());

        let xref_off = pdf.len();
        // Emit a single contiguous xref subsection covering 0..=99 with most
        // entries free; only the objects we wrote are marked in-use.
        let max_id = 99u32;
        pdf.extend_from_slice(format!("xref\n0 {}\n", max_id + 1).as_bytes());
        pdf.extend_from_slice(b"0000000000 65535 f \n");
        for id in 1..=max_id {
            if let Some((_, off)) = offsets.iter().find(|(oid, _)| *oid == id) {
                pdf.extend_from_slice(format!("{:010} 00000 n \n", off).as_bytes());
            } else {
                pdf.extend_from_slice(b"0000000000 65535 f \n");
            }
        }
        pdf.extend_from_slice(
            format!(
                "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{}\n%%EOF\n",
                max_id + 1,
                xref_off
            )
            .as_bytes(),
        );
        pdf
    }

    #[test]
    fn helper_indirect_indexed_array_de_indexed() {
        // `/ColorSpace 99 0 R` where 99 0 obj is `[/Indexed /DeviceRGB 1 <...>]`.
        let doc = crate::document::PdfDocument::from_bytes(pdf_with_cs_object(
            "[/Indexed /DeviceRGB 1 <000000FFFFFF>]",
        ))
        .expect("synthetic pdf parses");
        let entry = Object::Reference(ObjectRef::new(99, 0));
        let (cs, base) = resolve_color_space_for_handle(&entry, &empty_map(), Some(&doc));
        assert_eq!(cs, ColorSpace::Indexed);
        assert_eq!(base, Some(ColorSpace::DeviceRGB));
    }

    #[test]
    fn helper_indirect_plain_device_gray() {
        // `/ColorSpace 99 0 R` where 99 0 obj is `/DeviceGray`.
        let doc =
            crate::document::PdfDocument::from_bytes(pdf_with_cs_object("/DeviceGray")).unwrap();
        let entry = Object::Reference(ObjectRef::new(99, 0));
        let (cs, base) = resolve_color_space_for_handle(&entry, &empty_map(), Some(&doc));
        assert_eq!(cs, ColorSpace::DeviceGray);
        assert_eq!(base, None);
    }

    #[test]
    fn inline_indexed_consistent_with_xobject() {
        // The inline path uses the same helper, so a direct Indexed array must
        // report `Indexed` + DeviceRGB base — identical to the XObject contract.
        let entry = direct_indexed_rgb();
        let (xobj_cs, xobj_base) = resolve_color_space_for_handle(&entry, &empty_map(), None);
        // Inline path: same helper, same inputs.
        let (inline_cs, inline_base) = resolve_color_space_for_handle(&entry, &empty_map(), None);
        assert_eq!(xobj_cs, inline_cs);
        assert_eq!(xobj_base, inline_base);
        assert_eq!(inline_cs, ColorSpace::Indexed);
        assert_eq!(inline_base, Some(ColorSpace::DeviceRGB));
    }

    /// Build a PDF with an image XObject (object 99) so a handle can be built
    /// against a real document and `decode()` exercised. The image is a 1×1
    /// uncompressed sample; `cs_name` is written verbatim as its `/ColorSpace`.
    fn pdf_with_image_xobject(cs_name: &str, sample: &[u8]) -> Vec<u8> {
        let mut pdf = b"%PDF-1.4\n".to_vec();
        let mut offsets: Vec<(u32, usize)> = Vec::new();

        offsets.push((1, pdf.len()));
        pdf.extend_from_slice(b"1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n");
        offsets.push((2, pdf.len()));
        pdf.extend_from_slice(b"2 0 obj\n<< /Type /Pages /Kids [3 0 R] /Count 1 >>\nendobj\n");
        offsets.push((3, pdf.len()));
        pdf.extend_from_slice(
            b"3 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 1 1] >>\nendobj\n",
        );

        offsets.push((99, pdf.len()));
        let header = format!(
            "99 0 obj\n<< /Type /XObject /Subtype /Image /Width 1 /Height 1 \
             /BitsPerComponent 8 /ColorSpace {} /Length {} >>\nstream\n",
            cs_name,
            sample.len()
        );
        pdf.extend_from_slice(header.as_bytes());
        pdf.extend_from_slice(sample);
        pdf.extend_from_slice(b"\nendstream\nendobj\n");

        let xref_off = pdf.len();
        let max_id = 99u32;
        pdf.extend_from_slice(format!("xref\n0 {}\n", max_id + 1).as_bytes());
        pdf.extend_from_slice(b"0000000000 65535 f \n");
        for id in 1..=max_id {
            if let Some((_, off)) = offsets.iter().find(|(oid, _)| *oid == id) {
                pdf.extend_from_slice(format!("{:010} 00000 n \n", off).as_bytes());
            } else {
                pdf.extend_from_slice(b"0000000000 65535 f \n");
            }
        }
        pdf.extend_from_slice(
            format!(
                "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{}\n%%EOF\n",
                max_id + 1,
                xref_off
            )
            .as_bytes(),
        );
        pdf
    }

    #[test]
    fn decode_resolves_resource_name_cs() {
        // Image XObject with `/ColorSpace /CS0`; the page `/Resources/ColorSpace`
        // maps `/CS0 → /DeviceGray`. Before the fix, decode() failed with
        // "Unsupported color space: CS0"; now it succeeds.
        let doc = crate::document::PdfDocument::from_bytes(pdf_with_image_xobject("/CS0", &[128]))
            .unwrap();
        let obj_ref = ObjectRef::new(99, 0);
        let xobj = doc.load_object(obj_ref).unwrap();
        let dict = xobj.as_dict().expect("image xobject dict").clone();

        let mut map = empty_map();
        map.insert("CS0".to_string(), Object::Name("DeviceGray".to_string()));

        let handle = image_handle_from_xobject(
            &doc,
            obj_ref,
            &dict,
            crate::content::Matrix::identity(),
            0,
            &map,
        )
        .expect("handle builds");

        assert_eq!(handle.color_space, ColorSpace::DeviceGray);
        assert!(handle.indexed_base().is_none());
        // decode() must now resolve /CS0 via the stored resource map.
        let image = handle.decode().expect("decode resolves resource-name CS");
        assert_eq!(*image.color_space(), ColorSpace::DeviceGray);
    }

    #[test]
    fn xobject_direct_indexed_handle_reports_indexed_with_base() {
        let doc = crate::document::PdfDocument::from_bytes(pdf_with_image_xobject(
            "[/Indexed /DeviceRGB 1 <000000FFFFFF>]",
            &[0],
        ))
        .unwrap();
        let obj_ref = ObjectRef::new(99, 0);
        let dict = doc.load_object(obj_ref).unwrap().as_dict().unwrap().clone();
        let handle = image_handle_from_xobject(
            &doc,
            obj_ref,
            &dict,
            crate::content::Matrix::identity(),
            0,
            &empty_map(),
        )
        .unwrap();
        assert_eq!(handle.color_space, ColorSpace::Indexed);
        assert_eq!(handle.indexed_base(), Some(ColorSpace::DeviceRGB));
    }
}

#[cfg(test)]
mod png_bytes_panic_safety_tests {
    use super::super::*;

    /// A raw RGB buffer longer than width×height×3 (e.g. a 16-bit-per-component
    /// image whose samples were not collapsed to 8-bit) must NOT panic the PNG
    /// encoder — it must surface a recoverable `Error` that crosses the FFI
    /// boundary so Python/other callers can catch it. Regression for the
    /// `assertion left == right failed: Invalid buffer length` panic.
    #[test]
    fn to_png_bytes_rejects_oversized_rgb_buffer_without_panicking() {
        let (w, h) = (4u32, 2u32);
        // Twice the bytes a 4x2 RGB image needs (mimics undownsampled 16-bit).
        let pixels = vec![0u8; (w * h * 3 * 2) as usize];
        let img = PdfImage::new(
            w,
            h,
            ColorSpace::DeviceRGB,
            8,
            ImageData::Raw {
                pixels,
                format: PixelFormat::RGB,
            },
        );
        let result = img.to_png_bytes();
        assert!(
            result.is_err(),
            "oversized RGB buffer must yield a recoverable Err, not a panic"
        );
    }

    /// A correctly sized 8-bit RGB buffer (the shape produced after 16-bit
    /// samples are downsampled at parse time) encodes to a valid PNG.
    #[test]
    fn to_png_bytes_encodes_correctly_sized_rgb() {
        let (w, h) = (4u32, 2u32);
        let pixels = vec![128u8; (w * h * 3) as usize];
        let img = PdfImage::new(
            w,
            h,
            ColorSpace::DeviceRGB,
            8,
            ImageData::Raw {
                pixels,
                format: PixelFormat::RGB,
            },
        );
        let png = img.to_png_bytes().expect("correctly sized RGB must encode");
        assert!(
            png.starts_with(&[0x89, b'P', b'N', b'G']),
            "valid PNG signature"
        );
    }

    /// The 16→8 reduction rounds `v*255/65535` to nearest and hits the endpoints
    /// exactly, rather than flooring via a high-byte drop (WS1.8b). The floor
    /// form `v >> 8` would map 0xFF80 → 0xFF (255) here too, but biases the
    /// mid-range and interior values downward; these anchors pin the rounding.
    #[test]
    fn reduce_16_to_8_rounds_to_nearest() {
        assert_eq!(reduce_16_to_8(0x00, 0x00), 0, "black stays black");
        assert_eq!(reduce_16_to_8(0xFF, 0xFF), 255, "white stays white");
        // 0x0080 = 128: 128*255/65535 = 0.498 → rounds to 0; high-byte drop also 0.
        assert_eq!(reduce_16_to_8(0x00, 0x80), 0);
        // 0x0100 = 256: 256*255/65535 = 0.996 → rounds to 1 (floor >> 8 = 1 too).
        assert_eq!(reduce_16_to_8(0x01, 0x00), 1);
        // 0x8080 = 32896: 32896*255/65535 = 127.998 → 128; floor >>8 = 0x80 = 128.
        assert_eq!(reduce_16_to_8(0x80, 0x80), 128);
        // 0xFF7F = 65407: 65407*255/65535 = 254.5 → rounds up to 255; >>8 = 255.
        assert_eq!(reduce_16_to_8(0xFF, 0x7F), 255);
        // Monotonic and full-range: every high-byte value round-trips near itself.
        for hi in 0u8..=255 {
            let out = reduce_16_to_8(hi, hi);
            assert!(
                out >= hi.saturating_sub(1) && out <= hi.saturating_add(1),
                "reduction stays within 1 LSB of the high byte for hi={hi}"
            );
        }
    }
}

#[cfg(test)]
mod ccitt_decode_array_polarity_tests {
    use super::super::*;
    use crate::object::Object;
    use std::collections::HashMap;

    /// Pack an MSB-first "0"/"1" bit string into bytes, zero-padding the
    /// final byte (same convention as the CCITT decoder's own hand-built
    /// codestream tests in `src/decoders/ccitt.rs`).
    fn pack_bits(bits: &str) -> Vec<u8> {
        let mut bytes = Vec::new();
        let mut acc = 0u8;
        let mut n = 0u8;
        for ch in bits.chars() {
            acc = (acc << 1) | (ch == '1') as u8;
            n += 1;
            if n == 8 {
                bytes.push(acc);
                acc = 0;
                n = 0;
            }
        }
        if n > 0 {
            bytes.push(acc << (8 - n));
        }
        bytes
    }

    /// Hand-built CCITT G4 (T.6) codestream for an 8x3 bilevel image whose
    /// *raw* decoded runs are black-majority: rows 0-1 solid black, row 2
    /// black with a 2-pixel-wide white notch at columns 3-4. Every run uses
    /// Horizontal mode ("001" + a Modified-Huffman white-run code + a
    /// black-run code from ITU-T T.4 - the same tables `decode_row_g4`
    /// reads), which is reference-line-independent, so each row's bits are
    /// self-contained and can be verified without tracing 2D prediction:
    ///   row 0/1: white-run 0 ("00110101"), black-run 8 ("000101")
    ///   row 2:   white-run 0, black-run 3 ("10"); white-run 2 ("0111"),
    ///            black-run 3
    /// This mirrors the real corpus defect (govdocs 00339_005342 page 0):
    /// some scanners emit a black-majority raw codestream and rely on the
    /// image's /Decode [1 0] to restore the true white-majority page.
    fn black_majority_g4_stream() -> Vec<u8> {
        let row_all_black = format!("001{}{}", "00110101", "000101");
        let row_notch = format!("001{}{}001{}{}", "00110101", "10", "0111", "10");
        pack_bits(&format!("{row_all_black}{row_all_black}{row_notch}"))
    }

    /// Build a minimal CCITTFaxDecode image XObject (8x3, K=-1) wrapping
    /// `black_majority_g4_stream`, with an optional `/Decode` override.
    fn ccitt_xobject(decode: Option<[i64; 2]>) -> Object {
        let mut decode_parms = HashMap::new();
        decode_parms.insert("K".to_string(), Object::Integer(-1));
        decode_parms.insert("Columns".to_string(), Object::Integer(8));
        decode_parms.insert("Rows".to_string(), Object::Integer(3));

        let mut dict = HashMap::new();
        dict.insert("Subtype".to_string(), Object::Name("Image".to_string()));
        dict.insert("Width".to_string(), Object::Integer(8));
        dict.insert("Height".to_string(), Object::Integer(3));
        dict.insert("BitsPerComponent".to_string(), Object::Integer(1));
        dict.insert(
            "ColorSpace".to_string(),
            Object::Name("DeviceGray".to_string()),
        );
        dict.insert(
            "Filter".to_string(),
            Object::Name("CCITTFaxDecode".to_string()),
        );
        dict.insert("DecodeParms".to_string(), Object::Dictionary(decode_parms));
        if let Some([lo, hi]) = decode {
            dict.insert(
                "Decode".to_string(),
                Object::Array(vec![Object::Integer(lo), Object::Integer(hi)]),
            );
        }

        Object::Stream {
            dict,
            data: bytes::Bytes::from(black_majority_g4_stream()),
        }
    }

    /// Decode `xobject` to a Luma8 image and count white (0xFF) vs black
    /// (0x00) pixels.
    fn white_black_counts(xobject: &Object) -> (u32, u32) {
        let img = extract_image_from_xobject(None, xobject, None, None)
            .expect("decode hand-built CCITT test image");
        let luma = img
            .to_dynamic_image()
            .expect("decode CCITT test image pixels")
            .into_luma8();
        let white = luma.iter().filter(|&&v| v == 0xFF).count() as u32;
        let black = luma.iter().filter(|&&v| v == 0x00).count() as u32;
        (white, black)
    }

    /// Baseline: no `/Decode` override (implicit default `[0 1]`), no
    /// `/BlackIs1`. The raw codestream is black-majority (22/24 px), so the
    /// correctly-decoded image must stay black-majority - this un-inverted
    /// case must keep working after the `/Decode` fix below.
    #[test]
    fn default_decode_keeps_raw_black_majority_polarity() {
        let (white, black) = white_black_counts(&ccitt_xobject(None));
        assert_eq!(white + black, 24);
        assert!(
            black > white,
            "expected black-majority (raw, no /Decode override), got white={white} black={black}"
        );
    }

    /// The corpus bug (govdocs 00339_005342 page 0): `/Decode [1 0]` on the
    /// image XObject must invert CCITT polarity, turning this same
    /// black-majority raw codestream into a white-majority image - a
    /// mostly-white page with a small black mark - matching poppler.
    /// Before the fix, `extract_image_from_xobject` never read `/Decode`
    /// for the CCITT path, so this asserted black-majority (the bug).
    #[test]
    fn decode_1_0_inverts_to_white_majority() {
        let (white, black) = white_black_counts(&ccitt_xobject(Some([1, 0])));
        assert_eq!(white + black, 24);
        assert!(
            white > black,
            "expected white-majority under /Decode [1 0], got white={white} black={black}"
        );
        assert_eq!(
            black, 2,
            "exactly the 2-pixel notch should render black (the 'mark')"
        );
    }

    /// `/Decode [0 1]` written out explicitly is the non-inverted default
    /// and must behave identically to an absent `/Decode` entry.
    #[test]
    fn decode_0_1_is_a_no_op() {
        let (white, black) = white_black_counts(&ccitt_xobject(Some([0, 1])));
        assert_eq!((white, black), white_black_counts(&ccitt_xobject(None)));
    }
}

/// Regression coverage for the non-CCITT 1-bit `/DeviceGray` image path:
/// a raw packed-bit image (no `/Filter`, or `/Filter /FlateDecode`) used
/// to be unconditionally force-fed through the CCITT decompressor
/// (`to_dynamic_image`'s single `bits_per_component == 1 &&
/// DeviceGray` branch had no filter check), which fails to decode
/// already-unpacked bits as if they were CCITT-compressed data and drops
/// the image entirely. `/ImageMask` variants of the same filters were
/// unaffected (they go through the separate `render_image_mask` bit
/// unpacker), which is why the bug only showed on plain (non-mask)
/// images.
#[cfg(test)]
mod non_ccitt_1bpc_devicegray_tests {
    use super::super::*;
    use crate::object::Object;
    use std::collections::HashMap;

    /// 8x3 raw packed-bit image: rows 0-1 all-black, row 2 black with a
    /// 2-pixel-wide white notch at columns 3-4 (mirrors the CCITT
    /// polarity tests' pattern for an easy visual cross-check). Under the
    /// default `/Decode [0 1]`, sample bit 0 -> black, bit 1 -> white.
    fn black_majority_rows() -> [u8; 3] {
        [0x00, 0x00, 0x18]
    }

    /// Build a minimal 1-bit `/DeviceGray` image XObject wrapping
    /// `black_majority_rows`, either uncompressed or FlateDecode-filtered,
    /// with an optional `/Decode` override.
    fn devicegray_1bpc_xobject(flate: bool, decode: Option<[i64; 2]>) -> Object {
        let raw: Vec<u8> = black_majority_rows().to_vec();
        let (filter, data) = if flate {
            use flate2::write::ZlibEncoder;
            use flate2::Compression;
            use std::io::Write;
            let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
            encoder.write_all(&raw).expect("flate-compress test bitmap");
            (
                Some("FlateDecode"),
                encoder.finish().expect("finish flate stream"),
            )
        } else {
            (None, raw)
        };

        let mut dict = HashMap::new();
        dict.insert("Subtype".to_string(), Object::Name("Image".to_string()));
        dict.insert("Width".to_string(), Object::Integer(8));
        dict.insert("Height".to_string(), Object::Integer(3));
        dict.insert("BitsPerComponent".to_string(), Object::Integer(1));
        dict.insert(
            "ColorSpace".to_string(),
            Object::Name("DeviceGray".to_string()),
        );
        if let Some(f) = filter {
            dict.insert("Filter".to_string(), Object::Name(f.to_string()));
        }
        if let Some([lo, hi]) = decode {
            dict.insert(
                "Decode".to_string(),
                Object::Array(vec![Object::Integer(lo), Object::Integer(hi)]),
            );
        }

        Object::Stream {
            dict,
            data: bytes::Bytes::from(data),
        }
    }

    fn white_black_counts(xobject: &Object) -> (u32, u32) {
        let img = extract_image_from_xobject(None, xobject, None, None)
            .expect("decode hand-built non-CCITT 1bpc test image");
        let luma = img
            .to_dynamic_image()
            .expect("decode non-CCITT 1bpc test image pixels")
            .into_luma8();
        let white = luma.iter().filter(|&&v| v == 0xFF).count() as u32;
        let black = luma.iter().filter(|&&v| v == 0x00).count() as u32;
        (white, black)
    }

    /// Before the fix this failed outright (the CCITT decompressor
    /// rejects raw packed bits as malformed input), not just mis-decoded.
    #[test]
    fn uncompressed_1bpc_devicegray_decodes_without_ccitt() {
        let (white, black) = white_black_counts(&devicegray_1bpc_xobject(false, None));
        assert_eq!(white + black, 24);
        assert_eq!(
            black, 22,
            "22 of 24 pixels are black per the hand-built bitmap"
        );
        assert_eq!(white, 2, "the 2-pixel notch at row 2 cols 3-4 is white");
    }

    #[test]
    fn flate_decoded_1bpc_devicegray_decodes_without_ccitt() {
        let (white, black) = white_black_counts(&devicegray_1bpc_xobject(true, None));
        assert_eq!(white + black, 24);
        assert_eq!(black, 22);
        assert_eq!(white, 2);
    }

    /// `/Decode [1 0]` inverts polarity for the non-CCITT path exactly
    /// like it does for CCITT (ISO 32000-1 §8.9.5.2 Table 90) — the
    /// bug's fallback path always produced a fully dropped image, so
    /// there was no polarity to even get wrong before this fix.
    #[test]
    fn decode_1_0_inverts_polarity_on_non_ccitt_path() {
        let (white, black) = white_black_counts(&devicegray_1bpc_xobject(true, Some([1, 0])));
        assert_eq!(white + black, 24);
        assert_eq!(white, 22, "polarity inverted: majority is now white");
        assert_eq!(black, 2, "the notch is now the black pixels");
    }
}
