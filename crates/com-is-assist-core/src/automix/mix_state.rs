/// The automix control state for one ~100ms tick, decided *entirely* by the two voice-activity
/// detectors (COM/Dialogue and IS/Bed) plus the interview-passthrough toggle - nothing else. This
/// replaces the earlier implicit branching inside `AutomixEngine::process_tick` (voice-onset
/// fast-trigger → `!voice_active` release → `valid` gate → tolerance dead-band, all interleaved),
/// which produced behavior that was hard to predict; see `Specs/TechnicalConcept.md` section 5.
///
/// The full truth table:
///
/// | COM voice | IS voice | State                  | Behavior                                        |
/// |-----------|----------|------------------------|-------------------------------------------------|
/// | no        | no       | `ReleaseToUnity`       | release toward unity at the normal Recovery time |
/// | yes       | no       | `DuckToTarget`         | duck Bed to meet `target_ratio_lu`               |
/// | no        | yes      | `InterviewPassthrough` | release toward unity at the *faster* Interview recovery time |
/// | yes       | yes      | `DuckToOvervoice`      | duck Bed to meet the higher `overvoice_ratio_lu` |
///
/// `InterviewPassthrough` (an ambient interview picked up on the Bed while the commentator is
/// silent shouldn't stay ducked from prior program) is gated behind
/// `AutomixEngineConfig::interview_passthrough_enabled`: with it off, IS-only voice behaves exactly
/// like `ReleaseToUnity` - loud PA/stadium announcements read as "voice on IS," and releasing
/// *fast* on those can be dangerous on air, so the faster release is strictly opt-in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MixState {
    ReleaseToUnity,
    DuckToTarget,
    InterviewPassthrough,
    DuckToOvervoice,
}

impl MixState {
    /// Maps the two VAD booleans (+ the interview toggle) onto a state - the one place the state
    /// decision lives.
    pub fn from_vad(com_voice_active: bool, is_voice_active: bool, interview_passthrough_enabled: bool) -> Self {
        match (com_voice_active, is_voice_active) {
            (false, false) => MixState::ReleaseToUnity,
            (true, false) => MixState::DuckToTarget,
            (false, true) => {
                if interview_passthrough_enabled {
                    MixState::InterviewPassthrough
                } else {
                    MixState::ReleaseToUnity
                }
            }
            (true, true) => MixState::DuckToOvervoice,
        }
    }

    /// Whether this state actively ducks Bed toward a ratio target (as opposed to releasing
    /// toward unity). Used by `AutomixEngine` to detect duck-state entry for the onset
    /// fast-trigger.
    pub fn is_ducking(self) -> bool {
        matches!(self, MixState::DuckToTarget | MixState::DuckToOvervoice)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_truth_table_with_interview_enabled() {
        assert_eq!(MixState::from_vad(false, false, true), MixState::ReleaseToUnity);
        assert_eq!(MixState::from_vad(true, false, true), MixState::DuckToTarget);
        assert_eq!(MixState::from_vad(false, true, true), MixState::InterviewPassthrough);
        assert_eq!(MixState::from_vad(true, true, true), MixState::DuckToOvervoice);
    }

    #[test]
    fn interview_disabled_falls_back_to_normal_release() {
        assert_eq!(MixState::from_vad(false, true, false), MixState::ReleaseToUnity);
        // The other three states are unaffected by the toggle.
        assert_eq!(MixState::from_vad(false, false, false), MixState::ReleaseToUnity);
        assert_eq!(MixState::from_vad(true, false, false), MixState::DuckToTarget);
        assert_eq!(MixState::from_vad(true, true, false), MixState::DuckToOvervoice);
    }

    #[test]
    fn only_the_two_duck_states_report_ducking() {
        assert!(!MixState::ReleaseToUnity.is_ducking());
        assert!(!MixState::InterviewPassthrough.is_ducking());
        assert!(MixState::DuckToTarget.is_ducking());
        assert!(MixState::DuckToOvervoice.is_ducking());
    }
}
