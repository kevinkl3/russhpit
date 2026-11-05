//! Client queue, due-time selection, and log counters.
//!
//! Clients stay in arrival order.
//! Only the head client can become due.
//! Time values are monotonic milliseconds.

use std::collections::VecDeque;
use std::ops::{Add, Sub};

/// Monotonic time in milliseconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct MonoMs(pub u64);

impl Add<u64> for MonoMs {
    type Output = Self;

    /// The function adds `delay_ms` to this stamp.
    /// The sum is at most `u64::MAX`.
    fn add(self, delay_ms: u64) -> Self {
        Self(self.0.saturating_add(delay_ms))
    }
}

impl Sub for MonoMs {
    type Output = u64;

    /// The function returns the milliseconds from `earlier` to this stamp.
    /// The result is 0 when this stamp is not later than `earlier`.
    fn sub(self, earlier: Self) -> u64 {
        self.0.saturating_sub(earlier.0)
    }
}

/// The function returns `delay_ms` as a millisecond count.
/// A negative `delay_ms` returns 0.
fn non_negative_delay(delay_ms: i32) -> u64 {
    u64::try_from(delay_ms).unwrap_or(0)
}

/// The function adds `delay_ms` to `now`.
/// The sum is at most `u64::MAX`.
fn add_delay(now: MonoMs, delay_ms: i32) -> MonoMs {
    now + non_negative_delay(delay_ms)
}

/// One accepted connection.
///
/// `connected_at` is the accept time.
/// `send_next` is the next send time.
/// `bytes_sent` is the byte count for this client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Client {
    pub id: u64,
    pub log_fd: i32,
    pub host: String,
    pub port: u16,
    pub connected_at: MonoMs,
    pub send_next: MonoMs,
    pub bytes_sent: u64,
}

/// Clients in arrival order.
#[derive(Debug, Default)]
pub struct Queue {
    clients: VecDeque<Client>,
}

impl Queue {
    /// The function returns an empty queue.
    pub fn new() -> Self {
        Self {
            clients: VecDeque::new(),
        }
    }

    /// The function adds `client` at the tail.
    pub fn append(&mut self, client: Client) {
        self.clients.push_back(client);
    }

    /// The function removes the head client.
    /// An empty queue returns `None`.
    pub fn pop_front(&mut self) -> Option<Client> {
        self.clients.pop_front()
    }

    /// The function returns the client count.
    pub fn len(&self) -> usize {
        self.clients.len()
    }

    /// The function returns true when the queue has no clients.
    pub fn is_empty(&self) -> bool {
        self.clients.is_empty()
    }

    /// The function removes each due head client.
    ///
    /// A client is due when `send_next` is at or before `now`.
    /// The scan ends at the first client that is not due.
    /// The result order is head first.
    pub fn take_due(&mut self, now: MonoMs) -> Vec<Client> {
        let mut due = Vec::new();
        while self
            .clients
            .front()
            .is_some_and(|head| head.send_next <= now)
        {
            let Some(client) = self.clients.pop_front() else {
                break;
            };
            due.push(client);
        }
        due
    }

    /// The function sets `send_next` to `now` plus `delay_ms`.
    /// The function then adds the client at the tail.
    ///
    /// `now` is the stamp from `take_due`.
    /// A negative `delay_ms` adds 0.
    pub fn reschedule(&mut self, mut client: Client, now: MonoMs, delay_ms: i32) {
        client.send_next = add_delay(now, delay_ms);
        self.append(client);
    }

    /// The function returns the wait until the head client is due.
    ///
    /// An empty queue waits forever.
    /// A finite wait is at most `i32::MAX` milliseconds.
    pub fn timeout_until_head(&self, now: MonoMs) -> PollTimeout {
        let Some(head) = self.clients.front() else {
            return PollTimeout::Forever;
        };
        let remaining = head.send_next - now;
        match i32::try_from(remaining) {
            Ok(ms) => PollTimeout::AfterMs(ms),
            Err(_) => PollTimeout::AfterMs(i32::MAX),
        }
    }

