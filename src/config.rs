//! Config file loader.
//!
//! `load_str` and `load_path` copy keys onto a new `Config` value.
//! A startup load rejects a bad file.
//! A reload keeps the previous value for a bad key.

use std::path::Path;

/// Port, delay, line length, client cap, address family, and log level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Config {
    pub port: u16,
    pub delay_ms: i32,
    pub max_line_length: u8,
    pub max_clients: i32,
    pub bind_family: BindFamily,
    pub log_level: LogLevel,
}

/// Address family for the listening socket.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BindFamily {
    /// IPv4 and IPv6 on one socket.
    Dual,
    /// IPv4 only.
    V4,
    /// IPv6 only.
    V6,
}

/// How many log lines the program writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevel {
    /// No log lines.
    Quiet,
    /// Normal log lines.
    Standard,
    /// Normal and debug log lines.
    Debug,
}

/// A bad value rejects the load, or the load keeps the previous value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoadMode {
    /// A bad value rejects the load.
    Startup,
    /// A bad value does not change that key.
    Reload,
}

/// Select the result for a missing file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MissingFile {
    /// A missing file is not an error.
    Ignore,
    /// A missing file is an error.
    Error,
}

/// Text for one config problem.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    text: String,
}

impl Diagnostic {
    /// The diagnostic text.
    pub fn as_str(&self) -> &str {
        &self.text
    }
}

impl std::fmt::Display for Diagnostic {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.text)
    }
}

/// A config value and the warnings from the load.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadReport {
    pub config: Config,
    pub warnings: Vec<Diagnostic>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            port: 2222,
            delay_ms: 10_000,
            max_line_length: 32,
            max_clients: 4096,
            bind_family: BindFamily::Dual,
            log_level: LogLevel::Quiet,
        }
    }
}

impl Config {
    /// The function returns five config lines.
    ///
    /// The lines are `Port`, `Delay`, `MaxLineLength`, `MaxClients`, and `BindFamily`.
    /// A line has no trailing newline.
    pub fn echo_lines(&self) -> [String; 5] {
        [
            format!("Port {}", self.port),
            format!("Delay {}", self.delay_ms),
            format!("MaxLineLength {}", self.max_line_length),
            format!("MaxClients {}", self.max_clients),
            format!("BindFamily {}", self.bind_family.echo_name()),
        ]
    }
}

impl BindFamily {
    fn echo_name(self) -> &'static str {
        match self {
            Self::Dual => "IPv4 Mapped IPv6",
            Self::V4 => "IPv4 Only",
            Self::V6 => "IPv6 Only",
        }
    }
}

/// The function returns the default config path for this operating system.
///
/// FreeBSD uses `/usr/local/etc/russhpit.config`.
/// Every other system uses `/etc/russhpit/config`.
pub fn default_config_path() -> &'static str {
    if cfg!(target_os = "freebsd") {
        "/usr/local/etc/russhpit.config"
    } else {
        "/etc/russhpit/config"
    }
}

/// Read `text` as a config file.
///
/// `path_for_diagnostics` is the path in a diagnostic line.
/// A startup load with a fatal diagnostic returns that diagnostic.
/// The function does not write to `base`.
pub fn load_str(
    base: &Config,
    path_for_diagnostics: &str,
    text: &str,
    mode: LoadMode,
) -> Result<LoadReport, Diagnostic> {
    let events = scan(path_for_diagnostics, text);
    if mode == LoadMode::Startup
        && let Some(fatal) = first_fatal(&events)
    {
        return Err(fatal);
    }

    let mut config = *base;
    let mut warnings = Vec::new();
    for event in events {
        match event {
            Event::Warning { text, fatal } => {
                if mode == LoadMode::Reload || !fatal {
                    warnings.push(text);
                }
            }
            Event::Set(value) => assign(&mut config, value),
        }
    }
    Ok(LoadReport { config, warnings })
}

/// Read the file at `path`.
///
/// `MissingFile::Ignore` accepts a missing file.
/// `MissingFile::Error` reports a missing file.
/// Any other I/O error is fatal at startup.
/// The same I/O error is a warning on reload.
/// The function does not write to `base`.
pub fn load_path(
    base: &Config,
    path: &Path,
    mode: LoadMode,
    missing: MissingFile,
) -> Result<LoadReport, Diagnostic> {
    let shown = path.display().to_string();
    match std::fs::read_to_string(path) {
        Ok(text) => load_str(base, &shown, &text, mode),
        Err(error) => io_failure(base, &shown, &error, mode, missing),
    }
}

