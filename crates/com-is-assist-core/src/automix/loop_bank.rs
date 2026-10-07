use super::envelope_detector::EnvelopeDetector;

/// Time constants for one stage of the cascade, in seconds at Speed = 1.0.
#[derive(Debug, Clone, Copy)]
struct Ballistics {
    attack_seconds: f64,
    release_seconds: f64,
}

/// Slow stage: carries the bulk of the reduction and sets the programme balance. Its detector is
/// the 3s short-term window, so there is nothing to gain from moving faster than this.
const SLOW: Ballistics = Ballistics { attack_seconds: 0.8, release_seconds: 3.0 };
/// Mid stage: phrase-level tracking, fed by the 400ms momentary window.
const MID: Ballistics = Ballistics { attack_seconds: 0.15, release_seconds: 0.6 };
/// Fast stage: transient response, fed by the short K-weighted detector and stepped at audio
/// sub-chunk rate rather than at the control tick.
///
/// The release is **far** longer than the attack, and much longer than the detector window. That
/// asymmetry is deliberate and was measured rather than assumed: with a short release the stage
/// tracked the syllable envelope of real commentary and swung across its whole authority every
/// tick or two, modulating the Bed at syllable rate. A transient catcher wants to grab quickly and
/// let go slowly, so that what it responds to is *onsets* rather than the shape of every vowel.
const FAST: Ballistics = Ballistics { attack_seconds: 0.005, release_seconds: 0.4 };

/// How long the whole cascade holds its current reduction before any release is allowed to begin.
///
/// Measured, not guessed: real commentary pauses run ~1.2s, and with no hold at all the Bed audibly
/// swells in every inter-sentence gap and re-ducks on the next phrase (see
/// `Specs/TechnicalConcept.md` section 5.3). This sits on top of the COM detector's own
/// `hangover_seconds`, and the two together have to span a natural pause. It is deliberately fixed
/// rather than exposed: it is a property of speech, not a matter of taste, and the previous design
/// having it as a user control ("Hold time") is part of what made that surface unmanageable.
const RELEASE_HOLD_SECONDS: f64 = 0.8;

/// How much faster `MixState::InterviewPassthrough` recovers than an ordinary release. Replaces the
/// former `interview-release-ms` parameter — one fixed ratio is easier to reason about than a free
/// time that can be set slower than the normal release by mistake.
const INTERVIEW_RELEASE_SPEEDUP: f64 = 4.0;

#[derive(Debug, Clone, Copy)]
pub struct LoopBankConfig {
    /// The "Speed" macro: scales every stage's time constants together. >1 is faster, <1 calmer.
    /// One control replaces the five separate time parameters the single-loop design needed.
    pub speed: f64,
    pub max_gain_reduction_db: f64,
    /// How far the mid stage may trim the slow stage's estimate, in dB either way.
    pub mid_authority_db: f64,
    /// How far the fast stage may trim slow+mid. Set to 0.0 to disable the fast stage entirely.
    pub fast_authority_db: f64,
}

impl Default for LoopBankConfig {
    fn default() -> Self {
        Self {
            speed: 1.0,
            max_gain_reduction_db: 24.0,
            mid_authority_db: 6.0,
            // Smaller than the mid stage's: this one moves in milliseconds, so the audible cost of
            // it being wrong is higher. 4dB is enough to catch an onset before the 400ms window has
            // noticed it, without being enough to dominate.
            fast_authority_db: 4.0,
        }
    }
}

/// What each stage is currently contributing, in dB of reduction. Exposed for metering and for the
/// offline CSV, because the single most useful question when the mix does something unexpected is
/// "which stage did that?".
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct LoopContributions {
    pub slow_db: f64,
    pub mid_db: f64,
    pub fast_db: f64,
    pub total_db: f64,
}

/// The multi-loop gain computer: three cascaded stages whose reductions add in dB (equivalently,
/// whose gains multiply — the three VCAs of the Jünger reference topology).
///
/// Each stage is fed a `required` reduction derived **feed-forward** from the dry signals at its own
/// integration time (see `Specs/TechnicalConcept.md` section 5.2). Because reducing the Bed by 1 dB
/// raises the COM/IS ratio by exactly 1 LU, `required = target_ratio − (COM − BED)` is the complete
/// answer at that time scale — there is no loop to converge, which is what removes the windup and
/// pumping the earlier feedback design kept producing.
///
/// Stages are **hierarchical trims**, not three independent controllers: the slow stage states the
/// answer, the mid stage corrects it by at most `mid_authority_db`, and the fast stage corrects
/// *that* by at most `fast_authority_db`. Without the trim structure all three would act on the
/// same error and triple-count it. Bounding each faster stage is also what makes a 5ms attack safe
/// to have at all: the fast stage cannot run away, because it can never contribute more than its
/// authority.
///
/// The adaptive ballistics the reference describes — "relatively long attack times during
/// steady-state signal conditions but also very short attack times when there are impulsive input
/// transients" — are an emergent property of this structure, not something configured. A transient
/// moves only the fast stage, in milliseconds and by a bounded amount; a sustained level change
/// moves all three, and the slow stage ends up carrying it.
pub struct LoopBank {
    config: LoopBankConfig,
    slow: EnvelopeDetector,
    mid: EnvelopeDetector,
    fast: EnvelopeDetector,
    /// Counts down once the cascade is asked to release; while positive, release is blocked.
    hold_remaining_seconds: f64,
    /// Set by the most recent update so the fast stage, which steps between control ticks, knows
    /// whether it should be driving toward a target or toward unity.
    releasing: bool,
    release_speedup: f64,
}

