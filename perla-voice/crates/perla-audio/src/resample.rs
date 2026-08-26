//! Minimal linear-interpolation resampling. Voice audio at 24 kHz doesn't
//! need a polyphase filter; linear is what the latency budget wants.

/// Stateful streaming resampler (keeps the fractional position and the last
/// sample across calls so chunk boundaries don't click).
pub struct StreamResampler {
    ratio: f64,
    /// Position within the *input* stream, in samples, carried across calls.
    pos: f64,
    last: f32,
    have_last: bool,
}

impl StreamResampler {
    pub fn new(from_rate: u32, to_rate: u32) -> Self {
        Self {
            ratio: from_rate as f64 / to_rate as f64,
            pos: 0.0,
            last: 0.0,
            have_last: false,
        }
    }

    pub fn process(&mut self, input: &[f32]) -> Vec<f32> {
        if input.is_empty() {
            return Vec::new();
        }
        if (self.ratio - 1.0).abs() < f64::EPSILON {
            return input.to_vec();
        }
        // Virtual input: [last, input...] so interpolation can reach back
        // across the chunk boundary.
        let get = |idx: isize| -> f32 {
            if idx < 0 {
                if self.have_last {
                    self.last
                } else {
                    input[0]
                }
            } else {
                input[(idx as usize).min(input.len() - 1)]
            }
        };
        let mut out = Vec::with_capacity((input.len() as f64 / self.ratio) as usize + 2);
        // self.pos is relative to input[0]; negative means between last and input[0].
        let mut pos = self.pos - 1.0; // shift so integer part indexes the virtual stream
        while pos < (input.len() as f64 - 1.0) {
            let base = pos.floor();
            let frac = (pos - base) as f32;
            let a = get(base as isize);
            let b = get(base as isize + 1);
            out.push(a + (b - a) * frac);
            pos += self.ratio;
        }
        self.pos = pos - (input.len() as f64 - 1.0);
        self.last = input[input.len() - 1];
        self.have_last = true;
        out
    }
}

/// One-shot: PCM16 at `from_rate` → f32 at `to_rate`.
pub fn linear_i16_to_f32(input: &[i16], from_rate: u32, to_rate: u32) -> Vec<f32> {
    if input.is_empty() {
        return Vec::new();
    }
    let as_f32: Vec<f32> = input.iter().map(|s| *s as f32 / i16::MAX as f32).collect();
    if from_rate == to_rate {
        return as_f32;
    }
    let ratio = from_rate as f64 / to_rate as f64;
    let out_len = (input.len() as f64 / ratio).ceil() as usize;
    let mut out = Vec::with_capacity(out_len);
    let mut pos = 0.0f64;
    while pos < (as_f32.len() - 1) as f64 {
        let base = pos.floor() as usize;
        let frac = (pos - base as f64) as f32;
        let a = as_f32[base];
        let b = as_f32[(base + 1).min(as_f32.len() - 1)];
        out.push(a + (b - a) * frac);
        pos += ratio;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_passthrough() {
        let mut r = StreamResampler::new(24_000, 24_000);
        let input = vec![0.0, 0.5, 1.0];
        assert_eq!(r.process(&input), input);
    }

    #[test]
    fn downsample_halves_len_approximately() {
        let mut r = StreamResampler::new(48_000, 24_000);
        let input: Vec<f32> = (0..4800).map(|i| (i as f32 / 4800.0).sin()).collect();
        let out = r.process(&input);
        assert!((out.len() as i64 - 2400).abs() <= 2, "got {}", out.len());
    }

    #[test]
    fn upsample_roughly_doubles() {
        let out = linear_i16_to_f32(&vec![0i16; 2400], 24_000, 48_000);
        assert!((out.len() as i64 - 4800).abs() <= 4, "got {}", out.len());
    }
}
