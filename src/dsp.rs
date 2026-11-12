//! Small DSP pieces around DeepFilterNet: a high-pass filter before it and a
//! speech gate after it, plus the developer-tuned constants for both.

use std::sync::OnceLock;

/// Values tuned by the developer, not exposed in the UI. Each can be
/// overridden with an environment variable for listening tests, e.g.
/// `ESQUIE_GATE_OPEN_DB=-2 cargo run --release --locked`.
#[derive(Debug, Clone, Copy)]
pub struct Tuning {
    /// High-pass cutoff before the model (0 disables it).
    pub hpf_hz: f32,
    /// DeepFilterNet: below this local SNR a frame is treated as pure noise.
    pub min_db: f32,
    /// DeepFilterNet: above this local SNR a frame is left untouched.
    pub max_erb_db: f32,
    /// DeepFilterNet: above this local SNR the second (deep filter) stage is skipped.
    pub max_df_db: f32,
    /// Post-filter strength, used when "Extra cleanup" is on.
    pub pf_beta: f32,
    /// Gate opens when the model's local SNR goes above this.
    pub gate_open_db: f32,
    /// Frames in a row that must look like speech before the gate opens, so
    /// single-frame bursts (clicks, taps) don't open it.
    pub gate_confirm_frames: f32,
    /// How long the gate stays open after speech stops.
    pub gate_hold_ms: f32,
    /// Fade-in / fade-out times.
    pub gate_attack_ms: f32,
    pub gate_release_ms: f32,
    /// How far down the gate fades when closed.
    pub gate_floor_db: f32,
}

impl Default for Tuning {
    fn default() -> Self {
        Self {
            hpf_hz: 80.0,
            min_db: -10.0,
            max_erb_db: 30.0,
            max_df_db: 20.0,
            pf_beta: 0.02,
            gate_open_db: 0.0,
            gate_confirm_frames: 2.0,
            gate_hold_ms: 250.0,
            gate_attack_ms: 5.0,
            gate_release_ms: 120.0,
            gate_floor_db: -50.0,
        }
    }
}

impl Tuning {
    /// Defaults with any `ESQUIE_*` environment overrides applied.
    pub fn get() -> &'static Tuning {
        static T: OnceLock<Tuning> = OnceLock::new();
        T.get_or_init(|| {
            let mut t = Tuning::default();
            let fields: [(&str, &mut f32); 11] = [
                ("ESQUIE_HPF_HZ", &mut t.hpf_hz),
                ("ESQUIE_MIN_DB", &mut t.min_db),
                ("ESQUIE_MAX_ERB_DB", &mut t.max_erb_db),
                ("ESQUIE_MAX_DF_DB", &mut t.max_df_db),
                ("ESQUIE_PF_BETA", &mut t.pf_beta),
                ("ESQUIE_GATE_OPEN_DB", &mut t.gate_open_db),
                ("ESQUIE_GATE_CONFIRM_FRAMES", &mut t.gate_confirm_frames),
                ("ESQUIE_GATE_HOLD_MS", &mut t.gate_hold_ms),
                ("ESQUIE_GATE_ATTACK_MS", &mut t.gate_attack_ms),
                ("ESQUIE_GATE_RELEASE_MS", &mut t.gate_release_ms),
                ("ESQUIE_GATE_FLOOR_DB", &mut t.gate_floor_db),
            ];
            for (key, field) in fields {
                if let Some(v) = std::env::var(key).ok().and_then(|v| v.trim().parse::<f32>().ok()) {
                    *field = v;
                }
            }
            t.pf_beta = t.pf_beta.max(0.0);
            t
        })
    }
}

/// 2nd-order Butterworth high-pass (RBJ cookbook biquad).
pub struct HighPass {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
    x1: f32,
    x2: f32,
    y1: f32,
    y2: f32,
    bypass: bool,
}

impl HighPass {
    pub fn new(cutoff_hz: f32, sample_rate: f32) -> Self {
        let bypass = cutoff_hz <= 0.0;
        let w0 = 2.0 * std::f32::consts::PI * cutoff_hz.max(1.0) / sample_rate;
        let (sin, cos) = w0.sin_cos();
        let alpha = sin / (2.0 * std::f32::consts::FRAC_1_SQRT_2);
        let a0 = 1.0 + alpha;
        Self {
            b0: (1.0 + cos) / 2.0 / a0,
            b1: -(1.0 + cos) / a0,
            b2: (1.0 + cos) / 2.0 / a0,
            a1: -2.0 * cos / a0,
            a2: (1.0 - alpha) / a0,
            x1: 0.0,
            x2: 0.0,
            y1: 0.0,
            y2: 0.0,
            bypass,
        }
    }

