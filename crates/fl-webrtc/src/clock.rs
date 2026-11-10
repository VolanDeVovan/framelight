//! The audio source's clock mapped onto the media timeline.

/// Maps the audio source's timestamps (a packet count, restarting from zero with the
/// source and paced by the sound card) onto the media timeline, which only moves
/// forward: anchored to the arrival of a packet, and anchored again when the two
/// clocks have drifted more than `max_skew_us` apart.
pub(crate) struct Clock {
    offset_us: i64,
    last_pts: Option<u64>,
    last_out: u64,
    max_skew_us: u64,
}

impl Clock {
    pub fn new(max_skew_us: u64) -> Self {
        Self {
            offset_us: 0,
            last_pts: None,
            last_out: 0,
            max_skew_us,
        }
    }

    pub fn map(&mut self, pts_us: u64, now_us: u64) -> u64 {
        let mapped = pts_us as i64 + self.offset_us;
        let restarted = self.last_pts.is_none_or(|last| pts_us < last);
        let skewed = mapped.abs_diff(now_us as i64) > self.max_skew_us;
        if restarted || skewed {
            self.offset_us = now_us as i64 - pts_us as i64;
        }
        self.last_pts = Some(pts_us);
        let out = ((pts_us as i64 + self.offset_us).max(0) as u64).max(self.last_out + 1);
        self.last_out = out;
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn survives_source_restarts_and_never_goes_back() {
        let mut c = Clock::new(80_000);
        assert_eq!(c.map(0, 1_000_000), 1_000_000);
        assert_eq!(
            c.map(20_000, 1_030_000),
            1_020_000,
            "keeps the source's spacing, not arrival jitter"
        );
        assert_eq!(c.map(40_000, 1_041_000), 1_040_000);
        // The source restarted: its timestamps begin again.
        assert_eq!(c.map(0, 5_000_000), 5_000_000);
        assert_eq!(c.map(20_000, 5_030_000), 5_020_000);
        // Equal timestamps still move forward.
        assert_eq!(c.map(20_000, 5_030_000), 5_020_001);
    }

    #[test]
    fn reanchors_when_it_drifts() {
        let mut c = Clock::new(80_000);
        assert_eq!(c.map(0, 1_000_000), 1_000_000);
        assert_eq!(c.map(20_000, 1_025_000), 1_020_000);
        // The sound card's clock fell 100 ms behind ours.
        assert_eq!(c.map(40_000, 1_140_000), 1_140_000);
        assert_eq!(c.map(60_000, 1_161_000), 1_160_000);
    }
}