    /// The function returns true when the client count is below `max_clients`.
    ///
    /// A cap at or below the current length returns false.
    /// This function does not remove clients.
    pub fn accepting(&self, max_clients: i32) -> bool {
        let Ok(cap) = usize::try_from(max_clients) else {
            return false;
        };
        self.len() < cap
    }
}

/// Counters for `ACCEPT`, `CLOSE`, and `TOTALS`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Stats {
    pub connects: u64,
    pub closed_ms: u64,
    pub bytes_sent: u64,
}

impl Stats {
    /// The function adds 1 to `connects`.
    /// The count is at most `u64::MAX`.
    pub fn note_accept(&mut self) {
        self.connects = self.connects.saturating_add(1);
    }
}

/// Wait time for the next poll.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PollTimeout {
    /// The queue is empty.
    Forever,
    /// Wait this many milliseconds.
    AfterMs(i32),
}

/// One closed client and the duration added to `closed_ms`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloseRecord {
    pub id: u64,
    pub log_fd: i32,
    pub host: String,
    pub port: u16,
    pub duration_ms: u64,
    pub bytes_sent: u64,
}

/// The function builds a client.
///
/// `connected_at` is `now`.
/// `send_next` is `now` plus `delay_ms`.
/// This function does not change a queue.
/// An empty `host` and port 0 are valid.
/// A negative `delay_ms` adds 0.
pub fn admit(
    id: u64,
    log_fd: i32,
    host: impl Into<String>,
    port: u16,
    now: MonoMs,
    delay_ms: i32,
) -> Client {
    Client {
        id,
        log_fd,
        host: host.into(),
        port,
        connected_at: now,
        send_next: add_delay(now, delay_ms),
        bytes_sent: 0,
    }
}

/// The function adds `now - connected_at` to `closed_ms`.
/// The function does not add `bytes_sent` to the counters.
pub fn close_client(stats: &mut Stats, client: Client, now: MonoMs) -> CloseRecord {
    let duration_ms = now - client.connected_at;
    stats.closed_ms = stats.closed_ms.saturating_add(duration_ms);
    CloseRecord {
        id: client.id,
        log_fd: client.log_fd,
        host: client.host,
        port: client.port,
        duration_ms,
        bytes_sent: client.bytes_sent,
    }
}

/// The function adds `n` to the client and to `stats.bytes_sent`.
pub fn note_bytes(stats: &mut Stats, client: &mut Client, n: u64) {
    client.bytes_sent = client.bytes_sent.saturating_add(n);
    stats.bytes_sent = stats.bytes_sent.saturating_add(n);
}

/// The function returns `closed_ms` plus each open client's duration.
/// This function does not write `closed_ms`.
pub fn totals_ms(stats: &Stats, queue: &Queue, now: MonoMs) -> u64 {
    let mut total = stats.closed_ms;
    for client in &queue.clients {
        total = total.saturating_add(now - client.connected_at);
    }
    total
}

/// The function closes each client from head to tail.
pub fn drain_all(queue: &mut Queue, stats: &mut Stats, now: MonoMs) -> Vec<CloseRecord> {
    let mut records = Vec::with_capacity(queue.len());
    while let Some(client) = queue.pop_front() {
        records.push(close_client(stats, client, now));
    }
    records
}

/// The function formats `ms` as `seconds.milliseconds`.
/// The fraction uses three digits.
fn format_duration(ms: u64) -> String {
    format!("{}.{:03}", ms / 1000, ms % 1000)
}

/// The function returns the `ACCEPT` body.
///
/// `len` is the queue length after the append.
/// The body has no timestamp and no newline.
pub fn accept_line(host: &str, port: u16, log_fd: i32, len: usize, max_clients: i32) -> String {
    format!("ACCEPT host={host} port={port} fd={log_fd} n={len}/{max_clients}")
}

/// The function returns the `CLOSE` body.
///
/// `duration_ms` is this client's duration.
/// The body has no timestamp and no newline.
pub fn close_line(host: &str, port: u16, log_fd: i32, duration_ms: u64, bytes_sent: u64) -> String {
    format!(
        "CLOSE host={host} port={port} fd={log_fd} time={} bytes={bytes_sent}",
        format_duration(duration_ms)
    )
}

