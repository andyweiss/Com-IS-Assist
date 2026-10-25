use ebur128::{Channel, EbuR128, Error as Ebur128Error, Mode};

/// Errors constructing an [`Ebur128Meter`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigError {
    /// `channel_count` isn't one of the layouts this project's `bed_channel_map` knows how to
    /// build (expected 1-6 or 10). Production wrappers should prefer mapping from the
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

/// The Bed layouts this project supports, each with a fixed channel order - VST3's, for the
/// layouts the plugin ships (`kStereo`, `k51`, `k51_4`, `k71_2`).
///
/// Needed as a type of its own because the channel count is ambiguous: 5.1.4 and 7.1.2 are both
/// ten channels, with different roles in positions 6-9.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BedLayout {
    Mono,
    Stereo,
    /// L R C
    Lrc,
    /// L R Ls Rs
    Quad,
    /// L R C Ls Rs
    Surround50,
    /// L R C LFE Ls Rs
    Surround51,
    /// L R C LFE Ls Rs Ltf Rtf Ltr Rtr (VST3 `k51_4`)
    Surround514,
    /// L R C LFE Ls Rs Sl Sr Tsl Tsr (VST3 `k71_2`)
    Surround712,
}

impl BedLayout {
    /// The layout a bare channel count is taken to mean: 1-6 as listed on the variants, and 10 as
    /// **5.1.4**. 7.1.2 is never inferred - it has to be asked for by name.
    pub fn from_channel_count(channel_count: u32) -> Result<Self, ConfigError> {
        match channel_count {
            1 => Ok(Self::Mono),
            2 => Ok(Self::Stereo),
            3 => Ok(Self::Lrc),
            4 => Ok(Self::Quad),
            5 => Ok(Self::Surround50),
            6 => Ok(Self::Surround51),
            10 => Ok(Self::Surround514),
            other => Err(ConfigError::UnsupportedChannelCount(other)),
        }
    }

    pub fn channel_count(self) -> u32 {
        self.channel_map().len() as u32
    }

    /// BS.1770 channel map, one entry per interleaved channel.
    ///
    /// The LFE is `Unused`: BS.1770 excludes it from the loudness sum. Elevated channels sit at the
    /// ITU-R BS.2051 positions for their layout (5.1.4 is System D: U±030 front, U±110 rear; the
    /// 7.1.2 top pair is U±090) and are weighted 1.0, like the front pair.
    ///
    /// On 7.1.2, VST3's `Sl`/`Sr` are the side pair (M±090) and `Ls`/`Rs` move round to the rear
    /// (M±135). That matters for loudness, not just labelling: BS.1770 gives the +1.5 dB surround
    /// weight only to channels between 60° and 120°, so the side pair takes it and the rear pair,
    /// unlike Ls/Rs on 5.1 (M±110), does not.
    pub fn channel_map(self) -> Vec<Channel> {
        use Channel::{
            Center, Left, LeftSurround, Mm090, Mm135, Mp090, Mp135, Right, RightSurround, Um030, Um090,
            Um110, Unused, Up030, Up090, Up110,
        };
        match self {
            Self::Mono => vec![Center],
            Self::Stereo => vec![Left, Right],
            Self::Lrc => vec![Left, Right, Center],
            Self::Quad => vec![Left, Right, LeftSurround, RightSurround],
            Self::Surround50 => vec![Left, Right, Center, LeftSurround, RightSurround],
            Self::Surround51 => vec![Left, Right, Center, Unused, LeftSurround, RightSurround],
            Self::Surround514 => vec![
                Left,
                Right,
                Center,
                Unused, // LFE
                LeftSurround,
                RightSurround,
                Up030, // Top Front Left
                Um030, // Top Front Right
                Up110, // Top Rear Left
                Um110, // Top Rear Right
            ],
            Self::Surround712 => vec![
                Left,
                Right,
                Center,
                Unused, // LFE
                Mp135,  // Ls - rear on 7.1
                Mm135,  // Rs
                Mp090,  // Sl - side
                Mm090,  // Sr
                Up090,  // Top Side Left
                Um090,  // Top Side Right
            ],
        }
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
    /// independent (required for VST3 hosts, which deliver variable-size buffers).
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

    /// Default BS.1770 channel map for a Bed (IS) of `channel_count` channels - see
    /// [`BedLayout::from_channel_count`] for which layout each count stands for. A wrapper that
    /// knows the actual layout (the VST3 does) should use [`BedLayout::channel_map`] instead, since
    /// a channel count alone cannot tell 5.1.4 from 7.1.2.
    pub fn bed_channel_map(channel_count: u32) -> Result<Vec<Channel>, ConfigError> {
        Ok(BedLayout::from_channel_count(channel_count)?.channel_map())
    }

    /// Default BS.1770 channel map for this project's mono Dialogue (COM) signal.
    pub fn dialogue_channel_map() -> Vec<Channel> {
        vec![Channel::Center]
    }
}
