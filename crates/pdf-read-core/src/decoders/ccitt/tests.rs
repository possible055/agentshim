use super::*;

#[test]
fn test_ccitt_decode_passthrough() {
    let decoder = CcittFaxDecoder;
    let ccitt_data = b"\x00\x01\x02\x03";
    assert_eq!(decoder.decode(ccitt_data).unwrap(), ccitt_data);
}

#[test]
fn test_ccitt_decoder_name() {
    assert_eq!(CcittFaxDecoder.name(), "CCITTFaxDecode");
}

#[test]
fn tables_are_prefix_free() {
    for table in [WHITE_CODES, BLACK_CODES] {
        for &(la, ca, _) in table {
            for &(lb, cb, _) in table {
                if lb > la && (cb >> (lb - la)) == ca {
                    panic!("non-prefix-free pair: ({la},{ca:b}) vs ({lb},{cb:b})");
                }
            }
        }
    }
}

fn p(params: CcittParams, bits: &str) -> Result<CcittDecoded> {
    // Pack an MSB-first bit string into bytes (zero-padded to a byte).
    let mut bytes = Vec::new();
    let mut acc = 0u8;
    let mut n = 0u8;
    for ch in bits.chars().filter(|c| *c == '0' || *c == '1') {
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
    decode(&bytes, &params)
}

#[test]
fn g4_all_white_row_v0() {
    // 8-wide, 1 row. V0 against the imaginary white ref → all-white row.
    let params = CcittParams {
        k: -1,
        columns: 8,
        rows: Some(1),
        ..Default::default()
    };
    let d = p(params, "1").unwrap();
    assert_eq!(d.rows_decoded, 1);
    assert_eq!(d.data, vec![0u8]); // all white
    assert!(!d.recovered_partial);
}

#[test]
fn g4_horizontal_white3_black2() {
    // 8-wide row: Horizontal (001) + white run 3 (1000) + black run 2 (11)
    // ⇒ transitions [3,5], leaving a0=5; a trailing V0 (1) extends the final
    // white run to the right edge, completing the row.
    let params = CcittParams {
        k: -1,
        columns: 8,
        rows: Some(1),
        ..Default::default()
    };
    let d = p(params, "001 1000 11 1").unwrap();
    // pixels: white[0..3) black[3..5) white[5..8) ⇒ 0b00011000 = 0x18
    assert_eq!(d.data, vec![0b0001_1000]);
}

#[test]
fn g4_negative_k_with_unknown_rows_grows_output_fallibly() {
    let params = CcittParams {
        k: -2,
        columns: 8,
        rows: None,
        ..Default::default()
    };
    let d = p(params, "001 1000 11 1 000000000001").unwrap();
    assert_eq!(d.rows_decoded, 1);
    assert_eq!(d.data, vec![0b0001_1000]);
    assert!(!d.recovered_partial);
}

#[test]
fn g4_encoded_byte_align() {
    // Two all-white rows, each coded as a single V0 (`1`). Row 0's 1-bit
    // code is byte-padded, so the stream is [0x80, 0x80]. WITH
    // /EncodedByteAlign both rows decode; WITHOUT it the fill zeros after
    // row 0 mis-read as an invalid mode and the 2nd row is unrecoverable —
    // exactly the real-world fax-scanner failure of issue #738.
    let aligned = CcittParams {
        k: -1,
        columns: 8,
        rows: Some(2),
        encoded_byte_align: true,
        ..Default::default()
    };
    let d = decode(&[0x80, 0x80], &aligned).unwrap();
    assert_eq!(d.rows_decoded, 2);
    assert_eq!(d.data, vec![0u8, 0u8]); // both rows white
    assert!(!d.recovered_partial);

    let unaligned = CcittParams {
        encoded_byte_align: false,
        ..aligned
    };
    let d2 = decode(&[0x80, 0x80], &unaligned).unwrap();
    // Without alignment the 2nd row can't be found → 1 row decoded, the rest
    // recovered (white-padded) — NOT a silently-blank full page.
    assert_eq!(d2.rows_decoded, 1);
    assert!(d2.recovered_partial);
}

#[test]
fn g4_zero_rows_errs_not_white() {
    // Garbage that cannot start a row → Err, NOT an all-white Ok buffer.
    let params = CcittParams {
        k: -1,
        columns: 8,
        rows: Some(4),
        ..Default::default()
    };
    // "000000000000000" is the EOL/EOFB region but with no valid first mode
    // before data ends → no rows.
    let d = p(params, "000000000001"); // single EOL = immediate EOFB
                                       // EOFB on the very first read → EndOfBlock with 0 rows → padded white.
                                       // That is a legitimately-blank scan, so it returns Ok(all white). A
                                       // truly undecodable stream errors instead:
    assert!(d.is_ok());
    let bad = CcittParams {
        k: -1,
        columns: 8,
        rows: Some(4),
        ..Default::default()
    };
    // 0b0000001 = Extension as the very first mode → Error, 0 rows → Err.
    assert!(p(bad, "0000001").is_err());
}

// -- Group 3 (T.4) --------------------------------------------------------

#[test]
fn g3_1d_all_white_row() {
    // 8-wide K=0 row: a single white run of 8 (MH code 10011).
    let params = CcittParams {
        k: 0,
        columns: 8,
        rows: Some(1),
        ..Default::default()
    };
    let d = p(params, "10011").unwrap();
    assert_eq!(d.rows_decoded, 1);
    assert_eq!(d.data, vec![0u8]); // all white
    assert!(!d.recovered_partial);
}

#[test]
fn g3_1d_white3_black2() {
    // 8-wide K=0 row: white run 3 (1000) + black run 2 (11) + white run 3
    // (1000) ⇒ transitions [3,5] ⇒ 0b0001_1000, identical to the G4 case.
    let params = CcittParams {
        k: 0,
        columns: 8,
        rows: Some(1),
        ..Default::default()
    };
    let d = p(params, "1000 11 1000").unwrap();
    assert_eq!(d.data, vec![0b0001_1000]);
}

#[test]
fn g3_1d_eol_delimited_rows() {
    // Two all-white rows, each preceded by an EOL (000000000001) as a real
    // T.4 stream emits. The EOLs must be swallowed, not mis-read as runs.
    let params = CcittParams {
        k: 0,
        columns: 8,
        rows: Some(2),
        end_of_line: true,
        ..Default::default()
    };
    let d = p(params, "000000000001 10011 000000000001 10011").unwrap();
    assert_eq!(d.rows_decoded, 2);
    assert_eq!(d.data, vec![0u8, 0u8]);
    assert!(!d.recovered_partial);
}

#[test]
fn g3_1d_rtc_terminates() {
    // One white row then a return-to-control (6 EOLs) ⇒ stop cleanly; the
    // declared 3rd/4th rows are white-padded, not reported as decoded.
    let params = CcittParams {
        k: 0,
        columns: 8,
        rows: Some(4),
        ..Default::default()
    };
    let rtc = "000000000001".repeat(6);
    let d = p(params, &format!("10011 {rtc}")).unwrap();
    assert_eq!(d.rows_decoded, 1);
    assert_eq!(d.data, vec![0u8; 4]); // padded to declared height
}

// Real libtiff output for one 128×64 bilevel image, encoded both ways. The
// G4 path is already trusted, so decoding the G3 (K=0) stream to the *same*
// bitmap proves the new Modified-Huffman path — this is the codestream shape
// behind the blank-page reports (K=0 / Group 3 1-D), which the in-house
// decoder previously rejected outright.
#[rustfmt::skip]
    const G3_1D_STREAM: &[u8] = &[
        0,17,192,240,103,0,24,3,193,104,0,77,120,3,192,172,0,77,92,1,224,176,0,38,165,0,120,19,128,9,168,176,7,
        129,184,0,154,132,128,60,20,192,4,212,60,1,224,202,0,38,162,137,1,224,160,0,77,69,18,3,193,64,0,154,138,
        36,7,130,128,1,53,20,72,15,5,0,2,106,40,144,30,10,0,4,212,81,32,60,20,0,9,168,162,64,120,40,0,19,81,68,
        128,240,80,0,38,162,137,1,224,160,0,77,69,18,3,193,64,0,154,138,36,7,130,128,1,53,20,72,15,5,0,2,106,40,
        144,30,10,0,4,212,81,64,60,54,0,9,168,162,74,5,138,0,252,0,77,69,18,160,199,221,15,14,7,160,1,53,20,72,
        227,29,142,99,129,232,0,77,69,18,29,138,224,126,0,38,162,137,9,10,12,112,61,0,9,168,162,65,45,14,135,135,
        3,208,0,154,138,36,20,117,8,120,112,61,0,9,168,162,65,97,112,31,128,9,168,162,64,196,1,240,0,154,138,36,
        25,224,15,64,2,106,40,144,108,176,102,0,19,81,68,128,188,1,96,0,154,138,36,25,80,11,32,2,106,40,144,102,
        64,20,0,9,168,162,64,209,0,112,0,38,162,137,3,84,1,32,0,154,138,36,26,80,10,64,2,106,40,144,106,64,50,0,
        9,168,162,65,173,0,172,0,38,160,120,49,0,168,0,38,160,120,103,128,218,0,19,80,60,54,64,54,0,9,168,30,10,
        32,53,128,4,212,15,3,16,26,128,2,106,7,134,92,6,144,0,154,129,225,155,0,212,0,38,160,120,52,192,52,0,9,
        168,30,13,112,25,128,2,106,7,134,156,6,80,0,154,129,225,171,0,92,0,77,64,240,215,128,110,0,38,160,120,54,
        192,104,0,19,80,60,54,224,8,0,19,80,60,21,96,23,0,19,80,60,21,224,28,0,77,64,240,101,128,224,2,106,7,130,
        156,4,0,19,80,60,13,224,80,1,53,3,192,158,8,0,77,64,240,88,134,0,38,160,120,21,198,0,38,160,120,45,64,
    ];
#[rustfmt::skip]
    const G4_STREAM: &[u8] = &[
        35,129,224,206,28,17,227,12,60,48,240,195,195,15,12,60,138,37,255,255,255,255,255,255,195,225,21,4,88,52,
        120,97,160,71,116,8,195,248,97,164,227,248,99,16,151,134,202,128,200,224,122,236,66,40,114,135,253,132,16,
        255,226,16,66,15,196,53,225,135,225,131,240,97,248,97,248,97,248,97,248,97,248,97,248,97,248,97,226,24,120,
        97,225,135,134,30,24,120,97,225,135,134,30,24,120,97,225,135,134,30,24,120,97,225,135,134,30,24,120,97,225,
        135,134,30,24,120,97,225,134,0,32,2,
    ];

#[test]
fn g3_1d_matches_g4_for_same_image() {
    let g3 = decode(
        G3_1D_STREAM,
        &CcittParams {
            k: 0,
            columns: 128,
            rows: Some(64),
            end_of_line: true,
            ..Default::default()
        },
    )
    .unwrap();
    let g4 = decode(
        G4_STREAM,
        &CcittParams {
            k: -1,
            columns: 128,
            rows: Some(64),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(g3.rows_decoded, 64);
    assert!(!g3.recovered_partial);
    assert_eq!(g3.data.len(), 16 * 64);
    // Both encode the identical bitmap; the trusted G4 path is the oracle.
    assert_eq!(g3.data, g4.data);
    // And it is not a degenerate all-white decode.
    assert!(g3.data.iter().any(|&b| b != 0));
}

#[test]
fn packed_output_buffer_rejects_size_overflow() {
    let result = packed_output_buffer(usize::MAX, Some(2));
    assert!(matches!(result, Err(Error::Decode(message)) if message.contains("size overflow")));
}