fn io_failure(
    base: &Config,
    shown: &str,
    error: &std::io::Error,
    mode: LoadMode,
    missing: MissingFile,
) -> Result<LoadReport, Diagnostic> {
    if error.kind() == std::io::ErrorKind::NotFound && missing == MissingFile::Ignore {
        return Ok(LoadReport {
            config: *base,
            warnings: Vec::new(),
        });
    }
    let diagnostic = Diagnostic {
        text: format!("russhpit: cannot open config {shown}: {error}"),
    };
    match mode {
        LoadMode::Startup => Err(diagnostic),
        LoadMode::Reload => Ok(LoadReport {
            config: *base,
            warnings: vec![diagnostic],
        }),
    }
}

enum Event {
    Warning { text: Diagnostic, fatal: bool },
    Set(Value),
}

enum Value {
    Port(u16),
    Delay(i32),
    Line(u8),
    Clients(i32),
    Level(LogLevel),
    Family(BindFamily),
}

fn scan(path: &str, text: &str) -> Vec<Event> {
    let mut events = Vec::new();
    for (number, line) in physical_lines(text) {
        let event = match line {
            Err(()) => Err(line_message(path, number, "Line too long")),
            Ok(content) => interpret_line(path, number, content),
        };
        match event {
            Ok(None) => {}
            Ok(Some(event)) => events.push(event),
            Err(text) => events.push(Event::Warning { text, fatal: true }),
        }
    }
    events
}

fn first_fatal(events: &[Event]) -> Option<Diagnostic> {
    events.iter().find_map(|event| match event {
        Event::Warning { text, fatal: true } => Some(text.clone()),
        _ => None,
    })
}

fn physical_lines(text: &str) -> Vec<(usize, Result<&str, ()>)> {
    let bytes = text.as_bytes();
    let mut lines = Vec::new();
    let mut start = 0;
    let mut number = 1;
    for (index, byte) in bytes.iter().enumerate() {
        if *byte == b'\n' {
            lines.push((number, line_body(&text[start..index])));
            number += 1;
            start = index + 1;
        }
    }
    if start < bytes.len() {
        lines.push((number, line_body(&text[start..])));
    }
    lines
}

fn line_body(content: &str) -> Result<&str, ()> {
    if content.len() > 1024 {
        Err(())
    } else {
        Ok(content)
    }
}

fn interpret_line(path: &str, number: usize, content: &str) -> Result<Option<Event>, Diagnostic> {
    let tokens = tokens(content);
    match tokens.as_slice() {
        [] => Ok(None),
        [_] => Err(line_message(path, number, "Missing value")),
        [key, value] => match key_name(key) {
            None => Ok(Some(Event::Warning {
                text: line_message(path, number, &format!("Unknown option '{key}'")),
                fatal: false,
            })),
            Some(key) => parse_value(path, number, key, value).map(|value| Some(Event::Set(value))),
        },
        _ => Err(line_message(path, number, "Too many values")),
    }
}

fn tokens(line: &str) -> Vec<&str> {
    let line = match line.find('#') {
        Some(index) => &line[..index],
        None => line,
    };
    let bytes = line.as_bytes();
    let mut tokens = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        if is_token_break(bytes[index]) {
            index += 1;
            continue;
        }
        let start = index;
        while index < bytes.len() && !is_token_break(bytes[index]) {
            index += 1;
        }
        tokens.push(&line[start..index]);
    }
    tokens
}

fn is_token_break(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c)
}

enum Key {
    Port,
    Delay,
    MaxLineLength,
    MaxClients,
    LogLevel,
    BindFamily,
}

fn key_name(token: &str) -> Option<Key> {
    match token {
        "Port" => Some(Key::Port),
        "Delay" => Some(Key::Delay),
        "MaxLineLength" => Some(Key::MaxLineLength),
        "MaxClients" => Some(Key::MaxClients),
        "LogLevel" => Some(Key::LogLevel),
        "BindFamily" => Some(Key::BindFamily),
        _ => None,
    }
}

