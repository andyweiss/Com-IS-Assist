// Com-IS-Assist's VST3 plugin: same shared `com_is_assist_core::automix::AutomixProcessor` as the GStreamer
// element, wrapped in a single fixed 8-channel-in/8-channel-out bus instead of the GStreamer
// element's 2-sink/3-src multi-pad layout.
//
// Channel layout (fixed, both input and output): 0-5 = Bed (6ch, Left/Right/Center/LFE/Ls/Rs), 6 =
// Dialogue (mono), 7 = unused (silent). A single fixed bus (rather than nih-plug's named multi-bus
// model, which `Specs/Ressouces/vstDefinitions.md` had flagged as an open question - `nih-plug`'s
// bus model is "one main input + one main output, plus anonymous aux ports," which doesn't map
// cleanly onto this project's originally-planned 2-named-input/3-named-output layout) sidesteps
// that uncertainty entirely and avoids the multi-output-bus host-compatibility risk multi-bus VST3
// plugins can have.
//
// `mix-dialogue-to-bed` (the "bus insert mode" toggle): when on, output channels 0-2
// (Left/Right/Center) carry leveled Bed + Dialogue mixed in via `mix_dialogue_into_bed`'s
// divergence-based pan law (matches the GStreamer element's `mix_src`, restricted to L/R/C -
// Dialogue never touches LFE or surrounds); when off, output channels 0-5 are leveled Bed alone
// (matches `is_leveled_src` - for inserting directly on a Bed/IS channel strip where the console
// sums in commentary separately downstream). Channel 6 always carries the dry Dialogue passthrough
// regardless of this toggle.

use atomic_float::AtomicF32;
use com_is_assist_core::automix::{mix_dialogue_into_bed, AutomixEngineConfig, AutomixProcessor, MixState};
use com_is_assist_core::loudness::Ebur128Meter;
use com_is_assist_core::voice_activity::{SileroVad, VoiceActivityConfig};
use nih_plug::prelude::*;
use nih_plug_egui::widgets::ParamSlider;
use nih_plug_egui::{create_egui_editor, egui, resizable_window::ResizableWindow, EguiState};
use std::num::NonZeroU32;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::Arc;

const BED_CHANNELS: u32 = 6;
const TOTAL_CHANNELS: usize = 8; // 6 Bed + 1 Dialogue + 1 unused
const DIALOGUE_CHANNEL: usize = BED_CHANNELS as usize; // index 6 (the 7th channel)
const UNUSED_CHANNEL: usize = TOTAL_CHANNELS - 1; // index 7 (the 8th channel)
// Bed channel indices, per `Ebur128Meter::bed_channel_map`'s 6-channel layout: Left, Right,
// Center, Unused (LFE), LeftSurround, RightSurround.
const LEFT_CHANNEL: usize = 0;
const RIGHT_CHANNEL: usize = 1;
const CENTER_CHANNEL: usize = 2;
const TICK_SECONDS: f64 = 0.1;

struct ComISAssist {
    params: Arc<ComISAssistParams>,
    /// `None` until `initialize()` supplies the host's sample rate - `AutomixProcessor` needs it
    /// at construction time (for the `ebur128` meters), which isn't known any earlier than that.
    processor: Option<AutomixProcessor>,
    /// Real voice-activity detection on the Dialogue (COM) sidechain - see
    /// `com_is_assist_core::voice_activity`'s doc comment for why this replaced the old LUFS-floor
    /// "is COM currently silent?" stand-in. `None` for the same reason `processor` is: it also
    /// needs the host's sample rate, only known once `initialize()` runs - and, separately, if
    /// construction ever fails (e.g. the ONNX Runtime binary couldn't be obtained), so `process()`
    /// can fail open (treat COM as always voice-active, matching the old pre-VAD behavior) rather
    /// than silently never ducking at all.
    voice_activity: Option<SileroVad>,
    /// The second, Bed-side voice-activity detector (`Specs/UI.md`'s "voice detector IS"). Fed a
    /// mono downmix of the dry Bed; together with `voice_activity` above it selects the automix
    /// state (`com_is_assist_core::automix::MixState`) - which is what makes the interview
    /// passthrough and over-voice cases possible at all. `None` on the same fail-open terms as the
    /// COM detector, in which case IS is treated as never having voice (i.e. only the ordinary
    /// release/duck states can be reached).
    is_voice_activity: Option<SileroVad>,
    /// Live R128/gain-reduction readouts, shared between the audio thread (`process()`, writer)
    /// and the GUI thread (`editor()`, reader). This is the one thing `nih-plug` genuinely
    /// requires a custom GUI for - its parameter setter is deliberately private, so a plugin
    /// can't push a live value into a host-visible parameter (see the `editor()` doc comment).
    meters: Arc<Meters>,
}

/// See `ComISAssist::meters`'s doc comment. One struct (rather than several separately-`Arc`'d
/// atomics) since every field is written together, once per `process()` call, from the same
/// `AutomixProcessor` snapshot.
struct Meters {
    gain_reduction_db: AtomicF32,
    /// The actual control-loop ratio (`AutomixProcessor::applied_ratio_lu`, signed) - used only
    /// to decide the ratio bar's red/green tolerance color (see `editor()`), not to draw the bar
    /// itself. See `display_ratio_lu`'s doc comment for why these two are kept separate.
    ratio_lu: AtomicF32,
    /// The ratio bar's actual drawn value (`AutomixProcessor::display_ratio_lu`) - consistent with
    /// `bed_momentary_lufs`/`dialogue_momentary_lufs`, unlike `ratio_lu` above, which is computed
    /// from different meters/time-constants entirely and so wouldn't visually line up with a bar
    /// drawn between those two.
    display_ratio_lu: AtomicF32,
    bed_momentary_lufs: AtomicF32,
    dialogue_momentary_lufs: AtomicF32,
    /// Real voice-activity-detection state, for the GUI's "Voice activity Comm" indicator
    /// (`Specs/UI.md`'s originally-planned LED, blocked until real VAD existed).
    voice_active: AtomicBool,
    /// The Bed-side equivalent, for `Specs/UI.md`'s "Voice activity IS" LED.
    is_voice_active: AtomicBool,
    /// The current `MixState`, as its `u8` discriminant (see `mix_state_index`/`MIX_STATE_LABELS`).
    /// An `AtomicU8` because the state machine's current state is the single most useful thing to
    /// show when the question is "why is it doing that right now?".
    mix_state: AtomicU8,
    /// What each loop stage is contributing, in dB. The three have visibly different characters -
    /// slow carries the programme balance, mid tracks phrases, fast catches onsets - so seeing the
    /// split is the quickest way to tell *which* of them is responsible for what you are hearing.
    slow_db: AtomicF32,
    mid_db: AtomicF32,
    fast_db: AtomicF32,
}

