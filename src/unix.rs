//! The Unix reactor, the log sink, and the signal flags.
//!
//! The listener socket is non-blocking.
//! Stdout logs are line-buffered.
//! Syslog logs have no timestamp.
//! Handlers store atomic flags.
//! The handlers do not set `SA_RESTART`.

use std::collections::HashMap;
use std::ffi::CString;
use std::io::{self, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};
use std::os::fd::{AsRawFd, OwnedFd};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use rustix::event::{PollFd, PollFlags, Timespec};
use rustix::fs::{OFlags, fcntl_getfl, fcntl_setfl};
use rustix::io::Errno;
use rustix::net::sockopt;
use rustix::net::{
    AddressFamily, SendFlags, SocketAddrAny, SocketType, accept, bind, getpeername, getsockname,
    listen, send, socket,
};
use rustix::time::{ClockId, clock_gettime};

use crate::config::{BindFamily, LogLevel};
use crate::queue::{MonoMs, PollTimeout};
use crate::send::IoError;
use crate::tarpit::{AcceptError, Accepted, LogRecord, PollError, Reactor, RecordLevel, Signals};

/// Where one process writes log lines.
pub struct Log {
    level: AtomicU8,
    inner: Mutex<Sink>,
}

enum Sink {
    Stdout,
    Syslog { ident: CString, open: bool },
    Capture(String),
}

#[derive(Clone, Copy)]
enum Kind {
    Info,
    Debug,
}

impl Log {
    /// The function returns a stdout sink.
    ///
    /// Each line is flushed.
    pub fn stdout(level: LogLevel) -> Arc<Self> {
        Arc::new(Self::from_sink(level, Sink::Stdout))
    }

    /// The function returns a syslog sink.
    ///
    /// `ident` is the syslog identity.
    /// The facility is daemon.
    /// The record includes the process id.
    pub fn syslog(level: LogLevel, ident: &str) -> Arc<Self> {
        let ident = CString::new(ident.replace('\0', ""))
            .unwrap_or_else(|_| CString::new("russhpit").expect("identity"));
        let log = Arc::new(Self::from_sink(level, Sink::Syslog { ident, open: false }));
        log.open_syslog();
        log
    }

    /// The function returns an in-memory stdout sink.
    ///
    /// The text uses the stdout line format.
    pub fn capture(level: LogLevel) -> Arc<Self> {
        Arc::new(Self::from_sink(level, Sink::Capture(String::new())))
    }

    fn from_sink(level: LogLevel, sink: Sink) -> Self {
        Self {
            level: AtomicU8::new(level_code(level)),
            inner: Mutex::new(sink),
        }
    }

    fn open_syslog(&self) {
        let mut guard = self.inner.lock().expect("log lock");
        let Sink::Syslog { ident, open } = &mut *guard else {
            return;
        };
        // `openlog` stores this pointer for later `syslog` calls.
        unsafe {
            libc::openlog(ident.as_ptr(), libc::LOG_PID, libc::LOG_DAEMON);
        }
        *open = true;
    }

    /// The function sets the active log level.
    pub fn set_level(&self, level: LogLevel) {
        self.level.store(level_code(level), Ordering::Relaxed);
    }

    /// The function writes one info line when the level allows it.
    pub fn info(&self, body: &str) {
        self.emit(Kind::Info, body);
    }

    /// The function writes one debug line when the level allows it.
    pub fn debug(&self, body: &str) {
        self.emit(Kind::Debug, body);
    }

    /// The function returns the captured stdout text.
    ///
    /// A stdout sink returns an empty string.
    /// A syslog sink returns an empty string.
    pub fn captured_text(&self) -> String {
        match &*self.inner.lock().expect("log lock") {
            Sink::Capture(text) => text.clone(),
            Sink::Stdout | Sink::Syslog { .. } => String::new(),
        }
    }

    /// The function closes syslog when this sink opened it.
    pub fn close(&self) {
        let mut guard = self.inner.lock().expect("log lock");
        let Sink::Syslog { open, .. } = &mut *guard else {
            return;
        };
        if *open {
            unsafe {
                libc::closelog();
            }
            *open = false;
        }
    }