fn parse_value(path: &str, number: usize, key: Key, value: &str) -> Result<Value, Diagnostic> {
    match key {
        Key::Port => parse_ranged(value, 1, 65535, &format!("russhpit: Invalid port: {value}"))
            .map(|number| Value::Port(number as u16)),
        Key::Delay => parse_ranged(
            value,
            1,
            i32::MAX as i64,
            &format!("russhpit: Invalid delay: {value}"),
        )
        .map(|number| Value::Delay(number as i32)),
        Key::MaxLineLength => parse_ranged(
            value,
            3,
            255,
            &format!("russhpit: Invalid line length: {value}"),
        )
        .map(|number| Value::Line(number as u8)),
        Key::MaxClients => parse_ranged(
            value,
            1,
            i32::MAX as i64,
            &format!("russhpit: Invalid max clients: {value}"),
        )
        .map(|number| Value::Clients(number as i32)),
        Key::LogLevel => parse_log_level(path, number, value),
        Key::BindFamily => parse_family(value),
    }
}

fn parse_ranged(value: &str, low: i64, high: i64, message: &str) -> Result<i64, Diagnostic> {
    match parse_i64(value) {
        Some(number) if (low..=high).contains(&number) => Ok(number),
        _ => Err(Diagnostic {
            text: message.to_string(),
        }),
    }
}

