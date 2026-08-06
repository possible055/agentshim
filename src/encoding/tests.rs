#[cfg(test)]
mod tests {
    use std::io::{self, Read};

    use tokio_util::sync::CancellationToken;

    use super::{DecodeError, SourceEncoding, decode_to_string};

    struct ByteReader {
        bytes: Vec<u8>,
        offset: usize,
    }

    impl Read for ByteReader {
        fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
            let Some(byte) = self.bytes.get(self.offset) else {
                return Ok(0);
            };
            output[0] = *byte;
            self.offset += 1;
            Ok(1)
        }
    }

    fn utf16(text: &str, little_endian: bool) -> Vec<u8> {
        let mut bytes = if little_endian {
            vec![0xFF, 0xFE]
        } else {
            vec![0xFE, 0xFF]
        };
        for unit in text.encode_utf16() {
            bytes.extend(if little_endian {
                unit.to_le_bytes()
            } else {
                unit.to_be_bytes()
            });
        }
        bytes
    }

    #[test]
    fn strict_streaming_handles_split_utf8_and_utf16() {
        let cancellation = CancellationToken::new();
        let utf8 = ByteReader {
            bytes: "alpha界".as_bytes().to_vec(),
            offset: 0,
        };
        let (text, summary) =
            decode_to_string(utf8, None, 100, &cancellation).expect("decode UTF-8");
        assert_eq!(text, "alpha界");
        assert_eq!(summary.source_encoding, SourceEncoding::Utf8);

        for little_endian in [true, false] {
            let reader = ByteReader {
                bytes: utf16("alpha\r\nbeta", little_endian),
                offset: 0,
            };
            let (text, _) = decode_to_string(reader, Some("windows-1252"), 100, &cancellation)
                .expect("BOM takes priority");
            assert_eq!(text, "alpha\r\nbeta");
        }
    }

    #[test]
    fn malformed_binary_unknown_and_oversized_content_fail() {
        let cancellation = CancellationToken::new();
        assert!(matches!(
            decode_to_string(&[0xFF_u8][..], None, 100, &cancellation),
            Err(DecodeError::Malformed("UTF-8"))
        ));
        assert!(matches!(
            decode_to_string(&b"a\0b"[..], None, 100, &cancellation),
            Err(DecodeError::Binary)
        ));
        assert!(matches!(
            decode_to_string(&b"text"[..], Some("not-an-encoding"), 100, &cancellation),
            Err(DecodeError::UnknownEncoding(_))
        ));
        assert!(matches!(
            decode_to_string(&b"too large"[..], None, 3, &cancellation),
            Err(DecodeError::TooLarge)
        ));
    }

    #[test]
    fn cancellation_stops_before_reading() {
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        assert!(matches!(
            decode_to_string(&b"text"[..], None, 100, &cancellation),
            Err(DecodeError::Cancelled)
        ));
    }
}
