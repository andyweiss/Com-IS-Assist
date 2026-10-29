// Com-IS-Assist's VST3 plugin: a wrapper around the shared
// `com_is_assist_core::automix::AutomixProcessor`.
//
// Bus layout: the Bed (IS) is the **main** input and output bus, and the commentary (COM) arrives
// on a mono **sidechain** (VST3 aux input). The Bed's width is the only thing that differs between
// formats, so one binary exports four plugins - 2.0, 5.1, 5.1.4 and 7.1.2 (see `BedFormat`) - each
// with exactly one fixed layout, rather than one plugin offering all of them and leaving the choice
// to the host's bus negotiation:
//
// - Every host already knows how to put a fixed-width insert with a sidechain on a track of that
//   width. Hosts differ widely in whether, and how, they renegotiate a multi-layout VST3 to the
//   track's width, and getting it wrong there fails silently (a 5.1 bed processed as stereo).
// - Each plugin then reports, and only accepts, one exact VST3 speaker arrangement: `kStereo`,
//   `k51`, `k51_4` or `k71_2`, plus `kMono` for the sidechain. That needs a patched nih-plug
//   (`vendor/nih_plug/PATCHED.md`); upstream matches on channel count alone, which cannot tell
//   5.1.4 from 7.1.2 (both ten channels).
// - The parameter set can be trimmed to the format: a stereo bed has no duck depth or divergence
//   to automate, and the height and side groups exist only where those channels do.
//
// All four share every line of DSP and GUI code; they are the same generic type instantiated with
// a different `BedFormat`.
//
// `mix-dialogue-to-bed` (the "bus insert mode" toggle): when on, the Left/Right/Center outputs
// carry leveled Bed + Dialogue mixed in via `mix_dialogue_into_bed`'s divergence-based pan law
// (restricted to L/R/C - Dialogue never touches LFE, surrounds, sides or heights; on 2.0, an equal-power
// L/R split); when off, the output is the leveled Bed alone, for inserting directly on a Bed/IS
// channel strip where the console sums in commentary separately downstream. The sidechain is
// input-only: COM is never passed through, since it already has its own path in the host.

use atomic_float::AtomicF32;
use com_is_assist_core::automix::{
    mix_dialogue_into_bed, AutomixEngineConfig, AutomixProcessor, DuckDepth, MixState,
};
use com_is_assist_core::loudness::{BedLayout, Ebur128Meter};
use com_is_assist_core::voice_activity::{BackgroundVad, VoiceActivityConfig};
use nih_plug::prelude::*;
use nih_plug::wrapper::vst3::vst3_sys::vst::{k51, k51_4, k71_2, kStereo};
use nih_plug_egui::widgets::ParamSlider;
use nih_plug_egui::{create_egui_editor, egui, resizable_window::ResizableWindow, EguiState};
use std::marker::PhantomData;
use std::num::NonZeroU32;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::Arc;

/// What the help page reports as the running version: the GitHub release tag CI stamped this build
/// with, or `dev build (<crate version>)` locally. Produced by `build.rs` - see there for why it is
/// a build script rather than an `option_env!` in this file.
const DISPLAY_VERSION: &str = env!("COMIS_DISPLAY_VERSION");

// Bed channel indices, per `BedLayout::channel_map`. Left and Right lead every layout; Center is
// third everywhere but 2.0, which has none.
const LEFT_CHANNEL: usize = 0;
const RIGHT_CHANNEL: usize = 1;
const CENTER_CHANNEL: usize = 2;

/// The one thing that differs between the exported plugins: the Bed's layout. See the module doc
/// comment for why this is one plugin per layout rather than one with several.
trait BedFormat: 'static + Send + Sync {
    const LAYOUT: BedLayout;
    /// The exact VST3 speaker arrangement of the main bus - the only one the plugin accepts.
    const ARRANGEMENT: u64;
    const PLUGIN_NAME: &'static str;
    /// Shown beside the title and used as the host-facing layout name.
    const LABEL: &'static str;
    /// Each format needs its own: a host identifies a plugin by this, not by its name.
    const CLASS_ID: [u8; 16];
    /// The channel order the main bus expects, for the help page.
    const CHANNEL_ORDER: &'static str;
    /// Which of `BED_GROUP_LABELS` exist on this layout (as indices, in drawing order). Empty
    /// means the format has no duck-depth controls at all.
    const DUCK_GROUPS: &'static [usize];
    /// The IS meter's channel caption.
    const IS_CAPTION: &'static str;

    /// Main-bus width, in `BedLayout::channel_map` order.
    const CHANNELS: usize = Self::ARRANGEMENT.count_ones() as usize;
    const HAS_CENTER: bool = Self::CHANNELS >= 3;
}

struct Stereo;
struct Surround51;
struct Surround514;
struct Surround712;

impl BedFormat for Stereo {
    const LAYOUT: BedLayout = BedLayout::Stereo;
    const ARRANGEMENT: u64 = kStereo;
    const PLUGIN_NAME: &'static str = "Com-IS-Assist 2.0";
    const LABEL: &'static str = "2.0";
    const CLASS_ID: [u8; 16] = *b"ComISAssistDuk20";
    const CHANNEL_ORDER: &'static str = "L R";
    // None: with only one pair there is nothing to distribute the reduction across - holding
    // Front L/R back would just be a weaker duck, which is what the ratios are for.
    const DUCK_GROUPS: &'static [usize] = &[];
    const IS_CAPTION: &'static str = "Ch 1-2";
}

impl BedFormat for Surround51 {
    const LAYOUT: BedLayout = BedLayout::Surround51;
    const ARRANGEMENT: u64 = k51;
    const PLUGIN_NAME: &'static str = "Com-IS-Assist 5.1";
    const LABEL: &'static str = "5.1";
    /// The class ID the single 8-channel plugin shipped with, kept so existing sessions reopen
    /// on this format with their settings intact. (Reaper maps the old 7th input pin, COM, onto
    /// the sidechain - the first pin after the six main ones - so the routing carries over too.)
    const CLASS_ID: [u8; 16] = *b"ComISAssistDuck1";
    const CHANNEL_ORDER: &'static str = "L R C LFE Ls Rs";
    const DUCK_GROUPS: &'static [usize] = &[GROUP_FRONT, GROUP_CENTER, GROUP_LFE, GROUP_SURROUND];
    const IS_CAPTION: &'static str = "Ch 1-6";
}

impl BedFormat for Surround514 {
    const LAYOUT: BedLayout = BedLayout::Surround514;
    const ARRANGEMENT: u64 = k51_4;
    const PLUGIN_NAME: &'static str = "Com-IS-Assist 5.1.4";
    const LABEL: &'static str = "5.1.4";
    const CLASS_ID: [u8; 16] = *b"ComISAssistDk514";
    const CHANNEL_ORDER: &'static str = "L R C LFE Ls Rs Ltf Rtf Ltr Rtr";
    const DUCK_GROUPS: &'static [usize] =
        &[GROUP_FRONT, GROUP_CENTER, GROUP_LFE, GROUP_SURROUND, GROUP_TOP_FRONT, GROUP_TOP_REAR];
    const IS_CAPTION: &'static str = "Ch 1-10";
}

impl BedFormat for Surround712 {
    const LAYOUT: BedLayout = BedLayout::Surround712;
    const ARRANGEMENT: u64 = k71_2;
    const PLUGIN_NAME: &'static str = "Com-IS-Assist 7.1.2";
    const LABEL: &'static str = "7.1.2";
    const CLASS_ID: [u8; 16] = *b"ComISAssistDk712";
    const CHANNEL_ORDER: &'static str = "L R C LFE Ls Rs Sl Sr Tsl Tsr";
    // Side before Surround would match the speakers front to back, but Surround stays fourth on
    // every layout so the same control sits in the same place whichever plugin is open.
    const DUCK_GROUPS: &'static [usize] =
        &[GROUP_FRONT, GROUP_CENTER, GROUP_LFE, GROUP_SURROUND, GROUP_SIDE, GROUP_TOP_SIDE];
    const IS_CAPTION: &'static str = "Ch 1-10";
}

struct ComISAssist<F: BedFormat> {
    format: PhantomData<F>,
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
    voice_activity: Option<BackgroundVad>,

    /// Preallocated working buffers for `process()`. The audio thread must not allocate - a
    /// `vec![]` per block is a potential lock and an unbounded pause, which is precisely what a
    /// real-time thread cannot take. Sized at `initialize()` from the host's declared maximum
    /// block size and only ever re-sized if a host exceeds it (which the spec says it will not).
    interleaved_bed: Vec<f32>,
    dialogue_scratch: Vec<f32>,
    mono_bed_scratch: Vec<f32>,
    /// The second, Bed-side voice-activity detector (`Specs/UI.md`'s "voice detector IS"). Fed a
    /// mono downmix of the dry Bed; together with `voice_activity` above it selects the automix
    /// state (`com_is_assist_core::automix::MixState`) - which is what makes the interview
    /// passthrough and over-voice cases possible at all. `None` on the same fail-open terms as the
    /// COM detector, in which case IS is treated as never having voice (i.e. only the ordinary
    /// release/duck states can be reached).
    is_voice_activity: Option<BackgroundVad>,
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
        }
    }
}

/// Display names for each `MixState`, indexed by `mix_state_index`. Operator-facing wording
/// rather than the internal state names: what the mixer is *doing* reads faster on air than which
/// branch of the state machine produced it.
const MIX_STATE_LABELS: [&str; 4] = ["Idle", "IS \u{2013} Voice Mix", "Voice Passthrough", "Overvoice Mix"];

/// The accent colour for each state, indexed the same way. Mostly reuses the meter bank's existing
/// colour language, so the badge reads without a legend.
fn mix_state_color(index: usize) -> egui::Color32 {
    match index {
        1 => egui::Color32::from_rgb(0x52, 0xFF, 0xFE), // IS - Voice Mix (cyan)
        2 => egui::Color32::from_rgb(0x39, 0xC8, 0x39), // Voice Passthrough (green, as the VAD LEDs)
        // Over-voice gets a colour of its own rather than the COM bar's orange: orange is already
        // doing a different job on the ratio meter, where it marks surplus level. Reusing it here
        // would make "both are talking" and "COM is louder than it needs to be" look alike, when
        // they are unrelated conditions.
        3 => egui::Color32::from_rgb(0xB9, 0x8A, 0xFF), // Overvoice Mix (purple)
        _ => egui::Color32::from_gray(150),             // Idle
    }
}

