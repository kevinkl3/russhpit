//! Command line parser.
//!
//! `parse` reads arguments from left to right.
//! The function loads the implicit config file before the first flag.
//! The function does not exit the process.

use std::path::{Path, PathBuf};

use crate::config::{BindFamily, Config, Diagnostic, LoadMode, LogLevel, MissingFile, load_path};

/// Where log lines go.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogSink {
    /// Standard output.
    Stdout,
    /// The system log.
    Syslog,
}

/// The result of one argument list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invocation {
    /// The chosen config.
    pub config: Config,
    /// The path a later reload reads.
    pub config_path: PathBuf,
    /// Where log lines go.
    pub log_sink: LogSink,
    /// The next step for the caller.
    pub action: Action,
    /// Warnings from config files that loaded.
    pub warnings: Vec<Diagnostic>,
    /// One error line when `action` is `Error`.
    pub error_text: String,
}

/// The next step for the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Run the tarpit.
    Run,
    /// Write the help text.
    Help,
    /// Write the version text.
    Version,
    /// Write `error_text` and stop.
    Error,
}

/// Read arguments from left to right.
///
/// `args` does not include the program name.
/// The function loads `implicit_path` before the first flag.
/// The function does not exit the process.
pub fn parse(args: &[String], defaults: &Config, implicit_path: &Path) -> Invocation {
    let mut invocation = Invocation {
        config: *defaults,
        config_path: implicit_path.to_path_buf(),
        log_sink: LogSink::Stdout,
        action: Action::Run,
        warnings: Vec::new(),
        error_text: String::new(),
    };

    match load_path(
        defaults,
        implicit_path,
        LoadMode::Startup,
        MissingFile::Ignore,
    ) {
        Ok(report) => {
            invocation.config = report.config;
            invocation.warnings = report.warnings;
        }
        Err(error) => {
            fail(&mut invocation, error.to_string());
            return invocation;
        }
    }

    let mut index = 0;
    while index < args.len() {
        match read_arg(&mut invocation, args, index) {
            Step::Continue(next) => index = next,
            Step::Finish => return invocation,
        }
    }
    invocation
}

/// The function returns the static help text.
///
/// The numbers come from the built-in defaults.
/// The path is `implicit_path`.
/// Parsed flags do not change this text.
pub fn help_text(implicit_path: &str) -> String {
    let defaults = Config::default();
    "\
Usage: russhpit [-46hsvV] [-d MS] [-f CONFIG] [-l LEN] [-m LIMIT] [-p PORT]
  -4        Bind to IPv4 only
  -6        Bind to IPv6 only
  -d INT    Message millisecond delay [{delay}]
  -f        Set and load config file [{path}]
  -h        Print this help message and exit
  -l INT    Maximum banner line length (3-255) [{len}]
  -m INT    Maximum number of clients [{clients}]
  -p INT    Listening port [{port}]
  -s        Print diagnostics to syslog instead of standard output
  -v        Print diagnostics (repeatable)
  -V        Print version information and exit
"
    .replace("{delay}", &defaults.delay_ms.to_string())
    .replace("{len}", &defaults.max_line_length.to_string())
    .replace("{clients}", &defaults.max_clients.to_string())
    .replace("{port}", &defaults.port.to_string())
    .replace("{path}", implicit_path)
}

/// The function returns `russhpit` and the package version.
pub fn version_text() -> String {
    format!("russhpit {}\n", env!("CARGO_PKG_VERSION"))
}

enum Step {
    Continue(usize),
    Finish,
}

fn read_arg(invocation: &mut Invocation, args: &[String], index: usize) -> Step {
    let arg = &args[index];
    if arg == "--" && index + 1 < args.len() {
        fail(invocation, "russhpit: too many arguments".to_string());
        return Step::Finish;
    }
    if arg == "--" {
        return Step::Continue(args.len());
    }
    let Some(body) = arg.strip_prefix('-') else {
        fail(invocation, "russhpit: too many arguments".to_string());
        return Step::Finish;
    };
    if body.is_empty() {
        fail(invocation, "russhpit: too many arguments".to_string());
        return Step::Finish;
    }

    let chars: Vec<char> = body.chars().collect();
    let mut cursor = 0;
    while cursor < chars.len() {
        let option = chars[cursor];
        match option {
            '4' => invocation.config.bind_family = BindFamily::V4,
            '6' => invocation.config.bind_family = BindFamily::V6,
            's' => invocation.log_sink = LogSink::Syslog,
            'v' => raise_log_level(&mut invocation.config),
            'h' => {
                invocation.action = Action::Help;
                return Step::Finish;
            }
            'V' => {
                invocation.action = Action::Version;
                return Step::Finish;
            }
            'd' | 'l' | 'm' | 'p' | 'f' => {
                return read_value(invocation, args, index, option, &chars[cursor + 1..]);
            }
            _ => {
                fail(invocation, format!("russhpit: unknown option: -{option}"));
                return Step::Finish;
            }
        }
        cursor += 1;
    }
    Step::Continue(index + 1)
}

