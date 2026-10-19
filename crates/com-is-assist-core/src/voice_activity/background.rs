use super::{SileroVad, VoiceActivityConfig, VoiceActivityError};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;

/// How long the worker sleeps when it finds the ring empty.
///
/// Pure added detection latency, so it is kept well under the detector's own 32ms inference hop -
/// at which point it stops being the thing anyone can hear. It was 10ms on the argument that the
/// 800ms hangover made it irrelevant; that argument is wrong for the *onset*, which is the edge
/// the mix reacts to and the one that was audibly late. Waking 500 times a second to find an
/// empty queue costs almost nothing next to the inference it is waiting for.
const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(2);

/// One second of mono audio at 48kHz. The worker only has to keep up *on average*, and this
/// absorbs about a hundred poll intervals of scheduling jitter before anything is dropped.
const RING_CAPACITY: usize = 48_000;

/// A single-producer/single-consumer sample queue with no locks and no allocation after
/// construction - the audio thread writes, the VAD worker reads.
///
/// Hand-rolled rather than taken from a crate because the requirement is small and the safety
/// argument needs to be legible: exactly one writer touches `write`, exactly one reader touches
/// `read`, and each publishes its index with a `Release` store the other `Acquire`s.
struct Ring {
    buffer: Vec<std::cell::UnsafeCell<f32>>,
    write: AtomicUsize,
    read: AtomicUsize,
}

// SAFETY: access is disjoint by construction - the producer only writes slots the consumer has
// already passed, the consumer only reads slots the producer has already published, and the two
// atomic indices order the handoff. Nothing else touches the buffer.
unsafe impl Sync for Ring {}
unsafe impl Send for Ring {}

impl Ring {
    fn new(capacity: usize) -> Self {
        Self {
            buffer: (0..capacity).map(|_| std::cell::UnsafeCell::new(0.0)).collect(),
            write: AtomicUsize::new(0),
            read: AtomicUsize::new(0),
        }
    }

    /// Producer side. Returns how many samples did **not** fit; the caller reports that rather
    /// than blocking or allocating, because this runs on the audio thread.
    fn push(&self, samples: &[f32]) -> usize {
        let capacity = self.buffer.len();
        let write = self.write.load(Ordering::Relaxed);
        let read = self.read.load(Ordering::Acquire);
        // One slot is left permanently empty so that full and empty stay distinguishable.
        let free = (read + capacity - write - 1) % capacity;
        let n = samples.len().min(free);
        for (i, sample) in samples[..n].iter().enumerate() {
            // SAFETY: this slot is within `free` slots of `write`, all of which the consumer has
            // already read past and cannot touch again until the store below publishes them.
            unsafe { *self.buffer[(write + i) % capacity].get() = *sample };
        }
        self.write.store((write + n) % capacity, Ordering::Release);
        samples.len() - n
    }

    /// Consumer side. Copies what is available into `out` and returns how much it took.
    fn pop(&self, out: &mut [f32]) -> usize {
        let capacity = self.buffer.len();
        let read = self.read.load(Ordering::Relaxed);
        let write = self.write.load(Ordering::Acquire);
        let available = (write + capacity - read) % capacity;
        let n = out.len().min(available);
        for (i, slot) in out[..n].iter_mut().enumerate() {
            // SAFETY: this slot is within `available` slots of `read`, all of which the producer
            // has already published and will not overwrite until the store below frees them.
            *slot = unsafe { *self.buffer[(read + i) % capacity].get() };
        }
        self.read.store((read + n) % capacity, Ordering::Release);
        n
    }
}

