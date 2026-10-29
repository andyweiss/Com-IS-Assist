use super::envelope_detector::EnvelopeDetector;

/// Default **Attack**: how quickly the reduction moves toward a larger answer, as a one-pole time
/// constant on the 3s short-term detector.
///
/// Long on purpose. A follower that is quick to grab and slow to let go sits *above* a fluctuating
/// input - it rides the peaks - so the reduction it applies exceeds what the programme balance
/// needs, by an amount that depends on how much the Bed fluctuates rather than on how much ducking
/// is called for. Measured on real broadcast programme needing ~1.9 dB: 150ms applied 4.2 dB, 1s
/// applied 3.0 dB, 2.5s applied 2.2 dB, with the gain travelling 0.3-0.4 dB/s. With attack close to
/// release the follower averages instead of peak-holding. See `Specs/TechnicalConcept.md` section
/// 5.2.
pub const DEFAULT_ATTACK_SECONDS: f64 = 2.5;
/// Default **Release**: how quickly the reduction lets go once ducking stops or less is needed.
pub const DEFAULT_RELEASE_SECONDS: f64 = 3.0;

/// Over how long the release eases in after ducking stops.
///
/// Real commentary pauses run ~1.2s, and with no resistance at all the Bed audibly swells in every
/// inter-sentence gap and re-ducks on the next phrase (see `Specs/TechnicalConcept.md` section
/// 5.3). This sits on top of the COM detector's own `hangover_seconds`, and the two together have
/// to span a natural pause. Fixed rather than exposed: it is a property of speech, not a matter of
/// taste, and having it as a user control ("Hold time") was part of what made the old surface
/// unmanageable.
const RELEASE_HOLD_SECONDS: f64 = 0.8;

/// How much the release time constant is stretched at the very start of a release, easing back to
/// 1x over `RELEASE_HOLD_SECONDS`.
///
/// This replaced a hard freeze, and the reason is worth recording. Freezing the gain outright for
/// the hold window and then releasing normally is trivially simple and sounds wrong: it puts a
/// corner in the gain curve - measured on real commentary as exactly +0.000 dB/tick for 0.8s and
/// then a step straight to -0.48 dB/tick - and a discontinuity in the *rate* of change is audible
/// as "nothing happens, then it moves", which is precisely the hold people notice.
///
/// Stretching the time constant instead means recovery begins immediately but imperceptibly
/// slowly, then accelerates smoothly to the normal rate. Short gaps still recover almost nothing,
/// because the early rate is tiny — but there is no instant at which the behaviour switches.
/// Interpolation is geometric rather than linear, which is the natural spacing for time constants.
const HOLD_RELEASE_STRETCH: f64 = 60.0;

/// How much faster `MixState::InterviewPassthrough` recovers than an ordinary release. Replaces the
/// former `interview-release-ms` parameter — one fixed ratio is easier to reason about than a free
/// time that can be set slower than the normal release by mistake.
const INTERVIEW_RELEASE_SPEEDUP: f64 = 4.0;

#[derive(Debug, Clone, Copy)]
pub struct GainLoopConfig {
    pub attack_seconds: f64,
    pub release_seconds: f64,
    pub max_gain_reduction_db: f64,
}

impl Default for GainLoopConfig {
    fn default() -> Self {
        Self {
            attack_seconds: DEFAULT_ATTACK_SECONDS,
            release_seconds: DEFAULT_RELEASE_SECONDS,
            max_gain_reduction_db: 24.0,
        }
    }
}

