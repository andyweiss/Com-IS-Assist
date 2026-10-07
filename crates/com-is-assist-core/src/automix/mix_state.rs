/// The automix control state for one ~100ms tick, decided *entirely* by the two voice-activity
/// detectors (COM/Dialogue and IS/Bed) - nothing else. This
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
/// `InterviewPassthrough` is always active - there is no toggle. Voice on the original (IS) while
/// the commentator is silent means something on the Bed itself is worth hearing (an interview
/// picked up ambiently, say), so it should come back to unity quickly rather than stay ducked from
/// prior programme.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MixState {
    ReleaseToUnity,
    DuckToTarget,
    InterviewPassthrough,
    DuckToOvervoice,
}

impl MixState {
    /// Maps the two VAD booleans onto a state - the one place the state decision lives.
    pub fn from_vad(com_voice_active: bool, is_voice_active: bool) -> Self {
        match (com_voice_active, is_voice_active) {
            (false, false) => MixState::ReleaseToUnity,
            (true, false) => MixState::DuckToTarget,
            (false, true) => MixState::InterviewPassthrough,
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
    fn full_truth_table() {
        assert_eq!(MixState::from_vad(false, false), MixState::ReleaseToUnity);
        assert_eq!(MixState::from_vad(true, false), MixState::DuckToTarget);
        assert_eq!(MixState::from_vad(false, true), MixState::InterviewPassthrough);
        assert_eq!(MixState::from_vad(true, true), MixState::DuckToOvervoice);
    }

    #[test]
    fn only_the_two_duck_states_report_ducking() {
        assert!(!MixState::ReleaseToUnity.is_ducking());
        assert!(!MixState::InterviewPassthrough.is_ducking());
        assert!(MixState::DuckToTarget.is_ducking());
        assert!(MixState::DuckToOvervoice.is_ducking());
    }
}