    fn debug_enabled(&self) -> bool {
        self.allows(Kind::Debug)
    }

    fn allows(&self, kind: Kind) -> bool {
        let active = self.level.load(Ordering::Relaxed);
        match kind {
            Kind::Info => active >= 1,
            Kind::Debug => active >= 2,
        }
    }

    fn emit(&self, kind: Kind, body: &str) {
        if !self.allows(kind) {
            return;
        }
        let mut guard = self.inner.lock().expect("log lock");
        match &mut *guard {
            Sink::Stdout => write_stdout(&stamp_line(body)),
            Sink::Capture(text) => text.push_str(&stamp_line(body)),
            Sink::Syslog { open, .. } => {
                if *open {
                    write_syslog(kind, body);
                }
            }
        }
    }
}

impl Drop for Log {
    fn drop(&mut self) {
        self.close();
    }
}

fn level_code(level: LogLevel) -> u8 {
    match level {
        LogLevel::Quiet => 0,
        LogLevel::Standard => 1,
        LogLevel::Debug => 2,
    }
}

fn stamp_line(body: &str) -> String {
    let (secs, millis) = wall_parts();
    format!("{}{body}\n", format_timestamp(secs, millis))
}

fn write_stdout(line: &str) {
    let mut out = io::stdout().lock();
    let _ = out.write_all(line.as_bytes());
    let _ = out.flush();
}

fn write_syslog(kind: Kind, body: &str) {
    let priority = match kind {
        Kind::Info => libc::LOG_INFO,
        Kind::Debug => libc::LOG_DEBUG,
    };
    let text =
        CString::new(body.replace('\0', "")).unwrap_or_else(|_| CString::new("log").expect("text"));
    // The format string is fixed. The body is the next argument.
    unsafe {
        libc::syslog(priority, c"%s".as_ptr(), text.as_ptr());
    }
}

/// The function formats UTC time as `YYYY-MM-DDTHH:MM:SS.mmmZ `.
///
/// `millis` uses three digits.
/// The result ends with one space.
fn format_timestamp(unix_secs: i64, millis: u32) -> String {
    let millis = millis % 1000;
    let days = unix_secs.div_euclid(86_400);
    let tod = unix_secs.rem_euclid(86_400) as u32;
    let hour = tod / 3_600;
    let minute = (tod % 3_600) / 60;
    let second = tod % 60;
    let (year, month, day) = civil_date(days);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{millis:03}Z ")
}

fn wall_parts() -> (i64, u32) {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(duration) => {
            let secs = i64::try_from(duration.as_secs()).unwrap_or(i64::MAX);
            (secs, duration.subsec_millis())
        }
        Err(error) => {
            let duration = error.duration();
            let secs = i64::try_from(duration.as_secs()).unwrap_or(i64::MAX);
            (-secs, duration.subsec_millis())
        }
    }
}

/// The function returns the civil year, month, and day.
///
/// `days` is the count of days since 1970-01-01 UTC.
fn civil_date(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let month_index = (5 * doy + 2) / 153;
    let day = doy - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    let year = if month <= 2 { year + 1 } else { year };
    (year, month as u32, day as u32)
}

/// The function returns monotonic milliseconds.
pub fn mono_now() -> MonoMs {
    let stamp = clock_gettime(ClockId::Monotonic);
    let secs = u64::try_from(stamp.tv_sec).unwrap_or(0);
    let nanos = u64::try_from(stamp.tv_nsec).unwrap_or(0);
    MonoMs(secs.saturating_mul(1_000).saturating_add(nanos / 1_000_000))
}

/// The function formats an IPv6 address like `inet_ntop`.
///
/// The text has no brackets.
/// An IPv4-mapped address keeps the `::ffff:` prefix.
fn format_ipv6(ip: Ipv6Addr) -> String {
    ip.to_string()
}

fn peer_text(fd: &OwnedFd) -> (String, u16) {
    let Ok(Some(addr)) = getpeername(fd) else {
        return (String::new(), 0);
    };
    address_text(addr)
}

fn address_text(addr: SocketAddrAny) -> (String, u16) {
    let Ok(addr) = SocketAddr::try_from(addr) else {
        return (String::new(), 0);
    };
    let host = match addr.ip() {
        IpAddr::V4(ip) => ip.to_string(),
        IpAddr::V6(ip) => format_ipv6(ip),
    };
    (host, addr.port())
}

