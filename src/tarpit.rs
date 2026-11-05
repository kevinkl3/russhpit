//! One turn of the tarpit loop.
//!
//! The turn reloads the config, sends due lines, and accepts one client.
//! A reactor supplies poll, accept, write, close, and rebind.
//! This module does not open a socket.

use std::collections::HashMap;
use std::path::PathBuf;

use crate::banner::BannerRng;
use crate::config::{BindFamily, Config, LoadMode, MissingFile, load_path};
use crate::queue::{
    CloseRecord, MonoMs, PollTimeout, Queue, Stats, accept_line, admit, close_client, close_line,
    drain_all, totals_line, totals_ms,
};
use crate::send::{IoError, PendingLine, Step, drive_attempt};

/// Socket actions for one tarpit.
///
/// The Unix reactor arrives in phase 7.
/// Tests use a fake reactor.
pub trait Reactor {
    /// The function waits for the listener or the next send.
    ///
    /// `watch_listener` is true when the queue can take a client.
    /// The function returns true when the listener is readable.
    /// The function returns false on a timeout.
    /// The function returns false when the caller does not watch the listener.
    fn poll(&mut self, watch_listener: bool, timeout: PollTimeout) -> Result<bool, PollError>;

    /// The function accepts one connection.
    fn accept(&mut self) -> Result<Accepted, AcceptError>;

    /// The function writes `buf` to client `id`.
    fn write(&mut self, id: u64, buf: &[u8]) -> Result<usize, IoError>;

    /// The function closes client `id`.
    fn close(&mut self, id: u64);

    /// The function binds the listener to `port` and `family`.
    ///
    /// An error leaves the previous listener in place.
    fn rebind(&mut self, port: u16, family: BindFamily) -> Result<(), String>;

    /// The function returns the time for a new client.
    ///
    /// `turn_now` is the stamp for due clients and for the next send.
    /// The default returns `turn_now`.
    /// The Unix reactor reads the clock again.
    fn accept_stamp(&self, turn_now: MonoMs) -> MonoMs {
        turn_now
    }
}

/// One accepted connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Accepted {
    /// The client id.
    pub id: u64,
    /// The file descriptor in an accept line.
    pub log_fd: i32,
    /// The peer host text.
    pub host: String,
    /// The peer port.
    pub port: u16,
}

/// A failed accept.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AcceptError {
    /// The process file-descriptor limit is full.
    ProcessFdLimit,
    /// The system file-descriptor limit is full.
    SystemFdLimit,
    /// The caller can try the accept again.
    Transient(String),
    /// The caller cannot continue.
    Fatal(String),
}

/// A failed poll.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PollError {
    /// A signal interrupted the wait.
    Interrupted,
    /// The caller cannot continue.
    Fatal(String),
}

/// Flags for one turn.
///
/// Stop runs before reload and before the stats flag.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Signals {
    /// Stop ends the loop.
    pub stop: bool,
    /// Reload reads the config file.
    pub reload: bool,
    /// The flag requests one totals line.
    pub dump_stats: bool,
}

/// The level of one stored log line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordLevel {
    /// A normal log line.
    Info,
    /// A warning line.
    Warning,
    /// An error line.
    Error,
}

/// One stored log line.
///
/// The body has no timestamp.
/// The body has no newline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogRecord {
    /// The level of this line.
    pub level: RecordLevel,
    /// The log text.
    pub body: String,
}

/// The result of one turn.
#[derive(Debug, Clone, PartialEq, Eq)]
#[must_use]
pub enum Turn {
    /// The loop can run another turn.
    Continue,
    /// The loop drained the queue and stopped.
    Stop,
    /// The loop stops for this error.
    Fatal(String),
}

/// State for one tarpit loop.
///
/// The value holds the queue, the counters, the generator, and the log lines.
pub struct Tarpit<R: Reactor> {
    reactor: R,
    queue: Queue,
    stats: Stats,
    rng: BannerRng,
    config: Config,
    path: PathBuf,
    log_lines: Vec<LogRecord>,
    pending: HashMap<u64, PendingLine>,
    /// The next poll skips the listener after an empty-queue descriptor limit.
    accept_hold: bool,
}

impl<R: Reactor> Tarpit<R> {
    /// The function builds a tarpit.
    ///
    /// `path` is the file that a reload reads.
    /// `now` is the start time.
    /// This function does not bind a socket.
    /// Each later turn passes its own time.
    pub fn new(
        reactor: R,
        config: Config,
        path: impl Into<PathBuf>,
        rng: BannerRng,
        now: MonoMs,
    ) -> Self {
        let _ = now;
        Self {
            reactor,
            queue: Queue::new(),
            stats: Stats::default(),
            rng,
            config,
            path: path.into(),
            log_lines: Vec::new(),
            pending: HashMap::new(),
            accept_hold: false,
        }
    }

    /// The function returns the running config.
    pub fn config(&self) -> Config {
        self.config
    }

    /// The function returns the stored log lines.
    ///
    /// The lines stay in the store.
    pub fn log_lines(&self) -> &[LogRecord] {
        &self.log_lines
    }

    /// The function returns the stored log lines.
    ///
    /// The function then clears the store.
    pub fn take_log_lines(&mut self) -> Vec<LogRecord> {
        std::mem::take(&mut self.log_lines)
    }