    pub fn process(&mut self, buf: &mut [f32]) {
        if self.bypass {
            return;
        }
        for s in buf {
            let x = *s;
            let y = self.b0 * x + self.b1 * self.x1 + self.b2 * self.x2 - self.a1 * self.y1 - self.a2 * self.y2;
            self.x2 = self.x1;
            self.x1 = x;
            self.y2 = self.y1;
            self.y1 = y;
            *s = y;
        }
    }

    pub fn reset(&mut self) {
        self.x1 = 0.0;
        self.x2 = 0.0;
        self.y1 = 0.0;
        self.y2 = 0.0;
    }
}

/// Fades the cleaned output down between words, driven by DeepFilterNet's
/// per-frame speech estimate (local SNR). The model's estimate runs slightly
/// ahead of its output (lookahead), so word onsets are not clipped.
pub struct Gate {
    open_db: f32,
    confirm: u32,
    hold_frames: u32,
    attack: f32,
    release: f32,
    floor: f32,
    streak: u32,
    held: u32,
    gain: f32,
}

impl Gate {
    pub fn new(t: &Tuning, sample_rate: f32, hop: usize) -> Self {
        let coef = |ms: f32| {
            let n = ms.max(0.1) * 1e-3 * sample_rate;
            1.0 - (-1.0 / n).exp()
        };
        let frame_ms = hop as f32 * 1000.0 / sample_rate;
        Self {
            open_db: t.gate_open_db,
            confirm: t.gate_confirm_frames.round().max(1.0) as u32,
            hold_frames: (t.gate_hold_ms / frame_ms).round().max(0.0) as u32,
            attack: coef(t.gate_attack_ms),
            release: coef(t.gate_release_ms),
            floor: 10f32.powf(t.gate_floor_db.min(0.0) / 20.0),
            streak: 0,
            held: 0,
            gain: 1.0,
        }
    }

    /// Apply the gate to one model output frame with that frame's local SNR.
    pub fn process(&mut self, frame: &mut [f32], lsnr: f32) {
        let speech = lsnr > self.open_db;
        self.streak = if speech { self.streak + 1 } else { 0 };
        // Once open, any speech-like frame keeps it open; opening needs a streak.
        if speech && (self.held > 0 || self.streak >= self.confirm) {
            self.held = self.hold_frames.max(1);
        } else if self.held > 0 {
            self.held -= 1;
        }
        let target = if self.held > 0 { 1.0 } else { self.floor };
        let coef = if target > self.gain { self.attack } else { self.release };
        for s in frame {
            self.gain += (target - self.gain) * coef;
            *s *= self.gain;
        }
    }

    /// Fully open, e.g. after being switched off so it doesn't fade back in.
    pub fn reset(&mut self) {
        self.streak = 0;
        self.held = 0;
        self.gain = 1.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rms(x: &[f32]) -> f32 {
        (x.iter().map(|v| v * v).sum::<f32>() / x.len() as f32).sqrt()
    }

    fn sine(hz: f32, n: usize) -> Vec<f32> {
        (0..n).map(|i| (2.0 * std::f32::consts::PI * hz * i as f32 / 48_000.0).sin()).collect()
    }

    #[test]
    fn high_pass_cuts_rumble_keeps_voice() {
        let mut low = sine(30.0, 48_000);
        let mut voice = sine(300.0, 48_000);
        HighPass::new(80.0, 48_000.0).process(&mut low);
        HighPass::new(80.0, 48_000.0).process(&mut voice);
        let tail = 24_000..;
        // 2nd order at 80 Hz: 30 Hz is down ~17 dB.
        assert!(rms(&low[tail.clone()]) < 0.2 * std::f32::consts::FRAC_1_SQRT_2, "30 Hz not cut");
        assert!(rms(&voice[tail]) > 0.95 * std::f32::consts::FRAC_1_SQRT_2, "300 Hz cut");
    }

    #[test]
    fn gate_holds_then_closes() {
        let t = Tuning::default();
        let mut g = Gate::new(&t, 48_000.0, 480);
        let mut frame = vec![1.0; 480];
        g.process(&mut frame, 20.0); // speech
        g.process(&mut frame, 20.0);
        // Within the hold time it stays open.
        for _ in 0..10 {
            frame.fill(1.0);
            g.process(&mut frame, -15.0);
        }
        assert!(frame[479] > 0.99);
        // Well after hold + release it is down at the floor.
        for _ in 0..100 {
            frame.fill(1.0);
            g.process(&mut frame, -15.0);
        }
        assert!(frame[479] < 0.01);
        // A single speech-like frame (a click) doesn't open it.
        frame.fill(1.0);
        g.process(&mut frame, 20.0);
        assert!(frame[479] < 0.01);
    }
}