fn read_value(
    invocation: &mut Invocation,
    args: &[String],
    index: usize,
    option: char,
    attached: &[char],
) -> Step {
    let (value, next) = if attached.is_empty() {
        let Some(value) = args.get(index + 1) else {
            fail(
                invocation,
                format!("russhpit: option requires a value: -{option}"),
            );
            return Step::Finish;
        };
        (value.clone(), index + 2)
    } else {
        (attached.iter().collect(), index + 1)
    };

    let applied = match option {
        'd' => invocation.config.apply_delay(&value),
        'l' => invocation.config.apply_max_line_length(&value),
        'm' => invocation.config.apply_max_clients(&value),
        'p' => invocation.config.apply_port(&value),
        'f' => load_explicit_file(invocation, &value),
        _ => unreachable!("caller passes an option that takes a value"),
    };
    if let Err(error) = applied {
        fail(invocation, error.to_string());
        return Step::Finish;
    }
    Step::Continue(next)
}

fn load_explicit_file(invocation: &mut Invocation, value: &str) -> Result<(), Diagnostic> {
    let path = PathBuf::from(value);
    invocation.config_path = path.clone();
    let report = load_path(
        &invocation.config,
        &path,
        LoadMode::Startup,
        MissingFile::Error,
    )?;
    invocation.config = report.config;
    invocation.warnings.extend(report.warnings);
    Ok(())
}

fn raise_log_level(config: &mut Config) {
    config.log_level = match config.log_level {
        LogLevel::Quiet => LogLevel::Standard,
        LogLevel::Standard | LogLevel::Debug => LogLevel::Debug,
    };
}