/// Draws the current state as a filled, rounded badge rather than a line of text.
///
/// The state is the single most useful thing on screen - it explains everything the gain is doing -
/// so it gets a shape of its own instead of competing with the surrounding labels. The fill is the
/// state's accent colour at low alpha so it reads as a tint rather than a block, with a slightly
/// stronger outline to hold the edge against the dark background.
/// The fill colour `ParamSlider` paints its filled portion with (`Visuals::selection.bg_fill`), so
/// the meter bars can share it and the two halves of the window read as one instrument rather than
/// as a meter panel bolted to a control panel. Read from the live visuals instead of being
/// hardcoded alongside it, so the bars cannot drift away from the controls if the theme changes.
/// The gutter between the meter bank and the controls column. Both the content row and the title
/// row above it go through this, so the title row's Bypass checkbox lands at exactly the controls
/// column's left edge without either row needing to know the other's arithmetic.
///
/// Alignment holds because the two rows are built from the *same sequence of items* - left indent,
/// a `METER_BANK_WIDTH`-wide block, this gutter, then the content - so they accumulate egui's
/// automatic `item_spacing` identically. Zeroing that spacing instead would have been the more
/// direct fix, but a horizontal `Ui`'s spacing is inherited by its children, and the controls
/// column's own label/slider rows depend on it.
///
/// `rule` draws the dividing line; the title row passes `false` and reserves the same width blank,
/// since one rule down the middle of the window reads better than two stacked.
fn meters_to_controls_gutter(ui: &mut egui::Ui, rule: bool) {
    ui.add_space(CONTROLS_GUTTER_PAD);
    if rule {
        ui.add(egui::Separator::default().vertical().spacing(CONTROLS_RULE_WIDTH));
    } else {
        ui.allocate_exact_size(egui::vec2(CONTROLS_RULE_WIDTH, 1.0), egui::Sense::hover());
    }
    ui.add_space(CONTROLS_GUTTER_PAD);
}

fn control_accent(ui: &egui::Ui) -> egui::Color32 {
    ui.visuals().selection.bg_fill
}

fn state_badge(ui: &mut egui::Ui, label: &str, color: egui::Color32, width: f32) {
    const HEIGHT: f32 = 42.0;
    const CORNER: f32 = 9.0;

    // Explicit width rather than "whatever is available": it sits directly beneath the meter bank
    // and should stay flush with it, not stretch when the window is resized. This is the one
    // element that explains everything the gain is doing, so it is sized to be read at a glance
    // rather than scaled to its text.
    let (rect, _response) = ui.allocate_exact_size(egui::vec2(width, HEIGHT), egui::Sense::hover());

    let galley = ui
        .painter()
        .layout_no_wrap(label.to_owned(), egui::FontId::proportional(18.0), color);

    let tint = |alpha: u8| egui::Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), alpha);
    let painter = ui.painter();
    painter.rect_filled(rect, CORNER, tint(42));
    painter.rect_stroke(rect, CORNER, egui::Stroke::new(1.5_f32, tint(150)), egui::StrokeKind::Inside);
    painter.galley(rect.center() - (galley.size() / 2.0), galley, color);
}

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
    /// "Voice Detect Gate" - the guard band on COM voice detection. Speech reported on COM is ignored
    /// while COM's own momentary loudness is below this, so bed crosstalk in the commentary mic
    /// cannot duck the bed against itself. See `AutomixEngineConfig::com_gate_lufs`.
    #[id = "com-gate-lufs"]
    pub com_gate_lufs: FloatParam,
    /// "Attack", in milliseconds - how quickly the reduction moves toward a larger answer. See
    /// `AutomixEngineConfig::attack_seconds`. Together with `release` this replaced a single
    /// "Speed" macro, which could only move the way down and the way back together. Stored in ms
    /// so the parameter ID and unit survive from when its range ended at 600ms.
    #[id = "attack-ms"]
    pub attack_ms: FloatParam,
    /// "Release", in seconds - how quickly the reduction lets go. See
    /// `AutomixEngineConfig::release_seconds`.
    #[id = "release-s"]
    pub release_s: FloatParam,
    /// "IS voice detect" - `false` switches the Bed-side voice detector off, so the mixer treats
    /// the Bed as never carrying voice: only the ordinary duck and release states remain, and the
    /// over-voice ratio has nothing to do. For programme where the detector finds "voice" that is
    /// not worth protecting - crowd chants, sung music - and the faster interview recovery or the
    /// higher over-voice ducking would be wrong.
    #[id = "is-voice-detect"]
    pub is_voice_detect: BoolParam,
    /// "Duck depth" per Bed channel *group*: how much of the computed gain reduction those
    /// channels actually take. `1:1` is the reduction in full (6 dB of ducking reduces them by
    /// 6 dB), `1:0` is no ducking at all. See `com_is_assist_core::automix::DuckDepth`.
    ///
    /// Grouped rather than per channel: Left and Right must duck by the same amount or the
    /// difference is heard as the stereo image sliding sideways, and the same goes for the
    /// surround pair.
    #[id = "duck-depth-front"]
    pub duck_depth_front: FloatParam,
    #[id = "duck-depth-center"]
    pub duck_depth_center: FloatParam,
    #[id = "duck-depth-lfe"]
    pub duck_depth_lfe: FloatParam,
    #[id = "duck-depth-surround"]
    pub duck_depth_surround: FloatParam,
    /// Height pairs, 5.1.4 only. Present (hidden) on every format so the parameter set - and with
    /// it every ID - is the same type everywhere.
    #[id = "duck-depth-top-front"]
    pub duck_depth_top_front: FloatParam,
    #[id = "duck-depth-top-rear"]
    pub duck_depth_top_rear: FloatParam,
    /// 7.1.2 only, likewise present (hidden) everywhere else.
    #[id = "duck-depth-side"]
    pub duck_depth_side: FloatParam,
    #[id = "duck-depth-top-side"]
    pub duck_depth_top_side: FloatParam,
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

/// Every Bed channel group, indexed by the `GROUP_*` constants. Each format draws the subset it
/// has (`BedFormat::DUCK_GROUPS`). On 7.1.2 "Surround" is VST3's Ls/Rs, which there are the rear
/// pair - the same speakers as on 5.1, so the same control.
const BED_GROUP_LABELS: [&str; 8] = [
    "Front L/R",
    "Center",
    "LFE",
    "Surround L/R",
    "Top Front L/R",
    "Top Rear L/R",
    "Side L/R",
    "Top Side L/R",
];
const GROUP_FRONT: usize = 0;
const GROUP_CENTER: usize = 1;
const GROUP_LFE: usize = 2;
const GROUP_SURROUND: usize = 3;
const GROUP_TOP_FRONT: usize = 4;
const GROUP_TOP_REAR: usize = 5;
const GROUP_SIDE: usize = 6;
const GROUP_TOP_SIDE: usize = 7;

/// One channel group's duck-depth parameter, shown as a ratio rather than a percentage: `1:1`
/// takes the ducking in full, `1:0` takes none of it. The ratio is what a sound engineer reads
/// this kind of relationship in, and it is also how the control was described when it was asked
/// for - "1:1 means 6 dB of gain reduction reduces that channel by 6 dB, 1:0 means no reduction".
///
/// A formatter *and* a parser, because neither direction is the default: nih-plug would otherwise
/// render "0.50" and fail to read back the "1:0.50" it had just displayed.
fn duck_depth_param(name: &str) -> FloatParam {
    FloatParam::new(
        format!("Duck Depth {name}"),
        1.0,
        FloatRange::Linear { min: 0.0, max: 1.0 },
    )
    .with_step_size(0.05)
    .with_value_to_string(Arc::new(|value| format!("1:{value:.2}")))
    .with_string_to_value(Arc::new(|text| {
        // Accept what is displayed ("1:0.50") and a bare factor ("0.5") alike, so typing either
        // works.
        text.trim().rsplit(':').next().and_then(|factor| factor.trim().parse().ok())
    }))
}

impl ComISAssistParams {
    /// Every duck-depth parameter, indexed like `BED_GROUP_LABELS`. Callers pick a format's
    /// groups out of it with `BedFormat::DUCK_GROUPS`.
    pub fn duck_depth_params(&self) -> [&FloatParam; 8] {
        [
            &self.duck_depth_front,
            &self.duck_depth_center,
            &self.duck_depth_lfe,
            &self.duck_depth_surround,
            &self.duck_depth_top_front,
            &self.duck_depth_top_rear,
            &self.duck_depth_side,
            &self.duck_depth_top_side,
        ]
    }

    /// Those parameters as the core takes them. Resolving groups to individual channels is the
    /// core's job (`DuckDepth::per_channel`), since which role sits at which stream position
    /// depends on the negotiated channel count.
    pub fn duck_depth(&self) -> DuckDepth {
        DuckDepth {
            front_lr: self.duck_depth_front.value() as f64,
            center: self.duck_depth_center.value() as f64,
            lfe: self.duck_depth_lfe.value() as f64,
            surround: self.duck_depth_surround.value() as f64,
            top_front: self.duck_depth_top_front.value() as f64,
            top_rear: self.duck_depth_top_rear.value() as f64,
            side: self.duck_depth_side.value() as f64,
            top_side: self.duck_depth_top_side.value() as f64,
        }
    }
}

impl<F: BedFormat> Default for ComISAssist<F> {
    fn default() -> Self {
        Self {
            format: PhantomData,
            params: Arc::new(ComISAssistParams::for_format::<F>()),
            processor: None,
            voice_activity: None,
            interleaved_bed: Vec::new(),
            dialogue_scratch: Vec::new(),
            mono_bed_scratch: Vec::new(),
            is_voice_activity: None,
            meters: Arc::new(Meters::new()),
        }
    }
}

impl ComISAssistParams {
    /// The parameter set for one format. Every parameter exists on every format, but those a
    /// format has no use for are hidden from the host: a stereo bed offers no duck depth (one pair
    /// has nothing to distribute across) and no divergence (which only steers between Center and
    /// L/R), and each surround format only the groups it has channels for.
    fn for_format<F: BedFormat>() -> Self {
        let mut params = Self::all_visible();
        let hide = |param: FloatParam, present: bool| if present { param } else { param.hide() };
        let present = |group| F::DUCK_GROUPS.contains(&group);
        params.duck_depth_front = hide(params.duck_depth_front, present(GROUP_FRONT));
        params.duck_depth_center = hide(params.duck_depth_center, present(GROUP_CENTER));
        params.duck_depth_lfe = hide(params.duck_depth_lfe, present(GROUP_LFE));
        params.duck_depth_surround = hide(params.duck_depth_surround, present(GROUP_SURROUND));
        params.duck_depth_top_front = hide(params.duck_depth_top_front, present(GROUP_TOP_FRONT));
        params.duck_depth_top_rear = hide(params.duck_depth_top_rear, present(GROUP_TOP_REAR));
        params.duck_depth_side = hide(params.duck_depth_side, present(GROUP_SIDE));
        params.duck_depth_top_side = hide(params.duck_depth_top_side, present(GROUP_TOP_SIDE));
        params.divergence = hide(params.divergence, F::HAS_CENTER);
        params
    }