/// [`SileroVad`] moved off the audio thread.
///
/// **Why this exists.** A Silero inference costs 600-900us and runs once per 32ms of audio. On the
/// audio thread that is not an average cost but a spike: at a 64-sample buffer it lands entirely
/// within one block in every twenty-four, against a budget of 1333us for the whole block - and two
/// detectors can spike on the same one. Measured on this plugin, the pair accounted for ~1.6ms of
/// a worst-case block while the *mean* load sat near 1%. That is the shape of problem that forces
/// a host's buffer size up while its CPU meter looks unremarkable.
///
/// Moving inference to a worker reduces the audio thread's share to a bounded copy. The cost is up
/// to [`POLL_INTERVAL`] of extra detection latency, immaterial beside the detector's own 32ms hop
/// and 800ms hangover.
pub struct BackgroundVad {
    ring: Arc<Ring>,
    voice_active: Arc<AtomicBool>,
    dropped_samples: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl BackgroundVad {
    /// `idle_state` is what [`Self::voice_active`] reports until the first inference completes,
    /// and is the caller's fail-safe choice: the COM side fails *open* (assume the commentator is
    /// talking, so the mix still ducks), the IS side fails *closed*.
    pub fn new(sample_rate: u32, config: VoiceActivityConfig, idle_state: bool) -> Result<Self, VoiceActivityError> {
        // Built here rather than inside the worker so that a model which will not load is an error
        // the caller can handle at `initialize()` time, instead of a thread that quietly dies.
        let mut vad = SileroVad::new(sample_rate, config)?;

        let ring = Arc::new(Ring::new(RING_CAPACITY));
        let voice_active = Arc::new(AtomicBool::new(idle_state));
        let dropped_samples = Arc::new(AtomicU64::new(0));
        let stop = Arc::new(AtomicBool::new(false));

        let worker = {
            let (ring, voice_active, stop) = (ring.clone(), voice_active.clone(), stop.clone());
            std::thread::Builder::new()
                .name("com-is-assist-vad".into())
                .spawn(move || {
                    // Drained in chunks sized well past one poll interval of audio, so a backlog
                    // clears in a few iterations rather than one sample at a time.
                    let mut scratch = vec![0.0_f32; 4_096];
                    while !stop.load(Ordering::Relaxed) {
                        let mut did_work = false;
                        loop {
                            let n = ring.pop(&mut scratch);
                            if n == 0 {
                                break;
                            }
                            did_work = true;
                            // A failed inference leaves the previous answer standing, which is the
                            // conservative choice: better a stale flag than one that flaps.
                            if vad.feed(&scratch[..n]).is_ok() {
                                voice_active.store(vad.voice_active(), Ordering::Relaxed);
                            }
                        }
                        if !did_work {
                            std::thread::sleep(POLL_INTERVAL);
                        }
                    }
                })
                .map_err(VoiceActivityError::WorkerSpawn)?
        };

        Ok(Self { ring, voice_active, dropped_samples, stop, worker: Some(worker) })
    }

    /// Hands one block of mono audio to the worker. **Real-time safe**: a bounded copy into a
    /// preallocated ring and two atomic stores - no allocation, no lock, no syscall.
    ///
    /// Takes `&mut self` although it mutates nothing through the reference: the ring is sound only
    /// while exactly one thread produces, and an exclusive borrow is what makes the compiler
    /// enforce that rather than leaving it to a comment nobody reads.
    pub fn feed(&mut self, samples: &[f32]) {
        let dropped = self.ring.push(samples);
        if dropped > 0 {
            self.dropped_samples.fetch_add(dropped as u64, Ordering::Relaxed);
        }
    }

    /// The most recent answer the worker published.
    pub fn voice_active(&self) -> bool {
        self.voice_active.load(Ordering::Relaxed)
    }

    /// Samples the ring had no room for, cumulative. Non-zero means the worker is not keeping up,
    /// so the detector is reading discontinuous audio - worth surfacing rather than tolerating.
    pub fn dropped_samples(&self) -> u64 {
        self.dropped_samples.load(Ordering::Relaxed)
    }
}

impl Drop for BackgroundVad {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            // Joined rather than detached: the worker owns an ONNX session, and letting it outlive
            // a plugin instance the host is tearing down is how you get a crash on unload.
            let _ = worker.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_ring_round_trips_in_order_and_reports_what_did_not_fit() {
        let ring = Ring::new(8); // 7 usable slots
        assert_eq!(ring.push(&[1.0, 2.0, 3.0]), 0);
        let mut out = [0.0; 4];
        assert_eq!(ring.pop(&mut out), 3);
        assert_eq!(&out[..3], &[1.0, 2.0, 3.0]);

        // Overfilling reports the remainder rather than overwriting audio the worker has not read.
        let big: Vec<f32> = (0..10).map(|i| i as f32).collect();
        assert_eq!(ring.push(&big), 3);
    }

    #[test]
    fn the_ring_wraps_without_losing_or_reordering_samples() {
        let ring = Ring::new(8);
        let mut expected = 0.0_f32;
        let mut out = [0.0; 5];
        // Far more passes than the capacity, so every push and pop crosses the wrap point.
        for _ in 0..50 {
            let block: Vec<f32> = (0..5).map(|i| expected + i as f32).collect();
            assert_eq!(ring.push(&block), 0);
            assert_eq!(ring.pop(&mut out), 5);
            assert_eq!(out.to_vec(), block);
            expected += 5.0;
        }
    }

    #[test]
    fn silence_through_the_worker_never_reports_voice_and_the_worker_keeps_up() {
        let mut vad = BackgroundVad::new(48_000, VoiceActivityConfig::default(), false).expect("worker starts");
        // 20 x 50ms of audio delivered at roughly real-time rate.
        for _ in 0..20 {
            vad.feed(&vec![0.0_f32; 2_400]);
            std::thread::sleep(std::time::Duration::from_millis(15));
        }
        assert!(!vad.voice_active(), "silence must never register as voice");
        assert_eq!(vad.dropped_samples(), 0, "the worker should keep up with real-time-rate audio");
    }
}
