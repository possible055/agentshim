#[cfg(test)]
mod tests {
    use super::{CliCommand, parse_command};
    use codexshim::ReadScope;

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
}
