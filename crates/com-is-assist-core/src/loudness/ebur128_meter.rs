use ebur128::{Channel, EbuR128, Error as Ebur128Error, Mode};

/// Errors constructing an [`Ebur128Meter`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigError {
    /// `channel_count` isn't one of the layouts this project's `bed_channel_map` knows how to
    /// build (expected 1-6). Production wrappers (GStreamer/VST3) should prefer mapping from the
    /// host's actual channel position metadata over this default table where available.
    UnsupportedChannelCount(u32),
    /// The `ebur128` crate itself rejected the configuration (e.g. an invalid sample rate).
    Ebur128(Ebur128Error),
}

impl From<Ebur128Error> for ConfigError {
    fn from(e: Ebur128Error) -> Self {
        ConfigError::Ebur128(e)
    }
}

/// Thin wrapper around the `ebur128` crate (a pure-Rust, EBU-conformance-tested port of
/// libebur128), providing the subset of R128/BS.1770 loudness measurement the automix engine
/// needs (momentary, short-term, integrated). See `Specs/TechnicalConcept.md` section 4 for why
/// loudness measurement is delegated to a library rather than hand-derived.
pub struct Ebur128Meter {
    inner: EbuR128,
}

impl Ebur128Meter {
    /// Returned for silence/insufficient data instead of `-inf`, so callers can do plain
    /// arithmetic without special-casing (mirrors the original JSFX meter's `-100` sentinel
    /// convention). Confirmed against the `ebur128` crate's source: `loudness_momentary`/
    /// `loudness_shortterm` return `Ok(-inf)` for zero energy (including "not enough data pushed
    /// yet", which reads as silence in the underlying ring buffer) rather than a distinct error
    /// variant, so a plain finite-check is the correct and complete sanitization.
    pub const NEGATIVE_INFINITY_DB: f64 = -100.0;

    /// `channel_map` entries are the `ebur128` crate's `Channel` enum, one per interleaved
    /// channel. Use [`Ebur128Meter::bed_channel_map`]/[`Ebur128Meter::dialogue_channel_map`] for
    /// the conventional layouts this project targets.
    pub fn new(channel_map: &[Channel], sample_rate: u32) -> Result<Self, ConfigError> {
        let mode = Mode::M | Mode::S | Mode::I;
        let mut inner = EbuR128::new(channel_map.len() as u32, sample_rate, mode)?;
        inner.set_channel_map(channel_map)?;
        Ok(Self { inner })
    }

    /// Pushes one block of interleaved `f32` samples. Safe to call with any block size across
    /// calls; the underlying meter accumulates internally, so it's naturally block-size
    /// independent (required for GStreamer and VST3 hosts, which deliver variable-size buffers).
    ///
    /// Returns an error only if `interleaved.len()` isn't a multiple of the configured channel
    /// count — a caller bug, not a runtime signal condition.
    pub fn push_frames(&mut self, interleaved: &[f32]) -> Result<(), Ebur128Error> {
        if interleaved.iter().any(|s| !s.is_finite()) {
            // A NaN/Infinity sample - e.g. from a live-audio glitch such as an AGC/echo-cancellation
            // edge case, a resampler overflow, or a dropped-out interface - would otherwise
            // permanently poison the `ebur128` crate's internal K-weighting filter state: NaN in
            // the filter's history taps never clears on its own, so every future loudness reading
            // from this meter would silently stay frozen at "silence" forever, with no error raised.
            // Replacing non-finite samples with silence keeps the meter reacting normally once
            // clean audio resumes.
            let sanitized: Vec<f32> = interleaved
                .iter()
                .map(|s| if s.is_finite() { *s } else { 0.0 })
                .collect();
            return self.inner.add_frames_f32(&sanitized);
        }
        self.inner.add_frames_f32(interleaved)
    }

    pub fn momentary_loudness_db(&self) -> f64 {
        Self::sanitize(
            self.inner
                .loudness_momentary()
                .expect("Mode::M is always enabled by Ebur128Meter::new"),
        )
    }

    pub fn short_term_loudness_db(&self) -> f64 {
        Self::sanitize(
            self.inner
                .loudness_shortterm()
                .expect("Mode::S is always enabled by Ebur128Meter::new"),
        )
    }

    pub fn integrated_loudness_db(&self) -> f64 {
        Self::sanitize(
            self.inner
                .loudness_global()
                .expect("Mode::I is always enabled by Ebur128Meter::new"),
        )
    }

    fn sanitize(db: f64) -> f64 {
        if db.is_finite() {
            db
        } else {
            Self::NEGATIVE_INFINITY_DB
        }
    }

    /// Default BS.1770 channel map for this project's Bed (IS) signal. `channel_count` must be
    /// 1-6; production wrappers should prefer mapping from the host's actual channel position
    /// metadata over this default where available.
    pub fn bed_channel_map(channel_count: u32) -> Result<Vec<Channel>, ConfigError> {
        use Channel::{Center, Left, LeftSurround, Right, RightSurround, Unused};
        match channel_count {
            1 => Ok(vec![Center]),
            2 => Ok(vec![Left, Right]),
            3 => Ok(vec![Left, Right, Center]),
            4 => Ok(vec![Left, Right, LeftSurround, RightSurround]),
            5 => Ok(vec![Left, Right, Center, LeftSurround, RightSurround]),
            6 => Ok(vec![
                Left,
                Right,
                Center,
                Unused, // LFE
                LeftSurround,
                RightSurround,
            ]),
            other => Err(ConfigError::UnsupportedChannelCount(other)),
        }
    }

    /// Default BS.1770 channel map for this project's mono Dialogue (COM) signal.
    pub fn dialogue_channel_map() -> Vec<Channel> {
        vec![Channel::Center]
    }
}