/// Atomic flags for stop, reload, and stats.
///
/// `take` copies the flags and clears them.
pub struct SignalFlags {
    stop: AtomicBool,
    reload: AtomicBool,
    dump_stats: AtomicBool,
}

static FLAGS: OnceLock<SignalFlags> = OnceLock::new();

impl SignalFlags {
    /// The function returns cleared flags.
    pub fn new() -> Self {
        Self {
            stop: AtomicBool::new(false),
            reload: AtomicBool::new(false),
            dump_stats: AtomicBool::new(false),
        }
    }

    /// The function copies the flags and clears them.
    pub fn take(&self) -> Signals {
        Signals {
            stop: self.stop.swap(false, Ordering::Relaxed),
            reload: self.reload.swap(false, Ordering::Relaxed),
            dump_stats: self.dump_stats.swap(false, Ordering::Relaxed),
        }
    }

    /// The function requests a stop.
    pub fn request_stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }

    fn request_reload(&self) {
        self.reload.store(true, Ordering::Relaxed);
    }

    fn request_stats(&self) {
        self.dump_stats.store(true, Ordering::Relaxed);
    }
}

impl Default for SignalFlags {
    /// The function returns cleared flags.
    fn default() -> Self {
        Self::new()
    }
}

/// The function returns the process signal flags.
///
/// The first call creates the flags.
pub fn process_flags() -> &'static SignalFlags {
    FLAGS.get_or_init(SignalFlags::new)
}

/// The function ignores `SIGPIPE`.
pub fn ignore_sigpipe() -> io::Result<()> {
    install_raw(libc::SIGPIPE, libc::SIG_IGN)
}

/// The function installs the process handlers.
///
/// `SIGTERM` requests a stop.
/// `SIGHUP` requests a reload.
/// `SIGUSR1` requests totals.
/// `SIGPIPE` is ignored.
/// The handlers do not set `SA_RESTART`.
pub fn install_signals() -> io::Result<()> {
    let _flags = process_flags();
    ignore_sigpipe()?;
    install_fn(libc::SIGTERM, on_stop)?;
    install_fn(libc::SIGHUP, on_reload)?;
    install_fn(libc::SIGUSR1, on_stats)?;
    Ok(())
}

extern "C" fn on_stop(_signal: libc::c_int) {
    if let Some(flags) = FLAGS.get() {
        flags.request_stop();
    }
}

extern "C" fn on_reload(_signal: libc::c_int) {
    if let Some(flags) = FLAGS.get() {
        flags.request_reload();
    }
}

extern "C" fn on_stats(_signal: libc::c_int) {
    if let Some(flags) = FLAGS.get() {
        flags.request_stats();
    }
}

fn install_fn(signal: libc::c_int, handler: extern "C" fn(libc::c_int)) -> io::Result<()> {
    install_raw(signal, handler as libc::sighandler_t)
}