/// The function returns the `TOTALS` body.
///
/// `total_ms` includes open clients.
/// The body has no timestamp and no newline.
pub fn totals_line(connects: u64, total_ms: u64, bytes_sent: u64) -> String {
    format!(
        "TOTALS connects={connects} seconds={} bytes={bytes_sent}",
        format_duration(total_ms)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(clients: &[Client]) -> Vec<u64> {
        clients.iter().map(|client| client.id).collect()
    }

    #[test]
    fn fifo_append_and_pop_order() {
        let mut queue = Queue::new();
        queue.append(admit(1, 10, "a", 1, MonoMs(0), 1));
        queue.append(admit(2, 11, "b", 2, MonoMs(0), 1));
        queue.append(admit(3, 12, "c", 3, MonoMs(0), 1));
        assert_eq!(queue.len(), 3);
        assert_eq!(queue.pop_front().unwrap().id, 1);
        assert_eq!(queue.pop_front().unwrap().id, 2);
        assert_eq!(queue.pop_front().unwrap().id, 3);
        assert_eq!(queue.len(), 0);
        assert!(queue.is_empty());
        assert!(queue.pop_front().is_none());
    }

    #[test]
    fn take_due_stops_at_first_future_client() {
        let mut queue = Queue::new();
        let head = admit(1, 10, "a", 1, MonoMs(0), 200);
        let mut later = admit(2, 11, "b", 2, MonoMs(0), 200);
        later.send_next = MonoMs(50);
        queue.append(head);
        queue.append(later);

        let due = queue.take_due(MonoMs(100));
        assert!(due.is_empty());
        assert_eq!(
            ids(&[queue.pop_front().unwrap(), queue.pop_front().unwrap()]),
            vec![1, 2]
        );

        let mut queue = Queue::new();
        queue.append(admit(1, 10, "a", 1, MonoMs(0), 100));
        queue.append(admit(2, 11, "b", 2, MonoMs(0), 500));
        let mut hidden = admit(3, 12, "c", 3, MonoMs(0), 500);
        hidden.send_next = MonoMs(0);
        queue.append(hidden);

        let due = queue.take_due(MonoMs(100));
        assert_eq!(ids(&due), vec![1]);
        assert_eq!(queue.pop_front().unwrap().id, 2);
        assert_eq!(queue.pop_front().unwrap().send_next, MonoMs(0));

        let mut queue = Queue::new();
        queue.append(admit(8, 1, "a", 1, MonoMs(0), 40));
        let due = queue.take_due(MonoMs(40));
        assert_eq!(ids(&due), vec![8]);
        assert!(queue.is_empty());
    }

    #[test]
    fn reschedule_uses_the_given_now_and_goes_to_the_tail() {
        let mut queue = Queue::new();
        queue.append(admit(1, 1, "a", 1, MonoMs(0), 10));
        queue.append(admit(2, 2, "b", 2, MonoMs(0), 10));
        queue.append(admit(3, 3, "c", 3, MonoMs(0), 1_000));

        let now = MonoMs(100);
        let due = queue.take_due(now);
        assert_eq!(ids(&due), vec![1, 2]);
        assert_eq!(queue.len(), 1);

        for client in due {
            queue.reschedule(client, now, 25);
        }

        assert_eq!(queue.len(), 3);
        let waiting = queue.pop_front().unwrap();
        assert_eq!(waiting.id, 3);
        assert_eq!(waiting.send_next, MonoMs(1_000));
        let first = queue.pop_front().unwrap();
        assert_eq!(first.id, 1);
        assert_eq!(first.send_next, MonoMs(125));
        let second = queue.pop_front().unwrap();
        assert_eq!(second.id, 2);
        assert_eq!(second.send_next, MonoMs(125));
    }

    #[test]
    fn admit_sets_first_send_one_delay_ahead() {
        let mut queue = Queue::new();
        let client = admit(9, 3, "", 0, MonoMs(1_000), 10_000);
        assert_eq!(client.id, 9);
        assert_eq!(client.log_fd, 3);
        assert_eq!(client.host, "");
        assert_eq!(client.port, 0);
        assert_eq!(client.connected_at, MonoMs(1_000));
        assert_eq!(client.send_next, MonoMs(11_000));
        assert_eq!(client.bytes_sent, 0);
        assert!(queue.is_empty());

        let saturated = admit(1, 1, "h", 1, MonoMs(u64::MAX - 5), 100);
        assert_eq!(saturated.send_next, MonoMs(u64::MAX));
        let negative = admit(1, 1, "h", 1, MonoMs(20), -5);
        assert_eq!(negative.send_next, MonoMs(20));
        queue.append(client);
        assert_eq!(queue.len(), 1);
    }

    #[test]
    fn timeout_forever_when_empty() {
        let mut queue = Queue::new();
        assert_eq!(queue.timeout_until_head(MonoMs(50)), PollTimeout::Forever);
        queue.append(admit(1, 1, "a", 1, MonoMs(0), 10));
        assert!(queue.pop_front().is_some());
        assert_eq!(queue.timeout_until_head(MonoMs(50)), PollTimeout::Forever);
    }

    #[test]
    fn timeout_saturates_at_i32_max() {
        let mut queue = Queue::new();
        let mut client = admit(1, 1, "a", 1, MonoMs(0), 1);
        client.send_next = MonoMs(10 + i32::MAX as u64);
        queue.append(client);
        assert_eq!(
            queue.timeout_until_head(MonoMs(10)),
            PollTimeout::AfterMs(i32::MAX)
        );

        let mut queue = Queue::new();
        let mut client = admit(1, 1, "a", 1, MonoMs(0), 1);
        client.send_next = MonoMs(10 + i32::MAX as u64 + 50);
        queue.append(client);
        assert_eq!(
            queue.timeout_until_head(MonoMs(10)),
            PollTimeout::AfterMs(i32::MAX)
        );

        let mut queue = Queue::new();
        queue.append(admit(1, 1, "a", 1, MonoMs(0), 80));
        assert_eq!(
            queue.timeout_until_head(MonoMs(30)),
            PollTimeout::AfterMs(50)
        );
    }

    #[test]
    fn accepting_is_strictly_below_cap() {
        let empty = Queue::new();
        assert!(empty.accepting(1));
        assert!(!empty.accepting(0));

        let mut queue = Queue::new();
        queue.append(admit(1, 1, "a", 1, MonoMs(0), 1));
        queue.append(admit(2, 2, "b", 2, MonoMs(0), 1));
        assert!(!queue.accepting(2));
        assert!(queue.accepting(3));
        assert!(!queue.accepting(1));
        assert!(!queue.accepting(-1));
        assert_eq!(queue.len(), 2);
        assert_eq!(queue.pop_front().unwrap().id, 1);
        assert_eq!(queue.pop_front().unwrap().id, 2);
    }

    #[test]
    fn close_adds_duration_once() {
        let mut stats = Stats::default();
        let mut client = admit(1, 4, "198.51.100.4", 22, MonoMs(100), 10);
        note_bytes(&mut stats, &mut client, 15);
        let record = close_client(&mut stats, client, MonoMs(350));
        assert_eq!(record.duration_ms, 250);
        assert_eq!(record.bytes_sent, 15);
        assert_eq!(record.host, "198.51.100.4");
        assert_eq!(stats.closed_ms, 250);
        assert_eq!(stats.bytes_sent, 15);

        let record = close_client(
            &mut stats,
            admit(2, 5, "198.51.100.4", 22, MonoMs(1_000), 10),
            MonoMs(900),
        );
        assert_eq!(record.duration_ms, 0);
        assert_eq!(stats.closed_ms, 250);
        assert_eq!(stats.bytes_sent, 15);
    }

    #[test]
    fn totals_include_open_clients_without_storing_them() {
        let stats = Stats {
            closed_ms: 1_000,
            ..Stats::default()
        };
        let mut queue = Queue::new();
        queue.append(admit(1, 1, "a", 1, MonoMs(100), 10));
        queue.append(admit(2, 2, "b", 2, MonoMs(400), 10));
        let now = MonoMs(1_000);
        assert_eq!(totals_ms(&stats, &queue, now), 2_500);
        assert_eq!(stats.closed_ms, 1_000);
        assert_eq!(queue.len(), 2);
        assert_eq!(totals_ms(&stats, &queue, now), 2_500);
    }

    #[test]
    fn drain_then_totals_does_not_double_count() {
        let mut stats = Stats::default();
        let mut queue = Queue::new();
        queue.append(admit(1, 7, "a", 1, MonoMs(0), 10));
        queue.append(admit(2, 8, "b", 2, MonoMs(200), 10));
        let now = MonoMs(1_000);
        let while_open = totals_ms(&stats, &queue, now);
        assert_eq!(while_open, 1_800);

        let records = drain_all(&mut queue, &mut stats, now);
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].id, 1);
        assert_eq!(records[0].log_fd, 7);
        assert_eq!(records[0].duration_ms, 1_000);
        assert_eq!(records[1].id, 2);
        assert_eq!(records[1].duration_ms, 800);
        assert!(queue.is_empty());
        assert_eq!(stats.closed_ms, 1_800);
        assert_eq!(totals_ms(&stats, &queue, now), stats.closed_ms);
        assert_eq!(totals_ms(&stats, &queue, now), while_open);
        assert!(drain_all(&mut queue, &mut stats, now).is_empty());
        assert_eq!(stats.closed_ms, 1_800);
    }

    #[test]
    fn note_bytes_updates_client_and_global() {
        let mut stats = Stats::default();
        let mut first = admit(1, 1, "a", 1, MonoMs(0), 1);
        let mut second = admit(2, 2, "b", 2, MonoMs(0), 1);
        note_bytes(&mut stats, &mut first, 8);
        note_bytes(&mut stats, &mut first, 2);
        note_bytes(&mut stats, &mut second, 5);
        assert_eq!(first.bytes_sent, 10);
        assert_eq!(second.bytes_sent, 5);
        assert_eq!(stats.bytes_sent, 15);
        assert_eq!(stats.closed_ms, 0);
        assert_eq!(stats.connects, 0);
    }

    #[test]
    fn log_lines_match_the_contract() {
        let client = admit(4, 4, "203.0.113.8", 51514, MonoMs(0), 10_000);
        let mut queue = Queue::new();
        queue.append(client.clone());
        let accept = accept_line(&client.host, client.port, client.log_fd, queue.len(), 4096);
        assert_eq!(accept, "ACCEPT host=203.0.113.8 port=51514 fd=4 n=1/4096");
        assert!(!accept.contains('\n'));

        let mut stats = Stats::default();
        let mut closed = client;
        note_bytes(&mut stats, &mut closed, 32);
        let record = close_client(&mut stats, closed, MonoMs(1005));
        let close = close_line(
            &record.host,
            record.port,
            record.log_fd,
            record.duration_ms,
            record.bytes_sent,
        );
        assert_eq!(
            close,
            "CLOSE host=203.0.113.8 port=51514 fd=4 time=1.005 bytes=32"
        );
        assert!(!close.contains('\n'));

        stats.note_accept();
        stats.note_accept();
        stats.note_accept();
        stats.bytes_sent = 64;
        let totals = totals_line(stats.connects, 1005, stats.bytes_sent);
        assert_eq!(totals, "TOTALS connects=3 seconds=1.005 bytes=64");
        assert!(!totals.contains('\n'));

        assert_eq!(
            accept_line("", 0, 5, 1, 4096),
            "ACCEPT host= port=0 fd=5 n=1/4096"
        );
    }

    #[test]
    fn mono_sub_saturates_at_zero() {
        assert_eq!(MonoMs(10) - MonoMs(10), 0);
        assert_eq!(MonoMs(10) - MonoMs(11), 0);
        assert_eq!(MonoMs(0) - MonoMs(u64::MAX), 0);
        assert_eq!(MonoMs(11) - MonoMs(10), 1);
        assert_eq!(MonoMs(u64::MAX) - MonoMs(0), u64::MAX);
        assert_eq!(MonoMs(u64::MAX) + 1, MonoMs(u64::MAX));
        assert_eq!(MonoMs(u64::MAX - 5) + 10, MonoMs(u64::MAX));
        assert_eq!(add_delay(MonoMs(u64::MAX - 5), 10), MonoMs(u64::MAX));
        assert_eq!(add_delay(MonoMs(20), -5), MonoMs(20));
    }
}
