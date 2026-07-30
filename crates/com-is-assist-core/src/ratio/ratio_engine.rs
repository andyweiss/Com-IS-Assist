/// Result of one [`RatioEngine::update`] tick.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RatioResult {
    pub ratio_lu: f64,
    /// Once true, stays true forever (a meter-appropriate latch: "stop showing blank, show the
    /// last known value" — see the struct docs below). Consumers that make *control* decisions
    /// (not just display) should also check `voice_active`, since `valid` alone doesn't mean COM
    /// has real speech *right now*.
    pub valid: bool,
    /// True when real voice-activity detection (Silero VAD via
    /// `com_is_assist_core::voice_activity::SileroVad`) reports speech on COM *this tick* -
    /// independent of the `valid` latch above. Added because `valid` alone caused
    /// `AutomixEngine` to keep reacting to a frozen, increasingly-stale `ratio_lu` during long
    /// gaps after the first-ever speech burst — see `Specs/TechnicalConcept.md` section 5. This
    /// used to be a crude "is COM's short-term loudness above a fixed LUFS floor?" proxy, which
    /// couldn't tell real dialogue apart from a loud non-speech noise (room tone, static, another
    /// open mic) - a real VAD signal fixes that: Bed no longer gets reduced in response to COM
    /// having *some* sound if none of it is actually speech.
    pub voice_active: bool,
}

/// Configuration for [`RatioEngine`]'s "hold until sustained" gate.
#[derive(Debug, Clone, Copy)]
pub struct RatioEngineConfig {
    /// How many consecutive voice-active ticks are required before `ratio_lu` is (re)trusted -
    /// both the very first time ever, *and* every time voice activity resumes after a gap (since
    /// `consecutive_valid_ticks` resets to 0 on every `!voice_active` tick - see `update`). Kept
    /// deliberately small now that `voice_active` is real voice-activity detection
    /// (`voice_activity::SileroVad`), which already smooths moment-to-moment flicker via its own
    /// `hangover_seconds` - this is just a final debounce against a single stray VAD tick, not a
    /// second, redundant "wait for sustained speech" gate. It used to default to 20 (2 full
    /// seconds at the ~100ms tick rate) back when `voice_active` was a crude LUFS-floor proxy that
    /// genuinely needed a long sustain requirement to avoid trusting noise blips - once real VAD
    /// existed, that same 20-tick wait became a **pure, unnecessary latency tax paid on every
    /// single utterance**, not just once at session start: every pause-then-resume in speech
    /// re-zeroed `consecutive_valid_ticks`, so `held_ratio_lu` sat frozen at its pre-pause value
    /// for a further 2 seconds after each resumption before it would even start reflecting the new
    /// audio - see `Specs/TechnicalConcept.md` section 5 for the full latency-chain writeup (this
    /// gate was one of several contributors, alongside `AutomixEngine`'s VAD-onset fast-trigger,
    /// now implemented, which addresses the rest).
    pub valid_signal_hold_ticks: i32,
}

impl Default for RatioEngineConfig {
    fn default() -> Self {
        Self { valid_signal_hold_ticks: 2 }
    }
}

/// Tracks the COM/IS loudness ratio with a JSFX-style "hold until sustained
/// voice activity" gate (see `Specs/TechnicalConcept.md` section 2/4): rather
/// than reacting to every tick's short-term reading, the ratio only updates
/// once real voice-activity detection has reported speech on the dialogue
/// (COM) channel for a sustained number of ticks, and holds its last value
/// otherwise. This replicates the original meter's gating-histogram *policy*,
/// not its internals.
#[derive(Debug, Clone, Copy)]
pub struct RatioEngine {
    config: RatioEngineConfig,
    consecutive_valid_ticks: i32,
    held_ratio_lu: f64,
    ever_valid: bool,
}

impl RatioEngine {
    pub fn new(config: RatioEngineConfig) -> Self {
        Self {
            config,
            consecutive_valid_ticks: 0,
            held_ratio_lu: 0.0,
            ever_valid: false,
        }
    }

    /// Call once per measurement tick (JSFX/spec default: ~100ms) with the latest short-term
    /// loudness of the Bed (IS) and Dialogue (COM) signals, plus whether real voice-activity
    /// detection currently reports speech on COM (see `voice_active`'s doc comment on
    /// [`RatioResult`]).
    pub fn update(&mut self, bed_short_term_db: f64, com_short_term_db: f64, voice_active: bool) -> RatioResult {
        if voice_active {
            self.consecutive_valid_ticks += 1;
        } else {
            self.consecutive_valid_ticks = 0;
        }

        if self.consecutive_valid_ticks >= self.config.valid_signal_hold_ticks {
            self.held_ratio_lu = com_short_term_db - bed_short_term_db;
            self.ever_valid = true;
        }

        RatioResult {
            ratio_lu: self.held_ratio_lu,
            valid: self.ever_valid,
            voice_active,
        }
    }

    pub fn reset(&mut self) {
        self.consecutive_valid_ticks = 0;
        self.held_ratio_lu = 0.0;
        self.ever_valid = false;
    }
}

impl Default for RatioEngine {
    fn default() -> Self {
        Self::new(RatioEngineConfig::default())
    }
}
