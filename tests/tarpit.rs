//! Localhost clients read banner lines.
//!
//! An idle listener still waits one delay before the first line.

use std::io::Read;
use std::net::{Ipv6Addr, TcpStream};
use std::path::Path;
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use russhpit::banner::BannerRng;
use russhpit::config::{BindFamily, Config, LogLevel};
use russhpit::tarpit::{Tarpit, Turn};
use russhpit::unix::{
    Listener, Log, SignalFlags, UnixReactor, bind_ephemeral, ignore_sigpipe, mono_now, publish,
};

struct Server {
    flags: Arc<SignalFlags>,
    log: Arc<Log>,
    handle: JoinHandle<()>,
}

impl Server {
    fn stop(self) -> String {
        self.flags.request_stop();
        self.handle.join().expect("server thread panicked");
        self.log.captured_text()
    }
}

fn spawn(listener: Listener) -> Server {
    spawn_with_delay(listener, 50)
}

fn spawn_with_delay(listener: Listener, delay_ms: i32) -> Server {
    let log = Log::capture(LogLevel::Standard);
    let flags = Arc::new(SignalFlags::new());
    let thread_log = Arc::clone(&log);
    let thread_flags = Arc::clone(&flags);
    let handle = thread::spawn(move || {
        let reactor = UnixReactor::new(listener, Arc::clone(&thread_log));
        let config = Config {
            delay_ms,
            max_line_length: 32,
            max_clients: 4,
            log_level: LogLevel::Standard,
            ..Config::default()
        };
        let mut tarpit = Tarpit::new(
            reactor,
            config,
            Path::new("unused"),
            BannerRng::new(1),
            mono_now(),
        );
        loop {
            let now = mono_now();
            let mut signals = thread_flags.take();
            match tarpit.turn(now, &mut signals) {
                Turn::Continue => {
                    thread_log.set_level(tarpit.config().log_level);
                    publish(tarpit.take_log_lines(), &thread_log);
                }
                Turn::Stop => {
                    thread_log.set_level(tarpit.config().log_level);
                    publish(tarpit.take_log_lines(), &thread_log);
                    break;
                }
                Turn::Fatal(message) => panic!("server fatal: {message}"),
            }
        }
    });
    Server { flags, log, handle }
}

fn read_banner(stream: &mut TcpStream) -> Vec<u8> {
    stream
        .set_read_timeout(Some(Duration::from_millis(200)))
        .expect("read timeout");
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut buf = Vec::new();
    let mut tmp = [0u8; 64];
    while Instant::now() < deadline {
        match stream.read(&mut tmp) {
            Ok(0) => break,
            Ok(count) => {
                buf.extend_from_slice(&tmp[..count]);
                if buf.contains(&b'\n') {
                    break;
                }
            }
            Err(error)
                if error.kind() == std::io::ErrorKind::WouldBlock
                    || error.kind() == std::io::ErrorKind::TimedOut =>
            {
                continue;
            }
            Err(error) => panic!("read failed: {error}"),
        }
    }
    if let Some(end) = buf.iter().position(|byte| *byte == b'\n') {
        buf.truncate(end + 1);
    }
    buf
}

fn assert_banner(line: &[u8]) {
    assert!(line.ends_with(b"\r\n"), "line={line:?}");
    assert!((3..=32).contains(&line.len()), "length {}", line.len());
    assert!(
        line[..line.len() - 2]
            .iter()
            .all(|byte| (32..=126).contains(byte)),
        "line={line:?}"
    );
    assert!(!line.starts_with(b"SSH-"), "line={line:?}");
}

fn bodies(text: &str) -> Vec<&str> {
    text.lines()
        .map(|line| line.split_once(' ').map(|(_, body)| body).unwrap_or(line))
        .collect()
}

fn field<'a>(body: &'a str, key: &str) -> &'a str {
    body.split_whitespace()
        .find_map(|item| item.strip_prefix(key))
        .unwrap_or_else(|| panic!("missing {key} in {body}"))
}

