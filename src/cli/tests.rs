#[cfg(test)]
mod tests {
    use std::io;

    use super::{
        MAX_RECEIVE_FRAME_BYTES, ReceiveFrameReader, ShutdownReader, CliCommand, parse_command,
    };
    use codexshim::ReadScope;
    use serde_json::json;
    use tokio::io::{AsyncRead, AsyncReadExt, ReadBuf};

    fn parse(args: &[&str]) -> Result<CliCommand, String> {
        parse_command(args.iter().map(std::ffi::OsString::from))
    }

    #[test]
    fn read_scope_defaults_and_accepts_both_argument_forms() {
        assert_eq!(parse(&["serve"]), Ok(CliCommand::Serve(ReadScope::Normal)));
        assert_eq!(
            parse(&["serve", "--read-scope", "normal"]),
            Ok(CliCommand::Serve(ReadScope::Normal))
        );
        assert_eq!(
            parse(&["serve", "--read-scope", "unrestricted"]),
            Ok(CliCommand::Serve(ReadScope::Unrestricted))
        );
        assert_eq!(
            parse(&["doctor", "--read-scope=normal"]),
            Ok(CliCommand::Doctor(ReadScope::Normal))
        );
    }

    #[test]
    fn read_scope_rejects_incomplete_duplicate_and_unknown_arguments() {
        for args in [
            &["serve", "--read-scope"][..],
            &["serve", "--read-scope="][..],
            &["serve", "--read-scope", "unknown"][..],
            &[
                "serve",
                "--read-scope",
                "normal",
                "--read-scope=unrestricted",
            ][..],
            &["serve", "--unknown"][..],
            &["--version", "extra"][..],
        ] {
            assert!(parse(args).is_err(), "unexpectedly accepted {args:?}");
        }
    }

    #[test]
    fn parses_log_management_commands() {
        assert_eq!(parse(&["logs", "status"]), Ok(CliCommand::LogsStatus));
        assert_eq!(parse(&["logs", "purge"]), Ok(CliCommand::LogsPurge));
        assert!(parse(&["logs"]).is_err());
        assert!(parse(&["logs", "clear"]).is_err());
    }

    async fn read_frame(input: Vec<u8>) -> io::Result<Vec<u8>> {
        let mut reader = ReceiveFrameReader::new(std::io::Cursor::new(input));
        let mut output = Vec::new();
        reader.read_to_end(&mut output).await?;
        Ok(output)
    }

    #[tokio::test]
    async fn receive_frame_accepts_exact_limit_and_crlf() {
        let mut input = vec![b'x'; MAX_RECEIVE_FRAME_BYTES - 1];
        input.extend_from_slice(b"\r\n");

        let output = read_frame(input.clone()).await.expect("bounded frame");

        assert_eq!(output, input);
    }

    #[tokio::test]
    async fn receive_frame_rejects_one_byte_over_limit() {
        let mut input = vec![b'x'; MAX_RECEIVE_FRAME_BYTES + 1];
        input.push(b'\n');
        let mut reader = ReceiveFrameReader::new(std::io::Cursor::new(input));
        let mut output = Vec::new();

        let error = reader
            .read_to_end(&mut output)
            .await
            .expect_err("oversized frame");

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(output.len() <= MAX_RECEIVE_FRAME_BYTES);
    }

    #[tokio::test]
    async fn oversized_receive_frame_cancels_server_shutdown() {
        let mut input = vec![b'x'; MAX_RECEIVE_FRAME_BYTES + 1];
        input.push(b'\n');
        let shutdown = tokio_util::sync::CancellationToken::new();
        let mut reader = ShutdownReader {
            inner: ReceiveFrameReader::new(std::io::Cursor::new(input)),
            shutdown: shutdown.clone(),
            termination_reported: false,
        };
        let mut output = Vec::new();

        let error = reader
            .read_to_end(&mut output)
            .await
            .expect_err("oversized frame");

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(shutdown.is_cancelled());
        assert!(output.len() <= MAX_RECEIVE_FRAME_BYTES);
    }

    #[tokio::test]
    async fn receive_frame_resets_between_frames_in_one_underlying_read() {
        let input = b"first\nsecond\r\nthird\n".to_vec();
        let mut reader = ReceiveFrameReader::new(std::io::Cursor::new(input.clone()));
        let mut storage = vec![0_u8; input.len()];
        let mut buffer = ReadBuf::new(&mut storage);

        std::future::poll_fn(|context| {
            std::pin::Pin::new(&mut reader).poll_read(context, &mut buffer)
        })
        .await
        .expect("read frames");

        assert_eq!(buffer.filled(), input);
    }

    #[tokio::test]
    async fn receive_frame_accepts_maximum_escaped_stdin_request() {
        let request = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": "run_process",
                "arguments": {
                    "program": "cargo",
                    "stdin": "\u{1}".repeat(1_048_576),
                }
            }
        });
        let mut input = serde_json::to_vec(&request).expect("request JSON");
        assert!(input.len() <= MAX_RECEIVE_FRAME_BYTES);
        input.push(b'\n');

        let output = read_frame(input.clone()).await.expect("escaped stdin frame");

        assert_eq!(output, input);
    }

    #[tokio::test]
    async fn receive_frame_preserves_ordinary_cargo_arguments_and_environment() {
        let request = json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {
                "name": "run_process",
                "arguments": {
                    "program": "cargo",
                    "args": ["test", "--locked", "--all-targets", "--", "--nocapture"],
                    "env": {
                        "CARGO_TARGET_DIR": "target/frame-contract",
                        "RUST_BACKTRACE": "1",
                    },
                    "unset_env": ["RUSTFLAGS"],
                }
            }
        });
        let mut input = serde_json::to_vec(&request).expect("request JSON");
        input.push(b'\n');

        let output = read_frame(input.clone()).await.expect("ordinary Cargo frame");

        assert_eq!(output, input);
    }
}