fn install_raw(signal: libc::c_int, handler: libc::sighandler_t) -> io::Result<()> {
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = handler;
        action.sa_flags = 0;
        if libc::sigemptyset(&mut action.sa_mask) != 0 {
            return Err(io::Error::last_os_error());
        }
        if libc::sigaction(signal, &action, std::ptr::null_mut()) != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

/// One listening socket.
pub struct Listener {
    fd: OwnedFd,
    notes: Vec<String>,
}

impl Listener {
    /// The function returns the bound port.
    pub fn local_port(&self) -> io::Result<u16> {
        let addr = getsockname(&self.fd)?;
        let Ok(addr) = SocketAddr::try_from(addr) else {
            return Err(io::Error::other("listener address is not ip"));
        };
        Ok(addr.port())
    }
}

/// The function binds `port` for `family`.
///
/// The socket is non-blocking before `bind`.
/// `SO_REUSEADDR` may fail.
/// An IPv6 socket sets `IPV6_V6ONLY` to 0 or 1.
/// An IPv4 socket does not set that option.
/// The function returns an error from socket, bind, or listen.
pub fn bind_listener(port: u16, family: BindFamily) -> io::Result<Listener> {
    bind_socket(port, family)
}

/// The function binds port 0.
///
/// The kernel chooses the port.
/// Config parsing still rejects port 0.
pub fn bind_ephemeral(family: BindFamily) -> io::Result<Listener> {
    bind_socket(0, family)
}

fn bind_socket(port: u16, family: BindFamily) -> io::Result<Listener> {
    let domain = match family {
        BindFamily::V4 => AddressFamily::INET,
        BindFamily::Dual | BindFamily::V6 => AddressFamily::INET6,
    };
    let fd = socket(domain, SocketType::STREAM, None)?;
    set_nonblock(&fd)?;
    let raw = fd.as_raw_fd();
    let mut notes = Vec::new();
    let reuse = sockopt::set_socket_reuseaddr(&fd, true);
    notes.push(option_note(raw, "SO_REUSEADDR", 1, reuse));
    if let Some(value) = v6_only_request(family) {
        let result = sockopt::set_ipv6_v6only(&fd, value == 1);
        notes.push(option_note(raw, "IPV6_V6ONLY", value, result));
    }
    match family {
        BindFamily::V4 => {
            let addr = SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, port);
            bind(&fd, &addr)?;
        }
        BindFamily::Dual | BindFamily::V6 => {
            let addr = SocketAddrV6::new(Ipv6Addr::UNSPECIFIED, port, 0, 0);
            bind(&fd, &addr)?;
        }
    }
    listen(&fd, i32::MAX)?;
    Ok(Listener { fd, notes })
}

fn v6_only_request(family: BindFamily) -> Option<i32> {
    match family {
        BindFamily::V4 => None,
        BindFamily::Dual => Some(0),
        BindFamily::V6 => Some(1),
    }
}

fn option_note(fd: i32, name: &str, value: i32, result: Result<(), Errno>) -> String {
    match result {
        Ok(()) => format!("setsockopt({fd}, {name}, {value}) = 0"),
        Err(error) => format!("setsockopt({fd}, {name}, {value}) = -1 {error}"),
    }
}

fn set_nonblock(fd: &OwnedFd) -> Result<(), Errno> {
    let flags = fcntl_getfl(fd)?;
    fcntl_setfl(fd, flags | OFlags::NONBLOCK)
}

/// The reactor for one listening socket.
pub struct UnixReactor {
    listener: Listener,
    clients: HashMap<u64, OwnedFd>,
    next_id: u64,
    staged: Option<Result<Accepted, AcceptError>>,
    log: Arc<Log>,
}

impl UnixReactor {
    /// The function builds a reactor for `listener`.
    ///
    /// Debug notes from the bind go to `log`.
    pub fn new(mut listener: Listener, log: Arc<Log>) -> Self {
        for note in listener.notes.drain(..) {
            log.debug(&note);
        }
        Self {
            listener,
            clients: HashMap::new(),
            next_id: 1,
            staged: None,
            log,
        }
    }

    fn note(&self, body: &str) {
        if self.log.debug_enabled() {
            self.log.debug(body);
        }
    }
}

enum Intake {
    Ready(Result<Accepted, AcceptError>),
    Stale,
}

impl Reactor for UnixReactor {
    fn poll(&mut self, watch_listener: bool, timeout: PollTimeout) -> Result<bool, PollError> {
        if self.staged.is_some() {
            return Ok(true);
        }
        let spec = timeout_spec(timeout);
        let readable = if watch_listener {
            let mut fds = [PollFd::new(&self.listener.fd, PollFlags::IN)];
            wait_poll(&mut fds, spec.as_ref())?;
            fds[0]
                .revents()
                .intersects(PollFlags::IN | PollFlags::ERR | PollFlags::HUP)
        } else {
            wait_poll(&mut [], spec.as_ref())?;
            false
        };
        if !readable {
            return Ok(false);
        }
        match self.intake() {
            Intake::Stale => {
                self.note("accept() = -1 EAGAIN");
                Ok(false)
            }
            Intake::Ready(result) => {
                self.staged = Some(result);
                Ok(true)
            }
        }
    }

