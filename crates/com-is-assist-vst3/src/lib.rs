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
use com_is_assist_core::automix::{mix_dialogue_into_bed, AutomixEngineConfig, AutomixProcessor, GainComputerConfig};
use com_is_assist_core::loudness::Ebur128Meter;
use nih_plug::prelude::*;
use nih_plug_egui::widgets::ParamSlider;
use nih_plug_egui::{create_egui_editor, egui, resizable_window::ResizableWindow, EguiState};
use std::num::NonZeroU32;
use std::sync::atomic::Ordering;
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
const MIN_TIME_MS: f32 = 1.0;
const MAX_TIME_MS: f32 = 5000.0;

struct ComISAssist {
    params: Arc<ComISAssistParams>,
    /// `None` until `initialize()` supplies the host's sample rate - `AutomixProcessor` needs it
    /// at construction time (for the `ebur128` meters), which isn't known any earlier than that.
    processor: Option<AutomixProcessor>,
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
        }
    }
}

#[derive(Params)]
struct ComISAssistParams {
    #[id = "target-ratio"]
    pub target_ratio: FloatParam,
    #[id = "max-gain-reduction-db"]
    pub max_gain_reduction_db: FloatParam,
    /// Milliseconds, not seconds - see the module doc comment and `MIN_TIME_MS`/`MAX_TIME_MS`.
    #[id = "attack-ms"]
    pub attack_ms: FloatParam,
    #[id = "hold-ms"]
    pub hold_ms: FloatParam,
    #[id = "release-ms"]
    pub release_ms: FloatParam,
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

    /// Persisted together with the rest of the plugin's state so the GUI reopens at the same
    /// size. Not a live-meter value itself - see `ComISAssist::gain_reduction_db` for that.
    #[persist = "editor-state"]
    editor_state: Arc<EguiState>,
}

impl Default for ComISAssist {
    fn default() -> Self {
        Self {
            params: Arc::new(ComISAssistParams::default()),
            processor: None,
            meters: Arc::new(Meters::new()),
        }
    }
}

impl Default for ComISAssistParams {
    fn default() -> Self {
        let automix = AutomixEngineConfig::default();
        let gain = GainComputerConfig::default();
        Self {
            target_ratio: FloatParam::new(
                "Target Ratio",
                automix.target_ratio_lu as f32,
                FloatRange::Linear { min: -12.0, max: 12.0 },
            )
            .with_unit(" LU"),
            max_gain_reduction_db: FloatParam::new(
                "Max Gain Reduction",
                automix.max_gain_reduction_db as f32,
                FloatRange::Linear { min: 0.0, max: 48.0 },
            )
            .with_unit(" dB"),
            attack_ms: FloatParam::new(
                "Fade Down Time",
                (gain.attack_seconds * 1000.0) as f32,
                FloatRange::Skewed {
                    min: MIN_TIME_MS,
                    max: MAX_TIME_MS,
                    factor: FloatRange::skew_factor(-2.0),
                },
            )
            .with_step_size(1.0)
            .with_unit(" ms"),
            hold_ms: FloatParam::new(
                "Hold Time",
                (gain.hold_seconds * 1000.0) as f32,
                FloatRange::Skewed {
                    min: MIN_TIME_MS,
                    max: MAX_TIME_MS,
                    factor: FloatRange::skew_factor(-2.0),
                },
            )
            .with_step_size(1.0)
            .with_unit(" ms"),
            release_ms: FloatParam::new(
                "Recovery Time",
                (gain.release_seconds * 1000.0) as f32,
                FloatRange::Skewed {
                    min: MIN_TIME_MS,
                    max: MAX_TIME_MS,
                    factor: FloatRange::skew_factor(-2.0),
                },
            )
            .with_step_size(1.0)
            .with_unit(" ms"),
            bypass: BoolParam::new("Bypass", false),
            mix_dialogue_to_bed: BoolParam::new("Mix Dialogue to Bed", true),
            divergence: FloatParam::new("Voice Divergence", 0.0, FloatRange::Linear { min: 0.0, max: 100.0 })
                .with_unit(" %"),
            editor_state: EguiState::from_size(400, 480),
        }
    }
}