    /// The function runs one loop turn.
    ///
    /// Stop runs before reload and before the stats line.
    /// The function returns [`Turn::Stop`] after the drain and the totals line.
    pub fn turn(&mut self, now: MonoMs, signals: &mut Signals) -> Turn {
        if signals.stop {
            clear_signals(signals);
            self.drain(now);
            self.push_totals(now);
            return Turn::Stop;
        }
        if signals.reload {
            signals.reload = false;
            self.reload();
        }
        if signals.dump_stats {
            signals.dump_stats = false;
            self.push_totals(now);
        }
        self.send_due(now);
        let (watch, timeout) = self.poll_args(now);
        let readable = match self.reactor.poll(watch, timeout) {
            Ok(readable) => readable,
            Err(PollError::Interrupted) => return Turn::Continue,
            Err(PollError::Fatal(message)) => return Turn::Fatal(message),
        };
        if readable && let Turn::Fatal(message) = self.accept_one(now) {
            return Turn::Fatal(message);
        }
        Turn::Continue
    }

    fn reload(&mut self) {
        let old_port = self.config.port;
        let old_family = self.config.bind_family;
        let report = match load_path(
            &self.config,
            &self.path,
            LoadMode::Reload,
            MissingFile::Error,
        ) {
            Ok(report) => report,
            Err(error) => {
                self.push(RecordLevel::Warning, error.to_string());
                return;
            }
        };
        self.config = report.config;
        for warning in report.warnings {
            self.push(RecordLevel::Warning, warning.to_string());
        }
        for line in self.config.echo_lines() {
            self.push(RecordLevel::Info, line);
        }
        if self.config.port == old_port && self.config.bind_family == old_family {
            return;
        }
        if let Err(message) = self
            .reactor
            .rebind(self.config.port, self.config.bind_family)
        {
            self.push(RecordLevel::Error, message);
            self.config.port = old_port;
            self.config.bind_family = old_family;
        }
    }

    fn send_due(&mut self, now: MonoMs) {
        let max_line_length = self.config.max_line_length;
        let delay_ms = self.config.delay_ms;
        let due = self.queue.take_due(now);
        for client in due {
            self.send_one(client, now, max_line_length, delay_ms);
        }
    }

    fn send_one(
        &mut self,
        mut client: crate::queue::Client,
        now: MonoMs,
        max_line_length: u8,
        delay_ms: i32,
    ) {
        let id = client.id;
        let mut pending = self.pending.remove(&id).unwrap_or_default();
        let step = {
            let rng = &mut self.rng;
            let total_bytes = &mut self.stats.bytes_sent;
            let reactor = &mut self.reactor;
            drive_attempt(
                &mut pending,
                rng,
                max_line_length,
                &mut client.bytes_sent,
                total_bytes,
                |buf| reactor.write(id, buf),
            )
        };
        match step {
            Step::Drop => {
                let record = close_client(&mut self.stats, client, now);
                self.push_close(&record);
                self.reactor.close(id);
            }
            // `drive_attempt` returns `Pace` or `Drop`.
            // This arm keeps the client.
            Step::Pace | Step::RetryNow => {
                self.pending.insert(id, pending);
                self.queue.reschedule(client, now, delay_ms);
            }
        }
    }

    /// The function chooses the poll watch flag and the wait.
    ///
    /// An accept hold skips the listener for this poll.
    /// An empty queue then waits one delay.
    /// The hold ends after this choice.
    fn poll_args(&mut self, now: MonoMs) -> (bool, PollTimeout) {
        let mut watch = self.queue.accepting(self.config.max_clients);
        let mut timeout = self.queue.timeout_until_head(now);
        if self.accept_hold {
            watch = false;
            if self.queue.is_empty() {
                timeout = PollTimeout::AfterMs(self.config.delay_ms);
            }
            self.accept_hold = false;
        }
        (watch, timeout)
    }

    fn accept_one(&mut self, now: MonoMs) -> Turn {
        match self.reactor.accept() {
            Ok(accepted) => {
                let accepted_at = self.reactor.accept_stamp(now);
                self.store_accept(accepted, accepted_at);
            }
            Err(AcceptError::ProcessFdLimit | AcceptError::SystemFdLimit) => self.on_fd_limit(),
            Err(AcceptError::Transient(message)) => {
                self.push(
                    RecordLevel::Warning,
                    format!("russhpit: warning: {message}"),
                );
            }
            Err(AcceptError::Fatal(message)) => return Turn::Fatal(message),
        }
        Turn::Continue
    }

    fn store_accept(&mut self, accepted: Accepted, now: MonoMs) {
        let host = accepted.host.clone();
        let port = accepted.port;
        let log_fd = accepted.log_fd;
        let client = admit(
            accepted.id,
            log_fd,
            accepted.host,
            port,
            now,
            self.config.delay_ms,
        );
        self.pending.insert(client.id, PendingLine::new());
        self.queue.append(client);
        self.stats.note_accept();
        self.push(
            RecordLevel::Info,
            accept_line(
                &host,
                port,
                log_fd,
                self.queue.len(),
                self.config.max_clients,
            ),
        );
    }

    fn on_fd_limit(&mut self) {
        if self.queue.is_empty() {
            self.push(RecordLevel::Warning, "russhpit: warning: accept deferred");
            self.accept_hold = true;
            return;
        }
        let cap = i32::try_from(self.queue.len()).unwrap_or(i32::MAX);
        self.config.max_clients = cap;
        self.push(RecordLevel::Info, format!("MaxClients {cap}"));
    }

