//! SSH banner lines from a seed.
//!
//! The generator is a 32-bit linear congruential generator.
//! The multiply wraps at 2^32.
//! Each line contains printable ASCII bytes.
//! Each line ends with CR LF.
//! No line starts with the SSH identification prefix `SSH-`.

/// Replace a leading `SSH-` so the line is not an SSH identification string.
///
/// The function sets byte 0 to `X`.
/// The function does not change a line shorter than 4 bytes.
/// The function does not change a line that does not start with `SSH-`.
pub fn suppress_ssh_prefix(line: &mut [u8]) {
    if line.len() >= 4 && line.starts_with(b"SSH-") {
        line[0] = b'X';
    }
}

/// Generator of banner lines.
/// The same seed gives the same lines.
pub struct BannerRng {
    state: u32,
}

impl BannerRng {
    pub fn new(seed: u32) -> Self {
        Self { state: seed }
    }

    pub fn state(&self) -> u32 {
        self.state
    }

    /// Write one banner line at the start of `dest`.
    ///
    /// `maxlen` must be from 3 through 255.
    /// `dest` must contain at least `maxlen` bytes.
    /// The function returns the number of bytes that it wrote.
    /// The function panics if `maxlen` is outside that range.
    /// The function panics if `dest` is shorter than `maxlen`.
    pub fn write_line(&mut self, dest: &mut [u8], maxlen: u8) -> usize {
        assert!(
            (3..=255).contains(&maxlen),
            "maxlen {maxlen} is not in the range 3 through 255"
        );
        assert!(
            dest.len() >= maxlen as usize,
            "dest length {} is less than maxlen {maxlen}",
            dest.len()
        );

        let len = 3 + self.rand16() as usize % (maxlen as usize - 2);
        for byte in &mut dest[..len - 2] {
            *byte = 32 + (self.rand16() % 95) as u8;
        }
        dest[len - 2] = b'\r';
        dest[len - 1] = b'\n';
        suppress_ssh_prefix(&mut dest[..len]);
        len
    }

    fn rand16(&mut self) -> u32 {
        self.state = self.state.wrapping_mul(1_103_515_245).wrapping_add(12_345);
        (self.state >> 16) & 0xffff
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(rng: &mut BannerRng, maxlen: u8) -> (Vec<u8>, usize) {
        let mut dest = vec![0u8; maxlen as usize];
        let len = rng.write_line(&mut dest, maxlen);
        (dest, len)
    }

    #[test]
    fn lcg_known_vector_seed_1() {
        let mut rng = BannerRng::new(1);
        let expected: [(&[u8], u32); 3] = [
            (b"SK<j/-\"W!\r\n", 267_834_847),
            (b"E[s4X\"vR$j?fS^B:<;o6\r\n", 4_147_913_800),
            (b"m^Q\\8X~5DsfZ@X\r\n", 2_058_258_651),
        ];
        for (body, state_after) in expected {
            let (dest, len) = line(&mut rng, 32);
            assert_eq!(len, body.len());
            assert_eq!(&dest[..len], body);
            assert_eq!(rng.state(), state_after);
        }
    }

    #[test]
    fn short_line_seed_0_does_not_write_past_length() {
        let mut rng = BannerRng::new(0);
        let mut dest = vec![0xA5u8; 32];
        let len = rng.write_line(&mut dest, 32);
        assert_eq!(len, 3);
        assert_eq!(&dest[..3], b"v\r\n");
        assert!(dest[3..].iter().all(|byte| *byte == 0xA5));
        assert_eq!(rng.state(), 3_554_416_254);
    }

    #[test]
    fn length_bounds_at_maxlen_3_and_255() {
        for maxlen in [3u8, 255] {
            let mut rng = BannerRng::new(1);
            for _ in 0..64 {
                let (dest, len) = line(&mut rng, maxlen);
                assert!((3..=maxlen as usize).contains(&len));
                assert_eq!(&dest[len - 2..len], b"\r\n");
            }
        }
    }

    #[test]
    fn body_is_printable_and_line_ends_with_crlf() {
        let mut rng = BannerRng::new(7);
        for _ in 0..64 {
            let (dest, len) = line(&mut rng, 32);
            assert_eq!(&dest[len - 2..len], b"\r\n");
            assert!(dest[..len - 2].iter().all(|byte| (32..=126).contains(byte)));
            assert!(!dest[..len].starts_with(b"SSH-"));
        }
    }

    #[test]
    fn suppress_ssh_prefix_cases() {
        let mut ssh = b"SSH-hello\r\n".to_vec();
        suppress_ssh_prefix(&mut ssh);
        assert_eq!(ssh, b"XSH-hello\r\n");

        let mut short = b"SSH".to_vec();
        suppress_ssh_prefix(&mut short);
        assert_eq!(short, b"SSH");

        let mut cr = b"SSH\r\n".to_vec();
        suppress_ssh_prefix(&mut cr);
        assert_eq!(cr, b"SSH\r\n");

        let mut later = b"xSSH-".to_vec();
        suppress_ssh_prefix(&mut later);
        assert_eq!(later, b"xSSH-");
    }

    #[test]
    fn same_seed_replays_the_same_bytes() {
        let mut left = BannerRng::new(99);
        let mut right = BannerRng::new(99);
        for maxlen in [3u8, 32, 255] {
            let (a, a_len) = line(&mut left, maxlen);
            let (b, b_len) = line(&mut right, maxlen);
            assert_eq!(a_len, b_len);
            assert_eq!(a[..a_len], b[..b_len]);
            assert_eq!(left.state(), right.state());
        }
    }
}
