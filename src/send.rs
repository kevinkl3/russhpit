//! One pending banner line and the result of one write.
//!
//! The unsent part of the line is the tail.
//! A short write keeps that tail.
//! `Interrupted` tries the same tail again in this attempt.
//! The caller waits one delay after `Pace`.

use crate::banner::BannerRng;

const LINE_CAP: usize = 255;

/// One banner line and the sent cursor.
///
/// The line is idle when the cursor equals the line length.
/// The next attempt may write a new line when the line is idle.
/// The buffer holds 255 bytes.
#[derive(Debug)]
pub struct PendingLine {
    buf: [u8; LINE_CAP],
    len: usize,
    sent: usize,
}

impl PendingLine {
    /// The function returns an idle line.
    pub fn new() -> Self {
        Self {
            buf: [0; LINE_CAP],
            len: 0,
            sent: 0,
        }
    }

    fn is_idle(&self) -> bool {
        self.sent == self.len
    }

    fn tail(&self) -> &[u8] {
        debug_assert!(self.sent <= self.len);
        &self.buf[self.sent..self.len]
    }
}

impl Default for PendingLine {
    /// The function returns an idle line.
    fn default() -> Self {
        Self::new()
    }
}

/// The result of a failed write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IoError {
    /// An interrupt stopped this write.
    Interrupted,
    /// The write cannot complete now.
    WouldBlock,
    /// The write failed.
    Fatal,
}

/// The next action after one write result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// Write the same tail again in this attempt.
    RetryNow,
    /// Wait one delay before the next attempt.
    Pace,
    /// The caller destroys the client.
    Drop,
}

/// The function returns the unsent tail.
///
/// When the line is idle, the function writes one new line.
/// That write sets the sent cursor to 0.
/// When the line has a tail, the function does not call `rng`.
/// The tail has at least 3 bytes.
pub fn prepare<'a>(
    pending: &'a mut PendingLine,
    rng: &mut BannerRng,
    max_line_length: u8,
) -> &'a [u8] {
    if pending.is_idle() {
        let len = rng.write_line(&mut pending.buf, max_line_length);
        pending.len = len;
        pending.sent = 0;
    }
    pending.tail()
}

/// The function applies one write result and returns the next step.
///
/// The function adds a positive count that fits in the tail to both counters.
/// The sent cursor advances by that count.
/// A fitting count, `Ok(0)`, and `WouldBlock` return `Pace`.
/// `Interrupted` returns `RetryNow` and adds no bytes.
/// `Fatal` and a count past the tail return `Drop` and add no bytes.
pub fn observe(
    pending: &mut PendingLine,
    result: Result<usize, IoError>,
    client_bytes: &mut u64,
    total_bytes: &mut u64,
) -> Step {
    match result {
        Err(IoError::Interrupted) => Step::RetryNow,
        Err(IoError::WouldBlock) => Step::Pace,
        Err(IoError::Fatal) => Step::Drop,
        Ok(count) => {
            let remaining = pending.len.saturating_sub(pending.sent);
            if count > remaining {
                return Step::Drop;
            }
            if count > 0 {
                add_bytes(client_bytes, total_bytes, count);
                pending.sent += count;
            }
            Step::Pace
        }
    }
}

/// The function runs one send attempt.
///
/// The function prepares the tail.
/// The function calls `write_once` with that tail.
/// The function applies the write result.
/// `RetryNow` calls `write_once` again with the same tail.
/// The function returns on `Pace` or `Drop`.
pub fn drive_attempt<W>(
    pending: &mut PendingLine,
    rng: &mut BannerRng,
    max_line_length: u8,
    client_bytes: &mut u64,
    total_bytes: &mut u64,
    mut write_once: W,
) -> Step
where
    W: FnMut(&[u8]) -> Result<usize, IoError>,
{
    prepare(pending, rng, max_line_length);
    loop {
        let result = write_once(pending.tail());
        let step = observe(pending, result, client_bytes, total_bytes);
        if step != Step::RetryNow {
            return step;
        }
    }
}