    fn drain(&mut self, now: MonoMs) {
        let records = drain_all(&mut self.queue, &mut self.stats, now);
        for record in records {
            self.pending.remove(&record.id);
            self.push_close(&record);
            self.reactor.close(record.id);
        }
        self.pending.clear();
    }

    fn push_close(&mut self, record: &CloseRecord) {
        self.push(
            RecordLevel::Info,
            close_line(
                &record.host,
                record.port,
                record.log_fd,
                record.duration_ms,
                record.bytes_sent,
            ),
        );
    }

    fn push_totals(&mut self, now: MonoMs) {
        let total_ms = totals_ms(&self.stats, &self.queue, now);
        self.push(
            RecordLevel::Info,
            totals_line(self.stats.connects, total_ms, self.stats.bytes_sent),
        );
    }

    fn push(&mut self, level: RecordLevel, body: impl Into<String>) {
        self.log_lines.push(LogRecord {
            level,
            body: body.into(),
        });
    }
}

fn clear_signals(signals: &mut Signals) {
    signals.stop = false;
    signals.reload = false;
    signals.dump_stats = false;
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;
    use crate::config::{BindFamily, LogLevel};
    use crate::queue::admit;
    use crate::send::IoError;

    const FIRST: &[u8] = b"SK<j/-\"W!\r\n";
    const AFTER_FIRST: u32 = 267_834_847;

    struct Fake {
        polls: Vec<(bool, PollTimeout)>,
        poll_results: VecDeque<Result<bool, PollError>>,
        accepts: u32,
        accept_results: VecDeque<Result<Accepted, AcceptError>>,
        writes: Vec<(u64, Vec<u8>)>,
        write_results: VecDeque<Result<usize, IoError>>,
        closes: Vec<u64>,
        rebinds: Vec<(u16, BindFamily)>,
        rebind_error: Option<String>,
        accept_at: Option<MonoMs>,
    }

    impl Fake {
        fn new() -> Self {
            Self {
                polls: Vec::new(),
                poll_results: VecDeque::new(),
                accepts: 0,
                accept_results: VecDeque::new(),
                writes: Vec::new(),
                write_results: VecDeque::new(),
                closes: Vec::new(),
                rebinds: Vec::new(),
                rebind_error: None,
                accept_at: None,
            }
        }
    }

    impl Reactor for Fake {
        fn poll(&mut self, watch_listener: bool, timeout: PollTimeout) -> Result<bool, PollError> {
            self.polls.push((watch_listener, timeout));
            self.poll_results.pop_front().unwrap_or(Ok(false))
        }

        fn accept(&mut self) -> Result<Accepted, AcceptError> {
            self.accepts += 1;
            self.accept_results
                .pop_front()
                .expect("accept has no scripted result")
        }

        fn write(&mut self, id: u64, buf: &[u8]) -> Result<usize, IoError> {
            self.writes.push((id, buf.to_vec()));
            self.write_results
                .pop_front()
                .expect("write has no scripted result")
        }

        fn close(&mut self, id: u64) {
            self.closes.push(id);
        }

        fn rebind(&mut self, port: u16, family: BindFamily) -> Result<(), String> {
            self.rebinds.push((port, family));
            match &self.rebind_error {
                Some(message) => Err(message.clone()),
                None => Ok(()),
            }
        }

        fn accept_stamp(&self, turn_now: MonoMs) -> MonoMs {
            self.accept_at.unwrap_or(turn_now)
        }
    }

    struct TempFile {
        path: PathBuf,
    }

    impl TempFile {
        fn new(name: &str, body: &str) -> Self {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "russhpit-{name}-{}-{nanos}.cfg",
                std::process::id()
            ));
            std::fs::write(&path, body).unwrap();
            Self { path }
        }

        fn write(&self, body: &str) {
            std::fs::write(&self.path, body).unwrap();
        }
    }

    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }

    fn harness(config: Config, path: &Path) -> Tarpit<Fake> {
        Tarpit::new(Fake::new(), config, path, BannerRng::new(1), MonoMs(0))
    }

    struct Seed {
        id: u64,
        log_fd: i32,
        host: &'static str,
        port: u16,
        connected_at: u64,
        send_next: u64,
        bytes: u64,
    }

    impl Seed {
        fn at(id: u64, send_next: u64) -> Self {
            Self {
                id,
                log_fd: 4,
                host: "a",
                port: 1,
                connected_at: 0,
                send_next,
                bytes: 0,
            }
        }
    }

    fn plant(tarpit: &mut Tarpit<Fake>, seed: Seed) {
        let mut client = admit(
            seed.id,
            seed.log_fd,
            seed.host,
            seed.port,
            MonoMs(seed.connected_at),
            1,
        );
        client.send_next = MonoMs(seed.send_next);
        client.bytes_sent = seed.bytes;
        tarpit.stats.bytes_sent = tarpit.stats.bytes_sent.saturating_add(seed.bytes);
        tarpit.pending.insert(seed.id, PendingLine::new());
        tarpit.queue.append(client);
    }

    fn info_bodies(tarpit: &Tarpit<Fake>) -> Vec<&str> {
        tarpit
            .log_lines
            .iter()
            .filter(|line| line.level == RecordLevel::Info)
            .map(|line| line.body.as_str())
            .collect()
    }

    fn field_millis(body: &str, key: &str) -> u64 {
        let value = body
            .split(key)
            .nth(1)
            .unwrap_or_else(|| panic!("missing {key} in {body}"))
            .split_whitespace()
            .next()
            .unwrap_or_else(|| panic!("missing value for {key} in {body}"));
        let (seconds, millis) = value
            .split_once('.')
            .unwrap_or_else(|| panic!("missing fraction in {value}"));
        seconds.parse::<u64>().unwrap() * 1000 + millis.parse::<u64>().unwrap()
    }

    #[test]
    fn idle_turn_polls_forever_and_watches_the_listener() {
        let mut tarpit = harness(Config::default(), Path::new("unused"));
        let mut signals = Signals::default();
        let turn = tarpit.turn(MonoMs(0), &mut signals);
        assert_eq!(turn, Turn::Continue);
        assert_eq!(tarpit.reactor.polls, vec![(true, PollTimeout::Forever)]);
        assert!(tarpit.log_lines.is_empty());
        assert_eq!(tarpit.reactor.accepts, 0);
        assert_eq!(tarpit.reactor.writes, Vec::<(u64, Vec<u8>)>::new());
        assert_eq!(signals, Signals::default());
    }

    #[test]
    fn full_queue_polls_without_watching_the_listener() {
        let config = Config {
            max_clients: 2,
            ..Config::default()
        };
        let mut tarpit = harness(config, Path::new("unused"));
        plant(&mut tarpit, Seed::at(1, 80));
        plant(
            &mut tarpit,
            Seed {
                log_fd: 5,
                host: "b",
                port: 2,
                ..Seed::at(2, 90)
            },
        );
        let mut signals = Signals::default();
        let turn = tarpit.turn(MonoMs(30), &mut signals);
        assert_eq!(turn, Turn::Continue);
        assert_eq!(
            tarpit.reactor.polls,
            vec![(false, PollTimeout::AfterMs(50))]
        );
        assert_eq!(tarpit.reactor.accepts, 0);
        assert!(tarpit.reactor.writes.is_empty());
        assert_eq!(tarpit.queue.len(), 2);
    }

    #[test]
    fn due_client_is_written_the_seed_1_line_and_rescheduled_by_one_delay() {
        let config = Config {
            delay_ms: 10_000,
            ..Config::default()
        };
        let mut tarpit = harness(config, Path::new("unused"));
        plant(&mut tarpit, Seed::at(3, 40));
        tarpit.reactor.write_results.push_back(Ok(FIRST.len()));
        let turn = tarpit.turn(MonoMs(40), &mut Signals::default());
        assert_eq!(turn, Turn::Continue);
        assert_eq!(tarpit.reactor.writes, vec![(3, FIRST.to_vec())]);
        assert_eq!(tarpit.rng.state(), AFTER_FIRST);
        assert_eq!(tarpit.stats.bytes_sent, 11);
        assert!(tarpit.reactor.closes.is_empty());
        let client = tarpit.queue.pop_front().unwrap();
        assert_eq!(client.id, 3);
        assert_eq!(client.send_next, MonoMs(10_040));
        assert_eq!(client.bytes_sent, 11);
        assert!(tarpit.queue.is_empty());
        assert!(tarpit.pending.contains_key(&3));
    }

    #[test]
    fn partial_write_is_continued_on_the_next_due_turn_without_a_new_line() {
        let config = Config {
            delay_ms: 80,
            ..Config::default()
        };
        let mut tarpit = harness(config, Path::new("unused"));
        plant(&mut tarpit, Seed::at(3, 0));
        tarpit.reactor.write_results.push_back(Ok(4));
        assert_eq!(
            tarpit.turn(MonoMs(0), &mut Signals::default()),
            Turn::Continue
        );
        assert_eq!(tarpit.rng.state(), AFTER_FIRST);
        assert_eq!(tarpit.stats.bytes_sent, 4);

        tarpit.reactor.write_results.push_back(Ok(7));
        assert_eq!(
            tarpit.turn(MonoMs(80), &mut Signals::default()),
            Turn::Continue
        );
        assert_eq!(
            tarpit.reactor.writes,
            vec![(3, FIRST.to_vec()), (3, FIRST[4..].to_vec())]
        );
        assert_eq!(tarpit.rng.state(), AFTER_FIRST);
        assert_eq!(tarpit.stats.bytes_sent, 11);
        assert!(tarpit.reactor.closes.is_empty());
        let client = tarpit.queue.pop_front().unwrap();
        assert_eq!(client.send_next, MonoMs(160));
        assert_eq!(client.bytes_sent, 11);
        assert!(tarpit.pending.contains_key(&3));
    }

    #[test]
    fn would_block_reschedules_a_full_delay() {
        let config = Config {
            delay_ms: 500,
            ..Config::default()
        };
        let mut tarpit = harness(config, Path::new("unused"));
        plant(&mut tarpit, Seed::at(6, 20));
        tarpit
            .reactor
            .write_results
            .push_back(Err(IoError::WouldBlock));
        assert_eq!(
            tarpit.turn(MonoMs(20), &mut Signals::default()),
            Turn::Continue
        );
        assert_eq!(tarpit.stats.bytes_sent, 0);
        assert_eq!(tarpit.rng.state(), AFTER_FIRST);
        let client = tarpit.queue.pop_front().unwrap();
        assert_eq!(client.send_next, MonoMs(520));
        assert_eq!(client.bytes_sent, 0);
        tarpit.queue.append(client);

        tarpit.reactor.write_results.push_back(Ok(FIRST.len()));
        assert_eq!(
            tarpit.turn(MonoMs(520), &mut Signals::default()),
            Turn::Continue
        );
        assert_eq!(
            tarpit.reactor.writes,
            vec![(6, FIRST.to_vec()), (6, FIRST.to_vec())]
        );
        assert_eq!(tarpit.rng.state(), AFTER_FIRST);
        let client = tarpit.queue.pop_front().unwrap();
        assert_eq!(client.send_next, MonoMs(1_020));
        assert!(tarpit.reactor.closes.is_empty());
    }

    #[test]
    fn fatal_write_closes_and_logs_close_without_requeue() {
        let mut tarpit = harness(Config::default(), Path::new("unused"));
        plant(
            &mut tarpit,
            Seed {
                log_fd: 7,
                host: "198.51.100.9",
                port: 4000,
                connected_at: 100,
                ..Seed::at(8, 100)
            },
        );
        tarpit.reactor.write_results.push_back(Err(IoError::Fatal));
        let turn = tarpit.turn(MonoMs(350), &mut Signals::default());
        assert_eq!(turn, Turn::Continue);
        assert!(tarpit.queue.is_empty());
        assert!(!tarpit.pending.contains_key(&8));
        assert_eq!(tarpit.reactor.closes, vec![8]);
        assert_eq!(
            info_bodies(&tarpit),
            vec!["CLOSE host=198.51.100.9 port=4000 fd=7 time=0.250 bytes=0"]
        );
        assert_eq!(tarpit.stats.bytes_sent, 0);
        assert_eq!(tarpit.stats.closed_ms, 250);
    }

    #[test]
    fn accept_logs_and_increments_connects_and_is_not_due_until_delay() {
        let config = Config {
            log_level: LogLevel::Quiet,
            delay_ms: 10_000,
            ..Config::default()
        };
        let mut tarpit = harness(config, Path::new("unused"));
        tarpit.reactor.poll_results.push_back(Ok(true));
        tarpit.reactor.accept_results.push_back(Ok(Accepted {
            id: 4,
            log_fd: 4,
            host: "203.0.113.8".to_string(),
            port: 51514,
        }));
        assert_eq!(
            tarpit.turn(MonoMs(1_000), &mut Signals::default()),
            Turn::Continue
        );
        assert_eq!(tarpit.stats.connects, 1);
        assert_eq!(
            info_bodies(&tarpit),
            vec!["ACCEPT host=203.0.113.8 port=51514 fd=4 n=1/4096"]
        );
        assert_eq!(tarpit.log_lines[0].level, RecordLevel::Info);
        assert!(tarpit.reactor.writes.is_empty());
        assert_eq!(tarpit.reactor.polls, vec![(true, PollTimeout::Forever)]);

        assert_eq!(
            tarpit.turn(MonoMs(1_000), &mut Signals::default()),
            Turn::Continue
        );
        assert!(tarpit.reactor.writes.is_empty());
        assert_eq!(
            tarpit.reactor.polls[1],
            (true, PollTimeout::AfterMs(10_000))
        );
        assert_eq!(tarpit.stats.connects, 1);

        tarpit.reactor.write_results.push_back(Ok(FIRST.len()));
        assert_eq!(
            tarpit.turn(MonoMs(11_000), &mut Signals::default()),
            Turn::Continue
        );
        assert_eq!(tarpit.reactor.writes, vec![(4, FIRST.to_vec())]);
        let client = tarpit.queue.pop_front().unwrap();
        assert_eq!(client.send_next, MonoMs(21_000));
        assert_eq!(client.connected_at, MonoMs(1_000));
    }

    #[test]
    fn accept_stores_the_reactor_stamp_and_reschedule_keeps_the_turn_stamp() {
        let config = Config {
            delay_ms: 100,
            ..Config::default()
        };
        let mut tarpit = harness(config, Path::new("unused"));
        plant(&mut tarpit, Seed::at(3, 40));
        tarpit.reactor.write_results.push_back(Ok(FIRST.len()));
        tarpit.reactor.poll_results.push_back(Ok(true));
        tarpit.reactor.accept_results.push_back(Ok(Accepted {
            id: 4,
            log_fd: 9,
            host: "b".to_string(),
            port: 2,
        }));
        tarpit.reactor.accept_at = Some(MonoMs(5_000));
        assert_eq!(
            tarpit.turn(MonoMs(40), &mut Signals::default()),
            Turn::Continue
        );
        let due = tarpit.queue.pop_front().unwrap();
        let accepted = tarpit.queue.pop_front().unwrap();
        assert_eq!(due.id, 3);
        assert_eq!(due.send_next, MonoMs(140));
        assert_eq!(accepted.id, 4);
        assert_eq!(accepted.connected_at, MonoMs(5_000));
        assert_eq!(accepted.send_next, MonoMs(5_100));
        assert!(tarpit.queue.is_empty());
    }

    #[test]
    fn failed_accept_does_not_increment_connects() {
        let mut tarpit = harness(Config::default(), Path::new("unused"));
        tarpit.stats.connects = 4;
        tarpit.reactor.poll_results.push_back(Ok(true));
        tarpit
            .reactor
            .accept_results
            .push_back(Err(AcceptError::Transient("no buffers".to_string())));
        let turn = tarpit.turn(MonoMs(0), &mut Signals::default());
        assert_eq!(turn, Turn::Continue);
        assert_eq!(tarpit.stats.connects, 4);
        assert!(tarpit.queue.is_empty());
        assert_eq!(
            tarpit.log_lines,
            vec![LogRecord {
                level: RecordLevel::Warning,
                body: "russhpit: warning: no buffers".to_string(),
            }]
        );
    }

    #[test]
    fn fd_limit_with_clients_lowers_cap_until_reload() {
        let file = TempFile::new("cap", "MaxClients 30\n");
        let config = Config {
            max_clients: 10,
            ..Config::default()
        };
        let mut tarpit = harness(config, &file.path);
        plant(&mut tarpit, Seed::at(1, 5_000));
        plant(
            &mut tarpit,
            Seed {
                log_fd: 5,
                host: "b",
                port: 2,
                ..Seed::at(2, 6_000)
            },
        );
        tarpit.reactor.poll_results.push_back(Ok(true));
        tarpit
            .reactor
            .accept_results
            .push_back(Err(AcceptError::ProcessFdLimit));
        assert_eq!(
            tarpit.turn(MonoMs(1_000), &mut Signals::default()),
            Turn::Continue
        );
        assert_eq!(tarpit.config.max_clients, 2);
        assert_eq!(tarpit.stats.connects, 0);
        assert_eq!(info_bodies(&tarpit), vec!["MaxClients 2"]);
        assert_eq!(tarpit.log_lines[0].level, RecordLevel::Info);

        assert_eq!(
            tarpit.turn(MonoMs(1_000), &mut Signals::default()),
            Turn::Continue
        );
        assert_eq!(
            tarpit.reactor.polls[1],
            (false, PollTimeout::AfterMs(4_000))
        );

        let mut signals = Signals {
            reload: true,
            ..Signals::default()
        };
        assert_eq!(tarpit.turn(MonoMs(1_000), &mut signals), Turn::Continue);
        assert!(!signals.reload);
        assert_eq!(tarpit.config().max_clients, 30);
        assert!(tarpit.reactor.rebinds.is_empty());
        assert!(tarpit.reactor.polls[2].0);
        assert!(info_bodies(&tarpit).contains(&"MaxClients 30"));
        assert_eq!(tarpit.queue.len(), 2);
        assert_eq!(tarpit.queue.pop_front().unwrap().id, 1);
        assert_eq!(tarpit.queue.pop_front().unwrap().id, 2);
    }

    #[test]
    fn fd_limit_with_empty_queue_waits_one_delay_then_watches_again() {
        let config = Config {
            delay_ms: 2_500,
            max_clients: 8,
            ..Config::default()
        };
        let mut tarpit = harness(config, Path::new("unused"));
        tarpit.reactor.poll_results.push_back(Ok(true));
        tarpit
            .reactor
            .accept_results
            .push_back(Err(AcceptError::SystemFdLimit));
        assert_eq!(
            tarpit.turn(MonoMs(10), &mut Signals::default()),
            Turn::Continue
        );
        assert_eq!(tarpit.config.max_clients, 8);
        assert_eq!(tarpit.stats.connects, 0);
        assert_eq!(
            tarpit.log_lines,
            vec![LogRecord {
                level: RecordLevel::Warning,
                body: "russhpit: warning: accept deferred".to_string(),
            }]
        );

        assert_eq!(
            tarpit.turn(MonoMs(10), &mut Signals::default()),
            Turn::Continue
        );
        assert_eq!(
            tarpit.turn(MonoMs(10), &mut Signals::default()),
            Turn::Continue
        );
        assert_eq!(
            tarpit.reactor.polls,
            vec![
                (true, PollTimeout::Forever),
                (false, PollTimeout::AfterMs(2_500)),
                (true, PollTimeout::Forever),
            ]
        );
        assert_eq!(tarpit.reactor.accepts, 1);
        assert_eq!(tarpit.config.max_clients, 8);
    }

    #[test]
    fn transient_accept_warns_and_continues() {
        let mut tarpit = harness(Config::default(), Path::new("unused"));
        tarpit.reactor.poll_results.push_back(Ok(true));
        tarpit
            .reactor
            .accept_results
            .push_back(Err(AcceptError::Transient(
                "connection aborted".to_string(),
            )));
        let turn = tarpit.turn(MonoMs(0), &mut Signals::default());
        assert_eq!(turn, Turn::Continue);
        assert_eq!(tarpit.stats.connects, 0);
        assert!(tarpit.queue.is_empty());
        assert_eq!(
            tarpit.log_lines,
            vec![LogRecord {
                level: RecordLevel::Warning,
                body: "russhpit: warning: connection aborted".to_string(),
            }]
        );
    }

    #[test]
    fn fatal_accept_returns_fatal() {
        let mut tarpit = harness(Config::default(), Path::new("unused"));
        tarpit.reactor.poll_results.push_back(Ok(true));
        tarpit
            .reactor
            .accept_results
            .push_back(Err(AcceptError::Fatal("listener closed".to_string())));
        let turn = tarpit.turn(MonoMs(0), &mut Signals::default());
        assert_eq!(turn, Turn::Fatal("listener closed".to_string()));
        assert_eq!(tarpit.stats.connects, 0);
        assert!(tarpit.queue.is_empty());
        assert!(tarpit.log_lines.is_empty());
    }

    #[test]
    fn poll_interrupt_does_not_accept() {
        let mut tarpit = harness(Config::default(), Path::new("unused"));
        tarpit
            .reactor
            .poll_results
            .push_back(Err(PollError::Interrupted));
        tarpit.reactor.accept_results.push_back(Ok(Accepted {
            id: 1,
            log_fd: 3,
            host: "h".to_string(),
            port: 1,
        }));
        let turn = tarpit.turn(MonoMs(0), &mut Signals::default());
        assert_eq!(turn, Turn::Continue);
        assert_eq!(tarpit.reactor.accepts, 0);
        assert_eq!(tarpit.stats.connects, 0);
        assert!(tarpit.queue.is_empty());
    }

    #[test]
    fn reload_delay_does_not_rebind_and_does_not_reorder() {
        let file = TempFile::new(
            "delay",
            "Delay 15\nMaxLineLength 9\nMaxClients 3\nLogLevel 2\n",
        );
        let mut tarpit = harness(Config::default(), &file.path);
        plant(&mut tarpit, Seed::at(1, 100));
        plant(
            &mut tarpit,
            Seed {
                log_fd: 5,
                host: "b",
                port: 2,
                ..Seed::at(2, 200)
            },
        );
        let mut signals = Signals {
            reload: true,
            ..Signals::default()
        };
        assert_eq!(tarpit.turn(MonoMs(0), &mut signals), Turn::Continue);
        assert!(!signals.reload);
        assert!(tarpit.reactor.rebinds.is_empty());
        assert_eq!(tarpit.config.delay_ms, 15);
        assert_eq!(tarpit.config.max_line_length, 9);
        assert_eq!(tarpit.config.max_clients, 3);
        assert_eq!(tarpit.config.log_level, LogLevel::Debug);
        assert_eq!(tarpit.config.port, 2222);
        assert_eq!(tarpit.config.bind_family, BindFamily::Dual);
        assert_eq!(
            info_bodies(&tarpit),
            vec![
                "Port 2222",
                "Delay 15",
                "MaxLineLength 9",
                "MaxClients 3",
                "BindFamily IPv4 Mapped IPv6",
            ]
        );
        let first = tarpit.queue.pop_front().unwrap();
        let second = tarpit.queue.pop_front().unwrap();
        assert_eq!(first.id, 1);
        assert_eq!(first.send_next, MonoMs(100));
        assert_eq!(second.id, 2);
        assert_eq!(second.send_next, MonoMs(200));
    }

    #[test]
    fn reload_port_rebinds_and_failed_rebind_keeps_the_old_port() {
        let file = TempFile::new("port", "Port 9\nBindFamily 4\n");
        let mut tarpit = harness(Config::default(), &file.path);
        let mut signals = Signals {
            reload: true,
            ..Signals::default()
        };
        assert_eq!(tarpit.turn(MonoMs(0), &mut signals), Turn::Continue);
        assert_eq!(tarpit.config.port, 9);
        assert_eq!(tarpit.config.bind_family, BindFamily::V4);
        assert_eq!(tarpit.reactor.rebinds, vec![(9, BindFamily::V4)]);
        assert_eq!(
            info_bodies(&tarpit),
            Config {
                port: 9,
                bind_family: BindFamily::V4,
                ..Config::default()
            }
            .echo_lines()
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
        );

        file.write("Port 10\nBindFamily 6\nDelay 77\nMaxClients 12\n");
        tarpit.reactor.rebind_error = Some("listener bind failed".to_string());
        tarpit.take_log_lines();
        signals.reload = true;
        assert_eq!(tarpit.turn(MonoMs(0), &mut signals), Turn::Continue);
        assert_eq!(tarpit.config.port, 9);
        assert_eq!(tarpit.config.bind_family, BindFamily::V4);
        assert_eq!(tarpit.config.delay_ms, 77);
        assert_eq!(tarpit.config.max_clients, 12);
        assert_eq!(
            tarpit.reactor.rebinds,
            vec![(9, BindFamily::V4), (10, BindFamily::V6)]
        );
        let echoed = Config {
            port: 10,
            bind_family: BindFamily::V6,
            delay_ms: 77,
            max_clients: 12,
            ..Config::default()
        };
        let mut expected: Vec<String> = echoed.echo_lines().into_iter().collect();
        expected.push("listener bind failed".to_string());
        assert_eq!(
            tarpit
                .log_lines
                .iter()
                .map(|line| line.body.clone())
                .collect::<Vec<_>>(),
            expected
        );
        assert_eq!(tarpit.log_lines.last().unwrap().level, RecordLevel::Error);
        assert!(
            tarpit.log_lines[..5]
                .iter()
                .all(|line| line.level == RecordLevel::Info)
        );
    }

    #[test]
    fn reload_restores_a_shrunk_max_clients() {
        let file = TempFile::new("restore", "MaxClients 30\n");
        let config = Config {
            max_clients: 10,
            ..Config::default()
        };
        let mut tarpit = harness(config, &file.path);
        plant(&mut tarpit, Seed::at(1, 9_000));
        plant(
            &mut tarpit,
            Seed {
                log_fd: 5,
                host: "b",
                port: 2,
                ..Seed::at(2, 9_000)
            },
        );
        tarpit.reactor.poll_results.push_back(Ok(true));
        tarpit
            .reactor
            .accept_results
            .push_back(Err(AcceptError::ProcessFdLimit));
        assert_eq!(
            tarpit.turn(MonoMs(0), &mut Signals::default()),
            Turn::Continue
        );
        assert_eq!(tarpit.config.max_clients, 2);

        let mut signals = Signals {
            reload: true,
            ..Signals::default()
        };
        assert_eq!(tarpit.turn(MonoMs(0), &mut signals), Turn::Continue);
        assert_eq!(tarpit.config.max_clients, 30);
        assert!(tarpit.reactor.rebinds.is_empty());
        assert_eq!(tarpit.config.port, 2222);
        assert!(info_bodies(&tarpit).contains(&"MaxClients 30"));
    }

    #[test]
    fn sigusr1_totals_include_an_open_client() {
        let config = Config {
            delay_ms: 50_000,
            ..Config::default()
        };
        let mut tarpit = harness(config, Path::new("unused"));
        tarpit.reactor.poll_results.push_back(Ok(true));
        tarpit.reactor.accept_results.push_back(Ok(Accepted {
            id: 1,
            log_fd: 6,
            host: "2001:db8::1".to_string(),
            port: 22,
        }));
        assert_eq!(
            tarpit.turn(MonoMs(2_000), &mut Signals::default()),
            Turn::Continue
        );
        let mut signals = Signals {
            dump_stats: true,
            ..Signals::default()
        };
        assert_eq!(tarpit.turn(MonoMs(5_500), &mut signals), Turn::Continue);
        assert!(!signals.dump_stats);
        assert_eq!(tarpit.queue.len(), 1);
        assert_eq!(tarpit.stats.closed_ms, 0);
        assert_eq!(tarpit.stats.connects, 1);
        assert_eq!(
            info_bodies(&tarpit),
            vec![
                "ACCEPT host=2001:db8::1 port=22 fd=6 n=1/4096",
                "TOTALS connects=1 seconds=3.500 bytes=0",
            ]
        );
    }

    #[test]
    fn stop_drains_then_totals_once() {
        let mut tarpit = harness(Config::default(), Path::new("unused"));
        plant(
            &mut tarpit,
            Seed {
                log_fd: 7,
                host: "a.example",
                bytes: 4,
                ..Seed::at(1, 9_000)
            },
        );
        plant(
            &mut tarpit,
            Seed {
                log_fd: 8,
                host: "b.example",
                port: 2,
                connected_at: 400,
                bytes: 9,
                ..Seed::at(2, 9_000)
            },
        );
        let mut signals = Signals {
            stop: true,
            ..Signals::default()
        };
        let turn = tarpit.turn(MonoMs(2_500), &mut signals);
        assert_eq!(turn, Turn::Stop);
        assert_eq!(signals, Signals::default());
        assert!(tarpit.queue.is_empty());
        assert!(tarpit.pending.is_empty());
        assert_eq!(tarpit.reactor.closes, vec![1, 2]);
        assert!(tarpit.reactor.polls.is_empty());
        let bodies = info_bodies(&tarpit);
        assert_eq!(
            bodies,
            vec![
                "CLOSE host=a.example port=1 fd=7 time=2.500 bytes=4",
                "CLOSE host=b.example port=2 fd=8 time=2.100 bytes=9",
                "TOTALS connects=0 seconds=4.600 bytes=13",
            ]
        );
        let close_sum: u64 = bodies
            .iter()
            .filter(|body| body.starts_with("CLOSE "))
            .map(|body| field_millis(body, "time="))
            .sum();
        let totals: Vec<_> = bodies
            .iter()
            .filter(|body| body.starts_with("TOTALS "))
            .collect();
        assert_eq!(totals.len(), 1);
        assert_eq!(field_millis(totals[0], "seconds="), close_sum);
        assert_eq!(tarpit.stats.closed_ms, close_sum);
    }

    #[test]
    fn stop_discards_pending_reload_and_stats_then_drains() {
        let file = TempFile::new("discard", "Port 55\nDelay 1\n");
        let mut tarpit = harness(Config::default(), &file.path);
        plant(
            &mut tarpit,
            Seed {
                log_fd: 3,
                host: "c",
                port: 9,
                ..Seed::at(9, 8_000)
            },
        );
        let mut signals = Signals {
            stop: true,
            reload: true,
            dump_stats: true,
        };
        let turn = tarpit.turn(MonoMs(1_000), &mut signals);
        assert_eq!(turn, Turn::Stop);
        assert_eq!(signals, Signals::default());
        assert_eq!(tarpit.config.port, 2222);
        assert_eq!(tarpit.config.delay_ms, 10_000);
        assert!(tarpit.reactor.rebinds.is_empty());
        assert!(tarpit.reactor.polls.is_empty());
        assert_eq!(tarpit.reactor.closes, vec![9]);
        let bodies = info_bodies(&tarpit);
        assert_eq!(bodies.len(), 2);
        assert!(bodies[0].starts_with("CLOSE "));
        assert!(bodies[1].starts_with("TOTALS "));
        assert_eq!(
            bodies
                .iter()
                .filter(|body| body.starts_with("TOTALS "))
                .count(),
            1
        );
        assert!(bodies.iter().all(|body| !body.starts_with("Port ")));
        assert!(bodies.iter().all(|body| !body.starts_with("Delay ")));
        assert!(tarpit.pending.is_empty());
        assert!(tarpit.queue.is_empty());
    }
}