    fn all_visible() -> Self {
        let automix = AutomixEngineConfig::default();
        Self {
            target_ratio: FloatParam::new(
                "IS Voice Ratio",
                automix.target_ratio_lu as f32,
                // Positive only - COM is meant to sit *on top of* IS by this many LU, never below
                // it, so a negative target never made sense to expose here.
                FloatRange::Linear { min: 0.0, max: 12.0 },
            )
            .with_step_size(1.0)
            .with_value_to_string(formatters::v2s_f32_rounded(0))
            .with_unit(" LU"),
            overvoice_ratio: FloatParam::new(
                "Over-voice Ratio",
                automix.overvoice_ratio_lu as f32,
                FloatRange::Linear { min: 0.0, max: 24.0 },
            )
            .with_step_size(1.0)
            .with_value_to_string(formatters::v2s_f32_rounded(0))
            .with_unit(" LU"),
            max_gain_reduction_db: FloatParam::new(
                "Max Gain Reduction",
                automix.max_gain_reduction_db as f32,
                FloatRange::Linear { min: 0.0, max: 48.0 },
            )
            .with_step_size(1.0)
            .with_value_to_string(formatters::v2s_f32_rounded(0))
            .with_unit(" dB"),
            com_gate_lufs: FloatParam::new(
                "Voice Detect Gate",
                automix.com_gate_lufs as f32,
                FloatRange::Linear { min: -70.0, max: -10.0 },
            )
            .with_step_size(1.0)
            .with_value_to_string(formatters::v2s_f32_rounded(0))
            .with_unit(" LUFS"),
            attack_ms: FloatParam::new(
                "Attack",
                (automix.attack_seconds * 1000.0) as f32,
                centred_skew(ATTACK_MS_MIN, ATTACK_MS_MAX, (automix.attack_seconds * 1000.0) as f32),
            )
            // Milliseconds where they matter and seconds beyond - "2.5 s" reads at a glance where
            // "2500 ms" has to be counted. The parser takes either, so typing works whichever way
            // round the value is displayed.
            .with_value_to_string(Arc::new(|ms| {
                if ms < 1000.0 {
                    format!("{ms:.0} ms")
                } else if ms < 10_000.0 {
                    format!("{:.1} s", ms / 1000.0)
                } else {
                    format!("{:.0} s", ms / 1000.0)
                }
            }))
            .with_string_to_value(Arc::new(|text| {
                let text = text.trim();
                if let Some(seconds) = text.strip_suffix('s').filter(|t| !t.ends_with('m')) {
                    seconds.trim().parse::<f32>().ok().map(|s| s * 1000.0)
                } else {
                    text.trim_end_matches("ms").trim().parse().ok()
                }
            })),
            release_s: FloatParam::new(
                "Release",
                automix.release_seconds as f32,
                centred_skew(RELEASE_S_MIN, RELEASE_S_MAX, automix.release_seconds as f32),
            )
            .with_unit(" s")
            // A tenth of a second matters at the short end and nothing at the long one.
            .with_value_to_string(Arc::new(|value| {
                if value < 10.0 {
                    format!("{value:.1}")
                } else {
                    format!("{value:.0}")
                }
            })),
            is_voice_detect: BoolParam::new("IS Voice Detect", true),
            duck_depth_front: duck_depth_param("Front L/R"),
            duck_depth_center: duck_depth_param("Center"),
            duck_depth_lfe: duck_depth_param("LFE"),
            duck_depth_surround: duck_depth_param("Surround L/R"),
            duck_depth_top_front: duck_depth_param("Top Front L/R"),
            duck_depth_top_rear: duck_depth_param("Top Rear L/R"),
            duck_depth_side: duck_depth_param("Side L/R"),
            duck_depth_top_side: duck_depth_param("Top Side L/R"),
            bypass: BoolParam::new("Bypass", false),
            mix_dialogue_to_bed: BoolParam::new("Mix Dialogue to Bed", true),
            divergence: FloatParam::new("Voice Divergence", 0.0, FloatRange::Linear { min: 0.0, max: 100.0 })
                .with_unit(" %")
                // Whole percent, same as the level controls: the step size alone still renders
                // "40.0 %", so the formatter is what actually drops the decimal. A tenth of a
                // percent of pan position is far below what the control is deciding.
                .with_step_size(1.0)
                .with_value_to_string(formatters::v2s_f32_rounded(0)),
            editor_state: EguiState::from_size(820, 690),
        }
    }
}

impl ComISAssistParams {
    fn automix_config(&self) -> AutomixEngineConfig {
        AutomixEngineConfig {
            target_ratio_lu: self.target_ratio.value() as f64,
            overvoice_ratio_lu: self.overvoice_ratio.value() as f64,
            max_gain_reduction_db: self.max_gain_reduction_db.value() as f64,
            com_gate_lufs: self.com_gate_lufs.value() as f64,
            attack_seconds: self.attack_ms.value() as f64 / 1000.0,
            release_seconds: self.release_s.value() as f64,
            duck_depth: self.duck_depth(),
        }
    }
}

/// Width reserved for every control's name. Fixed rather than auto-sized so that rows laid out
/// *outside* the grid - the voice-detect gate, which sits behind its own rule - still line their
/// sliders up with the ones inside it. An auto-sized grid column would leave them half a word out.
const CONTROL_LABEL_WIDTH: f32 = 132.0;
/// Length of the control sliders.
const CONTROL_SLIDER_WIDTH: f32 = 190.0;
/// Gap between a control's label and its slider. Shared by the parameter `Grid` and by the two
/// rows laid out by hand outside it, which otherwise fell back to egui's default 8px `item_spacing`
/// and put their sliders 4px left of every other one.
const CONTROL_LABEL_GAP: f32 = 12.0;
/// Full width of the controls column, so the title row's Bypass can be right-aligned to exactly
/// where the sliders end.
const CONTROLS_COLUMN_WIDTH: f32 = CONTROL_LABEL_WIDTH + CONTROL_LABEL_GAP + CONTROL_SLIDER_WIDTH;

/// A control's name, at the fixed width every row shares.
fn control_label(ui: &mut egui::Ui, text: &str) -> egui::Rect {
    ui.add_sized(
        egui::vec2(CONTROL_LABEL_WIDTH, 18.0),
        egui::Label::new(text).halign(egui::Align::LEFT),
    )
    .rect
}

/// One label+slider row of the controls grid, tinted when the mixer is currently aiming at that
/// parameter's target.
///
/// The two ratio controls are mutually exclusive targets - only one of them is in play at any
/// moment, decided by the state machine - and which one is not otherwise visible from the controls
/// themselves. Tinting the live one turns "which number is the mixer actually chasing right now?"
/// into something read at a glance rather than inferred from the state badge.
///
/// The tint is painted *behind* the widgets by reserving a shape index first and filling it in once
/// the row's rect is known, which is the only way to get a background under widgets whose extent
/// egui decides as it lays them out.
///
/// `enabled: false` greys the row out - label and slider, each in its own grid cell - for a target
/// the state machine currently cannot reach (over-voice, with IS voice detection off).
fn ratio_control_row(
    ui: &mut egui::Ui,
    label: &str,
    param: &FloatParam,
    setter: &ParamSetter,
    active_color: Option<egui::Color32>,
    enabled: bool,
) {
    let background = ui.painter().add(egui::Shape::Noop);
    let label_rect = ui
        .scope(|ui| {
            if !enabled {
                ui.disable();
            }
            control_label(ui, label)
        })
        .inner;
    ui.add_enabled(enabled, ParamSlider::for_param(param, setter).with_width(CONTROL_SLIDER_WIDTH));

    if let Some(color) = active_color {
        // Behind the *name* only, not the slider: the tint marks which target is live, and
        // running it under the control would read as the control itself being highlighted or
        // disabled rather than as a status.
        let rect = label_rect.expand2(egui::vec2(6.0, 2.0));
        ui.painter().set(
            background,
            egui::Shape::rect_filled(
                rect,
                5.0,
                egui::Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), 38),
            ),
        );
    }
    ui.end_row();
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
const SCALE_COLUMN_WIDTH: f32 = 30.0;
/// A metered column is *exactly* its bar plus the gap plus its scale - no slack. That is what
/// makes the spacing read evenly: with the group filling the column the bar sits hard against the
/// column's left edge, so the gap either side of the (narrower, centred) Ratio bar works out the
/// same on both sides. Any padding here would land only on the scaled columns and throw it out.
const METERED_COLUMN_WIDTH: f32 = METER_BAR_WIDTH + SCALE_GAP + SCALE_COLUMN_WIDTH;
const GR_COLUMN_WIDTH: f32 = METERED_COLUMN_WIDTH;
const LOUDNESS_COLUMN_WIDTH: f32 = METERED_COLUMN_WIDTH;
/// Wide enough for the "Ratio" caption; the bar is centred within it.
const RATIO_COLUMN_WIDTH: f32 = 36.0;

const LOUDNESS_METER_MIN_LUFS: f32 = -60.0;
const LOUDNESS_METER_MAX_LUFS: f32 = 0.0;
/// Fixed display range for `gain_reduction_meter`'s bar and scale - matches `max-gain-reduction-db`'s
/// own hard ceiling (`FloatRange::Linear { max: 48.0, .. }`), rather than the live parameter value
/// (which defaults to 24), so headroom stays visible and the ticks stay a stable reference
/// regardless of the configured ceiling.
const GAIN_REDUCTION_METER_MAX_DB: f32 = 36.0;

/// Gap between a bar and its own scale. The scale is drawn *inside* the meter's column, right
/// beside the bar, rather than as a separate column - previously each 20px bar sat centred in a
/// 62px column, so its ticks began some 21px away and read as belonging to nothing in particular.
const SCALE_GAP: f32 = 3.0;

/// Tick spacing for every scale, loudness and gain-reduction alike. A multiple of 3 so the two
/// kinds of meter label on the same grid and can be read against each other.
const SCALE_STEP_DB: f32 = 6.0;

/// The gap every adjacent pair of meter *bars* shows, measured bar edge to bar edge.
///
/// This is the figure the eye actually judges, and it is not the same as the gap between columns:
/// each column carries its scale on the right, and the Ratio bar is centred in a wider column than
/// it needs, so identical column gaps produce visibly different bar gaps. The three column gaps
/// below are therefore *derived* from this one number rather than set by hand.
const BAR_TO_BAR_GAP: f32 = 51.0;

/// Width a scale adds to the right of its bar.
const SCALE_SPAN: f32 = SCALE_GAP + SCALE_COLUMN_WIDTH;
/// Slack either side of the Ratio bar inside its own (wider) column.
const RATIO_SIDE_SLACK: f32 = (RATIO_COLUMN_WIDTH - METER_BAR_WIDTH) / 2.0;

/// After a scaled column that is followed by another scaled column (GR -> IS).
const GAP_AFTER_SCALED: f32 = BAR_TO_BAR_GAP - SCALE_SPAN;
/// After a scaled column that is followed by the Ratio column (IS -> Ratio).
const GAP_BEFORE_RATIO: f32 = BAR_TO_BAR_GAP - SCALE_SPAN - RATIO_SIDE_SLACK;
/// After the Ratio column (Ratio -> COM).
const GAP_AFTER_RATIO: f32 = BAR_TO_BAR_GAP - RATIO_SIDE_SLACK;

/// Range of the Attack control, in ms. Long at the top because the loop reads the 3s short-term
/// window and a short attack on it rides the peaks of a fluctuating Bed (see
/// `com_is_assist_core::automix::DEFAULT_ATTACK_SECONDS`); the useful settings are seconds, and
/// 100ms is kept at the bottom only as the "as quick as it goes" end.
const ATTACK_MS_MIN: f32 = 100.0;
const ATTACK_MS_MAX: f32 = 10_000.0;
/// Range of the Release control, in seconds.
const RELEASE_S_MIN: f32 = 0.4;
const RELEASE_S_MAX: f32 = 12.0;

/// A skewed range that puts `centre` at the exact middle of the slider's travel. Times want
/// logarithmic-feeling travel, and the default sitting in the middle gives equal room either side.
/// `Skewed` unnormalizes as `t^(1/factor)·(max-min) + min`, so the factor that puts `centre` at
/// t = 0.5 is `ln(0.5) / ln((centre-min)/(max-min))` - derived rather than dialled in, so the
/// default stays centred if a limit is ever retuned.
fn centred_skew(min: f32, max: f32, centre: f32) -> FloatRange {
    FloatRange::Skewed { min, max, factor: 0.5_f32.ln() / ((centre - min) / (max - min)).ln() }
}