impl ComISAssistParams {
    fn automix_config(&self) -> AutomixEngineConfig {
        AutomixEngineConfig {
            target_ratio_lu: self.target_ratio.value() as f64,
            max_gain_reduction_db: self.max_gain_reduction_db.value() as f64,
            ..AutomixEngineConfig::default()
        }
    }

    fn gain_computer_config(&self) -> GainComputerConfig {
        GainComputerConfig {
            attack_seconds: self.attack_ms.value() as f64 / 1000.0,
            hold_seconds: self.hold_ms.value() as f64 / 1000.0,
            release_seconds: self.release_ms.value() as f64 / 1000.0,
            max_rate_db_per_s: None,
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

const METER_BAR_HEIGHT: f32 = 160.0;
const LOUDNESS_COLUMN_WIDTH: f32 = 64.0;
const RATIO_COLUMN_WIDTH: f32 = 60.0;
const SCALE_COLUMN_WIDTH: f32 = 38.0;

const LOUDNESS_METER_MIN_LUFS: f32 = -60.0;
const LOUDNESS_METER_MAX_LUFS: f32 = 0.0;

/// A vertical momentary-loudness meter that fills from the bottom up (the conventional level-meter
/// orientation), normalized against a fixed -60..0 LUFS display range chosen to comfortably cover
/// typical Bed/Dialogue program levels down to near-silence. `Ebur128Meter::NEGATIVE_INFINITY_DB`
/// (silence/insufficient data) is shown as "-inf" rather than a literal "-100.0", matching how the
/// original JSFX reference meter displays it.
fn loudness_meter(ui: &mut egui::Ui, label: &str, lufs: f32, color: egui::Color32) {
    fixed_width_column(ui, LOUDNESS_COLUMN_WIDTH, |ui| {
        ui.label(label);
        let (rect, _response) = ui.allocate_exact_size(egui::vec2(32.0, METER_BAR_HEIGHT), egui::Sense::hover());
        let painter = ui.painter();
        painter.rect_filled(rect, 3.0, egui::Color32::from_gray(25));

        let normalized = ((lufs - LOUDNESS_METER_MIN_LUFS) / (LOUDNESS_METER_MAX_LUFS - LOUDNESS_METER_MIN_LUFS))
            .clamp(0.0, 1.0);
        let fill_height = rect.height() * normalized;
        let filled = egui::Rect::from_min_max(egui::pos2(rect.min.x, rect.max.y - fill_height), rect.max);
        painter.rect_filled(filled, 3.0, color);
        painter.rect_stroke(rect, 3.0, egui::Stroke::new(1.0_f32, egui::Color32::from_gray(90)), egui::StrokeKind::Outside);

        if lufs <= Ebur128Meter::NEGATIVE_INFINITY_DB as f32 {
            ui.label("-inf LUFS");
        } else {
            ui.label(format!("{lufs:.1} LUFS"));
        }
    });
}

/// The IS (Bed) loudness meter: fills bottom-up to `pre_gain_lufs` like `loudness_meter`, but with
/// the currently-applied gain reduction drawn as a highlighted (red) segment carved out of the
/// *top* of that fill, extending down by `gain_reduction_db` - on the same LUFS-equivalent scale,
/// since 1dB of reduction is exactly 1 LU. The bar therefore splits into "what's actually audible"
/// (the cyan region, below) and "how much is being cut off the top" (red) of the full incoming
/// level - replacing an earlier, separate standalone gain-reduction meter, since the whole point
/// of gain reduction is "how much is being removed from *this* signal," which reads far more
/// directly as part of the bar it acts on than as a side-by-side meter with its own scale.
fn bed_loudness_meter(ui: &mut egui::Ui, pre_gain_lufs: f32, gain_reduction_db: f32, color: egui::Color32) {
    fixed_width_column(ui, LOUDNESS_COLUMN_WIDTH, |ui| {
        ui.label("IS");
        let (rect, _response) = ui.allocate_exact_size(egui::vec2(32.0, METER_BAR_HEIGHT), egui::Sense::hover());
        let painter = ui.painter();
        painter.rect_filled(rect, 3.0, egui::Color32::from_gray(25));

        let range = LOUDNESS_METER_MAX_LUFS - LOUDNESS_METER_MIN_LUFS;
        let total_normalized = ((pre_gain_lufs - LOUDNESS_METER_MIN_LUFS) / range).clamp(0.0, 1.0);
        // Can't carve out more of the bar than is currently filled - a reduction bigger than the
        // bar's own fill just means the whole visible bar is "cut" (audible portion is silence).
        let reduction_normalized = (gain_reduction_db / range).clamp(0.0, total_normalized);
        let effective_normalized = total_normalized - reduction_normalized;

        let audible_fill_height = rect.height() * effective_normalized;
        let audible_rect = egui::Rect::from_min_max(egui::pos2(rect.min.x, rect.max.y - audible_fill_height), rect.max);
        painter.rect_filled(audible_rect, 3.0, color);

        if reduction_normalized > 0.0 {
            let reduction_top_y = rect.max.y - rect.height() * total_normalized;
            let reduction_bottom_y = rect.max.y - rect.height() * effective_normalized;
            let reduction_rect =
                egui::Rect::from_min_max(egui::pos2(rect.min.x, reduction_top_y), egui::pos2(rect.max.x, reduction_bottom_y));
            painter.rect_filled(reduction_rect, 3.0, egui::Color32::from_rgb(224, 32, 32));
        }
        painter.rect_stroke(rect, 3.0, egui::Stroke::new(1.0_f32, egui::Color32::from_gray(90)), egui::StrokeKind::Outside);

        if pre_gain_lufs <= Ebur128Meter::NEGATIVE_INFINITY_DB as f32 {
            ui.label("-inf LUFS");
        } else {
            ui.label(format!("{pre_gain_lufs:.1} LUFS"));
        }
        ui.label(format!("-{gain_reduction_db:.1} dB"));
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
        let (rect, _response) = ui.allocate_exact_size(egui::vec2(32.0, METER_BAR_HEIGHT), egui::Sense::hover());
        let painter = ui.painter();
        painter.rect_filled(rect, 3.0, egui::Color32::from_gray(25));

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
        painter.rect_filled(filled, 3.0, color);
        painter.rect_stroke(rect, 3.0, egui::Stroke::new(1.0_f32, egui::Color32::from_gray(90)), egui::StrokeKind::Outside);

        ui.label(format!("{ratio_lu:+.1} LU"));
    });
}

/// A column pairing a name-label-height spacer with `vertical_scale`, so the ticks line up with
/// the bars' drawn area (which sits below each bar's own name label). Placed immediately to the
/// right of both the IS bar and the COM bar (see `editor()`), as close as the label/tick text
/// allows, so each bar reads against its own nearby axis rather than one shared scale off to a
/// single side.
fn loudness_scale_column(ui: &mut egui::Ui) {
    fixed_width_column(ui, SCALE_COLUMN_WIDTH, |ui| {
        ui.label(" ");
        vertical_scale(ui, METER_BAR_HEIGHT, LOUDNESS_METER_MIN_LUFS, LOUDNESS_METER_MAX_LUFS, 10.0);
    });
}

/// A vertical scale (tick marks + numeric labels) for a bottom-up bar meter (`min` at the bottom,
/// `max` at the top - e.g. -60 LUFS at bottom, 0 LUFS at top) spanning `[min, max]` over `height`
/// pixels. The allocated rect is wide enough to contain the tick text itself (not just the tick
/// line), so egui's own layout system - which doesn't know about anything drawn via `Painter`
/// outside the rect it was told about - correctly accounts for the scale's full visual footprint;
/// otherwise neighboring widgets could crowd or overlap the numbers. The topmost/bottommost labels
/// are also nudged inward (clamped half a line-height from the edge) so they stay fully visible
/// rather than clipping.
fn vertical_scale(ui: &mut egui::Ui, height: f32, min: f32, max: f32, step: f32) {
    let (rect, _response) = ui.allocate_exact_size(egui::vec2(SCALE_COLUMN_WIDTH, height), egui::Sense::hover());
    let painter = ui.painter();
    let half_line = 5.0;
    let mut value = min;
    while value <= max + 0.001 {
        let normalized = ((value - min) / (max - min)).clamp(0.0, 1.0);
        let y = rect.max.y - rect.height() * normalized;
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
                    .min_size(egui::Vec2::new(360.0, 420.0))
                    .show(egui_ctx, egui_state.as_ref(), |ui| {
                        ui.add_space(10.0);
                        ui.horizontal(|ui| {
                            ui.add_space(12.0);
                            ui.heading("Com-IS-Assist");
                        });
                        ui.add_space(8.0);

                        let gain_reduction_db = meters.gain_reduction_db.load(Ordering::Relaxed);
                        let is_lufs = meters.bed_momentary_lufs.load(Ordering::Relaxed); // pre-gain
                        // What's actually audible from Bed right now - the boundary between the
                        // cyan and red portions of `bed_loudness_meter`'s bar below. Used to anchor
                        // the ratio bar so it starts from the same point that bar visually ends at.
                        let effective_is_lufs = is_lufs - gain_reduction_db;
                        let com_lufs = meters.dialogue_momentary_lufs.load(Ordering::Relaxed);
                        // The bar's drawn height/label - consistent with `effective_is_lufs`/
                        // `com_lufs` above (see `AutomixProcessor::display_ratio_lu`'s doc comment).
                        let display_ratio_lu = meters.display_ratio_lu.load(Ordering::Relaxed);
                        // The actual control-loop ratio - used only for the tolerance check below,
                        // not for drawing (see `Meters::ratio_lu`'s doc comment).
                        let ratio_lu = meters.ratio_lu.load(Ordering::Relaxed);

                        // Red when the current ratio falls outside the same dead-band
                        // `AutomixEngine::process_tick` itself uses to decide whether Bed's gain
                        // needs adjusting - i.e. "the given COM/IS ratio isn't being met" means
                        // exactly what it means to the DSP, not an independently-invented
                        // GUI-only threshold. Deliberately checked against `ratio_lu` (the signed,
                        // real control-loop value), not `display_ratio_lu` (floored at 0) - the
                        // latter would make the lower bound of the tolerance band unreachable and
                        // this indicator would almost never go red. `max-tolerance`/`min-tolerance`
                        // aren't exposed as VST3 parameters (see `ComISAssistParams`'s doc
                        // comments), so this reads them from the same `AutomixEngineConfig::default()`
                        // the processor itself was built with.
                        let tolerance = AutomixEngineConfig::default();
                        let target_ratio_lu = params.target_ratio.value();
                        let ratio_in_tolerance = ratio_lu >= target_ratio_lu - tolerance.min_tolerance_lu as f32
                            && ratio_lu <= target_ratio_lu + tolerance.max_tolerance_lu as f32;
                        let ratio_color = if ratio_in_tolerance {
                            egui::Color32::from_rgb(0x39, 0xC8, 0x39)
                        } else {
                            egui::Color32::from_rgb(224, 32, 32)
                        };

                        ui.horizontal(|ui| {
                            // Precise control over gaps: egui's automatic `item_spacing` would
                            // otherwise stack on top of every explicit `add_space` below, making
                            // the intended-to-be-tight bar/scale gaps look much bigger than
                            // requested.
                            ui.spacing_mut().item_spacing.x = 0.0;
                            ui.add_space(12.0);
                            bed_loudness_meter(ui, is_lufs, gain_reduction_db, egui::Color32::from_rgb(0x52, 0xFF, 0xFE));
                            ui.add_space(2.0);
                            loudness_scale_column(ui);
                            ui.add_space(18.0);
                            ratio_meter(ui, effective_is_lufs, display_ratio_lu, ratio_color);
                            ui.add_space(18.0);
                            loudness_meter(ui, "COM", com_lufs, egui::Color32::from_rgb(0xEB, 0x9E, 0x34));
                            ui.add_space(2.0);
                            loudness_scale_column(ui);
                        });

                        ui.add_space(12.0);
                        ui.separator();
                        ui.horizontal(|ui| {
                            ui.add_space(12.0);
                            ui.vertical(|ui| {
                                ui.label("Controls");
                                ui.add_space(4.0);

                                egui::Grid::new("com-is-assist-controls")
                                    .num_columns(2)
                                    .spacing([12.0, 6.0])
                                    .show(ui, |ui| {
                                        ui.label("Target ratio");
                                        ui.add(ParamSlider::for_param(&params.target_ratio, setter));
                                        ui.end_row();

                                        ui.label("Max gain reduction");
                                        ui.add(ParamSlider::for_param(&params.max_gain_reduction_db, setter));
                                        ui.end_row();

                                        ui.label("Fade down time");
                                        ui.add(ParamSlider::for_param(&params.attack_ms, setter));
                                        ui.end_row();

                                        ui.label("Hold time");
                                        ui.add(ParamSlider::for_param(&params.hold_ms, setter));
                                        ui.end_row();

                                        ui.label("Recovery time");
                                        ui.add(ParamSlider::for_param(&params.release_ms, setter));
                                        ui.end_row();

                                        ui.label("Voice divergence");
                                        ui.add(ParamSlider::for_param(&params.divergence, setter));
                                        ui.end_row();
                                    });

                                ui.add_space(8.0);
                                ui.horizontal(|ui| {
                                    bool_param_checkbox(ui, setter, &params.bypass, "Bypass");
                                    ui.add_space(16.0);
                                    bool_param_checkbox(ui, setter, &params.mix_dialogue_to_bed, "Mix dialogue to bed");
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
        _context: &mut impl InitContext<Self>,
    ) -> bool {
        self.processor = AutomixProcessor::new(
            BED_CHANNELS,
            buffer_config.sample_rate as u32,
            self.params.automix_config(),
            self.params.gain_computer_config(),
            TICK_SECONDS,
        )
        .ok();
        self.processor.is_some()
    }

    fn process(
        &mut self,
        buffer: &mut Buffer,
        _aux: &mut AuxiliaryBuffers,
        _context: &mut impl ProcessContext<Self>,
    ) -> ProcessStatus {
        let Some(processor) = self.processor.as_mut() else {
            return ProcessStatus::Error("processor not initialized");
        };

        // `initialize()` only reads these parameters once, at plugin load - without refreshing
        // them here on every block, a live parameter change (e.g. raising `max-gain-reduction-db`
        // from its 24dB default toward its 48dB ceiling mid-session) would silently have no
        // effect, since `AutomixEngine`/`GainComputer` would keep using whatever values were
        // current back at `initialize()` time forever after. Cheap - just a couple of struct
        // copies - and preserves all accumulated state (loudness meters, the gain ramp, the
        // in-progress reduction target), unlike rebuilding the processor would.
        processor.set_config(self.params.automix_config(), self.params.gain_computer_config());

        let automix_enabled = !self.params.bypass.value();
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

        // Must happen before `apply_gain_to_bed_chunk` below, which mutates `interleaved_bed` in
        // place - this measures the dry/incoming Bed level for display (`Specs/UI.md`'s "IS
        // LUFS-M"), deliberately separate from the closed-loop `bed_meter` the control loop uses
        // (see `AutomixProcessor::feed_bed_pre_gain`'s doc comment). Gated on the GUI being open,
        // like the meter-publishing block below - this does real per-sample meter work, unlike a
        // cheap atomic store, so it's not worth paying for when nothing will read it.
        if self.params.editor_state.is_open() {
            let _ = processor.feed_bed_pre_gain(&interleaved_bed);
        }

        processor.apply_gain_to_bed_chunk(&mut interleaved_bed);
        // Closed loop (Specs/TechnicalConcept.md section 4): the Bed meter observes the
        // already-gained signal, not the dry one. A mismatched channel count here would be a
        // caller bug in this fixed-layout plugin, not a runtime condition - deliberately ignored
        // rather than crashing the audio thread over it.
        let _ = processor.feed_bed(&interleaved_bed);
        let _ = processor.feed_dialogue(&dialogue);
        processor.maybe_run_control_step(automix_enabled);

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
        }

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