/// The function adds `count` to both counters.
/// Each sum is at most `u64::MAX`.
fn add_bytes(client_bytes: &mut u64, total_bytes: &mut u64, count: usize) {
    let count = count as u64;
    *client_bytes = client_bytes.saturating_add(count);
    *total_bytes = total_bytes.saturating_add(count);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    const FIRST: &[u8] = b"SK<j/-\"W!\r\n";
    const SECOND: &[u8] = b"E[s4X\"vR$j?fS^B:<;o6\r\n";
    const AFTER_FIRST: u32 = 267_834_847;
    const AFTER_SECOND: u32 = 4_147_913_800;

    struct Script {
        results: VecDeque<Result<usize, IoError>>,
        seen: Vec<Vec<u8>>,
    }

    impl Script {
        fn new(results: impl IntoIterator<Item = Result<usize, IoError>>) -> Self {
            Self {
                results: results.into_iter().collect(),
                seen: Vec::new(),
            }
        }

        fn write(&mut self, buf: &[u8]) -> Result<usize, IoError> {
            self.seen.push(buf.to_vec());
            self.results.pop_front().expect("the script has no result")
        }
    }

    fn run(
        pending: &mut PendingLine,
        rng: &mut BannerRng,
        max_line_length: u8,
        client_bytes: &mut u64,
        total_bytes: &mut u64,
        script: &mut Script,
    ) -> Step {
        drive_attempt(
            pending,
            rng,
            max_line_length,
            client_bytes,
            total_bytes,
            |buf| script.write(buf),
        )
    }

    #[test]
    fn first_prepare_emits_the_seed_1_line_and_advances_rng() {
        let mut pending = PendingLine::new();
        let mut rng = BannerRng::new(1);
        let tail = prepare(&mut pending, &mut rng, 32);
        assert_eq!(tail, FIRST);
        assert_eq!(rng.state(), AFTER_FIRST);
    }

    #[test]
    fn second_prepare_while_pending_does_not_advance_rng() {
        let mut pending = PendingLine::new();
        let mut rng = BannerRng::new(1);
        let first = prepare(&mut pending, &mut rng, 32).to_vec();
        let state = rng.state();
        let second = prepare(&mut pending, &mut rng, 32);
        assert_eq!(second, first.as_slice());
        assert_eq!(second, FIRST);
        assert_eq!(rng.state(), state);
    }

    #[test]
    fn full_write_goes_idle_and_next_prepare_is_the_second_vector() {
        let mut pending = PendingLine::new();
        let mut rng = BannerRng::new(1);
        let mut client_bytes = 0;
        let mut total_bytes = 0;
        let mut script = Script::new([Ok(FIRST.len())]);
        let step = run(
            &mut pending,
            &mut rng,
            32,
            &mut client_bytes,
            &mut total_bytes,
            &mut script,
        );
        assert_eq!(step, Step::Pace);
        assert_eq!(script.seen, vec![FIRST.to_vec()]);
        assert_eq!(client_bytes, 11);
        assert_eq!(total_bytes, 11);
        assert_eq!(rng.state(), AFTER_FIRST);
        let next = prepare(&mut pending, &mut rng, 32);
        assert_eq!(next, SECOND);
        assert_eq!(rng.state(), AFTER_SECOND);
    }

    #[test]
    fn short_write_keeps_the_tail_and_counts_only_the_written_bytes() {
        let mut pending = PendingLine::new();
        let mut rng = BannerRng::new(1);
        let mut client_bytes = 0;
        let mut total_bytes = 0;
        let mut script = Script::new([Ok(4)]);
        let step = run(
            &mut pending,
            &mut rng,
            32,
            &mut client_bytes,
            &mut total_bytes,
            &mut script,
        );
        assert_eq!(step, Step::Pace);
        assert_eq!(script.seen, vec![FIRST.to_vec()]);
        assert_eq!(client_bytes, 4);
        assert_eq!(total_bytes, 4);
        let state = rng.state();
        let tail = prepare(&mut pending, &mut rng, 32);
        assert_eq!(tail, &FIRST[4..]);
        assert_eq!(rng.state(), state);
    }

    #[test]
    fn two_short_writes_finish_the_same_line() {
        let mut pending = PendingLine::new();
        let mut rng = BannerRng::new(1);
        let mut client_bytes = 0;
        let mut total_bytes = 0;
        let mut first = Script::new([Ok(4)]);
        let step = run(
            &mut pending,
            &mut rng,
            32,
            &mut client_bytes,
            &mut total_bytes,
            &mut first,
        );
        assert_eq!(step, Step::Pace);
        assert_eq!(first.seen, vec![FIRST.to_vec()]);
        assert_eq!(client_bytes, 4);
        assert_eq!(total_bytes, 4);
        assert_eq!(rng.state(), AFTER_FIRST);

        let mut second = Script::new([Ok(7)]);
        let step = run(
            &mut pending,
            &mut rng,
            32,
            &mut client_bytes,
            &mut total_bytes,
            &mut second,
        );
        assert_eq!(step, Step::Pace);
        assert_eq!(second.seen, vec![FIRST[4..].to_vec()]);
        assert_eq!(client_bytes, 11);
        assert_eq!(total_bytes, 11);
        assert_eq!(rng.state(), AFTER_FIRST);

        let next = prepare(&mut pending, &mut rng, 32);
        assert_eq!(next, SECOND);
        assert_eq!(rng.state(), AFTER_SECOND);
    }

    #[test]
    fn would_block_keeps_the_tail_and_counts_nothing() {
        let mut pending = PendingLine::new();
        let mut rng = BannerRng::new(1);
        let mut client_bytes = 0;
        let mut total_bytes = 0;
        let mut script = Script::new([Err(IoError::WouldBlock)]);
        let step = run(
            &mut pending,
            &mut rng,
            32,
            &mut client_bytes,
            &mut total_bytes,
            &mut script,
        );
        assert_eq!(step, Step::Pace);
        assert_eq!(script.seen, vec![FIRST.to_vec()]);
        assert_eq!(client_bytes, 0);
        assert_eq!(total_bytes, 0);
        let state = rng.state();
        let tail = prepare(&mut pending, &mut rng, 32);
        assert_eq!(tail, FIRST);
        assert_eq!(rng.state(), state);
    }

    #[test]
    fn write_zero_paces_without_progress() {
        let mut pending = PendingLine::new();
        let mut rng = BannerRng::new(1);
        let mut client_bytes = 0;
        let mut total_bytes = 0;
        let mut script = Script::new([Ok(0)]);
        let step = run(
            &mut pending,
            &mut rng,
            32,
            &mut client_bytes,
            &mut total_bytes,
            &mut script,
        );
        assert_eq!(step, Step::Pace);
        assert_eq!(script.seen, vec![FIRST.to_vec()]);
        assert_eq!(client_bytes, 0);
        assert_eq!(total_bytes, 0);
        let state = rng.state();
        let tail = prepare(&mut pending, &mut rng, 32);
        assert_eq!(tail, FIRST);
        assert_eq!(rng.state(), state);
    }

    #[test]
    fn interrupted_is_retried_then_a_full_write_paces() {
        let mut pending = PendingLine::new();
        let mut rng = BannerRng::new(1);
        let mut client_bytes = 0;
        let mut total_bytes = 0;
        let mut script = Script::new([
            Err(IoError::Interrupted),
            Err(IoError::Interrupted),
            Ok(FIRST.len()),
        ]);
        let step = run(
            &mut pending,
            &mut rng,
            32,
            &mut client_bytes,
            &mut total_bytes,
            &mut script,
        );
        assert_eq!(step, Step::Pace);
        assert_eq!(script.seen.len(), 3);
        assert!(script.seen.iter().all(|bytes| bytes.as_slice() == FIRST));
        assert_eq!(client_bytes, 11);
        assert_eq!(total_bytes, 11);
        assert_eq!(rng.state(), AFTER_FIRST);
    }

    #[test]
    fn fatal_drops_and_keeps_bytes_already_counted() {
        let mut pending = PendingLine::new();
        let mut rng = BannerRng::new(1);
        let mut client_bytes = 0;
        let mut total_bytes = 0;
        let mut short = Script::new([Ok(4)]);
        let step = run(
            &mut pending,
            &mut rng,
            32,
            &mut client_bytes,
            &mut total_bytes,
            &mut short,
        );
        assert_eq!(step, Step::Pace);
        assert_eq!(client_bytes, 4);
        assert_eq!(total_bytes, 4);

        let mut fatal = Script::new([Err(IoError::Fatal)]);
        let step = run(
            &mut pending,
            &mut rng,
            32,
            &mut client_bytes,
            &mut total_bytes,
            &mut fatal,
        );
        assert_eq!(step, Step::Drop);
        assert_eq!(fatal.seen, vec![FIRST[4..].to_vec()]);
        assert_eq!(client_bytes, 4);
        assert_eq!(total_bytes, 4);
        let state = rng.state();
        let tail = prepare(&mut pending, &mut rng, 32);
        assert_eq!(tail, &FIRST[4..]);
        assert_eq!(rng.state(), state);
    }

    #[test]
    fn oversize_write_result_drops_without_counting() {
        let mut pending = PendingLine::new();
        let mut rng = BannerRng::new(1);
        let mut client_bytes = 0;
        let mut total_bytes = 0;
        let mut script = Script::new([Ok(FIRST.len() + 1)]);
        let step = run(
            &mut pending,
            &mut rng,
            32,
            &mut client_bytes,
            &mut total_bytes,
            &mut script,
        );
        assert_eq!(step, Step::Drop);
        assert_eq!(script.seen, vec![FIRST.to_vec()]);
        assert_eq!(client_bytes, 0);
        assert_eq!(total_bytes, 0);
        let state = rng.state();
        let tail = prepare(&mut pending, &mut rng, 32);
        assert_eq!(tail, FIRST);
        assert_eq!(rng.state(), state);
    }

    #[test]
    fn maxlen_3_pending_line_is_three_bytes() {
        let mut pending = PendingLine::new();
        let mut rng = BannerRng::new(1);
        let tail = prepare(&mut pending, &mut rng, 3).to_vec();
        assert_eq!(tail.len(), 3);
        assert_eq!(&tail[1..], b"\r\n");

        let mut check = BannerRng::new(1);
        let mut dest = [0u8; 3];
        let len = check.write_line(&mut dest, 3);
        assert_eq!(tail, dest[..len]);
        assert_eq!(rng.state(), check.state());

        let state = rng.state();
        let again = prepare(&mut pending, &mut rng, 3);
        assert_eq!(again, tail.as_slice());
        assert_eq!(again.len(), 3);
        assert_eq!(rng.state(), state);
    }
}