impl Meters {
    fn new() -> Self {
        let silence = Ebur128Meter::NEGATIVE_INFINITY_DB as f32;
        Self {
            gain_reduction_db: AtomicF32::new(0.0),
            ratio_lu: AtomicF32::new(0.0),
            display_ratio_lu: AtomicF32::new(0.0),
            bed_momentary_lufs: AtomicF32::new(silence),
            dialogue_momentary_lufs: AtomicF32::new(silence),
            voice_active: AtomicBool::new(false),
            is_voice_active: AtomicBool::new(false),
            mix_state: AtomicU8::new(0),
            slow_db: AtomicF32::new(0.0),
            mid_db: AtomicF32::new(0.0),
            fast_db: AtomicF32::new(0.0),
        }
    }
}

/// Display names for each `MixState`, indexed by `mix_state_index`.
const MIX_STATE_LABELS: [&str; 4] = ["Release", "Duck \u{2192} target", "Interview passthrough", "Duck \u{2192} over-voice"];

fn mix_state_index(state: MixState) -> u8 {
    match state {
        MixState::ReleaseToUnity => 0,
        MixState::DuckToTarget => 1,
        MixState::InterviewPassthrough => 2,
        MixState::DuckToOvervoice => 3,
    }
}

#[derive(Params)]
struct ComISAssistParams {
    #[id = "target-ratio"]
    pub target_ratio: FloatParam,
    /// The higher ratio COM must clear while *both* COM and IS carry voice (the "over-voice"
    /// double-talk case - `MixState::DuckToOvervoice`). An absolute target, independent of
    /// `target_ratio`, so the two situations can be dialed in separately by ear.
    #[id = "overvoice-ratio"]
    pub overvoice_ratio: FloatParam,
    #[id = "max-gain-reduction-db"]
    pub max_gain_reduction_db: FloatParam,
    /// "Speed" - the single timing control. Scales every loop stage's ballistics together.
    /// Replaces the five separate time parameters the old single-loop design needed: with the
    /// multi-loop cascade the effective attack and release are emergent, so there is nothing
    /// meaningful left for individual times to set. Raise it if the mixer feels sluggish, lower it
    /// if it breathes.
    #[id = "speed"]
    pub speed: FloatParam,
    /// Lookahead in milliseconds, default 0. Non-zero delays both Bed and Dialogue so the loops can
    /// act on a transient before it reaches the output, and the plugin reports the delay to the
    /// host as latency.
    #[id = "lookahead-ms"]
    pub lookahead_ms: FloatParam,
    /// Enables the interview-passthrough state at all. Off by default: a loud PA or stadium
    /// announcement also reads as "voice on IS", and recovering the Bed fast on that is risky on
    /// air - see `com_is_assist_core::automix::MixState`.
    #[id = "interview-passthrough"]
    pub interview_passthrough: BoolParam,
    /// `true` bypasses automix entirely (Bed passes through at unity gain, no ducking) - `false`
    /// (default) is normal operation. Inverted from the earlier `automix-enable` naming/polarity
    /// to match the conventional meaning of a "Bypass" control.
    #[id = "bypass"]
    pub bypass: BoolParam,
    /// The "bus insert mode" toggle - see the module doc comment.
    #[id = "mix-dialogue-to-bed"]
    pub mix_dialogue_to_bed: BoolParam,
    /// "Voice divergence" (`Specs/Ressouces/UI.md`): 0% = Dialogue is Center-only when mixed in,
    /// 100% = split across Left/Right only. See `mix_dialogue_into_bed`'s doc comment for the pan
    /// law. Only has an effect while `mix_dialogue_to_bed` is on.
    #[id = "divergence"]
    pub divergence: FloatParam,

    /// Deliberately *not* `#[persist]` - the window is meant to always open at the size the GUI
    /// layout actually needs (see `ComISAssistParams::default()`), not whatever a host happened to
    /// save from a previous, differently-sized layout (which silently overrode every attempt to fix
    /// the window's size while this was still marked persistent - a real bug hit during development,
    /// not a hypothetical one). The user can still drag the resize corner within a session; that
    /// resize just won't be remembered across a host reload/reopen anymore.
    editor_state: Arc<EguiState>,
}

impl Default for ComISAssist {
    fn default() -> Self {
        Self {
            params: Arc::new(ComISAssistParams::default()),
            processor: None,
            voice_activity: None,
            is_voice_activity: None,
            meters: Arc::new(Meters::new()),
        }
    }
}