/// Padding either side of the rule dividing the meter bank from the controls column.
const CONTROLS_GUTTER_PAD: f32 = 20.0;
/// Width `egui::Separator` occupies in a horizontal layout. Pinned explicitly rather than left to
/// the default so the title row can reserve exactly the same width without drawing a rule.
const CONTROLS_RULE_WIDTH: f32 = 6.0;

/// Indent shared by the title row and the meter/controls row, so nothing sits flush against the
/// window edge.
const CONTENT_LEFT_PADDING: f32 = 32.0;

/// Diameter of the voice-activity lamp.
const LAMP_DIAMETER: f32 = 18.0;
/// Gap between the lamp and its "Voice" caption. Has to clear the gate cross, which overshoots the
/// circle, or the two collide when the guard band is closed.
const LAMP_CAPTION_GAP: f32 = 5.0;
/// Line box for the 10pt "Voice" caption.
const LAMP_CAPTION_HEIGHT: f32 = 12.0;
/// How far the halo (and the gate cross) reach past the circle itself.
const LAMP_GLYPH_OVERSHOOT: f32 = 3.0;
/// Total height the lamp reserves. The caption and halo are *painted*, so they contribute nothing
/// to layout on their own - without counting them here egui lays the next widget out over the top
/// of the caption, which is exactly what put the state badge through the word "Voice".
const LAMP_BLOCK_HEIGHT: f32 =
    LAMP_GLYPH_OVERSHOOT + LAMP_DIAMETER + LAMP_CAPTION_GAP + LAMP_CAPTION_HEIGHT;

/// Height of the caption's name line.
const CAPTION_NAME_HEIGHT: f32 = 16.0;
/// Total height every column's caption block occupies - name line plus the smaller channel line
/// beneath it. **Fixed for all four columns**, including the two that have no channel line to show,
/// because the bars start directly below it and the Ratio bar's leader lines (`ratio_leader_lines`)
/// are only exact while every bar shares one top edge.
const CAPTION_BLOCK_HEIGHT: f32 = CAPTION_NAME_HEIGHT + 11.0;

/// Breathing room between a column's caption and the top of its bar. Also used for the vertical
/// steps below the bars (bar -> voice lamp, lamp -> state badge), so the column reads on one rhythm.
const CAPTION_GAP: f32 = 7.0;


/// Half-width of the ratio meter's "on target" green band, in LU - a GUI-only readability
/// threshold, governing nothing in the DSP.
const RATIO_METER_GREEN_BAND_LU: f32 = 2.0;

/// Exact width of the meter bank row, derived from the columns and gaps that make it up rather
/// than hardcoded - the state badge sits beneath it and must stay flush with it if any column
/// width is ever retuned.
const METER_BANK_WIDTH: f32 = GR_COLUMN_WIDTH
    + GAP_AFTER_SCALED
    + LOUDNESS_COLUMN_WIDTH
    + GAP_BEFORE_RATIO
    + RATIO_COLUMN_WIDTH
    + GAP_AFTER_RATIO
    + LOUDNESS_COLUMN_WIDTH;


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
/// A column's caption, centred on **its bar** rather than on the column. The column also holds the
/// scale to the bar's right, so centring in the column would sit the name visibly off to one side
/// of the thing it names.
/// Every column's caption goes through this, including the Ratio column's, so that all four
/// captions occupy exactly the same 16px and the bars below them therefore start at the same y. The
/// Ratio bar's leader lines depend on that alignment being exact, and a plain `ui.label` here (what
/// the Ratio column used to use) is a line-height tall, not 16px, which offset its bar from the
/// others by a couple of pixels.
///
/// `center_on_column` picks what the text centres on: the Ratio column's bar is centred within a
/// column wider than itself, while a loudness column's bar sits at the column's left edge with the
/// scale beside it.
fn meter_caption(ui: &mut egui::Ui, text: &str, channels: Option<&str>, center_on_column: bool) {
    let width = ui.available_width();
    let (rect, _response) =
        ui.allocate_exact_size(egui::vec2(width, CAPTION_BLOCK_HEIGHT), egui::Sense::hover());
    let center_x = if center_on_column { rect.center().x } else { rect.min.x + METER_BAR_WIDTH / 2.0 };
    let painter = ui.painter();
    painter.text(
        egui::pos2(center_x, rect.min.y + CAPTION_NAME_HEIGHT / 2.0),
        egui::Align2::CENTER_CENTER,
        text,
        egui::FontId::proportional(13.0),
        ui.visuals().text_color(),
    );
    // The channel line is drawn smaller and dimmer than the name: it answers "which inputs am I
    // patching?", which is a setup question asked once, not something to read while mixing.
    if let Some(channels) = channels {
        painter.text(
            egui::pos2(center_x, rect.min.y + CAPTION_NAME_HEIGHT),
            egui::Align2::CENTER_TOP,
            channels,
            egui::FontId::proportional(9.0),
            egui::Color32::from_gray(130),
        );
    }
}

/// Everything that distinguishes one loudness column from another. A struct rather than a long
/// argument list: the two call sites differ in six of these, and positional `None, None, true`
/// tails are exactly the kind of thing that gets mis-wired silently.
struct LoudnessMeter<'a> {
    label: &'a str,
    /// Which input channels feed this meter, shown under the name. The plugin's bus is fixed
    /// (see the module doc comment), so these are constant - and worth stating, because a host
    /// gives no indication of which of the eight channels the plugin expects what on.
    channels: &'a str,
    lufs: f32,
    color: egui::Color32,
    /// `Some` on the IS bar only: carves the applied reduction out of the top of the fill.
    gain_reduction_db: Option<f32>,
    voice: Option<VoiceIndicator>,
    /// `Some` on the COM bar only: shades the band where voice detection is switched off.
    gate_lufs: Option<f32>,
}

/// Returns the bar's rect so the Ratio column's leader lines can be drawn to its edge from
/// outside the column (see `ratio_leader_lines`).
fn loudness_meter(ui: &mut egui::Ui, spec: LoudnessMeter) -> egui::Rect {
    let LoudnessMeter { label, channels, lufs, color, gain_reduction_db, voice, gate_lufs } = spec;
    let mut bar_rect = egui::Rect::NOTHING;
    fixed_width_column(ui, LOUDNESS_COLUMN_WIDTH, |ui| {
        meter_caption(ui, label, Some(channels), false);
        ui.add_space(CAPTION_GAP);
        let mut rect = egui::Rect::NOTHING;
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 0.0;
            rect = ui
                .allocate_exact_size(egui::vec2(METER_BAR_WIDTH, METER_BAR_HEIGHT), egui::Sense::hover())
                .0;
            ui.add_space(SCALE_GAP);
            vertical_scale(
                ui,
                METER_BAR_HEIGHT,
                LOUDNESS_METER_MIN_LUFS,
                LOUDNESS_METER_MAX_LUFS,
                SCALE_STEP_DB,
                false,
            );
        });
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

        // Shade the band in which voice detection is switched off, so the guard-band setting is
        // visible *against the signal it acts on* rather than being a number in isolation - the
        // question being "is my commentator above the line, and is the spill below it?".
        if let Some(gate) = gate_lufs {
            let gate_normalized = ((gate - LOUDNESS_METER_MIN_LUFS)
                / (LOUDNESS_METER_MAX_LUFS - LOUDNESS_METER_MIN_LUFS))
                .clamp(0.0, 1.0);
            let gate_y = rect.max.y - rect.height() * gate_normalized;
            // Darkens what is underneath rather than washing it out with a light tint: the band
            // should read as "this region is inactive", which is a shadow, not a highlight. It
            // also keeps the orange fill legible through it instead of desaturating it to mud.
            painter.rect_filled(
                egui::Rect::from_min_max(egui::pos2(rect.min.x, gate_y), rect.max),
                2.0,
                egui::Color32::from_rgba_unmultiplied(0, 0, 0, 140),
            );
            painter.hline(
                rect.x_range(),
                gate_y,
                egui::Stroke::new(1.0_f32, egui::Color32::from_gray(125)),
            );
        }

        painter.rect_stroke(rect, 2.0, egui::Stroke::new(1.0_f32, egui::Color32::from_gray(90)), egui::StrokeKind::Outside);

        if let Some(indicator) = voice {
            ui.add_space(CAPTION_GAP);
            // Centred on the *bar* rather than the column: the column also holds the scale, so
            // centring here would park the lamp under the numbers instead of under the signal it
            // reports on.
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 0.0;
                voice_activity_led(ui, indicator);
            });
        }
        bar_rect = rect;
    });
    bar_rect
}