fn parse_i64(token: &str) -> Option<i64> {
    let digits = token.strip_prefix('+').unwrap_or(token);
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

fn parse_log_level(path: &str, number: usize, value: &str) -> Result<Value, Diagnostic> {
    match parse_i64(value) {
        Some(0) => Ok(Value::Level(LogLevel::Quiet)),
        Some(1) => Ok(Value::Level(LogLevel::Standard)),
        Some(2) => Ok(Value::Level(LogLevel::Debug)),
        _ => Err(line_message(
            path,
            number,
            &format!("Invalid log level '{value}'"),
        )),
    }
}

fn parse_family(value: &str) -> Result<Value, Diagnostic> {
    let family = match value {
        "0" => BindFamily::Dual,
        "4" => BindFamily::V4,
        "6" => BindFamily::V6,
        _ => {
            return Err(Diagnostic {
                text: format!("russhpit: Invalid address family: {value}"),
            });
        }
    };
    Ok(Value::Family(family))
}

fn line_message(path: &str, number: usize, message: &str) -> Diagnostic {
    Diagnostic {
        text: format!("{path}:{number}: {message}"),
    }
}

impl Config {
    pub(crate) fn apply_port(&mut self, value: &str) -> Result<(), Diagnostic> {
        self.apply_field(Key::Port, value)
    }

    pub(crate) fn apply_delay(&mut self, value: &str) -> Result<(), Diagnostic> {
        self.apply_field(Key::Delay, value)
    }

    pub(crate) fn apply_max_line_length(&mut self, value: &str) -> Result<(), Diagnostic> {
        self.apply_field(Key::MaxLineLength, value)
    }

    pub(crate) fn apply_max_clients(&mut self, value: &str) -> Result<(), Diagnostic> {
        self.apply_field(Key::MaxClients, value)
    }

    fn apply_field(&mut self, key: Key, value: &str) -> Result<(), Diagnostic> {
        match parse_value("", 1, key, value) {
            Ok(parsed) => {
                assign(self, parsed);
                Ok(())
            }
            Err(error) => Err(error),
        }
    }
}

fn assign(config: &mut Config, value: Value) {
    match value {
        Value::Port(port) => config.port = port,
        Value::Delay(delay) => config.delay_ms = delay,
        Value::Line(length) => config.max_line_length = length,
        Value::Clients(clients) => config.max_clients = clients,
        Value::Level(level) => config.log_level = level,
        Value::Family(family) => config.bind_family = family,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    const PATH: &str = "file.cfg";

    fn startup(base: &Config, text: &str) -> Result<LoadReport, Diagnostic> {
        load_str(base, PATH, text, LoadMode::Startup)
    }

    fn reload(base: &Config, text: &str) -> LoadReport {
        load_str(base, PATH, text, LoadMode::Reload).unwrap()
    }

    fn load_ok(text: &str) -> Config {
        startup(&Config::default(), text).unwrap().config
    }

    fn load_err(text: &str) -> String {
        startup(&Config::default(), text).unwrap_err().to_string()
    }

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let id = NEXT.fetch_add(1, Ordering::Relaxed);
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "russhpit-config-{}-{nanos}-{id}",
                std::process::id()
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn default_matches_c_defaults() {
        assert_eq!(
            Config::default(),
            Config {
                port: 2222,
                delay_ms: 10_000,
                max_line_length: 32,
                max_clients: 4096,
                bind_family: BindFamily::Dual,
                log_level: LogLevel::Quiet,
            }
        );
        #[cfg(target_os = "freebsd")]
        assert_eq!(default_config_path(), "/usr/local/etc/russhpit.config");
        #[cfg(not(target_os = "freebsd"))]
        assert_eq!(default_config_path(), "/etc/russhpit/config");
    }

    #[test]
    fn sample_file_parses() {
        let sample = "\
# The port on which to listen for new SSH connections.
Port 2222

# The endless banner is sent one line at a time. This is the delay
# in milliseconds between individual lines.
Delay 10000

# The length of each line is randomized. This controls the maximum
# length of each line. Shorter lines may keep clients on for longer if
# they give up after a certain number of bytes.
MaxLineLength 32

# Maximum number of connections to accept at a time. Connections beyond
# this are not immediately rejected, but will wait in the queue.
MaxClients 4096

# Set the detail level for the log.
#   0 = Quiet
#   1 = Standard, useful log messages
#   2 = Very noisy debugging information
LogLevel 0

# Set the family of the listening socket
#   0 = Use IPv4 Mapped IPv6 (Both v4 and v6, default)
#   4 = Use IPv4 only
#   6 = Use IPv6 only
BindFamily 0
";
        let report = startup(&Config::default(), sample).unwrap();
        assert_eq!(report.config, Config::default());
        assert!(report.warnings.is_empty());
    }

    #[test]
    fn each_key_overrides_only_itself() {
        let base = Config {
            port: 11,
            delay_ms: 12,
            max_line_length: 13,
            max_clients: 14,
            bind_family: BindFamily::V6,
            log_level: LogLevel::Debug,
        };
        let cases = [
            ("Port 9\n", Config { port: 9, ..base }),
            (
                "Delay 8\n",
                Config {
                    delay_ms: 8,
                    ..base
                },
            ),
            (
                "MaxLineLength 20\n",
                Config {
                    max_line_length: 20,
                    ..base
                },
            ),
            (
                "MaxClients 3\n",
                Config {
                    max_clients: 3,
                    ..base
                },
            ),
            (
                "LogLevel 1\n",
                Config {
                    log_level: LogLevel::Standard,
                    ..base
                },
            ),
            (
                "BindFamily 4\n",
                Config {
                    bind_family: BindFamily::V4,
                    ..base
                },
            ),
        ];
        for (text, expect) in cases {
            let report = startup(&base, text).unwrap();
            assert_eq!(report.config, expect);
            assert!(report.warnings.is_empty());
        }
    }

    #[test]
    fn comments_blank_lines_and_tabs() {
        let text = "\
# comment only

\t
Port\t2200
Delay\u{000b}15#ms
MaxLineLength 32 # trailing comment
MaxClients\t\t10
LogLevel 1
BindFamily 4
";
        assert_eq!(
            load_ok(text),
            Config {
                port: 2200,
                delay_ms: 15,
                max_line_length: 32,
                max_clients: 10,
                bind_family: BindFamily::V4,
                log_level: LogLevel::Standard,
            }
        );
    }

    #[test]
    fn trailing_line_without_newline() {
        assert_eq!(
            load_ok("MaxClients 7"),
            Config {
                max_clients: 7,
                ..Config::default()
            }
        );
    }

    #[test]
    fn numeric_edges() {
        assert_eq!(
            load_ok("Port 1"),
            Config {
                port: 1,
                ..Config::default()
            }
        );
        assert_eq!(
            load_ok("Port 65535"),
            Config {
                port: 65535,
                ..Config::default()
            }
        );
        assert_eq!(
            load_ok("Port +2222"),
            Config {
                port: 2222,
                ..Config::default()
            }
        );
        assert_eq!(
            load_ok("Port 02222"),
            Config {
                port: 2222,
                ..Config::default()
            }
        );
        assert_eq!(
            load_ok("Delay 1"),
            Config {
                delay_ms: 1,
                ..Config::default()
            }
        );
        assert_eq!(
            load_ok("Delay 2147483647"),
            Config {
                delay_ms: i32::MAX,
                ..Config::default()
            }
        );
        assert_eq!(
            load_ok("MaxLineLength 3"),
            Config {
                max_line_length: 3,
                ..Config::default()
            }
        );
        assert_eq!(
            load_ok("MaxLineLength 255"),
            Config {
                max_line_length: 255,
                ..Config::default()
            }
        );
        assert_eq!(
            load_ok("MaxClients 1"),
            Config {
                max_clients: 1,
                ..Config::default()
            }
        );
        assert_eq!(
            load_ok("MaxClients 2147483647"),
            Config {
                max_clients: i32::MAX,
                ..Config::default()
            }
        );
        assert_eq!(load_ok("LogLevel 0").log_level, LogLevel::Quiet);
        assert_eq!(load_ok("LogLevel 1").log_level, LogLevel::Standard);
        assert_eq!(load_ok("LogLevel 2").log_level, LogLevel::Debug);

        assert_eq!(load_err("Port 0"), "russhpit: Invalid port: 0");
        assert_eq!(load_err("Port 65536"), "russhpit: Invalid port: 65536");
        assert_eq!(load_err("Port 2222abc"), "russhpit: Invalid port: 2222abc");
        assert_eq!(load_err("Port 0x10"), "russhpit: Invalid port: 0x10");
        assert_eq!(load_err("Delay 0"), "russhpit: Invalid delay: 0");
        assert_eq!(
            load_err("Delay 2147483648"),
            "russhpit: Invalid delay: 2147483648"
        );
        assert_eq!(
            load_err("MaxLineLength 2"),
            "russhpit: Invalid line length: 2"
        );
        assert_eq!(
            load_err("MaxLineLength 256"),
            "russhpit: Invalid line length: 256"
        );
        assert_eq!(load_err("MaxClients 0"), "russhpit: Invalid max clients: 0");
        assert_eq!(
            load_err("MaxClients 2147483648"),
            "russhpit: Invalid max clients: 2147483648"
        );
        assert_eq!(
            load_err("LogLevel -1"),
            "file.cfg:1: Invalid log level '-1'"
        );
        assert_eq!(load_err("LogLevel 3"), "file.cfg:1: Invalid log level '3'");
        assert_eq!(load_ok("BindFamily 0").bind_family, BindFamily::Dual);
        assert_eq!(load_ok("BindFamily 4").bind_family, BindFamily::V4);
        assert_eq!(load_ok("BindFamily 6").bind_family, BindFamily::V6);
    }

    #[test]
    fn bind_family_whole_token() {
        assert_eq!(load_ok("BindFamily 0").bind_family, BindFamily::Dual);
        assert_eq!(load_ok("BindFamily 4").bind_family, BindFamily::V4);
        assert_eq!(load_ok("BindFamily 6").bind_family, BindFamily::V6);
        assert_eq!(
            load_err("BindFamily 4foo"),
            "russhpit: Invalid address family: 4foo"
        );
        assert_eq!(
            load_err("BindFamily 04"),
            "russhpit: Invalid address family: 04"
        );
        assert_eq!(
            load_err("BindFamily 8"),
            "russhpit: Invalid address family: 8"
        );
    }

    #[test]
    fn keys_are_case_sensitive() {
        let report = startup(&Config::default(), "port 2222\n").unwrap();
        assert_eq!(report.config, Config::default());
        assert_eq!(
            report.warnings[0].as_str(),
            "file.cfg:1: Unknown option 'port'"
        );
    }

    #[test]
    fn startup_bad_value_keeps_base() {
        let base = Config::default();
        let error = startup(&base, "Port nope\nDelay 5\n").unwrap_err();
        assert_eq!(error.as_str(), "russhpit: Invalid port: nope");
        assert_eq!(base, Config::default());
    }

    #[test]
    fn startup_unknown_key_warns_and_applies_other_keys() {
        let report = startup(&Config::default(), "Port 9\nNope 1\nDelay 6\n").unwrap();
        assert_eq!(
            report.config,
            Config {
                port: 9,
                delay_ms: 6,
                ..Config::default()
            }
        );
        assert_eq!(report.warnings.len(), 1);
        assert_eq!(
            report.warnings[0].as_str(),
            "file.cfg:2: Unknown option 'Nope'"
        );
    }

    #[test]
    fn reload_bad_value_keeps_that_key_and_applies_later_keys() {
        let base = Config {
            port: 50,
            delay_ms: 60,
            max_line_length: 40,
            max_clients: 70,
            ..Config::default()
        };
        let report = reload(
            &base,
            "Port nope\nDelay 5\nMaxLineLength bad\nMaxClients 9\n",
        );
        assert_eq!(
            report.config,
            Config {
                port: 50,
                delay_ms: 5,
                max_line_length: 40,
                max_clients: 9,
                ..base
            }
        );
        assert_eq!(report.warnings[0].as_str(), "russhpit: Invalid port: nope");
        assert_eq!(
            report.warnings[1].as_str(),
            "russhpit: Invalid line length: bad"
        );
    }

    #[test]
    fn reload_unknown_and_missing_value_are_warnings() {
        let base = Config {
            port: 50,
            delay_ms: 60,
            ..Config::default()
        };
        let report = reload(&base, "Foo bar\nPort\nDelay 5\nPort 1 2\n");
        assert_eq!(
            report.config,
            Config {
                delay_ms: 5,
                ..base
            }
        );
        assert_eq!(
            report
                .warnings
                .iter()
                .map(Diagnostic::as_str)
                .collect::<Vec<_>>(),
            vec![
                "file.cfg:1: Unknown option 'Foo'",
                "file.cfg:2: Missing value",
                "file.cfg:4: Too many values",
            ]
        );

        let error = startup(&base, "Port\nDelay 5\n").unwrap_err();
        assert_eq!(error.as_str(), "file.cfg:1: Missing value");
        assert_eq!(base.delay_ms, 60);

        let error = startup(&base, "Port 1 2\nDelay 5\n").unwrap_err();
        assert_eq!(error.as_str(), "file.cfg:1: Too many values");
        assert_eq!(base.delay_ms, 60);
    }

    #[test]
    fn line_over_1024_is_an_error() {
        let long = format!("{}\nDelay 5\n", "x".repeat(1025));
        let base = Config {
            delay_ms: 60,
            ..Config::default()
        };
        assert_eq!(
            startup(&base, &long).unwrap_err().as_str(),
            "file.cfg:1: Line too long"
        );
        let report = reload(&base, &long);
        assert_eq!(report.warnings[0].as_str(), "file.cfg:1: Line too long");
        assert_eq!(report.config.delay_ms, 5);

        let exact = format!("{}\n", " ".repeat(1024));
        let report = startup(&Config::default(), &exact).unwrap();
        assert_eq!(report.config, Config::default());
        assert!(report.warnings.is_empty());
    }

    #[test]
    fn missing_path_ignore_versus_error() {
        let dir = TempDir::new();
        let missing = dir.0.join("no-such-file");
        let base = Config {
            port: 9,
            ..Config::default()
        };
        let shown = missing.display().to_string();
        let io_error = fs::read_to_string(&missing).unwrap_err();
        let open_error = format!("russhpit: cannot open config {shown}: {io_error}");

        let report = load_path(&base, &missing, LoadMode::Startup, MissingFile::Ignore).unwrap();
        assert_eq!(report.config, base);
        assert!(report.warnings.is_empty());

        let report = load_path(&base, &missing, LoadMode::Reload, MissingFile::Ignore).unwrap();
        assert_eq!(report.config, base);
        assert!(report.warnings.is_empty());

        let error = load_path(&base, &missing, LoadMode::Startup, MissingFile::Error).unwrap_err();
        assert_eq!(error.as_str(), open_error);
        assert_eq!(base.port, 9);

        let report = load_path(&base, &missing, LoadMode::Reload, MissingFile::Error).unwrap();
        assert_eq!(report.config, base);
        assert_eq!(report.warnings[0].as_str(), open_error);

        let io_error = fs::read_to_string(&dir.0).unwrap_err();
        let shown = dir.0.display().to_string();
        let open_error = format!("russhpit: cannot open config {shown}: {io_error}");
        let error = load_path(&base, &dir.0, LoadMode::Startup, MissingFile::Ignore).unwrap_err();
        assert_eq!(error.as_str(), open_error);
        let report = load_path(&base, &dir.0, LoadMode::Reload, MissingFile::Ignore).unwrap();
        assert_eq!(report.config, base);
        assert_eq!(report.warnings[0].as_str(), open_error);

        let present = dir.0.join("ok.cfg");
        fs::write(&present, "Port 8\n").unwrap();
        let report = load_path(&base, &present, LoadMode::Startup, MissingFile::Error).unwrap();
        assert_eq!(report.config.port, 8);
        assert!(report.warnings.is_empty());
    }

    #[test]
    fn echo_lines_for_each_family() {
        let mut config = Config {
            port: 7,
            delay_ms: 8,
            max_line_length: 9,
            max_clients: 10,
            bind_family: BindFamily::Dual,
            log_level: LogLevel::Debug,
        };
        assert_eq!(
            config.echo_lines(),
            [
                "Port 7",
                "Delay 8",
                "MaxLineLength 9",
                "MaxClients 10",
                "BindFamily IPv4 Mapped IPv6",
            ]
        );
        config.bind_family = BindFamily::V4;
        assert_eq!(config.echo_lines()[4], "BindFamily IPv4 Only");
        config.bind_family = BindFamily::V6;
        assert_eq!(config.echo_lines()[4], "BindFamily IPv6 Only");
    }
}