/// The gain computer: one feed-forward loop.
///
/// It is fed the reduction the programme balance requires, derived **feed-forward** from the dry
/// signals on the 3s short-term window (see `Specs/TechnicalConcept.md` section 5.2). Because
/// reducing the Bed by 1 dB raises the COM/IS ratio by exactly 1 LU, `required = target_ratio −
/// (COM − BED)` is the complete answer - there is no loop to converge, which is what removes the
/// windup and pumping the earlier feedback design kept producing. Attack and Release smooth the
/// way there.
///
/// This used to be a cascade of three stages (3s, 400ms and a 50ms detector), each trimming the
/// one before. The faster two were removed: on a fluctuating Bed they rode the peaks, so a mix
/// needing a subtle reduction got a heavy-handed and busy one, and the result was harder to judge
/// by ear than one honest loop. See `Specs/TechnicalConcept.md` section 5.2.
pub struct GainLoop {
    config: GainLoopConfig,
    reduction: EnvelopeDetector,
    /// Counts down once the loop is asked to release; while positive, release is eased in.
    hold_remaining_seconds: f64,
    releasing: bool,
    /// Whether the interview state is driving this release (it skips the ease-in entirely).
    interview_release: bool,
}

impl GainLoop {
    pub fn new(config: GainLoopConfig) -> Self {
        Self {
            config,
            reduction: EnvelopeDetector::new(0.0),
            hold_remaining_seconds: 0.0,
            releasing: false,
            interview_release: false,
        }
    }

    pub fn set_config(&mut self, config: GainLoopConfig) {
        self.config = config;
    }

    /// The release time constant in force: the configured Release, stretched by the ease-in at the
    /// start of a release and shortened while the interview state drives it.
    fn release_seconds(&self) -> f64 {
        let release = self.config.release_seconds.max(1e-4);
        if !self.releasing {
            return release;
        }
        if self.interview_release {
            return release / INTERVIEW_RELEASE_SPEEDUP;
        }
        // `HOLD_RELEASE_STRETCH` at the instant ducking stops, easing geometrically to 1x over
        // `RELEASE_HOLD_SECONDS`. Interview passthrough skips it - getting back to unity promptly
        // is that state's entire purpose.
        let remaining = (self.hold_remaining_seconds / RELEASE_HOLD_SECONDS).clamp(0.0, 1.0);
        release * HOLD_RELEASE_STRETCH.powf(remaining)
    }

    /// Moves toward the reduction the programme balance requires; `dt_seconds` is the time since
    /// the previous call. Called at audio sub-chunk rate, so the gain follows a smooth curve - the
    /// ballistics are exact for any step size.
    ///
    /// `None` means "the measurement is not usable right now" (a reading at the silence sentinel,
    /// say) and holds the reduction where it is rather than letting a sentinel value propagate as
    /// a gigantic required reduction.
    pub fn update(&mut self, required_db: Option<f64>, dt_seconds: f64) {
        self.releasing = false;
        self.interview_release = false;
        self.hold_remaining_seconds = RELEASE_HOLD_SECONDS;

        if let Some(required) = required_db {
            let target = required.clamp(0.0, self.config.max_gain_reduction_db);
            let attack = self.config.attack_seconds.max(1e-4);
            self.reduction.process(target, attack, self.release_seconds(), dt_seconds);
        }
    }

    /// Recovers toward unity — `MixState::ReleaseToUnity` or `InterviewPassthrough`.
    pub fn update_release(&mut self, interview: bool, dt_seconds: f64) {
        self.releasing = true;
        self.interview_release = interview;

        // Advance the ease-in. Recovery is never blocked outright; it simply starts far too slowly
        // to hear and accelerates from there (see `HOLD_RELEASE_STRETCH`).
        self.hold_remaining_seconds = (self.hold_remaining_seconds - dt_seconds).max(0.0);

        let attack = self.config.attack_seconds.max(1e-4);
        self.reduction.process(0.0, attack, self.release_seconds(), dt_seconds);
    }

    /// Current reduction in dB, clamped to the configured ceiling and to zero (automix only ever
    /// reduces Bed, never boosts it).
    pub fn reduction_db(&self) -> f64 {
        self.reduction.value().clamp(0.0, self.config.max_gain_reduction_db)
    }

    pub fn reset(&mut self) {
        self.reduction.reset(0.0);
        self.hold_remaining_seconds = 0.0;
        self.releasing = false;
        self.interview_release = false;
    }
}