impl Default for ComISAssistParams {
    fn default() -> Self {
        let automix = AutomixEngineConfig::default();
        Self {
            target_ratio: FloatParam::new(
                "Target Ratio",
                automix.target_ratio_lu as f32,
                // Positive only - COM is meant to sit *on top of* IS by this many LU, never below
                // it, so a negative target never made sense to expose here.
                FloatRange::Linear { min: 0.0, max: 12.0 },
            )
            .with_step_size(0.1)
            .with_unit(" LU"),
            overvoice_ratio: FloatParam::new(
                "Over-voice Ratio",
                automix.overvoice_ratio_lu as f32,
                FloatRange::Linear { min: 0.0, max: 24.0 },
            )
            .with_step_size(0.1)
            .with_unit(" LU"),
            max_gain_reduction_db: FloatParam::new(
                "Max Gain Reduction",
                automix.max_gain_reduction_db as f32,
                FloatRange::Linear { min: 0.0, max: 48.0 },
            )
            .with_step_size(0.1)
            .with_unit(" dB"),
            speed: FloatParam::new(
                "Speed",
                automix.speed as f32,
                // Multiplicative, so a symmetric log-ish sweep around 1.0 feels even either way.
                FloatRange::Skewed { min: 0.25, max: 4.0, factor: FloatRange::skew_factor(0.0) },
            )
            .with_step_size(0.05)
            .with_unit("x"),
            lookahead_ms: FloatParam::new(
                "Lookahead",
                0.0,
                FloatRange::Linear { min: 0.0, max: 20.0 },
            )
            .with_step_size(0.5)
            .with_unit(" ms"),
            interview_passthrough: BoolParam::new("Interview Passthrough", automix.interview_passthrough_enabled),
            bypass: BoolParam::new("Bypass", false),
            mix_dialogue_to_bed: BoolParam::new("Mix Dialogue to Bed", true),
            divergence: FloatParam::new("Voice Divergence", 0.0, FloatRange::Linear { min: 0.0, max: 100.0 })
                .with_unit(" %"),
            editor_state: EguiState::from_size(820, 600),
        }
    }
}

impl ComISAssistParams {
    fn automix_config(&self) -> AutomixEngineConfig {
        AutomixEngineConfig {
            target_ratio_lu: self.target_ratio.value() as f64,
            overvoice_ratio_lu: self.overvoice_ratio.value() as f64,
            max_gain_reduction_db: self.max_gain_reduction_db.value() as f64,
            speed: self.speed.value() as f64,
            interview_passthrough_enabled: self.interview_passthrough.value(),
        }
    }
}

/// Wraps `add_contents` in a fixed-width, horizontally-centered column. Used for every meter
/// column so none of them changes width frame-to-frame as its live numeric label's text changes
/// length (e.g. "-inf LUFS" vs "-6.3 LUFS") - without this, columns further along the row visibly
/// shift/jump left and right as values change, since an unconstrained `ui.vertical` sizes itself to
/// its widest line of content.
fn fixed_width_column(ui: &mut egui::Ui, width: f32, add_contents: impl FnOnce(&mut egui::Ui)) {
    ui.scope(|ui| {
        ui.set_width(width);
        ui.vertical_centered(|ui| add_contents(ui));
    });
}

// Professional-meter proportions (IEC 60268-18-style bargraph: tall, narrow, segmented) rather
// than the earlier compact/solid-fill bars - purely a look-and-feel change, no DSP/value meaning
// changed here. `METER_BAR_HEIGHT` in particular is the main "make it long" lever.
const METER_BAR_HEIGHT: f32 = 420.0;
const METER_BAR_WIDTH: f32 = 20.0;
const GR_COLUMN_WIDTH: f32 = 58.0;
const LOUDNESS_COLUMN_WIDTH: f32 = 62.0;
const RATIO_COLUMN_WIDTH: f32 = 58.0;
const SCALE_COLUMN_WIDTH: f32 = 32.0;

const LOUDNESS_METER_MIN_LUFS: f32 = -60.0;
const LOUDNESS_METER_MAX_LUFS: f32 = 0.0;
/// Fixed display range for `gain_reduction_meter`'s bar and scale - matches `max-gain-reduction-db`'s
/// own hard ceiling (`FloatRange::Linear { max: 48.0, .. }`), rather than the live parameter value
/// (which defaults to 24), so headroom stays visible and the ticks stay a stable reference
/// regardless of the configured ceiling.
const GAIN_REDUCTION_METER_MAX_DB: f32 = 48.0;

/// Half-width of the ratio meter's "on target" green band, in LU - a GUI-only readability
/// threshold, deliberately independent of `AutomixEngineConfig`'s `tolerance_lu` (which governs
/// the DSP's actual correction decisions, not what this indicator shows).
const RATIO_METER_GREEN_BAND_LU: f32 = 2.0;

/// Meter background - shared by every bar's unfilled portion.
const METER_BACKGROUND: egui::Color32 = egui::Color32::from_gray(22);

