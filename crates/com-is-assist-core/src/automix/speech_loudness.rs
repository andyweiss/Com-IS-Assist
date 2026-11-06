/// Loudness measured **only while voice is detected**, integrated per utterance, with the previous
/// utterance carried over as a fixed-weight starting point.
///
/// The question it answers is "how loud is the commentator when they speak?", which the 3s
/// short-term window answers badly: over speech-with-gaps it also averages in the pauses, so it
/// reads systematically low, and it swings within a sentence as words and pauses move through it.
///
/// How it integrates, following BS.1770's integrated loudness:
///
/// - Fed one 400ms momentary reading per 100ms hop (BS.1770's gating blocks, 75% overlap), with
///   the voice flag for that moment.
/// - While voice is active, each block's energy is accumulated, unless it falls below the
///   absolute gate (-70 LUFS) or, optionally, more than `relative_gate_lu` below the current
///   estimate - which keeps the gaps between words out, since the detector's hangover holds the
///   flag through them.
/// - While voice is inactive, nothing changes: the level holds through pauses.
/// - When an utterance ends, its result becomes the **prior** for the next one, with a fixed
///   weight of `prior_weight_blocks`. So the estimate starts each sentence from where the last
///   one left it, and is pulled toward the new sentence as it accumulates - but history never
///   piles up: an older sentence's influence shrinks geometrically with every new one.
#[derive(Debug, Clone)]
pub struct SpeechLoudness {
    prior_weight_blocks: f64,
    relative_gate_lu: Option<f64>,
    /// Mean energy of the carried-over level, if there has been an utterance yet.
    prior_energy: Option<f64>,
    current_energy_sum: f64,
    current_blocks: u32,
    voice_was_active: bool,
}

const ABSOLUTE_GATE_LUFS: f64 = -70.0;

fn to_energy(lufs: f64) -> f64 {
    10f64.powf((lufs + 0.691) / 10.0)
}

fn to_lufs(energy: f64) -> f64 {
    -0.691 + 10.0 * energy.max(1e-30).log10()
}

impl SpeechLoudness {
    /// BS.1770's gating-block hop: feed one reading per this much audio.
    pub const HOP_SECONDS: f64 = 0.1;
    /// How much the previous utterance counts for at the start of the next one, in hops: 3s of
    /// speech. Long enough that a sentence's first syllables do not swing the level; short enough
    /// that a commentator who has got louder is followed within a sentence or two.
    pub const PRIOR_WEIGHT_BLOCKS: f64 = 30.0;
    /// Blocks this far below the current estimate are gaps between words, not speech.
    pub const RELATIVE_GATE_LU: f64 = 10.0;

    /// The settings the processor measures the commentator with.
    pub fn for_commentary() -> Self {
        Self::new(Self::PRIOR_WEIGHT_BLOCKS, Some(Self::RELATIVE_GATE_LU))
    }

    pub fn new(prior_weight_blocks: f64, relative_gate_lu: Option<f64>) -> Self {
        Self {
            prior_weight_blocks,
            relative_gate_lu,
            prior_energy: None,
            current_energy_sum: 0.0,
            current_blocks: 0,
            voice_was_active: false,
        }
    }

    /// Feeds one 100ms hop: the 400ms momentary reading at this moment and whether voice is
    /// active.
    pub fn push(&mut self, momentary_lufs: f64, voice_active: bool) {
        if voice_active {
            if !self.voice_was_active {
                self.current_energy_sum = 0.0;
                self.current_blocks = 0;
            }
            let above_absolute = momentary_lufs > ABSOLUTE_GATE_LUFS;
            let above_relative = match (self.relative_gate_lu, self.level_lufs()) {
                (Some(gate), Some(level)) => momentary_lufs > level - gate,
                _ => true,
            };
            if above_absolute && above_relative {
                self.current_energy_sum += to_energy(momentary_lufs);
                self.current_blocks += 1;
            }
        } else if self.voice_was_active && self.current_blocks > 0 {
            // The utterance is over: what it measured, blended with the prior, is the new prior.
            self.prior_energy = Some(self.combined_energy());
            self.current_energy_sum = 0.0;
            self.current_blocks = 0;
        }
        self.voice_was_active = voice_active;
    }

    fn combined_energy(&self) -> f64 {
        let (prior_sum, prior_weight) = match self.prior_energy {
            Some(energy) => (energy * self.prior_weight_blocks, self.prior_weight_blocks),
            None => (0.0, 0.0),
        };
        (prior_sum + self.current_energy_sum) / (prior_weight + self.current_blocks as f64)
    }

    /// The current estimate, or `None` before anything has been measured.
    pub fn level_lufs(&self) -> Option<f64> {
        if self.prior_energy.is_none() && self.current_blocks == 0 {
            return None;
        }
        Some(to_lufs(self.combined_energy()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn integrates_only_while_voice_is_active_and_holds_through_pauses() {
        let mut m = SpeechLoudness::new(30.0, Some(10.0));
        for _ in 0..20 {
            m.push(-20.0, true);
        }
        assert!((m.level_lufs().unwrap() + 20.0).abs() < 1e-9);
        // A pause - quiet, voice off - changes nothing.
        for _ in 0..30 {
            m.push(-60.0, false);
        }
        assert!((m.level_lufs().unwrap() + 20.0).abs() < 1e-9);
    }

    #[test]
    fn gaps_between_words_are_gated_out_while_the_flag_holds() {
        let mut m = SpeechLoudness::new(30.0, Some(10.0));
        for i in 0..40 {
            // Every fourth block is a gap between words, with the detector still holding.
            m.push(if i % 4 == 3 { -45.0 } else { -20.0 }, true);
        }
        assert!((m.level_lufs().unwrap() + 20.0).abs() < 0.01);
    }

    /// The prior has a fixed weight: a new sentence pulls the estimate toward itself in proportion
    /// to its length, and older sentences fade rather than accumulate.
    #[test]
    fn the_previous_sentence_is_a_fixed_weight_prior_not_a_growing_history() {
        let mut m = SpeechLoudness::new(30.0, None);
        for _ in 0..300 {
            m.push(-20.0, true); // a long first stretch at -20
        }
        m.push(-60.0, false);
        // A new sentence at -14, 30 blocks long: equal weight with the prior, whatever came before.
        for _ in 0..30 {
            m.push(-14.0, true);
        }
        let expected = to_lufs((to_energy(-20.0) + to_energy(-14.0)) / 2.0);
        assert!((m.level_lufs().unwrap() - expected).abs() < 1e-9);
    }
}