    fn accept(&mut self) -> Result<Accepted, AcceptError> {
        self.staged.take().unwrap_or(Err(AcceptError::Fatal(
            "accept without a connection".to_string(),
        )))
    }

    fn accept_stamp(&self, _turn_now: MonoMs) -> MonoMs {
        mono_now()
    }

    fn write(&mut self, id: u64, buf: &[u8]) -> Result<usize, IoError> {
        let Some(fd) = self.clients.get(&id) else {
            return Err(IoError::Fatal);
        };
        let raw = fd.as_raw_fd();
        match send(fd, buf, send_flags()) {
            Ok(count) => {
                self.note(&format!("write({raw}) = {count}"));
                Ok(count)
            }
            Err(error) if error == Errno::INTR => {
                self.note(&format!("write({raw}) = -1 EINTR"));
                Err(IoError::Interrupted)
            }
            Err(error) if is_again(error) => {
                self.note(&format!("write({raw}) = -1 EAGAIN"));
                Err(IoError::WouldBlock)
            }
            Err(error) => {
                self.note(&format!("write({raw}) = -1 {error}"));
                Err(IoError::Fatal)
            }
        }
    }

    fn close(&mut self, id: u64) {
        let Some(fd) = self.clients.remove(&id) else {
            return;
        };
        self.note(&format!("close({})", fd.as_raw_fd()));
        drop(fd);
    }

    fn rebind(&mut self, port: u16, family: BindFamily) -> Result<(), String> {
        let mut listener = bind_listener(port, family).map_err(|error| error.to_string())?;
        for note in listener.notes.drain(..) {
            self.log.debug(&note);
        }
        self.listener = listener;
        Ok(())
    }
}

impl UnixReactor {
    fn intake(&mut self) -> Intake {
        match accept(&self.listener.fd) {
            Err(error) if is_again(error) => Intake::Stale,
            Err(error) => Intake::Ready(Err(map_accept(error))),
            Ok(fd) => Intake::Ready(self.prepare(fd)),
        }
    }

    fn prepare(&mut self, fd: OwnedFd) -> Result<Accepted, AcceptError> {
        let log_fd = fd.as_raw_fd();
        self.note(&format!("accept() = {log_fd}"));
        suppress_sigpipe(&fd, &self.log);
        let recv = sockopt::set_socket_recv_buffer_size(&fd, 1);
        self.log.debug(&option_note(log_fd, "SO_RCVBUF", 1, recv));
        let (host, port) = peer_text(&fd);
        if let Err(error) = set_nonblock(&fd) {
            drop(fd);
            return Err(AcceptError::Transient(error.to_string()));
        }
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        self.clients.insert(id, fd);
        Ok(Accepted {
            id,
            log_fd,
            host,
            port,
        })
    }
}

fn suppress_sigpipe(fd: &OwnedFd, log: &Log) {
    #[cfg(any(
        target_vendor = "apple",
        target_os = "freebsd",
        target_os = "dragonfly",
        target_os = "netbsd"
    ))]
    {
        if let Err(error) = sockopt::set_socket_nosigpipe(fd, true) {
            log.debug(&option_note(fd.as_raw_fd(), "SO_NOSIGPIPE", 1, Err(error)));
        }
    }
    #[cfg(not(any(
        target_vendor = "apple",
        target_os = "freebsd",
        target_os = "dragonfly",
        target_os = "netbsd"
    )))]
    {
        let _ = (fd, log);
    }
}

fn send_flags() -> SendFlags {
    #[cfg(not(target_vendor = "apple"))]
    {
        SendFlags::NOSIGNAL
    }
    #[cfg(target_vendor = "apple")]
    {
        // rustix hides `MSG_NOSIGNAL` on Apple. libc still defines it.
        SendFlags::from_bits_retain(u32::try_from(libc::MSG_NOSIGNAL).unwrap_or(0))
    }
}

fn map_accept(error: Errno) -> AcceptError {
    let text = error.to_string();
    match error {
        Errno::MFILE => AcceptError::ProcessFdLimit,
        Errno::NFILE => AcceptError::SystemFdLimit,
        Errno::CONNABORTED | Errno::INTR | Errno::NOBUFS | Errno::NOMEM | Errno::PROTO => {
            AcceptError::Transient(text)
        }
        _ => AcceptError::Fatal(text),
    }
}