/// A vertical momentary-loudness meter that fills from the bottom up (the conventional level-meter
/// orientation), normalized against a fixed -60..0 LUFS display range chosen to comfortably cover
/// typical Bed/Dialogue program levels down to near-silence. `Ebur128Meter::NEGATIVE_INFINITY_DB`
/// (silence/insufficient data) is shown as "-inf" rather than a literal "-100.0", matching how the
/// original JSFX reference meter displays it. `gain_reduction_db`, when `Some` (the IS bar only),
/// carves the currently-applied reduction out as a red segment at the *top* of the fill, extending
/// down by the reduction amount (1dB of reduction = 1 LU on this same scale, so this is exact) - the
/// boundary between the bar's own color and red is Bed's effective, audible level. `voice_active`,
/// when `Some` (the COM bar only), draws the voice-activity LED directly under this bar - COM is
/// what the VAD actually listens to, so the indicator reads most naturally right under it rather
/// than off in its own row spanning the whole meter bank.
fn loudness_meter(
    ui: &mut egui::Ui,
    label: &str,
    lufs: f32,
    color: egui::Color32,
    gain_reduction_db: Option<f32>,
    voice_active: Option<bool>,
) {
    fixed_width_column(ui, LOUDNESS_COLUMN_WIDTH, |ui| {
        ui.label(label);
        let (rect, _response) = ui.allocate_exact_size(egui::vec2(METER_BAR_WIDTH, METER_BAR_HEIGHT), egui::Sense::hover());
        let painter = ui.painter();
        painter.rect_filled(rect, 2.0, METER_BACKGROUND);

        let normalized = ((lufs - LOUDNESS_METER_MIN_LUFS) / (LOUDNESS_METER_MAX_LUFS - LOUDNESS_METER_MIN_LUFS))
            .clamp(0.0, 1.0);
        let fill_height = rect.height() * normalized;
        let filled = egui::Rect::from_min_max(egui::pos2(rect.min.x, rect.max.y - fill_height), rect.max);
        painter.rect_filled(filled, 2.0, color);

        if let Some(reduction_db) = gain_reduction_db {
            let reduction_height = (rect.height()
                * (reduction_db / (LOUDNESS_METER_MAX_LUFS - LOUDNESS_METER_MIN_LUFS)))
                .clamp(0.0, fill_height);
            let reduced = egui::Rect::from_min_max(
                egui::pos2(rect.min.x, filled.min.y),
                egui::pos2(rect.max.x, filled.min.y + reduction_height),
            );
            painter.rect_filled(reduced, 2.0, egui::Color32::from_rgb(224, 32, 32));
        }

        painter.rect_stroke(rect, 2.0, egui::Stroke::new(1.0_f32, egui::Color32::from_gray(90)), egui::StrokeKind::Outside);

        if lufs <= Ebur128Meter::NEGATIVE_INFINITY_DB as f32 {
            ui.label("-inf LUFS");
        } else {
            ui.label(format!("{lufs:.1} LUFS"));
        }

        if let Some(active) = voice_active {
            ui.add_space(4.0);
            voice_activity_led(ui, active);
        }
    });
}

/// A vertical gain-reduction meter that fills top-down (0dB at top, increasing downward - the
/// conventional GR-meter orientation, opposite of `loudness_meter`'s bottom-up convention),
/// normalized against the fixed `GAIN_REDUCTION_METER_MAX_DB` range. Standalone (in addition to the
/// carve-out on the IS bar above) so gain reduction and ratio can both be read clearly at a glance,
/// over a wider range than the IS bar's carve-out alone would show, while tuning attack/hold/release.
fn gain_reduction_meter(ui: &mut egui::Ui, gain_reduction_db: f32) {
    fixed_width_column(ui, GR_COLUMN_WIDTH, |ui| {
        ui.label("GR");
        let (rect, _response) = ui.allocate_exact_size(egui::vec2(METER_BAR_WIDTH, METER_BAR_HEIGHT), egui::Sense::hover());
        let painter = ui.painter();
        painter.rect_filled(rect, 2.0, METER_BACKGROUND);

        let normalized = (gain_reduction_db / GAIN_REDUCTION_METER_MAX_DB).clamp(0.0, 1.0);
        let filled = egui::Rect::from_min_max(rect.min, egui::pos2(rect.max.x, rect.min.y + rect.height() * normalized));
        painter.rect_filled(filled, 2.0, egui::Color32::from_rgb(224, 32, 32));
        painter.rect_stroke(rect, 2.0, egui::Stroke::new(1.0_f32, egui::Color32::from_gray(90)), egui::StrokeKind::Outside);

        ui.label(format!("{gain_reduction_db:.1} dB"));
    });
}

/// The COM/IS ratio, drawn as a bar spanning the gap between the IS momentary-loudness level and
/// `ratio_lu` above it, on the *same* shared LUFS scale as `loudness_meter` (LU is literally a
/// difference of LUFS values, so this is valid, not just visually convenient) - mirrors how the
/// original JSFX reference meter draws its RATIO bar as the visible gap between the adjacent IS and
/// COM bars, rather than as an independent meter with its own arbitrary range. `ratio_lu` should be
/// `AutomixProcessor::display_ratio_lu` (already floored at 0, and computed from the same momentary
/// meters the IS/COM bars show) - passing `applied_ratio_lu` here instead would make the bar's
/// height inconsistent with what's actually drawn on either side of it (see that method's doc
/// comment for why). `color` is the caller's call on whether the current ratio meets the
/// configured target - see the `editor()` call site, which goes red outside the same tolerance
/// band `AutomixEngine` itself uses to decide whether to adjust gain.
fn ratio_meter(ui: &mut egui::Ui, is_lufs: f32, ratio_lu: f32, color: egui::Color32) {
    fixed_width_column(ui, RATIO_COLUMN_WIDTH, |ui| {
        ui.label("Ratio");
        let (rect, _response) = ui.allocate_exact_size(egui::vec2(METER_BAR_WIDTH, METER_BAR_HEIGHT), egui::Sense::hover());
        let painter = ui.painter();
        painter.rect_filled(rect, 2.0, METER_BACKGROUND);

        let normalize = |lufs: f32| {
            ((lufs - LOUDNESS_METER_MIN_LUFS) / (LOUDNESS_METER_MAX_LUFS - LOUDNESS_METER_MIN_LUFS)).clamp(0.0, 1.0)
        };
        let (low, high) = {
            let (a, b) = (normalize(is_lufs), normalize(is_lufs + ratio_lu));
            if a <= b { (a, b) } else { (b, a) }
        };
        let filled = egui::Rect::from_min_max(
            egui::pos2(rect.min.x, rect.max.y - rect.height() * high),
            egui::pos2(rect.max.x, rect.max.y - rect.height() * low),
        );
        painter.rect_filled(filled, 2.0, color);
        painter.rect_stroke(rect, 2.0, egui::Stroke::new(1.0_f32, egui::Color32::from_gray(90)), egui::StrokeKind::Outside);

        ui.label(format!("{ratio_lu:+.1} LU"));
    });
}

/// A column pairing a name-label-height spacer with `vertical_scale`, so the ticks line up with
/// the bars' drawn area (which sits below each bar's own name label). Placed immediately to the
/// right of the IS bar and the COM bar (see `editor()`), as close as the label/tick text allows, so
/// each bar reads against its own nearby axis rather than one shared scale off to a single side.
fn loudness_scale_column(ui: &mut egui::Ui) {
    fixed_width_column(ui, SCALE_COLUMN_WIDTH, |ui| {
        ui.label(" ");
        vertical_scale(ui, METER_BAR_HEIGHT, LOUDNESS_METER_MIN_LUFS, LOUDNESS_METER_MAX_LUFS, 5.0, false);
    });
}

