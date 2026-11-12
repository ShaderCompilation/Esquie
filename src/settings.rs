//! Tiny key=value settings file at ~/.config/esquie/settings.

use std::path::PathBuf;

#[derive(Clone)]
pub struct Settings {
    pub mic: Option<String>,
    pub enabled: bool,
    pub strength: f32, // 0..=100, 100 = no limit
    pub extra_cleanup: bool,
    pub gate: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            mic: None,
            enabled: true,
            strength: 100.0,
            extra_cleanup: true,
            gate: true,
        }
    }
}

fn path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("esquie").join("settings"))
}

impl Settings {
    pub fn load() -> Self {
        let mut s = Self::default();
        let Some(text) = path().and_then(|p| std::fs::read_to_string(p).ok()) else {
            return s;
        };
        for line in text.lines() {
            match line.split_once('=') {
                Some(("mic", v)) if !v.is_empty() => s.mic = Some(v.to_string()),
                Some(("enabled", v)) => s.enabled = v != "false",
                Some(("extra_cleanup", v)) => s.extra_cleanup = v != "false",
                Some(("gate", v)) => s.gate = v != "false",
                Some(("strength", v)) => {
                    s.strength = v.parse::<f32>().unwrap_or(100.0).clamp(0.0, 100.0)
                }
                _ => {}
            }
        }
        s
    }

    pub fn save(&self) {
        let Some(p) = path() else { return };
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let text = format!(
            "mic={}\nenabled={}\nstrength={}\nextra_cleanup={}\ngate={}\n",
            self.mic.as_deref().unwrap_or(""),
            self.enabled,
            self.strength,
            self.extra_cleanup,
            self.gate
        );
        let _ = std::fs::write(p, text);
    }
}

/// Strength slider (0..100) to DeepFilterNet's attenuation limit in dB.
/// 100% means "remove as much as possible" (no limit).
pub fn strength_to_db(strength: f32) -> f32 {
    if strength >= 99.5 {
        100.0
    } else {
        6.0 + strength * 0.5
    }
}
