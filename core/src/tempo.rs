//! Playback speed and the BPM it gives, plus ReplayGain (#6, #8).
//!
//! Speed is a [`Rate`]: the factor the player applies. With pitch kept
//! (`scaletempo`), a rate of 1.1 plays 10 % faster at the same pitch. The
//! BPM mode works out the rate that takes a track from its BPM to a target
//! BPM, using the BPM that folksonic gives (the only tempo source; see
//! ademant/dancesonic#82).

use std::fmt;

/// Slowest rate offered: 50 %. Below this the sound degrades.
pub const MIN_RATE: f64 = 0.5;

/// Fastest rate offered: 150 %. Above this the sound degrades.
pub const MAX_RATE: f64 = 1.5;

/// The lowest and highest BPM a target may name, the same bounds folksonic uses.
pub const BPM_RANGE: std::ops::RangeInclusive<f64> = 20.0..=300.0;

/// A playback speed factor between [`MIN_RATE`] and [`MAX_RATE`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rate(f64);

impl Rate {
    /// Normal speed.
    pub const NORMAL: Self = Self(1.0);

    /// A rate from a factor. Out-of-range or non-finite values are refused.
    pub fn new(factor: f64) -> Result<Self, TempoError> {
        if factor.is_finite() && (MIN_RATE..=MAX_RATE).contains(&factor) {
            Ok(Self(factor))
        } else {
            Err(TempoError::OutOfRange)
        }
    }

    /// A rate from a percentage of the original speed, such as `104.0`.
    pub fn from_percent(percent: f64) -> Result<Self, TempoError> {
        Self::new(percent / 100.0)
    }

    /// The factor the player applies.
    pub fn factor(self) -> f64 {
        self.0
    }

    /// The percentage of the original speed, such as `104.0`.
    pub fn percent(self) -> f64 {
        self.0 * 100.0
    }

    /// The rate that plays a track of `track_bpm` at `target_bpm`. Both must be
    /// in [`BPM_RANGE`], and the rate must land inside the offered range.
    pub fn for_target_bpm(track_bpm: f64, target_bpm: f64) -> Result<Self, TempoError> {
        if !BPM_RANGE.contains(&track_bpm) || !BPM_RANGE.contains(&target_bpm) {
            return Err(TempoError::BadBpm);
        }
        Self::new(target_bpm / track_bpm)
    }

    /// The BPM the track sounds at with this rate. `None` when the track's BPM
    /// is not known, so the caller can fall back to percentages.
    pub fn bpm_of(self, track_bpm: Option<f64>) -> Option<f64> {
        track_bpm
            .filter(|bpm| BPM_RANGE.contains(bpm))
            .map(|bpm| bpm * self.0)
    }
}

impl fmt::Display for Rate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:.0} %", self.percent())
    }
}

/// Errors from speed and tempo values.
#[derive(Debug, thiserror::Error, PartialEq)]
pub enum TempoError {
    #[error("the speed is outside {MIN_RATE} to {MAX_RATE}")]
    OutOfRange,
    #[error("the BPM is outside 20 to 300")]
    BadBpm,
}

/// Limits DanceBrainz puts on gain, in dB.
pub const GAIN_LIMIT_DB: f64 = 60.0;

/// The volume factor for a track's ReplayGain, so it plays at the level the
/// album or track was measured at.
///
/// The factor never boosts above 1.0: a boost would clip the peak and the
/// player has no headroom to spare. When `peak` is known the factor is also
/// capped so the peak stays at full scale. Values outside the sane range are
/// ignored (factor 1.0): a broken tag must not silence or blast a track.
pub fn replay_volume(gain_db: Option<f64>, peak: Option<f64>) -> f64 {
    let Some(gain) = gain_db.filter(|g| g.is_finite() && g.abs() <= GAIN_LIMIT_DB) else {
        return 1.0;
    };
    let mut factor = 10f64.powf(gain / 20.0);
    if let Some(peak) = peak.filter(|p| p.is_finite() && (0.0..=10.0).contains(p) && *p > 0.0) {
        factor = factor.min(1.0 / peak);
    }
    factor.clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rates_outside_the_offered_range_are_refused() {
        assert!(Rate::new(0.49).is_err());
        assert!(Rate::new(1.51).is_err());
        assert!(Rate::new(f64::NAN).is_err());
        assert!(Rate::new(f64::INFINITY).is_err());
        assert_eq!(Rate::new(1.0).unwrap(), Rate::NORMAL);
    }

    #[test]
    fn percent_and_factor_agree() {
        let rate = Rate::from_percent(104.0).unwrap();
        assert!((rate.factor() - 1.04).abs() < 1e-12);
        assert!((rate.percent() - 104.0).abs() < 1e-9);
        assert_eq!(rate.to_string(), "104 %");
    }

    #[test]
    fn target_bpm_gives_the_rate_that_reaches_it() {
        let rate = Rate::for_target_bpm(120.0, 132.0).unwrap();
        assert!((rate.factor() - 1.1).abs() < 1e-12);
        assert!((rate.bpm_of(Some(120.0)).unwrap() - 132.0).abs() < 1e-9);
    }

    #[test]
    fn a_target_too_far_away_is_refused() {
        assert_eq!(
            Rate::for_target_bpm(60.0, 120.0),
            Err(TempoError::OutOfRange),
            "2.0x is above the offered range"
        );
        assert_eq!(
            Rate::for_target_bpm(120.0, 10.0),
            Err(TempoError::BadBpm),
            "a target below 20 BPM is not a tempo"
        );
    }

    #[test]
    fn an_unknown_track_bpm_gives_no_bpm() {
        assert_eq!(Rate::NORMAL.bpm_of(None), None);
        assert_eq!(Rate::NORMAL.bpm_of(Some(500.0)), None);
    }

    #[test]
    fn replay_gain_lowers_loud_tracks_and_never_boosts() {
        assert!((replay_volume(Some(-6.0), None) - 0.501).abs() < 1e-3);
        assert_eq!(replay_volume(Some(4.0), None), 1.0, "no boost above 1.0");
        assert_eq!(replay_volume(None, None), 1.0);
    }

    #[test]
    fn replay_gain_keeps_an_over_full_scale_peak_in_range() {
        // A tag whose peak is already above full scale is pulled back to it,
        // even when the gain alone would leave it there.
        let factor = replay_volume(Some(-1.0), Some(1.2));
        assert!((factor - 1.0 / 1.2).abs() < 1e-9, "{factor}");
    }

    #[test]
    fn broken_gain_values_are_ignored() {
        assert_eq!(replay_volume(Some(900.0), None), 1.0);
        assert_eq!(replay_volume(Some(f64::NAN), None), 1.0);
        assert_eq!(
            replay_volume(Some(-6.0), Some(-1.0)),
            10f64.powf(-6.0 / 20.0)
        );
    }
}
