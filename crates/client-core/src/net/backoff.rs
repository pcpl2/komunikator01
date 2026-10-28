//! Reconnect delays: 0.5 s, 1 s, 2 s, 4 s, then 5 s forever.

use std::time::Duration;

const STEPS_MS: [u64; 5] = [500, 1000, 2000, 4000, 5000];

#[derive(Debug, Default)]
pub struct Backoff {
    attempt: usize,
}

impl Backoff {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn next_delay(&mut self) -> Duration {
        let ms = STEPS_MS[self.attempt.min(STEPS_MS.len() - 1)];
        self.attempt = self.attempt.saturating_add(1);
        Duration::from_millis(ms)
    }

    pub fn reset(&mut self) {
        self.attempt = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn backoff_sequence() {
        let mut b = Backoff::new();
        let got: Vec<_> = (0..6).map(|_| b.next_delay()).collect();
        let ms = |v| Duration::from_millis(v);
        assert_eq!(got, [ms(500), ms(1000), ms(2000), ms(4000), ms(5000), ms(5000)]);
    }

    #[test]
    fn backoff_reset() {
        let mut b = Backoff::new();
        b.next_delay();
        b.next_delay();
        b.reset();
        assert_eq!(b.next_delay(), Duration::from_millis(500));
    }
}