fn is_again(error: Errno) -> bool {
    error == Errno::AGAIN || error == Errno::WOULDBLOCK
}

fn timeout_spec(timeout: PollTimeout) -> Option<Timespec> {
    match timeout {
        PollTimeout::Forever => None,
        PollTimeout::AfterMs(ms) => {
            let ms = i64::from(ms.max(0));
            Some(Timespec {
                tv_sec: ms / 1_000,
                tv_nsec: ((ms % 1_000) * 1_000_000) as _,
            })
        }
    }
}

fn wait_poll(fds: &mut [PollFd<'_>], timeout: Option<&Timespec>) -> Result<(), PollError> {
    match rustix::event::poll(fds, timeout) {
        Ok(_) => Ok(()),
        Err(error) if error == Errno::INTR => Err(PollError::Interrupted),
        Err(error) => Err(PollError::Fatal(error.to_string())),
    }
}

/// The function writes loop records.
///
/// A warning line goes to stderr.
/// Info and error lines go through `log`.
pub fn publish(lines: Vec<LogRecord>, log: &Log) {
    for line in lines {
        match line.level {
            RecordLevel::Warning => eprintln!("{}", line.body),
            RecordLevel::Info | RecordLevel::Error => log.info(&line.body),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    #[test]
    fn format_timestamp_truncates_millis() {
        assert_eq!(format_timestamp(0, 5), "1970-01-01T00:00:00.005Z ");
    }

    #[test]
    fn quiet_level_emits_nothing() {
        let log = Log::capture(LogLevel::Quiet);
        log.info("ACCEPT host=a port=1 fd=3 n=1/4");
        log.debug("setsockopt(3, IPV6_V6ONLY, 0) = 0");
        assert_eq!(log.captured_text(), "");
    }

    #[test]
    fn standard_level_skips_debug() {
        let log = Log::capture(LogLevel::Standard);
        log.info("ACCEPT host=a port=1 fd=3 n=1/4");
        log.debug("setsockopt(3, IPV6_V6ONLY, 0) = 0");
        let text = log.captured_text();
        assert!(text.contains("Z ACCEPT host=a port=1 fd=3 n=1/4\n"));
        assert!(!text.contains("IPV6_V6ONLY"));
    }

    #[test]
    fn publish_uses_the_level_set_before_the_write() {
        let lines: Vec<LogRecord> = Config::default()
            .echo_lines()
            .into_iter()
            .map(|body| LogRecord {
                level: RecordLevel::Info,
                body,
            })
            .collect();

        let quiet = Log::capture(LogLevel::Quiet);
        publish(lines.clone(), &quiet);
        assert_eq!(quiet.captured_text(), "");
        quiet.set_level(LogLevel::Standard);
        publish(lines.clone(), &quiet);
        let text = quiet.captured_text();
        assert!(text.contains("Port 2222\n"), "{text}");
        assert!(text.contains("Delay 10000\n"), "{text}");

        let standard = Log::capture(LogLevel::Standard);
        standard.set_level(LogLevel::Quiet);
        publish(lines, &standard);
        assert_eq!(standard.captured_text(), "");
    }

    #[test]
    fn ipv4_mapped_text_keeps_the_ffff_prefix() {
        let ip = Ipv6Addr::new(0, 0, 0, 0, 0, 0xffff, 0xc000, 0x0201);
        assert_eq!(format_ipv6(ip), "::ffff:192.0.2.1");
    }

    #[test]
    fn listener_is_nonblocking_and_unwatched_poll_waits() {
        let listener = bind_ephemeral(BindFamily::V4).expect("bind");
        let flags = fcntl_getfl(&listener.fd).expect("flags");
        assert!(flags.contains(OFlags::NONBLOCK));
        let log = Log::capture(LogLevel::Quiet);
        let mut reactor = UnixReactor::new(listener, log);
        let started = std::time::Instant::now();
        let result = reactor.poll(false, PollTimeout::AfterMs(30));
        assert_eq!(result, Ok(false));
        assert!(started.elapsed() >= std::time::Duration::from_millis(20));
    }
}
