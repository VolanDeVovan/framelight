//! Bitrate of the one shared encode.
//!
//! The viewer page sets a ceiling; below it the rate follows the worst viewer's packet
//! loss (RTCP receiver reports): back off while packets are being lost, by as much as
//! is being lost, and climb back once the link has been clean for a while: briskly up
//! to the neighbourhood of the rate that last lost packets, in small steps from there.
//! Loss that NACK repairs cheaply (a percent or two on Wi-Fi) is not a reason to give
//! up picture quality.

use std::time::{Duration, Instant};

pub const FLOOR_KBPS: u32 = 1_500;
pub const MAX_KBPS: u32 = 50_000;

const LOSS_BAD: f32 = 0.04;
const LOSS_CLEAN: f32 = 0.01;
const HOLD_AFTER_CUT: Duration = Duration::from_secs(2);
const CLEAN_BEFORE_RAISE: Duration = Duration::from_secs(4);
/// How long the rate that lost packets is remembered as the place to be careful.
const WALL_MEMORY: Duration = Duration::from_secs(90);

pub struct Bitrate {
    ceiling: u32,
    current: u32,
    last_change: Instant,
    /// Since when no viewer has reported loss above [`LOSS_CLEAN`].
    clean_since: Instant,
    /// The rate at which packets were last lost, and when.
    wall: Option<(u32, Instant)>,
}

impl Bitrate {
    pub fn new(ceiling_kbps: u32, now: Instant) -> Self {
        let ceiling = ceiling_kbps.clamp(FLOOR_KBPS, MAX_KBPS);
        Self {
            ceiling,
            current: ceiling,
            last_change: now,
            clean_since: now,
            wall: None,
        }
    }

    pub fn current(&self) -> u32 {
        self.current
    }

    pub fn ceiling(&self) -> u32 {
        self.ceiling
    }

    /// A viewer picked a new ceiling: go straight to it.
    pub fn set_ceiling(&mut self, kbps: u32, now: Instant) -> u32 {
        self.ceiling = kbps.clamp(FLOOR_KBPS, MAX_KBPS);
        self.current = self.ceiling;
        self.last_change = now;
        self.clean_since = now;
        self.wall = None;
        self.current
    }

    /// Feed the worst loss fraction among the viewers' latest reports. Returns the new
    /// bitrate when it changes.
    pub fn on_loss(&mut self, worst: f32, now: Instant) -> Option<u32> {
        if worst > LOSS_CLEAN {
            self.clean_since = now;
        }
        let next = if worst > LOSS_BAD {
            if now.duration_since(self.last_change) < HOLD_AFTER_CUT {
                return None;
            }
            // Roughly what got through, and never a timid step: heavy loss means the
            // link carries a fraction of what is being sent.
            self.wall = Some((self.current, now));
            (self.current as f32 * (1.0 - worst).clamp(0.5, 0.85)) as u32
        } else if now.duration_since(self.clean_since) >= CLEAN_BEFORE_RAISE
            && now.duration_since(self.last_change) >= CLEAN_BEFORE_RAISE
        {
            let near_wall = self.wall.is_some_and(|(rate, at)| {
                now.duration_since(at) < WALL_MEMORY && self.current as u64 * 10 >= rate as u64 * 7
            });
            if near_wall {
                self.current + self.current / 25
            } else {
                self.current + self.current / 6 + 250
            }
        } else {
            return None;
        };
        let next = next.clamp(FLOOR_KBPS.min(self.ceiling), self.ceiling);
        if next == self.current {
            return None;
        }
        self.current = next;
        self.last_change = now;
        Some(next)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backs_off_on_loss_and_recovers_to_the_ceiling() {
        let t0 = Instant::now();
        let s = Duration::from_secs;
        let mut b = Bitrate::new(12_000, t0);
        assert_eq!(b.on_loss(0.0, t0 + s(1)), None, "already at the ceiling");
        assert_eq!(
            b.on_loss(0.02, t0 + s(3)),
            None,
            "repairable loss is tolerated"
        );
        assert_eq!(b.on_loss(0.10, t0 + s(4)), Some(10_200));
        assert_eq!(b.on_loss(0.10, t0 + s(5)), None, "one cut per hold period");
        assert_eq!(
            b.on_loss(0.70, t0 + s(7)),
            Some(5_100),
            "heavy loss halves it"
        );
        assert_eq!(b.on_loss(0.0, t0 + s(9)), None, "not clean for long enough");
        assert_eq!(
            b.on_loss(0.0, t0 + s(12)),
            Some(6_200),
            "a brisk step while far below the trouble"
        );
        // 10 200 kbit/s lost packets a moment ago: from 70 % of that on, small steps.
        assert_eq!(b.on_loss(0.0, t0 + s(17)), Some(7_483));
        assert_eq!(b.on_loss(0.0, t0 + s(22)), Some(7_782));
        // Long after, the old trouble is forgotten and the ceiling is reached again.
        let mut t = 200;
        while b.current() < 12_000 {
            t += 5;
            b.on_loss(0.0, t0 + s(t));
            assert!(t < 300);
        }
        assert_eq!(b.on_loss(0.0, t0 + s(t + 10)), None);
    }

    #[test]
    fn never_leaves_the_allowed_range() {
        let t0 = Instant::now();
        let mut b = Bitrate::new(2_000, t0);
        for i in 1..50 {
            b.on_loss(0.5, t0 + Duration::from_secs(i * 3));
        }
        assert_eq!(b.current(), FLOOR_KBPS);
        assert_eq!(b.set_ceiling(1, t0), FLOOR_KBPS);
        assert_eq!(b.set_ceiling(u32::MAX, t0), MAX_KBPS);
        assert_eq!(b.current(), MAX_KBPS);
    }
}