fn millis(text: &str) -> u64 {
    let (secs, frac) = text
        .split_once('.')
        .unwrap_or_else(|| panic!("missing fraction in {text}"));
    secs.parse::<u64>().expect("seconds") * 1000 + frac.parse::<u64>().expect("millis")
}

fn assert_log(text: &str, read_len: usize) {
    let lines = bodies(text);
    let accepts: Vec<_> = lines
        .iter()
        .copied()
        .filter(|body| body.starts_with("ACCEPT "))
        .collect();
    let closes: Vec<_> = lines
        .iter()
        .copied()
        .filter(|body| body.starts_with("CLOSE "))
        .collect();
    let totals: Vec<_> = lines
        .iter()
        .copied()
        .filter(|body| body.starts_with("TOTALS "))
        .collect();
    assert_eq!(accepts.len(), 1, "{text}");
    assert_eq!(closes.len(), 1, "{text}");
    assert_eq!(totals.len(), 1, "{text}");
    let bytes: u64 = field(closes[0], "bytes=").parse().expect("bytes");
    assert_eq!(bytes, read_len as u64, "{text}");
    assert_eq!(field(totals[0], "connects="), "1", "{text}");
    let close_ms = millis(field(closes[0], "time="));
    let total_ms = millis(field(totals[0], "seconds="));
    assert!(total_ms >= close_ms, "{text}");
    assert_eq!(total_ms, close_ms, "{text}");
}

#[test]
fn one_client_reads_one_banner_line() {
    ignore_sigpipe().expect("ignore SIGPIPE");
    let listener = bind_ephemeral(BindFamily::V4).expect("bind 127.0.0.1");
    let port = listener.local_port().expect("port");
    let server = spawn(listener);
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    let line = read_banner(&mut stream);
    assert_banner(&line);
    let text = server.stop();
    drop(stream);
    assert_log(&text, line.len());
}

#[test]
fn idle_listener_waits_one_delay_before_the_first_line() {
    ignore_sigpipe().expect("ignore SIGPIPE");
    let listener = bind_ephemeral(BindFamily::V4).expect("bind 127.0.0.1");
    let port = listener.local_port().expect("port");
    let server = spawn_with_delay(listener, 500);
    thread::sleep(Duration::from_millis(1_000));
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    let started = Instant::now();
    let line = read_banner(&mut stream);
    let elapsed = started.elapsed();
    assert_banner(&line);
    assert!(
        elapsed >= Duration::from_millis(250),
        "first line arrived in {elapsed:?}"
    );
    assert!(elapsed < Duration::from_secs(2), "elapsed {elapsed:?}");
    let text = server.stop();
    drop(stream);
    assert_log(&text, line.len());
}

#[test]
fn dual_stack_client_skips_when_loopback_v6_is_down() {
    if std::net::TcpListener::bind((Ipv6Addr::LOCALHOST, 0)).is_err() {
        eprintln!("skip: ::1 is unavailable");
        return;
    }
    let listener = match bind_ephemeral(BindFamily::Dual) {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!("skip: dual-stack bind failed: {error}");
            return;
        }
    };
    let port = match listener.local_port() {
        Ok(port) => port,
        Err(error) => {
            eprintln!("skip: {error}");
            return;
        }
    };
    ignore_sigpipe().expect("ignore SIGPIPE");
    let server = spawn(listener);
    let mut stream = match TcpStream::connect((Ipv6Addr::LOCALHOST, port)) {
        Ok(stream) => stream,
        Err(error) => {
            let _ = server.stop();
            eprintln!("skip: ::1 connect failed: {error}");
            return;
        }
    };
    let line = read_banner(&mut stream);
    assert_banner(&line);
    let text = server.stop();
    drop(stream);
    assert_log(&text, line.len());
    let host = bodies(&text)
        .into_iter()
        .find(|body| body.starts_with("ACCEPT "))
        .expect("accept");
    assert!(host.contains("host=::1"), "{host}");
    assert!(!host.contains('['), "{host}");
}