impl LoopBank {
    pub fn new(config: LoopBankConfig) -> Self {
        Self {
            config,
            slow: EnvelopeDetector::new(0.0),
            mid: EnvelopeDetector::new(0.0),
            fast: EnvelopeDetector::new(0.0),
            hold_remaining_seconds: 0.0,
            releasing: false,
            release_speedup: 1.0,
        }
    }

    pub fn set_config(&mut self, config: LoopBankConfig) {
        self.config = config;
    }

    fn scaled(&self, ballistics: Ballistics) -> (f64, f64) {
        let speed = self.config.speed.max(0.01);
        (
            ballistics.attack_seconds / speed,
            ballistics.release_seconds / (speed * self.release_speedup),
        )
    }

    fn clamp_authority(value: f64, authority_db: f64) -> f64 {
        value.clamp(-authority_db, authority_db)
    }

    /// Steps the slow and mid stages. Call once per control tick with the reductions required by
    /// the 3s and 400ms measurements; `dt_seconds` is the elapsed time since the previous call.
    ///
    /// `None` means "that measurement is not usable this tick" (a reading at the silence sentinel,
    /// say) and holds the stage where it is rather than letting a sentinel value propagate as a
    /// gigantic required reduction.
    pub fn update_slow_mid(
        &mut self,
        required_slow_db: Option<f64>,
        required_mid_db: Option<f64>,
        dt_seconds: f64,
    ) {
        self.releasing = false;
        self.release_speedup = 1.0;
        self.hold_remaining_seconds = RELEASE_HOLD_SECONDS;

        if let Some(required) = required_slow_db {
            let target = required.clamp(0.0, self.config.max_gain_reduction_db);
            let (attack, release) = self.scaled(SLOW);
            self.slow.process(target, attack, release, dt_seconds);
        }

        if let Some(required) = required_mid_db {
            let target = Self::clamp_authority(required - self.slow.value(), self.config.mid_authority_db);
            let (attack, release) = self.scaled(MID);
            self.mid.process(target, attack, release, dt_seconds);
        }
    }

    /// Steps the fast stage. Call at audio sub-chunk rate with the reduction required by the ~30ms
    /// detector. Trims whatever slow+mid currently contribute, within `fast_authority_db`.
    pub fn update_fast(&mut self, required_fast_db: Option<f64>, dt_seconds: f64) {
        let target = match (self.releasing, required_fast_db) {
            (true, _) => 0.0,
            (false, None) => self.fast.value(), // unusable reading: hold
            (false, Some(required)) => {
                // **Positive-only**, unlike the mid stage's symmetric trim. The fast stage exists to
                // add reduction quickly when COM surges; it must never *remove* reduction the slower
                // stages have decided on, because at this time scale a dip in the measurement is
                // usually just a gap between syllables rather than evidence the Bed should come up.
                // Clamping at zero instead of going negative is what turns this from an envelope
                // follower into a transient catcher.
                let coarse = self.slow.value() + self.mid.value();
                (required - coarse).clamp(0.0, self.config.fast_authority_db)
            }
        };
        let (attack, release) = self.scaled(FAST);
        self.fast.process(target, attack, release, dt_seconds);
    }

    /// Asks the whole cascade to recover toward unity — `MixState::ReleaseToUnity` or
    /// `InterviewPassthrough`. Each stage releases at its own rate, so recovery is naturally
    /// progressive: the fast stage lets go first and the slow stage last, which is the shape a
    /// single-envelope design could only approximate.
    pub fn update_release(&mut self, interview: bool, dt_seconds: f64) {
        self.releasing = true;
        self.release_speedup = if interview { INTERVIEW_RELEASE_SPEEDUP } else { 1.0 };

        // Hold the current reduction through short gaps before letting go at all.
        if self.hold_remaining_seconds > 0.0 && !interview {
            self.hold_remaining_seconds -= dt_seconds;
            return;
        }

        let (attack, release) = self.scaled(SLOW);
        self.slow.process(0.0, attack, release, dt_seconds);
        let (attack, release) = self.scaled(MID);
        self.mid.process(0.0, attack, release, dt_seconds);
    }

    /// Current per-stage and total reduction in dB. The total is clamped to the configured ceiling
    /// and to zero (automix only ever reduces Bed, never boosts it).
    pub fn contributions(&self) -> LoopContributions {
        let slow_db = self.slow.value();
        let mid_db = self.mid.value();
        let fast_db = self.fast.value();
        LoopContributions {
            slow_db,
            mid_db,
            fast_db,
            total_db: (slow_db + mid_db + fast_db).clamp(0.0, self.config.max_gain_reduction_db),
        }
    }

    pub fn total_reduction_db(&self) -> f64 {
        self.contributions().total_db
    }

    pub fn reset(&mut self) {
        self.slow.reset(0.0);
        self.mid.reset(0.0);
        self.fast.reset(0.0);
        self.hold_remaining_seconds = 0.0;
        self.releasing = false;
        self.release_speedup = 1.0;
    }
}