/// Same idea as `loudness_scale_column`, for `gain_reduction_meter`'s fixed 0..48dB range.
fn gain_reduction_scale_column(ui: &mut egui::Ui) {
    fixed_width_column(ui, SCALE_COLUMN_WIDTH, |ui| {
        ui.label(" ");
        vertical_scale(ui, METER_BAR_HEIGHT, 0.0, GAIN_REDUCTION_METER_MAX_DB, 6.0, true);
    });
}

/// A vertical scale (tick marks + numeric labels) spanning `[min, max]` over `height` pixels.
/// `top_down` selects the tick direction to match the bar it labels: `false` for the bottom-up
/// loudness/ratio convention (`min` at the bottom, `max` at the top - e.g. -60 LUFS at bottom, 0
/// LUFS at top), `true` for `gain_reduction_meter`'s top-down convention (`min` i.e. 0dB at the
/// top, `max` at the bottom). The allocated rect is wide enough to contain the tick text itself
/// (not just the tick line), so egui's own layout system - which doesn't know about anything drawn
/// via `Painter` outside the rect it was told about - correctly accounts for the scale's full
/// visual footprint; otherwise neighboring widgets could crowd or overlap the numbers. The
/// topmost/bottommost labels are also nudged inward (clamped half a line-height from the edge) so
/// they stay fully visible rather than clipping.
fn vertical_scale(ui: &mut egui::Ui, height: f32, min: f32, max: f32, step: f32, top_down: bool) {
    let (rect, _response) = ui.allocate_exact_size(egui::vec2(SCALE_COLUMN_WIDTH, height), egui::Sense::hover());
    let painter = ui.painter();
    let half_line = 5.0;
    let mut value = min;
    while value <= max + 0.001 {
        let normalized = ((value - min) / (max - min)).clamp(0.0, 1.0);
        let y = if top_down {
            rect.min.y + rect.height() * normalized
        } else {
            rect.max.y - rect.height() * normalized
        };
        let text_y = y.clamp(rect.min.y + half_line, rect.max.y - half_line);
        painter.hline(rect.min.x..=(rect.min.x + 4.0), y, egui::Stroke::new(1.0_f32, egui::Color32::from_gray(140)));
        painter.text(
            egui::pos2(rect.min.x + 7.0, text_y),
            egui::Align2::LEFT_CENTER,
            format!("{value:.0}"),
            egui::FontId::proportional(9.0),
            egui::Color32::from_gray(160),
        );
        value += step;
    }
}

/// A small filled circle + short label, green when `active` else dark gray - `Specs/UI.md`'s
/// originally planned "Voice activity Comm (green LED)", blocked until real voice-activity
/// detection existed (`com_is_assist_core::voice_activity::SileroVad`). Stacked vertically
/// (dot above label) rather than side by side, so it fits inside the narrow COM meter column it's
/// drawn under (see `loudness_meter`'s `voice_active` parameter) instead of needing a whole row's
/// width for a longer horizontal label.
fn voice_activity_led(ui: &mut egui::Ui, active: bool) {
    ui.vertical_centered(|ui| {
        let (rect, _response) = ui.allocate_exact_size(egui::vec2(10.0, 10.0), egui::Sense::hover());
        let color = if active {
            egui::Color32::from_rgb(0x39, 0xC8, 0x39)
        } else {
            egui::Color32::from_gray(60)
        };
        ui.painter().circle_filled(rect.center(), rect.width() / 2.0, color);
        ui.label("Voice");
    });
}

/// `nih_plug_egui` has no built-in bool-parameter widget (`ParamSlider` treats everything as a
/// continuous/stepped slider) - a checkbox reads far more naturally for a two-state toggle like
/// `bypass`/`mix-dialogue-to-bed`, so this drives one directly through `ParamSetter`, the same
/// begin/set/end sequence `ParamSlider` itself uses internally.
fn bool_param_checkbox(ui: &mut egui::Ui, setter: &ParamSetter, param: &BoolParam, label: &str) {
    let mut value = param.value();
    if ui.checkbox(&mut value, label).changed() {
        setter.begin_set_parameter(param);
        setter.set_parameter(param, value);
        setter.end_set_parameter(param);
    }
}

impl Plugin for ComISAssist {
    const NAME: &'static str = "Com-IS-Assist";
    const VENDOR: &'static str = "Com-IS-Assist";
    const URL: &'static str = "https://github.com/andyweiss/Com-IS-Assist";
    const EMAIL: &'static str = "info@example.com";
    const VERSION: &'static str = env!("CARGO_PKG_VERSION");

