//! Cheap token estimates, corrected with the provider's reported usage.

/// Characters per token used for raw estimates.
pub const CHARS_PER_TOKEN: f64 = 3.5;

/// Raw token estimate for `chars` characters.
pub fn estimate_tokens(chars: usize) -> u64 {
    (chars as f64 / CHARS_PER_TOKEN).ceil() as u64
}

/// Learns the ratio between real prompt tokens and our raw estimate, so budgets
/// track the actual tokenizer after the first model call.
#[derive(Debug, Clone, Copy)]
pub struct TokenCalibrator {
    ratio: f64,
    samples: u32,
}

impl Default for TokenCalibrator {
    fn default() -> Self {
        Self {
            ratio: 1.0,
            samples: 0,
        }
    }
}

impl TokenCalibrator {
    /// Records one observation: we estimated `estimated` and the provider reported `actual`.
    pub fn observe(&mut self, estimated: u64, actual: u64) {
        if estimated == 0 || actual == 0 {
            return;
        }
        let r = (actual as f64 / estimated as f64).clamp(0.25, 4.0);
        // Exponential moving average; the first sample replaces the prior.
        self.ratio = if self.samples == 0 {
            r
        } else {
            0.7 * self.ratio + 0.3 * r
        };
        self.samples += 1;
    }

    pub fn ratio(&self) -> f64 {
        self.ratio
    }

    pub fn calibrate(&self, raw: u64) -> u64 {
        (raw as f64 * self.ratio).ceil() as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn estimate_rounds_up() {
        assert_eq!(estimate_tokens(0), 0);
        assert_eq!(estimate_tokens(1), 1);
        assert_eq!(estimate_tokens(7), 2);
    }

    #[test]
    fn calibrator_tracks_ratio() {
        let mut c = TokenCalibrator::default();
        assert_eq!(c.calibrate(100), 100);
        c.observe(100, 150);
        assert!((c.ratio() - 1.5).abs() < 1e-9);
        c.observe(100, 150);
        assert!((c.ratio() - 1.5).abs() < 1e-9);
        c.observe(0, 10);
        assert!((c.ratio() - 1.5).abs() < 1e-9);
    }
}
