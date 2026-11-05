//! Process entry for russhpit.
//!
//! The function parses arguments, opens the log, and runs the loop.

use std::path::Path;
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use russhpit::banner::BannerRng;
use russhpit::cli::{Action, LogSink, help_text, parse, version_text};
use russhpit::config::{Config, default_config_path};
use russhpit::tarpit::{Tarpit, Turn};
use russhpit::unix::{
    Log, UnixReactor, bind_listener, install_signals, mono_now, process_flags, publish,
};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let path = default_config_path();
    let invocation = parse(&args, &Config::default(), Path::new(path));
    match invocation.action {
        Action::Help => {
            print!("{}", help_text(path));
            return ExitCode::SUCCESS;
        }
        Action::Version => {
            print!("{}", version_text());
            return ExitCode::SUCCESS;
        }
        Action::Error => {
            eprintln!("{}", invocation.error_text);
            eprint!("{}", help_text(path));
            return ExitCode::from(1);
        }
        Action::Run => {}
    }

    for warning in &invocation.warnings {
        eprintln!("{warning}");
    }

    let log = match invocation.log_sink {
        LogSink::Stdout => Log::stdout(invocation.config.log_level),
        LogSink::Syslog => Log::syslog(invocation.config.log_level, &program_name()),
    };
    for line in invocation.config.echo_lines() {
        log.info(&line);
    }

    if let Err(error) = install_signals() {
        return fail(&log, &error.to_string());
    }
    let flags = process_flags();

    let listener = match bind_listener(invocation.config.port, invocation.config.bind_family) {
        Ok(listener) => listener,
        Err(error) => return fail(&log, &error.to_string()),
    };
    let reactor = UnixReactor::new(listener, std::sync::Arc::clone(&log));
    let mut tarpit = Tarpit::new(
        reactor,
        invocation.config,
        invocation.config_path,
        BannerRng::new(banner_seed()),
        mono_now(),
    );

    loop {
        let now = mono_now();
        let mut signals = flags.take();
        match tarpit.turn(now, &mut signals) {
            Turn::Continue => publish_turn(&log, &mut tarpit),
            Turn::Stop => {
                publish_turn(&log, &mut tarpit);
                break;
            }
            Turn::Fatal(message) => return fail(&log, &message),
        }
    }

    log.close();
    ExitCode::SUCCESS
}

/// The function publishes the lines from one turn.
///
/// The sink uses the config level from that turn.
/// The function sets that level before the write.
fn publish_turn(log: &Log, tarpit: &mut Tarpit<UnixReactor>) {
    log.set_level(tarpit.config().log_level);
    publish(tarpit.take_log_lines(), log);
}

fn fail(log: &Log, message: &str) -> ExitCode {
    eprintln!("russhpit: fatal: {message}");
    log.close();
    ExitCode::from(1)
}

/// The function returns the file name of `argv[0]`.
fn program_name() -> String {
    let Some(arg) = std::env::args_os().next() else {
        return "russhpit".to_string();
    };
    Path::new(&arg)
        .file_name()
        .map(|text| text.to_string_lossy().into_owned())
        .filter(|text| !text.is_empty())
        .unwrap_or_else(|| "russhpit".to_string())
}

fn banner_seed() -> u32 {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    millis as u32
}