/// A vertical gain-reduction meter that fills top-down (0dB at top, increasing downward - the
/// conventional GR-meter orientation, opposite of `loudness_meter`'s bottom-up convention),
/// normalized against the fixed `GAIN_REDUCTION_METER_MAX_DB` range. Standalone (in addition to the
/// carve-out on the IS bar above) so gain reduction and ratio can both be read clearly at a glance,
/// over a wider range than the IS bar's carve-out alone would show, while tuning attack/hold/release.
fn gain_reduction_meter(ui: &mut egui::Ui, gain_reduction_db: f32) {
    fixed_width_column(ui, GR_COLUMN_WIDTH, |ui| {
        meter_caption(ui, "GR", None, false);
        ui.add_space(CAPTION_GAP);
        let mut rect = egui::Rect::NOTHING;
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 0.0;
            rect = ui
                .allocate_exact_size(egui::vec2(METER_BAR_WIDTH, METER_BAR_HEIGHT), egui::Sense::hover())
                .0;
            ui.add_space(SCALE_GAP);
            vertical_scale(ui, METER_BAR_HEIGHT, 0.0, GAIN_REDUCTION_METER_MAX_DB, SCALE_STEP_DB, true);
        });
        let painter = ui.painter();
        painter.rect_filled(rect, 2.0, METER_BACKGROUND);

        let normalized = (gain_reduction_db / GAIN_REDUCTION_METER_MAX_DB).clamp(0.0, 1.0);
        let filled = egui::Rect::from_min_max(rect.min, egui::pos2(rect.max.x, rect.min.y + rect.height() * normalized));
        painter.rect_filled(filled, 2.0, egui::Color32::from_rgb(224, 32, 32));
        painter.rect_stroke(rect, 2.0, egui::Stroke::new(1.0_f32, egui::Color32::from_gray(90)), egui::StrokeKind::Outside);
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
/// `excess_from_lu`, when `Some`, is the point (in LU above `is_lufs`) beyond which the bar is
/// "more than needed". Only the portion *above* that is drawn in `excess_color`; everything below
/// keeps `color`.
///
/// Recolouring the whole bar instead - which this used to do - answers the wrong question. The
/// useful reading while mixing is "how much of this is surplus", and a bar that flips entirely to
/// orange the moment it crosses the band hides exactly that: the part of the level that is correct
/// looks identical to the part that is excessive.
/// Thin horizontal leaders tying the Ratio bar's two edges back to the levels it is the distance
/// between: its bottom to the Bed's effective (post-reduction) level - the underside of the red
/// gain-reduction block on the IS bar - and its top to the COM level.
///
/// They can be horizontal, and land exactly, because the Ratio bar is already drawn on the *same*
/// LUFS scale as the two loudness bars rather than on a 0..N ratio scale of its own: it spans from
/// `effective_is_lufs` up by `ratio_lu`, which is the COM level by definition. The lines make that
/// shared geometry visible instead of leaving it as something to notice.
///
/// Painted from the parent after the meter row has been laid out, so they sit on top of everything
/// and are not clipped by the columns they cross.
fn ratio_leader_lines(ui: &egui::Ui, span: &RatioSpan, is_bar: egui::Rect, com_bar: egui::Rect) {
    if !span.visible {
        return;
    }
    // Deliberately faint: the left leader crosses the IS column's scale numbers, and a leader is
    // an aid to reading the meters rather than a reading of its own - it should be findable when
    // looked for and invisible when not.
    let stroke = egui::Stroke::new(1.0_f32, egui::Color32::from_gray(80));
    let painter = ui.painter();
    painter.line_segment(
        [egui::pos2(is_bar.max.x, span.bottom_y), egui::pos2(span.bar.min.x, span.bottom_y)],
        stroke,
    );
    painter.line_segment(
        [egui::pos2(span.bar.max.x, span.top_y), egui::pos2(com_bar.min.x, span.top_y)],
        stroke,
    );
}

/// Where the Ratio bar's drawn span landed on screen, so the leader lines tying it back to the two
/// levels it is the distance between can be drawn from outside the column.
struct RatioSpan {
    bar: egui::Rect,
    /// Screen y of the bar's top edge - the COM level, since the span runs from the ducked Bed up
    /// by exactly the ratio.
    top_y: f32,
    /// Screen y of the bar's bottom edge - the Bed's effective (post-reduction) level.
    bottom_y: f32,
    /// False when the drawn ratio has collapsed to nothing (COM at or below the ducked Bed): there
    /// is then no span to point at, and leaders would claim a relationship that is not on screen.
    visible: bool,
}

fn ratio_meter(
    ui: &mut egui::Ui,
    is_lufs: f32,
    ratio_lu: f32,
    color: egui::Color32,
    excess_from_lu: Option<f32>,
    excess_color: egui::Color32,
) -> RatioSpan {
    let mut span = RatioSpan { bar: egui::Rect::NOTHING, top_y: 0.0, bottom_y: 0.0, visible: false };
    fixed_width_column(ui, RATIO_COLUMN_WIDTH, |ui| {
        // The Ratio column is just the bar, centred in a column wider than it, so its caption
        // centres on the column rather than on a bar sitting at the column's left edge.
        meter_caption(ui, "Ratio", None, true);
        ui.add_space(CAPTION_GAP);
        let (rect, _response) = ui.allocate_exact_size(egui::vec2(METER_BAR_WIDTH, METER_BAR_HEIGHT), egui::Sense::hover());
        let painter = ui.painter();
        painter.rect_filled(rect, 2.0, METER_BACKGROUND);

        let normalize = |lufs: f32| {
            ((lufs - LOUDNESS_METER_MIN_LUFS) / (LOUDNESS_METER_MAX_LUFS - LOUDNESS_METER_MIN_LUFS)).clamp(0.0, 1.0)
        };
        let y_at = |lufs: f32| rect.max.y - rect.height() * normalize(lufs);

        let (low_lufs, high_lufs) = {
            let top = is_lufs + ratio_lu;
            if is_lufs <= top { (is_lufs, top) } else { (top, is_lufs) }
        };
        let (top_y, bottom_y) = (y_at(high_lufs), y_at(low_lufs));
        painter.rect_filled(
            egui::Rect::from_min_max(egui::pos2(rect.min.x, top_y), egui::pos2(rect.max.x, bottom_y)),
            2.0,
            color,
        );

        // Overdraw just the surplus portion, clamped into the bar so it can never extend past it.
        if let Some(threshold_lu) = excess_from_lu {
            let split_y = y_at(is_lufs + threshold_lu).clamp(top_y, bottom_y);
            if split_y > top_y {
                painter.rect_filled(
                    egui::Rect::from_min_max(egui::pos2(rect.min.x, top_y), egui::pos2(rect.max.x, split_y)),
                    2.0,
                    excess_color,
                );
            }
        }

        painter.rect_stroke(rect, 2.0, egui::Stroke::new(1.0_f32, egui::Color32::from_gray(90)), egui::StrokeKind::Outside);

        span = RatioSpan { bar: rect, top_y, bottom_y, visible: bottom_y - top_y >= 1.0 };
    });
    span
}


/// A vertical scale (tick marks + numeric labels) spanning `[min, max]` over `height` pixels.
/// `top_down` selects the tick direction to match the bar it labels: `false` for the bottom-up
/// loudness/ratio convention (`min` at the bottom, `max` at the top - e.g. -60 LUFS at bottom, 0
/// LUFS at top), `true` for `gain_reduction_meter`'s top-down convention (`min` i.e. 0dB at the
/// top, `max` at the bottom). The allocated rect is wide enough to contain the tick text itself
/// (not just the tick line), so egui's own layout system - which doesn't know about anything drawn
/// via `Painter` outside the rect it was told about - correctly accounts for the scale's full
/// visual footprint; otherwise neighboring widgets could crowd or overlap the numbers. The
/// Numbers are centred on their own tick with no edge clamping - see the note in the body.
fn vertical_scale(
    ui: &mut egui::Ui,
    height: f32,
    min: f32,
    max: f32,
    step: f32,
    top_down: bool,
) {
    let (rect, _response) = ui.allocate_exact_size(egui::vec2(SCALE_COLUMN_WIDTH, height), egui::Sense::hover());
    let painter = ui.painter();
    let mut value = min;
    while value <= max + 0.001 {
        let normalized = ((value - min) / (max - min)).clamp(0.0, 1.0);
        let y = if top_down {
            rect.min.y + rect.height() * normalized
        } else {
            rect.max.y - rect.height() * normalized
        };
        painter.hline(
            rect.min.x..=(rect.min.x + 4.0),
            y,
            egui::Stroke::new(1.0_f32, egui::Color32::from_gray(140)),
        );
        // Sits exactly on its tick. The end labels were previously nudged inwards to keep them
        // inside the allocated rect, which left the first and last numbers visibly off their own
        // lines - the one place a scale has to be trusted. Half a line of overhang at the very top
        // and bottom is the cheaper error.
        painter.text(
            egui::pos2(rect.min.x + 7.0, y),
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
/// What the voice indicator under a bar should show. `gated_out` means the detector is currently
/// disabled by the guard band (COM only) - drawn as a cross through the lamp, so "not detecting"
/// is visibly distinct from "detecting nothing".
#[derive(Debug, Clone, Copy)]
struct VoiceIndicator {
    active: bool,
    gated_out: bool,
}

fn voice_activity_led(ui: &mut egui::Ui, indicator: VoiceIndicator) {
    let VoiceIndicator { active, gated_out } = indicator;
    // Allocates the whole block - circle, halo overshoot and caption - even though the latter two
    // are painted rather than laid out, so whatever follows in the column starts below the caption.
    let (rect, _response) =
        ui.allocate_exact_size(egui::vec2(METER_BAR_WIDTH, LAMP_BLOCK_HEIGHT), egui::Sense::hover());
    let radius = LAMP_DIAMETER / 2.0;
    let center = egui::pos2(rect.center().x, rect.min.y + LAMP_GLYPH_OVERSHOOT + radius);
    let color = if active {
        egui::Color32::from_rgb(0x39, 0xC8, 0x39)
    } else {
        egui::Color32::from_gray(60)
    };
    let painter = ui.painter();
    // A soft halo while lit, so the indicator carries at a glance from across a gallery rather
    // than needing to be looked for.
    if active {
        painter.circle_filled(
            center,
            radius + LAMP_GLYPH_OVERSHOOT,
            egui::Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), 45),
        );
    }
    painter.circle_filled(center, radius, color);

    // A cross through the lamp while the guard band is suppressing detection. Without it the
    // dark lamp is ambiguous - "nobody is talking" and "I have been switched off" look
    // identical, and those call for opposite actions from the operator.
    if gated_out {
        let r = radius + 2.0;
        // Only just lighter than the unlit lamp itself (which is `from_gray(60)`): the gate
        // being closed is a setting working as intended, so the marking should be legible on
        // inspection without competing with the live meters beside it. A red or bright cross
        // reads as a fault condition.
        let stroke = egui::Stroke::new(2.0_f32, egui::Color32::from_gray(82));
        painter.line_segment([center + egui::vec2(-r, -r), center + egui::vec2(r, r)], stroke);
        painter.line_segment([center + egui::vec2(-r, r), center + egui::vec2(r, -r)], stroke);
    }

    // Drawn rather than laid out: "Voice" is wider than the bar the lamp has to stay centred on,
    // and laying it out would widen the row and push the lamp off that bar.
    painter.text(
        egui::pos2(center.x, rect.min.y + LAMP_GLYPH_OVERSHOOT + LAMP_DIAMETER + LAMP_CAPTION_GAP),
        egui::Align2::CENTER_TOP,
        "Voice",
        egui::FontId::proportional(10.0),
        egui::Color32::from_gray(150),
    );
}

/// Every control row goes through this, including the two that sit alone between the dividing
/// rules, so all of them share one geometry: the same label column, the same label-to-slider gap,
/// and - because `Grid` installs its spacing on the `Ui` the cells inherit - the same gap inside
/// `ParamSlider` between its bar and its value box.
///
/// Those two rows were previously laid out by hand with `ui.horizontal`. Matching the grid by
/// setting `item_spacing.x` on the row does not work: `ParamSlider` builds its own inner
/// `horizontal`, so zeroing the row's spacing to control the label gap also closed up the gap
/// between the slider and its value readout, which is what made those two rows look cramped.
/// Returns the x of the grid's right edge - the end of its value boxes - which is what the title
/// row's Bypass is aligned to. The *column's* own rect is no use for that: the rules dividing the
/// control groups are `ui.separator()`s, which stretch to the full remaining window width and drag
/// the column's rect out with them, far past where any row actually ends.
fn control_grid(ui: &mut egui::Ui, id: &str, add_rows: impl FnOnce(&mut egui::Ui)) -> f32 {
    egui::Grid::new(id)
        .num_columns(2)
        .spacing([CONTROL_LABEL_GAP, 10.0])
        .show(ui, add_rows)
        .response
        .rect
        .right()
}

/// What each control does, in one line, for the help page. Deliberately written for someone sitting
/// at the desk rather than reading the source: the trade each control makes, not what it is wired
/// to. `Specs/UI.md` is the longer form of the same material.
const PARAMETER_HELP: &[(&str, &str)] = &[
    ("IS voice ratio", "How far the commentary should sit above the bed while the commentator alone is talking. Higher means the bed ducks further."),
    ("Over-voice ratio", "The same, for when there is voice on the bed too - an interview under the commentary needs more headroom to stay intelligible. An absolute target, not an offset."),
    ("Max gain reduction", "A hard floor on how far the bed may ever be pushed down, whatever the ratios ask for."),
    ("Attack", "How quickly the bed is ducked when it needs to come down. A few seconds keeps the ducking to what the balance actually needs; much shorter follows every swell of the bed and ducks more than needed."),
    ("Release", "How quickly the bed recovers once it may come back up. Longer keeps the bed down through pauses in the commentary; shorter brings it back sooner."),
    ("Voice detect gate", "A level below which voice on the commentary channel is ignored, so bed spill in the commentary mic cannot duck the bed against its own bleed. Set it 20-30 dB under the commentator's normal level - the shaded area on the COM meter shows where it is off."),
    ("IS voice detect", "Voice detection on the bed. On, voice on the bed makes the bed recover faster, and commentary over it uses the over-voice ratio. Switch it off for crowd chants or sung music the detector mistakes for voice - the bed lamp then shows a cross and the over-voice ratio greys out."),
    ("Duck depth", "How much of the ducking each part of the bed takes. 1:1 is the full amount, 1:0 none at all. Holding the surrounds, heights and LFE back keeps the commentary clear without flattening the scene. Only the groups this format has are shown."),
    ("Mix dialogue to bed", "Folds the commentary into the bed output, for running the plugin as a bus insert rather than alongside a separate commentary path."),
    ("Voice divergence", "Where the mixed-in commentary sits: centre only at 0%, spread across left and right at 100%. Only does anything with the mix above switched on."),
    ("Bypass", "Passes the bed through untouched. Metering keeps running."),
];

/// The about/help overlay. A window rather than a panel so it can be opened over the mixer without
/// the layout underneath moving - the meters stay where they were while it is being read.
fn help_window<F: BedFormat>(ctx: &egui::Context, open: &mut bool) {
    egui::Window::new("About Com-IS-Assist")
        .open(open)
        .collapsible(false)
        .resizable(false)
        .default_width(430.0)
        .show(ctx, |ui| {
            ui.add_space(2.0);
            ui.label(egui::RichText::new(F::PLUGIN_NAME).size(17.0).strong());
            ui.label(egui::RichText::new(DISPLAY_VERSION).color(egui::Color32::from_gray(150)));
            ui.add_space(8.0);
            ui.label("By Andreas Weiss.");
            ui.label("Based on an idea by Jonas Engel and Felix Krückels.");
            ui.add_space(10.0);
            ui.separator();
            ui.add_space(8.0);
            ui.label(
                "Ducks the international sound bed against the mono commentary, holding a chosen \
                 loudness distance between them. The commentary itself is never changed - it is \
                 only measured and, depending on the mode, mixed into the bed.",
            );
            ui.add_space(4.0);
            ui.label(
                egui::RichText::new(format!(
                    "Bed ({}) on the main input, in the order {}. Commentary on the mono \
                     sidechain input. The output is the ducked bed, with the commentary mixed \
                     in when \"Mix dialogue to bed\" is on.",
                    F::LABEL,
                    F::CHANNEL_ORDER
                ))
                .color(egui::Color32::from_gray(150)),
            );
            ui.add_space(10.0);
            ui.separator();
            ui.add_space(8.0);
            ui.label(egui::RichText::new("Controls").strong());
            ui.add_space(4.0);
            egui::ScrollArea::vertical().max_height(260.0).show(ui, |ui| {
                // Only controls this format actually shows: divergence steers between Center and
                // L/R, and duck depth distributes the reduction across channel groups, so neither
                // has anything to do on 2.0.
                let entries = PARAMETER_HELP.iter().filter(|(name, _)| match *name {
                    "Voice divergence" => F::HAS_CENTER,
                    "Duck depth" => !F::DUCK_GROUPS.is_empty(),
                    _ => true,
                });
                for (name, description) in entries {
                    ui.add_space(4.0);
                    ui.label(egui::RichText::new(*name).strong());
                    ui.label(egui::RichText::new(*description).color(egui::Color32::from_gray(185)));
                }
            });
            ui.add_space(4.0);
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

impl<F: BedFormat> Plugin for ComISAssist<F> {
    const NAME: &'static str = F::PLUGIN_NAME;
    const VENDOR: &'static str = "Andy Weiss";
    const URL: &'static str = "https://github.com/andyweiss/Com-IS-Assist";
    const EMAIL: &'static str = "info@example.com";
    const VERSION: &'static str = env!("CARGO_PKG_VERSION");

    // Bed in and out on the main bus, COM on a mono sidechain - exactly one layout per format, see
    // the module doc comment.
    const AUDIO_IO_LAYOUTS: &'static [AudioIOLayout] = &[AudioIOLayout {
        main_input_channels: NonZeroU32::new(F::CHANNELS as u32),
        main_output_channels: NonZeroU32::new(F::CHANNELS as u32),
        aux_input_ports: &[new_nonzero_u32(1)],
        aux_output_ports: &[],
        names: PortNames {
            layout: Some(F::LABEL),
            main_input: Some("IS"),
            main_output: Some("IS"),
            aux_inputs: &["COM"],
            aux_outputs: &[],
        },
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
        // The controls column's rendered width, carried from one frame to the next so the title
        // row's Bypass can be right-aligned to it. It cannot be computed: each `ParamSlider` sizes
        // its value box to whatever text it currently holds ("5 LU" against "-32 LUFS"), so the
        // column's right edge is only known once the column has been laid out - and the title row
        // is drawn before it. Measuring the previous frame is exact in every frame but the first,
        // and the layout is otherwise static.
        let controls_width = Arc::new(AtomicF32::new(CONTROLS_COLUMN_WIDTH));
        // Whether the about/help overlay is showing. GUI-only state, so it lives with the editor
        // rather than in `Meters` or the parameters - nothing outside the window needs to know,
        // and it should not end up in a saved session.
        let show_help = Arc::new(AtomicBool::new(false));
        create_egui_editor(
            self.params.editor_state.clone(),
            (),
            |_, _| {},
            move |egui_ctx, setter, _state| {
                let controls_width = &controls_width;
                let show_help = &show_help;
                ResizableWindow::new("com-is-assist-editor-window")
                    .min_size(egui::Vec2::new(760.0, 650.0))
                    .show(egui_ctx, egui_state.as_ref(), |ui| {
                        ui.add_space(10.0);
                        ui.horizontal(|ui| {
                            ui.add_space(CONTENT_LEFT_PADDING);
                            // Reserves the meter bank's width so the gutter after it, and the
                            // Bypass after that, line up with the content row below.
                            ui.scope(|ui| {
                                ui.set_width(METER_BANK_WIDTH);
                                ui.label(egui::RichText::new("Com-IS-Assist").size(23.0).strong());
                                // The format, since the four plugins are otherwise identical on
                                // screen and loading the wrong width is an easy patching mistake.
                                ui.label(
                                    egui::RichText::new(F::LABEL)
                                        .size(15.0)
                                        .color(egui::Color32::from_gray(150)),
                                );
                            });
                            meters_to_controls_gutter(ui, false);
                            // Bypass sits in the title row rather than among the tuning parameters
                            // - it is the one control an operator reaches for without looking -
                            // over on the right, but ending exactly where the sliders below it end
                            // rather than at the window's edge. `set_width` pins the child's max
                            // rect as well as its min, which is what gives the right-to-left layout
                            // an edge to align to.
                            ui.scope(|ui| {
                                ui.set_width(controls_width.load(Ordering::Relaxed));
                                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                    // Right-to-left, so the first widget added is the rightmost:
                                    // Bypass keeps the aligned edge and Help sits just inside it.
                                    bool_param_checkbox(ui, setter, &params.bypass, "Bypass");
                                    ui.add_space(12.0);
                                    // Labelled, not just "?": the glyph alone is a guess until you
                                    // hover it, and this is the one control a new operator most
                                    // needs to find.
                                    if ui.button("? Help").on_hover_text("About and help").clicked() {
                                        let showing = show_help.load(Ordering::Relaxed);
                                        show_help.store(!showing, Ordering::Relaxed);
                                    }
                                });
                            });
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
                        // signal, governing nothing in the DSP:
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
                        let is_voice_detect = params.is_voice_detect.value();
                        let target_ratio_lu = if mix_state_index == 3 {
                            params.overvoice_ratio.value()
                        } else {
                            params.target_ratio.value()
                        };
                        // The guard band is on COM only. "Gated out" is a property of the level
                        // alone, not of whether the detector happens to be firing: the useful
                        // reading is "detection is off down here", which is also what the shaded
                        // band on the COM bar shows.
                        let com_gate_lufs = params.com_gate_lufs.value();
                        let com_gated_out = com_lufs < com_gate_lufs;

                        const RATIO_GREEN: egui::Color32 = egui::Color32::from_rgb(0x39, 0xC8, 0x39);
                        const RATIO_ORANGE: egui::Color32 = egui::Color32::from_rgb(224, 140, 32);
                        let ratio_color = if ratio_lu < 0.0 {
                            egui::Color32::from_gray(120)
                        } else if ratio_lu < target_ratio_lu - RATIO_METER_GREEN_BAND_LU {
                            egui::Color32::from_rgb(224, 32, 32)
                        } else {
                            RATIO_GREEN
                        };
                        // Where "correct" ends and "surplus" begins. Only the portion of the bar
                        // above this is drawn orange - the level up to here is exactly what was
                        // asked for and should still read as on-target.
                        let ratio_excess_from = (ratio_lu >= 0.0)
                            .then_some(target_ratio_lu + RATIO_METER_GREEN_BAND_LU)
                            .filter(|threshold| display_ratio_lu > *threshold);

                        ui.horizontal(|ui| {
                            ui.add_space(CONTENT_LEFT_PADDING);

                            ui.vertical(|ui| {
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
                                // One accent for both level bars: they are the same kind of
                                // reading, and distinguishing them by colour was never what told
                                // them apart - the captions and their positions do that.
                                let accent = control_accent(ui);
                                gain_reduction_meter(ui, gain_reduction_db);
                                ui.add_space(GAP_AFTER_SCALED);
                                let is_bar = loudness_meter(
                                        ui,
                                        LoudnessMeter {
                                            label: "IS",
                                            channels: F::IS_CAPTION,
                                            lufs: is_lufs,
                                            color: accent,
                                            gain_reduction_db: Some(gain_reduction_db),
                                            voice: Some(VoiceIndicator {
                                                active: is_voice_detect
                                                    && meters.is_voice_active.load(Ordering::Relaxed),
                                                // The same cross as COM's guard band: "not
                                                // detecting" has to look different from "detecting
                                                // nothing", since they call for different actions.
                                                gated_out: !is_voice_detect,
                                            }),
                                            gate_lufs: None,
                                        },
                                    );
                                ui.add_space(GAP_BEFORE_RATIO);
                                let ratio_span = ratio_meter(
                                    ui,
                                    effective_is_lufs,
                                    display_ratio_lu,
                                    ratio_color,
                                    ratio_excess_from,
                                    RATIO_ORANGE,
                                );
                                ui.add_space(GAP_AFTER_RATIO);
                                let com_bar = loudness_meter(
                                        ui,
                                        LoudnessMeter {
                                            label: "COM",
                                            channels: "Sidechain",
                                            lufs: com_lufs,
                                            color: accent,
                                            gain_reduction_db: None,
                                            voice: Some(VoiceIndicator {
                                                active: meters.voice_active.load(Ordering::Relaxed),
                                                gated_out: com_gated_out,
                                            }),
                                            gate_lufs: Some(com_gate_lufs),
                                        },
                                    );

                                // Drawn last, so the leaders sit over the columns they cross.
                                ratio_leader_lines(ui, &ratio_span, is_bar, com_bar);
                            });

                            // The state readout lives under the meters rather than among the
                            // controls: it describes what the metering is showing, and this is
                            // where the eye already is while mixing. Spaced by `CAPTION_GAP` like
                            // every other vertical step in this column, so caption->bar,
                            // bar->lamp and lamp->badge all share one rhythm.
                            ui.add_space(CAPTION_GAP);
                            let index = mix_state_index as usize;
                            state_badge(
                                ui,
                                MIX_STATE_LABELS.get(index).copied().unwrap_or("-"),
                                mix_state_color(index),
                                METER_BANK_WIDTH,
                            );
                            });

                            meters_to_controls_gutter(ui, true);

                            // Parameters (right).
                            let controls = ui.vertical(|ui| {
                                // The widest row wins: the value boxes are not all the same width,
                                // so the column's right edge is whichever grid holds the longest
                                // one (today, the gate's "-32 LUFS").
                                let mut rows_right = f32::NEG_INFINITY;
                                rows_right = rows_right.max(control_grid(ui, "com-is-assist-controls", |ui| {
                                        // Whichever ratio the state machine is currently aiming at
                                        // gets tinted in that state's own colour, so the control
                                        // and the state badge agree without having to be compared.
                                        ratio_control_row(
                                            ui,
                                            "IS voice ratio",
                                            &params.target_ratio,
                                            setter,
                                            (mix_state_index == 1).then(|| mix_state_color(1)),
                                            true,
                                        );
                                        // Over-voice needs voice on the Bed, which nothing can
                                        // report with the IS detector off - greyed out rather
                                        // than hidden, so it is clear why it does nothing.
                                        ratio_control_row(
                                            ui,
                                            "Over-voice ratio",
                                            &params.overvoice_ratio,
                                            setter,
                                            (mix_state_index == 3).then(|| mix_state_color(3)),
                                            is_voice_detect,
                                        );

                                        control_label(ui, "Max gain reduction");
                                        ui.add(ParamSlider::for_param(&params.max_gain_reduction_db, setter).with_width(CONTROL_SLIDER_WIDTH));
                                        ui.end_row();

                                        for (label, param) in [("Attack", &params.attack_ms), ("Release", &params.release_s)] {
                                            control_label(ui, label);
                                            ui.add(ParamSlider::for_param(param, setter).with_width(CONTROL_SLIDER_WIDTH));
                                            ui.end_row();
                                        }
                                    }));

                                // Set off on its own: these govern *detection* rather than shaping
                                // the mix, so they belong with neither the tuning parameters above
                                // nor the routing group below.
                                ui.add_space(14.0);
                                ui.separator();
                                ui.add_space(10.0);
                                rows_right = rows_right.max(control_grid(ui, "com-is-assist-gate", |ui| {
                                    control_label(ui, "Voice detect gate");
                                    ui.add(
                                        ParamSlider::for_param(&params.com_gate_lufs, setter)
                                            .with_width(CONTROL_SLIDER_WIDTH),
                                    );
                                    ui.end_row();

                                    // In the grid, so the checkbox lines up with the slider above.
                                    control_label(ui, "IS voice detect");
                                    bool_param_checkbox(ui, setter, &params.is_voice_detect, "");
                                    ui.end_row();
                                }));

                                // Bus-insert group, set off from the tuning parameters above by a
                                // rule: these two are about *routing* rather than how the automix
                                // behaves, and divergence is meaningless on its own.
                                ui.add_space(16.0);
                                ui.separator();
                                ui.add_space(10.0);

                                let mix_to_bed = params.mix_dialogue_to_bed.value();
                                bool_param_checkbox(ui, setter, &params.mix_dialogue_to_bed, "Mix dialogue to bed");
                                // Divergence only places Dialogue *within* the Bed, so with the mix
                                // off it controls nothing. Greyed out rather than hidden, so the
                                // relationship between the two stays visible. Left out entirely on
                                // 2.0, though: with no Center there is nothing to diverge from.
                                if F::HAS_CENTER {
                                    ui.add_space(8.0);
                                    rows_right = rows_right.max(
                                        ui.add_enabled_ui(mix_to_bed, |ui| {
                                            control_grid(ui, "com-is-assist-divergence", |ui| {
                                                control_label(ui, "Voice divergence");
                                                ui.add(ParamSlider::for_param(&params.divergence, setter).with_width(CONTROL_SLIDER_WIDTH));
                                                ui.end_row();
                                            })
                                        })
                                        .inner,
                                    );
                                }

                                // The live state-machine readout. With the mixer's behavior now
                                // defined entirely by which of four states it's in, showing that
                                // state directly is the fastest answer to "why is it doing that?"
                                // - far more legible than inferring it from the meters.
                                // Per-channel duck depth. Replaces the loop-contribution bars that
                                // used to sit here, a debugging view of the old three-stage
                                // cascade rather than anything to mix with.
                                // The whole section, rule included, is left out on formats with
                                // no groups (2.0).
                                if !F::DUCK_GROUPS.is_empty() {
                                    ui.add_space(16.0);
                                    ui.separator();
                                    ui.add_space(10.0);
                                    ui.label("Duck depth");
                                    ui.add_space(6.0);
                                    rows_right = rows_right.max(control_grid(ui, "com-is-assist-duck-depth", |ui| {
                                        let all_params = params.duck_depth_params();
                                        for (label, param) in
                                            F::DUCK_GROUPS.iter().map(|&group| (BED_GROUP_LABELS[group], all_params[group]))
                                        {
                                            control_label(ui, label);
                                            ui.add(ParamSlider::for_param(param, setter).with_width(CONTROL_SLIDER_WIDTH));
                                            ui.end_row();
                                        }
                                    }));
                                }
                                rows_right
                            });
                            // Hand this frame's measurement to the next frame's title row: the
                            // distance from the column's left edge to the end of its widest value
                            // box, which is what Bypass aligns its right edge to.
                            if controls.inner.is_finite() {
                                controls_width
                                    .store(controls.inner - controls.response.rect.left(), Ordering::Relaxed);
                            }
                        });
                        ui.add_space(14.0);
                    });

                let mut showing = show_help.load(Ordering::Relaxed);
                if showing {
                    // `Window::open` writes the close button's result back into `showing`.
                    help_window::<F>(egui_ctx, &mut showing);
                    show_help.store(showing, Ordering::Relaxed);
                }
            },
        )
    }

    fn initialize(
        &mut self,
        _audio_io_layout: &AudioIOLayout,
        buffer_config: &BufferConfig,
        _context: &mut impl InitContext<Self>,
    ) -> bool {
        self.processor = Some(AutomixProcessor::with_layout(
            F::LAYOUT,
            buffer_config.sample_rate as u32,
            self.params.automix_config(),
        ));
        // `.ok()`, not `.expect(...)`: if this fails (e.g. the ONNX Runtime binary couldn't be
        // obtained), `process()` fails open rather than the whole plugin refusing to initialize
        // over what's ultimately just a display/gating refinement, not the core automix path.
        // Off the audio thread: a Silero inference is a ~600-900us spike landing on one block in
        // twenty-four at a 64-sample buffer, which no real-time budget absorbs. `BackgroundVad`
        // leaves a bounded copy here and runs the model on a worker. `idle_state` is the fail-safe
        // answer until the first inference lands - open on COM, so the mix still ducks.
        self.voice_activity =
            BackgroundVad::new(buffer_config.sample_rate as u32, VoiceActivityConfig::default(), true).ok();
        // A second, independent detector instance for the Bed - same model and settings, its own
        // recurrent state and hangover timer (see `is_voice_activity`'s doc comment).
        self.is_voice_activity =
            BackgroundVad::new(buffer_config.sample_rate as u32, VoiceActivityConfig::default(), false).ok();
        self.processor.is_some()
    }

    fn process(
        &mut self,
        buffer: &mut Buffer,
        aux: &mut AuxiliaryBuffers,
        _context: &mut impl ProcessContext<Self>,
    ) -> ProcessStatus {
        let Some(processor) = self.processor.as_mut() else {
            return ProcessStatus::Error("processor not initialized");
        };

        // `initialize()` only reads these parameters once, at plugin load - without refreshing
        // them here on every block, a live parameter change would silently have no effect. Cheap -
        // a couple of struct copies - and preserves all accumulated state (loudness detectors, the
        // loop's in-flight reduction), unlike rebuilding the processor would.
        processor.set_config(self.params.automix_config());
        processor.set_automix_enabled(!self.params.bypass.value());

        let mix_dialogue_to_bed = self.params.mix_dialogue_to_bed.value();
        let divergence = self.params.divergence.value() / 100.0;

        let bed_channels = F::CHANNELS;
        let num_samples = buffer.samples();
        let channels = buffer.as_slice();
        if channels.len() < bed_channels {
            return ProcessStatus::Error("main bus narrower than this format's bed");
        }

        // com-is-assist-core works with interleaved audio;
        // nih-plug hands us planar per-channel slices, so de-interleave Bed for the shared
        // processor and re-interleave on the way back out.
        // `resize` only touches the allocator if the host hands over a bigger block than it said
        // it would; in steady state these are already the right length and this is a no-op.
        self.interleaved_bed.resize(num_samples * bed_channels, 0.0);
        self.dialogue_scratch.resize(num_samples, 0.0);
        self.mono_bed_scratch.resize(num_samples, 0.0);
        let interleaved_bed = &mut self.interleaved_bed[..num_samples * bed_channels];
        let dialogue = &mut self.dialogue_scratch[..num_samples];
        let mono_bed = &mut self.mono_bed_scratch[..num_samples];

        // COM from the sidechain. An unconnected sidechain reads as silence - which, through the
        // guard band, simply means "nobody is commentating" and the bed stays at unity - rather
        // than as an error: inserting the plugin before wiring its sidechain is the normal order
        // of doing things, not a fault.
        match aux.inputs.first().and_then(|sidechain| sidechain.as_slice_immutable().first()) {
            Some(com) if com.len() >= num_samples => dialogue.copy_from_slice(&com[..num_samples]),
            _ => dialogue.fill(0.0),
        }

        for frame in 0..num_samples {
            // Mono downmix of the *dry* Bed for the IS-side detector - Silero is a mono model, and
            // "is anyone speaking anywhere on the Bed" is a whole-Bed question, so averaging the
            // channels is the right reduction. Accumulated here, in the de-interleave pass that
            // has the samples in hand anyway, rather than in a second pass afterwards.
            let mut sum = 0.0;
            for ch in 0..bed_channels {
                let sample = channels[ch][frame];
                interleaved_bed[frame * bed_channels + ch] = sample;
                sum += sample;
            }
            mono_bed[frame] = sum / bed_channels as f32;
        }

        // Feed-forward: Dialogue is measured dry, before anything is applied to it.
        // A mismatched channel count here would be a caller bug in this fixed-layout plugin, not a
        // runtime condition - deliberately ignored rather than crashing the audio thread over it.
        let _ = processor.feed_dialogue(dialogue);

        // Fails open (treats COM as always voice-active) if VAD couldn't be constructed at
        // `initialize()` time - see `voice_activity`'s doc comment.
        let voice_active = match self.voice_activity.as_mut() {
            Some(vad) => {
                vad.feed(dialogue);
                vad.voice_active()
            }
            None => true,
        };
        // Fails *closed* on the IS side (no detector => "no voice on IS"), unlike COM's fail-open:
        // IS voice only ever selects the interview/over-voice states, so assuming it absent just
        // falls back to the ordinary duck/release behavior rather than inventing a state.
        // Switched off, the detector is not fed at all - it would only be spending the worker's
        // time on an answer nothing reads - and the Bed counts as never carrying voice.
        let is_voice_active = match self.is_voice_activity.as_mut() {
            Some(vad) if self.params.is_voice_detect.value() => {
                vad.feed(mono_bed);
                vad.voice_active()
            }
            _ => false,
        };
        // Published before `process_bed` so a state change takes effect from this block's first
        // sub-chunk - see `set_voice_activity`.
        processor.set_voice_activity(voice_active, is_voice_active);

        // Measures the dry Bed, then steps the gain loop and applies the resulting gain in
        // sub-chunks - all inside the shared core, so every wrapper gets identical DSP. In place
        // and unbuffered, so the plugin adds no latency and reports none.
        let _ = processor.process_bed(interleaved_bed);

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
            // The *gated* flag, not the raw detector output: that is what drives the mix, so a
            // lit LED while the guard band is suppressing detection would be a lie.
            self.meters.voice_active.store(processor.com_voice_gated(), Ordering::Relaxed);
            self.meters.is_voice_active.store(is_voice_active, Ordering::Relaxed);
            self.meters
                .mix_state
                .store(mix_state_index(processor.mix_state()), Ordering::Relaxed);
        }

        if mix_dialogue_to_bed {
            mix_dialogue_into_bed(
                interleaved_bed,
                dialogue,
                bed_channels as u32,
                LEFT_CHANNEL,
                RIGHT_CHANNEL,
                F::HAS_CENTER.then_some(CENTER_CHANNEL),
                divergence,
            );
        }

        for frame in 0..num_samples {
            for ch in 0..bed_channels {
                channels[ch][frame] = interleaved_bed[frame * bed_channels + ch];
            }
        }

        ProcessStatus::Normal
    }
}

impl<F: BedFormat> Vst3Plugin for ComISAssist<F> {
    const VST3_CLASS_ID: [u8; 16] = F::CLASS_ID;
    const VST3_MAIN_BUS_ARRANGEMENT: Option<u64> = Some(F::ARRANGEMENT);
    const VST3_SUBCATEGORIES: &'static [Vst3SubCategory] = &[Vst3SubCategory::Fx, Vst3SubCategory::Dynamics];
}

nih_export_vst3!(
    ComISAssist<Stereo>,
    ComISAssist<Surround51>,
    ComISAssist<Surround514>,
    ComISAssist<Surround712>
);

#[cfg(test)]
mod tests {
    use super::*;

    /// The help page's version string has to actually say something. A stamped release build
    /// should carry its tag; an unstamped one should say it is a dev build rather than go blank or
    /// claim a release that was never cut.
    #[test]
    fn the_display_version_is_never_empty() {
        assert!(!DISPLAY_VERSION.is_empty(), "DISPLAY_VERSION resolved to an empty string");
        match std::env::var("COMIS_RELEASE_VERSION") {
            Ok(tag) if !tag.trim().is_empty() => assert_eq!(DISPLAY_VERSION, tag),
            _ => assert!(
                DISPLAY_VERSION.starts_with("dev build"),
                "an unstamped build should say so, got {DISPLAY_VERSION:?}"
            ),
        }
    }

    /// The time controls' defaults must sit at the exact centre of their sliders - that is what
    /// `centred_skew` is for, and an off-centre default is easy to miss by eye.
    #[test]
    fn the_time_defaults_sit_at_the_centre_of_their_sliders() {
        let params = ComISAssistParams::for_format::<Surround51>();
        for param in [&params.attack_ms, &params.release_s] {
            let normalized = param.preview_normalized(param.default_plain_value());
            assert!((normalized - 0.5).abs() < 1e-4, "{} default sits at {normalized}", param.name());
        }
    }

    /// Attack is displayed in ms below a second and in seconds above, and typing either back in
    /// must land on the same value.
    #[test]
    fn attack_reads_back_what_it_displays() {
        let params = ComISAssistParams::for_format::<Surround51>();
        for (text, ms) in [("2.5 s", 2500.0), ("250 ms", 250.0), ("2.5s", 2500.0), ("400", 400.0)] {
            let parsed = params.attack_ms.string_to_normalized_value(text).map(|n| params.attack_ms.preview_plain(n));
            assert!(parsed.is_some_and(|p| (p - ms).abs() < 0.01), "{text} parsed as {parsed:?}, expected {ms}");
        }
        assert_eq!(params.attack_ms.normalized_value_to_string(params.attack_ms.preview_normalized(2500.0), true), "2.5 s");
        assert_eq!(params.attack_ms.normalized_value_to_string(params.attack_ms.preview_normalized(250.0), true), "250 ms");
    }

    /// Every shipped default in one assertion, pinned where a reader can see them together.
    ///
    /// Worth having because these have now moved more than once, and the GUI gives no clue whether
    /// a value on screen is the default or something the host restored from a saved session - so
    /// "is the default right?" is a question the plugin itself cannot answer by inspection.
    #[test]
    fn the_shipped_defaults_are_what_the_specs_say() {
        let params = ComISAssistParams::for_format::<Surround514>();

        assert_eq!(params.target_ratio.default_plain_value(), 5.0);
        assert_eq!(params.overvoice_ratio.default_plain_value(), 13.0);
        assert_eq!(params.max_gain_reduction_db.default_plain_value(), 24.0);
        assert_eq!(params.com_gate_lufs.default_plain_value(), -32.0);
        assert_eq!(params.attack_ms.default_plain_value(), 2500.0);
        assert_eq!(params.release_s.default_plain_value(), 3.0);
        assert!(params.is_voice_detect.default_plain_value());
        // Centre-only: Dialogue is not spread into L/R unless asked for.
        assert_eq!(params.divergence.default_plain_value(), 0.0);
        assert!(!params.bypass.default_plain_value());
        assert!(params.mix_dialogue_to_bed.default_plain_value());

        // Every channel group takes the ducking in full, which is the pre-feature behaviour.
        for param in params.duck_depth_params() {
            assert_eq!(param.default_plain_value(), 1.0, "{} should default to 1:1", param.name());
        }
    }

    /// The two wrappers must not be able to disagree about what the mixer does out of the box, so
    /// the VST3's defaults are checked against the core's rather than just against themselves.
    #[test]
    fn the_defaults_match_the_shared_core() {
        let params = ComISAssistParams::for_format::<Surround514>();
        let core = AutomixEngineConfig::default();

        assert_eq!(params.target_ratio.default_plain_value() as f64, core.target_ratio_lu);
        assert_eq!(params.overvoice_ratio.default_plain_value() as f64, core.overvoice_ratio_lu);
        assert_eq!(params.max_gain_reduction_db.default_plain_value() as f64, core.max_gain_reduction_db);
        assert_eq!(params.com_gate_lufs.default_plain_value() as f64, core.com_gate_lufs);
        assert!((params.attack_ms.default_plain_value() as f64 / 1000.0 - core.attack_seconds).abs() < 1e-9);
        assert_eq!(params.release_s.default_plain_value() as f64, core.release_seconds);

        let depth = params.duck_depth();
        assert_eq!(depth.front_lr, core.duck_depth.front_lr);
        assert_eq!(depth.center, core.duck_depth.center);
        assert_eq!(depth.lfe, core.duck_depth.lfe);
        assert_eq!(depth.surround, core.duck_depth.surround);
        assert_eq!(depth.top_front, core.duck_depth.top_front);
        assert_eq!(depth.top_rear, core.duck_depth.top_rear);
        assert_eq!(depth.side, core.duck_depth.side);
        assert_eq!(depth.top_side, core.duck_depth.top_side);
    }

    fn hidden(param: &FloatParam) -> bool {
        param.flags().contains(ParamFlags::HIDDEN)
    }

    /// Each format offers the host exactly the duck-depth groups it has channels for - a hidden
    /// parameter is also non-automatable, so this is what keeps a stereo session's automation
    /// list free of controls that would do nothing.
    #[test]
    fn each_format_exposes_only_its_own_channel_groups() {
        fn visible_groups<F: BedFormat>() -> Vec<&'static str> {
            let params = ComISAssistParams::for_format::<F>();
            let visible: Vec<_> = params
                .duck_depth_params()
                .iter()
                .zip(BED_GROUP_LABELS)
                .filter(|(p, _)| !hidden(p))
                .map(|(_, label)| label)
                .collect();
            // The GUI draws `DUCK_GROUPS`; the host sees the unhidden params. They must agree.
            let drawn: Vec<_> = F::DUCK_GROUPS.iter().map(|&g| BED_GROUP_LABELS[g]).collect();
            let mut sorted_drawn = drawn.clone();
            sorted_drawn.sort_by_key(|label| BED_GROUP_LABELS.iter().position(|l| l == label));
            assert_eq!(visible, sorted_drawn, "{}", F::LABEL);
            drawn
        }
        assert!(visible_groups::<Stereo>().is_empty(), "2.0 has no duck depth at all");
        assert_eq!(visible_groups::<Surround51>(), ["Front L/R", "Center", "LFE", "Surround L/R"]);
        assert_eq!(
            visible_groups::<Surround514>(),
            ["Front L/R", "Center", "LFE", "Surround L/R", "Top Front L/R", "Top Rear L/R"]
        );
        assert_eq!(
            visible_groups::<Surround712>(),
            ["Front L/R", "Center", "LFE", "Surround L/R", "Side L/R", "Top Side L/R"]
        );

        assert!(hidden(&ComISAssistParams::for_format::<Stereo>().divergence), "no Center on 2.0");
        assert!(!hidden(&ComISAssistParams::for_format::<Surround51>().divergence));
        assert!(!hidden(&ComISAssistParams::for_format::<Surround514>().divergence));
        assert!(!hidden(&ComISAssistParams::for_format::<Surround712>().divergence));
    }

    /// The bus contract: Bed in and out on the main bus at the format's width and exact speaker
    /// arrangement, COM on one mono sidechain, nothing else.
    #[test]
    fn each_format_has_a_bed_main_bus_and_a_mono_sidechain() {
        fn check<F: BedFormat>(expected_arrangement: u64, expected_channels: u32) {
            let layouts = <ComISAssist<F> as Plugin>::AUDIO_IO_LAYOUTS;
            assert_eq!(layouts.len(), 1, "{}: one fixed layout", F::LABEL);
            let layout = layouts[0];
            assert_eq!(layout.main_input_channels.map(NonZeroU32::get), Some(expected_channels));
            assert_eq!(layout.main_output_channels.map(NonZeroU32::get), Some(expected_channels));
            assert_eq!(layout.aux_input_ports, &[new_nonzero_u32(1)]);
            assert!(layout.aux_output_ports.is_empty());
            assert_eq!(
                <ComISAssist<F> as Vst3Plugin>::VST3_MAIN_BUS_ARRANGEMENT,
                Some(expected_arrangement),
                "{}",
                F::LABEL
            );
            // The core's layout must be the same width as the bus, or it would read the wrong
            // channels.
            assert_eq!(F::LAYOUT.channel_count(), expected_channels, "{}", F::LABEL);
        }
        check::<Stereo>(kStereo, 2);
        check::<Surround51>(k51, 6);
        check::<Surround514>(k51_4, 10);
        check::<Surround712>(k71_2, 10);
    }

    /// A host tells plugins apart by class ID alone - two formats sharing one would load as each
    /// other. The 5.1 one is also pinned, since it is what existing sessions were saved with.
    #[test]
    fn every_format_has_its_own_class_id() {
        let ids = [Stereo::CLASS_ID, Surround51::CLASS_ID, Surround514::CLASS_ID, Surround712::CLASS_ID];
        for (i, a) in ids.iter().enumerate() {
            for b in &ids[i + 1..] {
                assert_ne!(a, b);
            }
        }
        assert_eq!(&Surround51::CLASS_ID, b"ComISAssistDuck1");
    }
}
