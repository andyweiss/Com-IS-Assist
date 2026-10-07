/// A fixed-length delay for interleaved multichannel audio, used for the optional lookahead in the
/// multi-loop design (`Specs/TechnicalConcept.md` section 5.2).
///
/// The detectors tap the signal *before* this delay, so by the time a given sample reaches the gain
/// stage the control value derived from it is already in place — the "preview" the Jünger reference
/// describes. Both Bed and Dialogue are delayed by the same amount so the Mix output stays
/// sample-aligned; delaying only the ducked path would smear the very thing the delay is meant to
/// tighten.
///
/// Default length is zero, which is a genuine no-op: `process` returns immediately, nothing is
/// copied, and both wrappers keep reporting zero added latency. That matters because the GStreamer
/// element's zero-latency guarantee is a deliberate design property for OB-van use
/// (`Specs/TechnicalConcept.md` section 7), not an accident to be traded away silently.
pub struct DelayLine {
    channels: usize,
    /// Ring of `delay_frames * channels` samples. Empty when the delay is zero.
    buffer: Vec<f32>,
    /// Next frame index to read from / write to (they are the same position in a pure delay).
    position: usize,
    delay_frames: usize,
}

impl DelayLine {
    pub fn new(channels: u32) -> Self {
        Self {
            channels: channels as usize,
            buffer: Vec::new(),
            position: 0,
            delay_frames: 0,
        }
    }

    pub fn delay_frames(&self) -> usize {
        self.delay_frames
    }

    /// Resizes the delay. A no-op when the length is unchanged, so this is safe (and free) to call
    /// every block from a live parameter. An actual change clears the buffer rather than trying to
    /// preserve its contents: any resize is a discontinuity in the output regardless, and silence
    /// is a cleaner one than a reinterpreted ring.
    pub fn set_delay_frames(&mut self, delay_frames: usize) {
        if delay_frames == self.delay_frames {
            return;
        }
        self.delay_frames = delay_frames;
        self.buffer.clear();
        self.buffer.resize(delay_frames * self.channels, 0.0);
        self.position = 0;
    }

    /// Delays one chunk of interleaved audio in place: each frame is swapped with the one written
    /// `delay_frames` ago.
    pub fn process(&mut self, interleaved: &mut [f32]) {
        if self.delay_frames == 0 || self.channels == 0 || self.buffer.is_empty() {
            return;
        }
        for frame in interleaved.chunks_exact_mut(self.channels) {
            let base = self.position * self.channels;
            for (channel, sample) in frame.iter_mut().enumerate() {
                std::mem::swap(sample, &mut self.buffer[base + channel]);
            }
            self.position = (self.position + 1) % self.delay_frames;
        }
    }

    pub fn reset(&mut self) {
        self.buffer.iter_mut().for_each(|s| *s = 0.0);
        self.position = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_delay_is_a_pure_no_op() {
        let mut delay = DelayLine::new(2);
        let mut audio = vec![1.0, 2.0, 3.0, 4.0];
        delay.process(&mut audio);
        assert_eq!(audio, vec![1.0, 2.0, 3.0, 4.0]);
        assert_eq!(delay.delay_frames(), 0);
    }

    #[test]
    fn delays_by_exactly_the_requested_number_of_frames() {
        let mut delay = DelayLine::new(1);
        delay.set_delay_frames(3);

        // First three frames out are the pre-filled silence.
        let mut first = vec![1.0, 2.0, 3.0];
        delay.process(&mut first);
        assert_eq!(first, vec![0.0, 0.0, 0.0]);

        // Then the original samples emerge, in order.
        let mut second = vec![4.0, 5.0, 6.0];
        delay.process(&mut second);
        assert_eq!(second, vec![1.0, 2.0, 3.0]);
    }

    #[test]
    fn keeps_channels_independent() {
        let mut delay = DelayLine::new(2);
        delay.set_delay_frames(1);

        let mut first = vec![1.0, -1.0]; // one stereo frame
        delay.process(&mut first);
        assert_eq!(first, vec![0.0, 0.0]);

        let mut second = vec![2.0, -2.0];
        delay.process(&mut second);
        assert_eq!(second, vec![1.0, -1.0], "left and right must not cross over");
    }

    #[test]
    fn works_across_chunk_boundaries_of_differing_sizes() {
        let mut delay = DelayLine::new(1);
        delay.set_delay_frames(2);

        let mut out = Vec::new();
        for chunk in [vec![1.0], vec![2.0, 3.0], vec![4.0, 5.0, 6.0]] {
            let mut chunk = chunk;
            delay.process(&mut chunk);
            out.extend(chunk);
        }
        assert_eq!(out, vec![0.0, 0.0, 1.0, 2.0, 3.0, 4.0]);
    }

    #[test]
    fn resizing_to_the_same_length_does_not_disturb_the_contents() {
        let mut delay = DelayLine::new(1);
        delay.set_delay_frames(2);
        let mut primed = vec![7.0, 8.0];
        delay.process(&mut primed);

        delay.set_delay_frames(2); // no-op
        let mut next = vec![9.0, 10.0];
        delay.process(&mut next);
        assert_eq!(next, vec![7.0, 8.0], "a no-op resize must not clear the ring");
    }
}