    // Single fixed 8-in/8-out bus - see the module doc comment for why this replaces the
    // originally-planned named multi-bus layout.
    const AUDIO_IO_LAYOUTS: &'static [AudioIOLayout] = &[AudioIOLayout {
        main_input_channels: NonZeroU32::new(TOTAL_CHANNELS as u32),
        main_output_channels: NonZeroU32::new(TOTAL_CHANNELS as u32),
        aux_input_ports: &[],
        aux_output_ports: &[],
        names: PortNames::const_default(),
    }];

    const MIDI_INPUT: MidiConfig = MidiConfig::None;
    const MIDI_OUTPUT: MidiConfig = MidiConfig::None;
    const SAMPLE_ACCURATE_AUTOMATION: bool = true;

    type SysExMessage = ();
    type BackgroundTask = ();

    fn params(&self) -> Arc<dyn Params> {
        self.params.clone()
    }

    /// A custom GUI, needed in the first place only because `nih-plug` has no way to push a live
    /// value into a host-visible parameter from `process()` (see `Meters`'s doc comment) - but
    /// since a GUI now exists, it also exposes the parameters directly (via `ParamSlider`/
    /// `ui.checkbox`, both still routed through the host's automation via `ParamSetter`) rather
    /// than relying solely on the host's own generic parameter UI.
    fn editor(&mut self, _async_executor: AsyncExecutor<Self>) -> Option<Box<dyn Editor>> {
        let meters = self.meters.clone();
        let params = self.params.clone();
        let egui_state = self.params.editor_state.clone();
        create_egui_editor(
            self.params.editor_state.clone(),
            (),
            |_, _| {},
            move |egui_ctx, setter, _state| {
                ResizableWindow::new("com-is-assist-editor-window")
                    .min_size(egui::Vec2::new(760.0, 560.0))
                    .show(egui_ctx, egui_state.as_ref(), |ui| {
                        ui.add_space(10.0);
                        ui.horizontal(|ui| {
                            ui.add_space(12.0);
                            ui.heading("Com-IS-Assist");
                        });
                        ui.add_space(10.0);

                        let gain_reduction_db = meters.gain_reduction_db.load(Ordering::Relaxed);
                        let is_lufs = meters.bed_momentary_lufs.load(Ordering::Relaxed); // pre-gain
                        // What's actually audible from Bed right now - not shown directly on the
                        // (now standalone) IS bar, but still the right anchor for the ratio bar,
                        // matching `AutomixProcessor::display_ratio_lu`'s own definition of "the
                        // gap that's actually audible."
                        let effective_is_lufs = is_lufs - gain_reduction_db;
                        let com_lufs = meters.dialogue_momentary_lufs.load(Ordering::Relaxed);
                        // The bar's drawn height/label - consistent with `effective_is_lufs`/
                        // `com_lufs` above (see `AutomixProcessor::display_ratio_lu`'s doc comment).
                        let display_ratio_lu = meters.display_ratio_lu.load(Ordering::Relaxed);
                        // The actual control-loop ratio - used only for the tolerance check below,
                        // not for drawing (see `Meters::ratio_lu`'s doc comment).
                        let ratio_lu = meters.ratio_lu.load(Ordering::Relaxed);

                        // Ratio bar color is an at-a-glance read of where the current COM/IS ratio
                        // sits relative to the user's target, for tuning by ear - a GUI-only
                        // signal, not the DSP's own dead-band (`AutomixEngine::process_tick` still
                        // uses `AutomixEngineConfig`'s tolerances for its actual gain decisions,
                        // unchanged by this):
                        //   grey   - ratio is negative: COM is currently *quieter* than IS (a
                        //            distinct situation from "not loud enough yet", worth its own
                        //            neutral color rather than alarming red)
                        //   red    - positive but under target by more than the green band
                        //   green  - within +/-RATIO_METER_GREEN_BAND_LU of target (on target)
                        //   orange - above the green band: COM louder than needed (not wrong, just
                        //            excess headroom)
                        // Deliberately keyed off `ratio_lu` (the signed, real control-loop value),
                        // not `display_ratio_lu` (floored at 0) - the latter can never be negative,
                        // so the grey state would be unreachable.
                        // Must follow whichever target the engine is *actually* holding to right
                        // now, not always `target_ratio`: while the state machine is in
                        // `DuckToOvervoice` it is correctly aiming at the (higher) over-voice
                        // ratio, and colouring that against the normal target would show orange
                        // ("excess headroom") for exactly as long as it does the right thing.
                        let mix_state_index = meters.mix_state.load(Ordering::Relaxed);
                        let target_ratio_lu = if mix_state_index == 3 {
                            params.overvoice_ratio.value()
                        } else {
                            params.target_ratio.value()
                        };
                        let ratio_color = if ratio_lu < 0.0 {
                            egui::Color32::from_gray(120)
                        } else if ratio_lu < target_ratio_lu - RATIO_METER_GREEN_BAND_LU {
                            egui::Color32::from_rgb(224, 32, 32)
                        } else if ratio_lu <= target_ratio_lu + RATIO_METER_GREEN_BAND_LU {
                            egui::Color32::from_rgb(0x39, 0xC8, 0x39)
                        } else {
                            egui::Color32::from_rgb(224, 140, 32)
                        };

                        ui.horizontal(|ui| {
                            ui.add_space(12.0);

                            // Meter bank (left): a tall, segmented professional bargraph panel -
                            // gain reduction and ratio sit right next to each other so the two are
                            // easy to read together while tuning. The voice-activity LED is drawn
                            // as part of the COM column itself (see `loudness_meter`'s
                            // `voice_active` parameter), directly under the bar it actually reflects.
                            ui.horizontal(|ui| {
                                // Precise control over gaps: egui's automatic `item_spacing` would
                                // otherwise stack on top of every explicit `add_space` below,
                                // making the intended-to-be-tight bar/scale gaps look much bigger
                                // than requested.
                                ui.spacing_mut().item_spacing.x = 0.0;
                                gain_reduction_meter(ui, gain_reduction_db);
                                ui.add_space(2.0);
                                gain_reduction_scale_column(ui);
                                ui.add_space(18.0);
                                loudness_meter(
                                    ui,
                                    "IS",
                                    is_lufs,
                                    egui::Color32::from_rgb(0x52, 0xFF, 0xFE),
                                    Some(gain_reduction_db),
                                    Some(meters.is_voice_active.load(Ordering::Relaxed)),
                                );
                                ui.add_space(2.0);
                                loudness_scale_column(ui);
                                ui.add_space(18.0);
                                ratio_meter(ui, effective_is_lufs, display_ratio_lu, ratio_color);
                                ui.add_space(18.0);
                                loudness_meter(
                                    ui,
                                    "COM",
                                    com_lufs,
                                    egui::Color32::from_rgb(0xEB, 0x9E, 0x34),
                                    None,
                                    Some(meters.voice_active.load(Ordering::Relaxed)),
                                );
                                ui.add_space(2.0);
                                loudness_scale_column(ui);
                            });

                            ui.add_space(20.0);
                            ui.separator();
                            ui.add_space(20.0);

                            // Parameters (right).
                            ui.vertical(|ui| {
                                ui.label("Controls");
                                ui.add_space(6.0);

                                egui::Grid::new("com-is-assist-controls")
                                    .num_columns(2)
                                    .spacing([12.0, 10.0])
                                    .show(ui, |ui| {
                                        ui.label("Target ratio");
                                        ui.add(ParamSlider::for_param(&params.target_ratio, setter));
                                        ui.end_row();

                                        ui.label("Over-voice ratio");
                                        ui.add(ParamSlider::for_param(&params.overvoice_ratio, setter));
                                        ui.end_row();

                                        ui.label("Max gain reduction");
                                        ui.add(ParamSlider::for_param(&params.max_gain_reduction_db, setter));
                                        ui.end_row();

                                        ui.label("Speed");
                                        ui.add(ParamSlider::for_param(&params.speed, setter));
                                        ui.end_row();

                                        ui.label("Lookahead");
                                        ui.add(ParamSlider::for_param(&params.lookahead_ms, setter));
                                        ui.end_row();

                                        ui.label("Voice divergence");
                                        ui.add(ParamSlider::for_param(&params.divergence, setter));
                                        ui.end_row();
                                    });

                                ui.add_space(20.0);
                                bool_param_checkbox(ui, setter, &params.bypass, "Bypass");
                                ui.add_space(10.0);
                                bool_param_checkbox(ui, setter, &params.mix_dialogue_to_bed, "Mix dialogue to bed");
                                ui.add_space(10.0);
                                bool_param_checkbox(
                                    ui,
                                    setter,
                                    &params.interview_passthrough,
                                    "Interview passthrough (IS voice \u{2192} fast recovery)",
                                );

                                // The live state-machine readout. With the mixer's behavior now
                                // defined entirely by which of four states it's in, showing that
                                // state directly is the fastest answer to "why is it doing that?"
                                // - far more legible than inferring it from the meters.
                                ui.add_space(20.0);
                                ui.separator();
                                ui.add_space(8.0);
                                ui.horizontal(|ui| {
                                    ui.label("State:");
                                    let index = mix_state_index as usize;
                                    let label = MIX_STATE_LABELS.get(index).copied().unwrap_or("-");
                                    let color = match index {
                                        1 => egui::Color32::from_rgb(0x52, 0xFF, 0xFE), // duck -> target
                                        2 => egui::Color32::from_rgb(0x39, 0xC8, 0x39), // interview
                                        3 => egui::Color32::from_rgb(224, 140, 32),     // over-voice
                                        _ => egui::Color32::from_gray(150),             // release
                                    };
                                    ui.colored_label(color, label);
                                });
                                ui.add_space(4.0);
                                ui.horizontal(|ui| {
                                    ui.label("Loops:");
                                    ui.colored_label(
                                        egui::Color32::from_rgb(0x52, 0xFF, 0xFE),
                                        format!("slow {:.1}", meters.slow_db.load(Ordering::Relaxed)),
                                    );
                                    ui.colored_label(
                                        egui::Color32::from_rgb(0x39, 0xC8, 0x39),
                                        format!("mid {:+.1}", meters.mid_db.load(Ordering::Relaxed)),
                                    );
                                    ui.colored_label(
                                        egui::Color32::from_rgb(224, 140, 32),
                                        format!("fast {:+.1}", meters.fast_db.load(Ordering::Relaxed)),
                                    );
                                });
                            });
                        });
                        ui.add_space(14.0);
                    });
            },
        )
    }

    fn initialize(
        &mut self,
        _audio_io_layout: &AudioIOLayout,
        buffer_config: &BufferConfig,
        context: &mut impl InitContext<Self>,
    ) -> bool {
        self.processor = AutomixProcessor::new(
            BED_CHANNELS,
            buffer_config.sample_rate as u32,
            self.params.automix_config(),
            TICK_SECONDS,
        )
        .ok();
        if let Some(processor) = self.processor.as_mut() {
            processor.set_lookahead_seconds(self.params.lookahead_ms.value() as f64 / 1000.0);
            // Tell the host up front, so delay compensation is right from the first block.
            context.set_latency_samples(processor.lookahead_frames() as u32);
        }
        // `.ok()`, not `.expect(...)`: if this fails (e.g. the ONNX Runtime binary couldn't be
        // obtained), `process()` fails open rather than the whole plugin refusing to initialize
        // over what's ultimately just a display/gating refinement, not the core automix path.
        self.voice_activity = SileroVad::new(buffer_config.sample_rate as u32, VoiceActivityConfig::default()).ok();
        // A second, independent detector instance for the Bed - same model and settings, its own
        // recurrent state and hangover timer (see `is_voice_activity`'s doc comment).
        self.is_voice_activity =
            SileroVad::new(buffer_config.sample_rate as u32, VoiceActivityConfig::default()).ok();
        self.processor.is_some()
    }

    fn process(
        &mut self,
        buffer: &mut Buffer,
        _aux: &mut AuxiliaryBuffers,
        context: &mut impl ProcessContext<Self>,
    ) -> ProcessStatus {
        let Some(processor) = self.processor.as_mut() else {
            return ProcessStatus::Error("processor not initialized");
        };

        // `initialize()` only reads these parameters once, at plugin load - without refreshing
        // them here on every block, a live parameter change would silently have no effect. Cheap -
        // a couple of struct copies - and preserves all accumulated state (loudness detectors, the
        // loop stages' in-flight values), unlike rebuilding the processor would.
        processor.set_config(self.params.automix_config());
        let previous_lookahead = processor.lookahead_frames();
        processor.set_lookahead_seconds(self.params.lookahead_ms.value() as f64 / 1000.0);
        if processor.lookahead_frames() != previous_lookahead {
            // Only on an actual change: hosts generally rebuild their delay-compensation graph on
            // this, so calling it every block would be wasteful and disruptive.
            context.set_latency_samples(processor.lookahead_frames() as u32);
        }
        processor.set_automix_enabled(!self.params.bypass.value());

        let mix_dialogue_to_bed = self.params.mix_dialogue_to_bed.value();
        let divergence = self.params.divergence.value() / 100.0;

        let num_samples = buffer.samples();
        let channels = buffer.as_slice();
        if channels.len() < TOTAL_CHANNELS {
            return ProcessStatus::Error("expected a fixed 8-channel bus");
        }

        // com-is-assist-core works with interleaved audio (matching the GStreamer element's convention);
        // nih-plug hands us planar per-channel slices, so de-interleave Bed for the shared
        // processor and re-interleave on the way back out.
        let mut interleaved_bed = vec![0.0f32; num_samples * BED_CHANNELS as usize];
        for frame in 0..num_samples {
            for ch in 0..BED_CHANNELS as usize {
                interleaved_bed[frame * BED_CHANNELS as usize + ch] = channels[ch][frame];
            }
        }
        let dialogue: Vec<f32> = channels[DIALOGUE_CHANNEL][..num_samples].to_vec();

        // Mono downmix of the *dry* Bed for the IS-side detector - Silero is a mono model, and
        // "is anyone speaking anywhere on the Bed" is a whole-Bed question, so averaging the
        // channels is the right reduction. Taken before `process_bed` mutates `interleaved_bed`.
        let mono_bed: Vec<f32> = interleaved_bed
            .chunks_exact(BED_CHANNELS as usize)
            .map(|frame| frame.iter().sum::<f32>() / BED_CHANNELS as f32)
            .collect();

        // Feed-forward: Dialogue is measured dry, before anything is applied to it.
        // A mismatched channel count here would be a caller bug in this fixed-layout plugin, not a
        // runtime condition - deliberately ignored rather than crashing the audio thread over it.
        let _ = processor.feed_dialogue(&dialogue);

        // Fails open (treats COM as always voice-active) if VAD couldn't be constructed at
        // `initialize()` time - see `voice_activity`'s doc comment.
        let voice_active = match self.voice_activity.as_mut() {
            Some(vad) => {
                let _ = vad.feed(&dialogue);
                vad.voice_active()
            }
            None => true,
        };
        // Fails *closed* on the IS side (no detector => "no voice on IS"), unlike COM's fail-open:
        // IS voice only ever selects the interview/over-voice states, so assuming it absent just
        // falls back to the ordinary duck/release behavior rather than inventing a state.
        let is_voice_active = match self.is_voice_activity.as_mut() {
            Some(vad) => {
                let _ = vad.feed(&mono_bed);
                vad.voice_active()
            }
            None => false,
        };
        // Measures the dry Bed, steps the fast stage and applies the resulting gain in sub-chunks,
        // then applies the lookahead delay - all inside the shared core, so every wrapper gets
        // identical DSP.
        let _ = processor.process_bed(&mut interleaved_bed);
        processor.maybe_run_control_step(voice_active, is_voice_active);

        // Only bother publishing to the meters while the GUI is actually open - matches
        // nih-plug's own guidance for keeping this off the hot path otherwise.
        if self.params.editor_state.is_open() {
            self.meters.gain_reduction_db.store(processor.applied_gain_reduction_db() as f32, Ordering::Relaxed);
            self.meters.ratio_lu.store(processor.applied_ratio_lu() as f32, Ordering::Relaxed);
            self.meters.display_ratio_lu.store(processor.display_ratio_lu() as f32, Ordering::Relaxed);
            self.meters.bed_momentary_lufs.store(processor.bed_momentary_lufs() as f32, Ordering::Relaxed);
            self.meters
                .dialogue_momentary_lufs
                .store(processor.dialogue_momentary_lufs() as f32, Ordering::Relaxed);
            self.meters.voice_active.store(voice_active, Ordering::Relaxed);
            self.meters.is_voice_active.store(is_voice_active, Ordering::Relaxed);
            self.meters
                .mix_state
                .store(mix_state_index(processor.mix_state()), Ordering::Relaxed);
            let contributions = processor.contributions();
            self.meters.slow_db.store(contributions.slow_db as f32, Ordering::Relaxed);
            self.meters.mid_db.store(contributions.mid_db as f32, Ordering::Relaxed);
            self.meters.fast_db.store(contributions.fast_db as f32, Ordering::Relaxed);
        }

        // The Bed path is delayed by the lookahead, so Dialogue must be too or the Mix output goes
        // out of alignment. No-op while lookahead is 0.
        let mut dialogue_out = dialogue;
        processor.delay_dialogue(&mut dialogue_out);
        let dialogue = dialogue_out;

        if mix_dialogue_to_bed {
            mix_dialogue_into_bed(
                &mut interleaved_bed,
                &dialogue,
                BED_CHANNELS,
                LEFT_CHANNEL,
                RIGHT_CHANNEL,
                Some(CENTER_CHANNEL),
                divergence,
            );
        }

        for frame in 0..num_samples {
            for ch in 0..BED_CHANNELS as usize {
                channels[ch][frame] = interleaved_bed[frame * BED_CHANNELS as usize + ch];
            }
            channels[DIALOGUE_CHANNEL][frame] = dialogue[frame];
        }
        // Channel 8 (index 7) is always unused/silent, regardless of what the host sent in.
        channels[UNUSED_CHANNEL][..num_samples].fill(0.0);

        ProcessStatus::Normal
    }
}

impl Vst3Plugin for ComISAssist {
    const VST3_CLASS_ID: [u8; 16] = *b"ComISAssistDuck1";
    const VST3_SUBCATEGORIES: &'static [Vst3SubCategory] = &[Vst3SubCategory::Fx, Vst3SubCategory::Dynamics];
}

nih_export_vst3!(ComISAssist);