fn fail(invocation: &mut Invocation, text: String) {
    invocation.action = Action::Error;
    invocation.error_text = text;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let id = NEXT.fetch_add(1, Ordering::Relaxed);
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir()
                .join(format!("russhpit-cli-{}-{nanos}-{id}", std::process::id()));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn arg_list(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| (*item).to_string()).collect()
    }

    fn parse_args(items: &[&str], defaults: &Config, implicit: &Path) -> Invocation {
        parse(&arg_list(items), defaults, implicit)
    }

    fn write_file(dir: &TempDir, name: &str, text: &str) -> PathBuf {
        let path = dir.0.join(name);
        fs::write(&path, text).unwrap();
        path
    }

    #[test]
    fn no_args_loads_implicit_file_and_runs() {
        let dir = TempDir::new();
        let implicit = write_file(&dir, "implicit.cfg", "Port 9\nNope 1\n");
        let invocation = parse_args(&[], &Config::default(), &implicit);
        assert_eq!(invocation.action, Action::Run);
        assert_eq!(invocation.config.port, 9);
        assert_eq!(invocation.config_path, implicit);
        assert_eq!(invocation.log_sink, LogSink::Stdout);
        assert!(invocation.error_text.is_empty());
        assert_eq!(
            invocation.warnings[0].as_str(),
            format!("{}:2: Unknown option 'Nope'", implicit.display())
        );
    }

    #[test]
    fn missing_implicit_file_runs_with_defaults() {
        let dir = TempDir::new();
        let implicit = dir.0.join("missing.cfg");
        let defaults = Config {
            port: 5,
            ..Config::default()
        };
        let invocation = parse_args(&[], &defaults, &implicit);
        assert_eq!(invocation.action, Action::Run);
        assert_eq!(invocation.config, defaults);
        assert!(invocation.warnings.is_empty());
        assert!(invocation.error_text.is_empty());
    }

    #[test]
    fn invalid_implicit_file_is_an_error() {
        let dir = TempDir::new();
        let implicit = write_file(&dir, "bad.cfg", "Port nope\n");
        let defaults = Config {
            port: 5,
            ..Config::default()
        };
        let invocation = parse_args(&["-p", "1"], &defaults, &implicit);
        assert_eq!(invocation.action, Action::Error);
        assert_eq!(invocation.error_text, "russhpit: Invalid port: nope");
        assert_eq!(invocation.config.port, 5);
    }

    #[test]
    fn flag_before_file_is_overlaid_by_file() {
        let dir = TempDir::new();
        let implicit = dir.0.join("missing.cfg");
        let file = write_file(&dir, "overlay.cfg", "Port 22\n");
        let invocation = parse_args(
            &["-p", "1", "-f", file.to_str().unwrap()],
            &Config::default(),
            &implicit,
        );
        assert_eq!(invocation.action, Action::Run);
        assert_eq!(invocation.config.port, 22);
        assert_eq!(invocation.config_path, file);
    }

    #[test]
    fn flag_after_file_overrides_file() {
        let dir = TempDir::new();
        let implicit = dir.0.join("missing.cfg");
        let file = write_file(&dir, "later.cfg", "Port 22\nDelay 50\n");
        let invocation = parse_args(
            &["-f", file.to_str().unwrap(), "-p", "1"],
            &Config::default(),
            &implicit,
        );
        assert_eq!(invocation.action, Action::Run);
        assert_eq!(invocation.config.port, 1);
        assert_eq!(invocation.config.delay_ms, 50);
    }

    #[test]
    fn second_file_overlays_the_first_without_resetting() {
        let dir = TempDir::new();
        let implicit = dir.0.join("missing.cfg");
        let first = write_file(&dir, "first.cfg", "Port 11\nDelay 12\n");
        let second = write_file(&dir, "second.cfg", "Delay 99\n");
        let invocation = parse_args(
            &[
                "-f",
                first.to_str().unwrap(),
                "-f",
                second.to_str().unwrap(),
            ],
            &Config::default(),
            &implicit,
        );
        assert_eq!(invocation.action, Action::Run);
        assert_eq!(invocation.config.port, 11);
        assert_eq!(invocation.config.delay_ms, 99);
        assert_eq!(invocation.config_path, second);
    }

    #[test]
    fn verbose_then_file_log_level_then_verbose() {
        let dir = TempDir::new();
        let implicit = dir.0.join("missing.cfg");
        let file = write_file(&dir, "level.cfg", "LogLevel 0\n");
        let invocation = parse_args(
            &["-v", "-f", file.to_str().unwrap(), "-v"],
            &Config::default(),
            &implicit,
        );
        assert_eq!(invocation.action, Action::Run);
        assert_eq!(invocation.config.log_level, LogLevel::Standard);
    }

    #[test]
    fn dash_s_survives_a_later_file() {
        let dir = TempDir::new();
        let implicit = dir.0.join("missing.cfg");
        let file = write_file(&dir, "sink.cfg", "Port 1\nLogLevel 1\n");
        let invocation = parse_args(
            &["-s", "-f", file.to_str().unwrap()],
            &Config::default(),
            &implicit,
        );
        assert_eq!(invocation.action, Action::Run);
        assert_eq!(invocation.log_sink, LogSink::Syslog);
        assert_eq!(invocation.config.port, 1);
        assert_eq!(invocation.config.log_level, LogLevel::Standard);
    }

    #[test]
    fn last_bind_family_flag_wins() {
        let dir = TempDir::new();
        let implicit = dir.0.join("missing.cfg");
        let v6 = parse_args(&["-4", "-6"], &Config::default(), &implicit);
        assert_eq!(v6.config.bind_family, BindFamily::V6);
        let v4 = parse_args(&["-6", "-4"], &Config::default(), &implicit);
        assert_eq!(v4.config.bind_family, BindFamily::V4);
    }

    #[test]
    fn clustered_and_attached_values() {
        let dir = TempDir::new();
        let implicit = dir.0.join("missing.cfg");
        let defaults = Config::default();

        let both = parse_args(&["-sv"], &defaults, &implicit);
        assert_eq!(both.log_sink, LogSink::Syslog);
        assert_eq!(both.config.log_level, LogLevel::Standard);

        let swapped = parse_args(&["-vs"], &defaults, &implicit);
        assert_eq!(swapped.log_sink, LogSink::Syslog);
        assert_eq!(swapped.config.log_level, LogLevel::Standard);

        let family = parse_args(&["-46"], &defaults, &implicit);
        assert_eq!(family.config.bind_family, BindFamily::V6);

        let debug = parse_args(&["-vvv"], &defaults, &implicit);
        assert_eq!(debug.config.log_level, LogLevel::Debug);

        let help = parse_args(&["-vh", "-p", "nope"], &defaults, &implicit);
        assert_eq!(help.action, Action::Help);
        assert_eq!(help.config.log_level, LogLevel::Standard);
        assert!(help.error_text.is_empty());

        let attached = parse_args(&["-p2222", "-d10"], &defaults, &implicit);
        assert_eq!(attached.action, Action::Run);
        assert_eq!(attached.config.port, 2222);
        assert_eq!(attached.config.delay_ms, 10);

        let separate = parse_args(&["-p", "2222"], &defaults, &implicit);
        assert_eq!(separate.config.port, 2222);

        let bad_cluster = parse_args(&["-ph"], &defaults, &implicit);
        assert_eq!(bad_cluster.action, Action::Error);
        assert_eq!(bad_cluster.error_text, "russhpit: Invalid port: h");

        let file = write_file(&dir, "attached.cfg", "Port 3\n");
        let flag = format!("-f{}", file.display());
        let from_file = parse_args(&[&flag], &defaults, &implicit);
        assert_eq!(from_file.action, Action::Run);
        assert_eq!(from_file.config.port, 3);
        assert_eq!(from_file.config_path, file);
    }

    #[test]
    fn help_stops_before_a_later_bad_flag() {
        let dir = TempDir::new();
        let implicit = dir.0.join("missing.cfg");
        let invocation = parse_args(&["-h", "-p", "nope"], &Config::default(), &implicit);
        assert_eq!(invocation.action, Action::Help);
        assert!(invocation.error_text.is_empty());
        assert_eq!(invocation.config.port, Config::default().port);
    }

    #[test]
    fn bad_flag_before_help_is_an_error() {
        let dir = TempDir::new();
        let implicit = dir.0.join("missing.cfg");
        let invocation = parse_args(&["-p", "nope", "-h"], &Config::default(), &implicit);
        assert_eq!(invocation.action, Action::Error);
        assert_eq!(invocation.error_text, "russhpit: Invalid port: nope");
    }

    #[test]
    fn version_text_uses_package_version() {
        assert_eq!(version_text(), "russhpit 0.1.0\n");
        let dir = TempDir::new();
        let implicit = dir.0.join("missing.cfg");
        let invocation = parse_args(&["-V", "-p", "nope"], &Config::default(), &implicit);
        assert_eq!(invocation.action, Action::Version);
        assert!(invocation.error_text.is_empty());
    }

    #[test]
    fn help_text_matches_the_contract_and_has_no_c_flag() {
        let text = help_text("/etc/russhpit/config");
        assert_eq!(
            text,
            "\
Usage: russhpit [-46hsvV] [-d MS] [-f CONFIG] [-l LEN] [-m LIMIT] [-p PORT]
  -4        Bind to IPv4 only
  -6        Bind to IPv6 only
  -d INT    Message millisecond delay [10000]
  -f        Set and load config file [/etc/russhpit/config]
  -h        Print this help message and exit
  -l INT    Maximum banner line length (3-255) [32]
  -m INT    Maximum number of clients [4096]
  -p INT    Listening port [2222]
  -s        Print diagnostics to syslog instead of standard output
  -v        Print diagnostics (repeatable)
  -V        Print version information and exit
"
        );
        assert!(text.contains("-s"));
        assert!(text.contains("-V"));
        assert!(!text.contains("-c"));
    }

    #[test]
    fn help_text_preserves_braces_in_implicit_path() {
        let text = help_text("/tmp/{port}/config");
        assert!(text.contains("[/tmp/{port}/config]"));
    }

    #[test]
    fn unknown_flag_extra_operand_and_missing_value() {
        let dir = TempDir::new();
        let implicit = dir.0.join("missing.cfg");
        let defaults = Config::default();

        let unknown = parse_args(&["-x"], &defaults, &implicit);
        assert_eq!(unknown.action, Action::Error);
        assert_eq!(unknown.error_text, "russhpit: unknown option: -x");

        let clustered = parse_args(&["-sx"], &defaults, &implicit);
        assert_eq!(clustered.error_text, "russhpit: unknown option: -x");

        let operand = parse_args(&["-p", "1", "foo"], &defaults, &implicit);
        assert_eq!(operand.error_text, "russhpit: too many arguments");

        let bare = parse_args(&["foo"], &defaults, &implicit);
        assert_eq!(bare.error_text, "russhpit: too many arguments");

        let end = parse_args(&["--", "foo"], &defaults, &implicit);
        assert_eq!(end.error_text, "russhpit: too many arguments");

        let only_end = parse_args(&["--"], &defaults, &implicit);
        assert_eq!(only_end.action, Action::Run);
        assert!(only_end.error_text.is_empty());

        let missing = parse_args(&["-p"], &defaults, &implicit);
        assert_eq!(missing.error_text, "russhpit: option requires a value: -p");

        let missing_file = parse_args(&["-f"], &defaults, &implicit);
        assert_eq!(
            missing_file.error_text,
            "russhpit: option requires a value: -f"
        );
    }

    #[test]
    fn explicit_missing_file_is_an_error() {
        let dir = TempDir::new();
        let implicit = dir.0.join("missing-implicit.cfg");
        let missing = dir.0.join("missing-explicit.cfg");
        let invocation = parse_args(
            &["-f", missing.to_str().unwrap()],
            &Config::default(),
            &implicit,
        );
        let io_error = fs::read_to_string(&missing).unwrap_err();
        assert_eq!(invocation.action, Action::Error);
        assert_eq!(
            invocation.error_text,
            format!(
                "russhpit: cannot open config {}: {io_error}",
                missing.display()
            )
        );
        assert_eq!(invocation.config, Config::default());
    }
}
