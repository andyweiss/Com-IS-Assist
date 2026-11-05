//! The real-time path must not allocate - not once per block, and not when a setting changes.
//!
//! An allocation on the audio thread is a potential lock and an unbounded pause. It usually goes
//! unnoticed, because the allocator is fast while nothing else is using it - and then it shows up
//! as dropouts exactly when the GUI is busy, which is while someone is dragging a control. That is
//! how this was found: `set_config` rebuilt the duck-depth table from a freshly allocated channel
//! map on every block.
//!
//! This binary installs a counting global allocator and holds the whole real-time call sequence a
//! wrapper makes per block - with the config changing on every block, as it does under automation
//! or a dragged slider - to exactly zero allocations. It is its own test binary, with a single test,
//! so no other test's allocations can land in the count.

use com_is_assist_core::automix::{AutomixEngineConfig, AutomixProcessor, DuckDepth};
use com_is_assist_core::loudness::BedLayout;
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

struct CountingAllocator;

static COUNTING: AtomicBool = AtomicBool::new(false);
static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if COUNTING.load(Ordering::Relaxed) {
            ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        }
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if COUNTING.load(Ordering::Relaxed) {
            ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        }
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

const RATE: u32 = 48_000;
const BLOCK: usize = 64;

#[test]
fn the_real_time_path_never_allocates_even_while_settings_change() {
    for layout in [BedLayout::Stereo, BedLayout::Surround51, BedLayout::Surround514, BedLayout::Surround712] {
        let channels = layout.channel_count() as usize;
        let mut processor = AutomixProcessor::with_layout(layout, RATE, AutomixEngineConfig::default());
        // Everything the wrapper preallocates, allocated before counting starts.
        let mut bed = vec![0.0_f32; BLOCK * channels];
        let mut dialogue = vec![0.0_f32; BLOCK];

        COUNTING.store(true, Ordering::Relaxed);
        for block in 0..2_000 {
            for (i, sample) in dialogue.iter_mut().enumerate() {
                *sample = 0.2 * ((block * BLOCK + i) as f32 * 0.09).sin();
            }
            for (i, sample) in bed.iter_mut().enumerate() {
                *sample = 0.4 * ((block * BLOCK * channels + i) as f32 * 0.05).sin();
            }
            // A different config every block: a slider being dragged through its range.
            let x = (block % 100) as f64 / 100.0;
            let config = AutomixEngineConfig {
                target_ratio_lu: 2.0 + 8.0 * x,
                attack_seconds: 0.1 + 5.0 * x,
                release_seconds: 0.4 + 6.0 * x,
                com_gate_lufs: -60.0 + 30.0 * x,
                duck_depth: DuckDepth { surround: x, lfe: 1.0 - x, top_front: x, side: x, ..DuckDepth::default() },
                ..AutomixEngineConfig::default()
            };
            processor.set_config(config);
            processor.set_automix_enabled(block % 300 != 0);
            processor.feed_dialogue(&dialogue).unwrap();
            processor.set_voice_activity(block % 50 < 40, block % 70 < 10);
            processor.process_bed(&mut bed).unwrap();
            std::hint::black_box((
                processor.applied_gain_reduction_db(),
                processor.display_ratio_lu(),
                processor.applied_ratio_lu(),
            ));
        }
        COUNTING.store(false, Ordering::Relaxed);

        let allocations = ALLOCATIONS.swap(0, Ordering::Relaxed);
        assert_eq!(allocations, 0, "{layout:?}: the real-time path allocated {allocations} times in 2000 blocks");
    }
}
