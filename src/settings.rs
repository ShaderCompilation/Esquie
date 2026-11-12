//! Tiny key=value settings file at ~/.config/esquie/settings.

use std::path::PathBuf;

#[derive(Clone)]
pub struct Settings {
    pub mic: Option<String>,
    pub enabled: bool,
    pub strength: f32, // 0..=100, 100 = no limit
    pub extra_cleanup: bool,
    pub gate: bool,
    pub setup_complete: bool,
    pub install_prompt: InstallPrompt,
    pub install_directory: Option<PathBuf>,
    // Persistent handoff target; never infer it from the original download later.
    pub installed_path: Option<PathBuf>,
    pub automatic_checks: bool,
    pub last_check_attempt: Option<u64>,
    pub skipped_version: Option<String>,
    // Restored window size (width, height) in logical pixels.
    pub window_size: Option<(u32, u32)>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum InstallPrompt {
    #[default]
    Ask,
    Later,
    Never,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            mic: None,
            enabled: true,
            strength: 100.0,
            extra_cleanup: true,
            gate: true,
            setup_complete: false,
            install_prompt: InstallPrompt::Ask,
            install_directory: None,
            installed_path: None,
            automatic_checks: true,
            last_check_attempt: None,
            skipped_version: None,
            window_size: None,
        }
    }
}

fn path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("esquie").join("settings"))
}

impl Settings {
    pub fn load() -> Self {
        let Some(text) = path().and_then(|p| std::fs::read_to_string(p).ok()) else {
            return Self::default();
        };
        Self::parse(&text)
    }

    fn parse(text: &str) -> Self {
        // Existing installations have already chosen their audio preferences.
        // Only a missing settings file starts the new-user setup flow.
        let mut s = Self {
            setup_complete: true,
            ..Self::default()
        };
        for line in text.lines() {
            match line.split_once('=') {
                Some(("mic", v)) if !v.is_empty() => s.mic = Some(v.to_string()),
                Some(("enabled", v)) => s.enabled = v != "false",
                Some(("extra_cleanup", v)) => s.extra_cleanup = v != "false",
                Some(("gate", v)) => s.gate = v != "false",
                Some(("setup_complete", v)) => s.setup_complete = v != "false",
                Some(("strength", v)) => {
                    s.strength = v
                        .parse::<f32>()
                        .ok()
                        .filter(|v| v.is_finite())
                        .unwrap_or(100.0)
                        .clamp(0.0, 100.0)
                }
                Some(("install_prompt", "never")) => s.install_prompt = InstallPrompt::Never,
                Some(("install_prompt", "later")) => s.install_prompt = InstallPrompt::Later,
                Some(("install_directory", v)) => {
                    s.install_directory = serde_json::from_str::<String>(v).ok().map(PathBuf::from)
                }
                Some(("installed_path", v)) => {
                    s.installed_path = serde_json::from_str::<String>(v).ok().map(PathBuf::from)
                }
                Some(("automatic_checks", v)) => s.automatic_checks = v != "false",
                Some(("last_check_attempt", v)) => s.last_check_attempt = v.parse().ok(),
                Some(("skipped_version", v)) => s.skipped_version = serde_json::from_str(v).ok(),
                Some(("window_size", v)) => {
                    // "width height" in logical pixels, clamped to a sane range.
                    s.window_size = v
                        .split_whitespace()
                        .map(|t| t.parse::<u32>().ok())
                        .collect::<Option<Vec<_>>>()
                        .and_then(|dims| match dims.as_slice() {
                            [w, h] => Some(((*w).clamp(480, 1000), (*h).clamp(480, 1400))),
                            _ => None,
                        })
                }
                _ => {}
            }
        }
        s
    }

    pub fn save(&self) {
        if let Err(e) = self.try_save() {
            tracing::warn!("Could not save settings: {e}");
        }
    }

    pub fn try_save(&self) -> std::io::Result<()> {
        let p =
            path().ok_or_else(|| std::io::Error::other("No settings directory is available"))?;
        crate::file_ops::write(&p, self.serialize().as_bytes(), 0o600)
    }

    fn serialize(&self) -> String {
        let mut text = format!(
            "mic={}\nenabled={}\nstrength={}\nextra_cleanup={}\ngate={}\nsetup_complete={}\n",
            self.mic.as_deref().unwrap_or(""),
            self.enabled,
            self.strength,
            self.extra_cleanup,
            self.gate,
            self.setup_complete
        );
        text.push_str(&format!(
            "install_prompt={}\nautomatic_checks={}\n",
            match self.install_prompt {
                InstallPrompt::Ask => "ask",
                InstallPrompt::Later => "later",
                InstallPrompt::Never => "never",
            },
            self.automatic_checks
        ));
        for (key, value) in [
            (
                "install_directory",
                self.install_directory.as_ref().and_then(|p| p.to_str()),
            ),
            (
                "installed_path",
                self.installed_path.as_ref().and_then(|p| p.to_str()),
            ),
            ("skipped_version", self.skipped_version.as_deref()),
        ] {
            if let Some(value) = value {
                text.push_str(&format!(
                    "{key}={}\n",
                    serde_json::to_string(value).unwrap()
                ));
            }
        }
        if let Some(attempt) = self.last_check_attempt {
            text.push_str(&format!("last_check_attempt={attempt}\n"));
        }
        if let Some((w, h)) = self.window_size {
            text.push_str(&format!("window_size={w} {h}\n"));
        }
        text
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn old_settings_preserve_audio_and_default_new_preferences() {
        let s = Settings::parse(
            "mic=alsa_input.test\nenabled=false\nstrength=37\nextra_cleanup=false\ngate=false\nfuture_key=ignored\n",
        );
        assert_eq!(s.mic.as_deref(), Some("alsa_input.test"));
        assert!(!s.enabled && !s.extra_cleanup && !s.gate);
        assert_eq!(s.strength, 37.0);
        assert_eq!(s.install_prompt, InstallPrompt::Ask);
        assert!(s.automatic_checks);
        assert!(s.setup_complete);
        assert!(s.install_directory.is_none() && s.last_check_attempt.is_none());
    }

    #[test]
    fn new_preferences_round_trip_without_injecting_settings() {
        let s = Settings {
            install_prompt: InstallPrompt::Never,
            install_directory: Some(PathBuf::from("/home/My Apps/100%\n=odd\\path")),
            installed_path: Some(PathBuf::from("/home/My Apps/esquie")),
            automatic_checks: false,
            last_check_attempt: Some(1790000000),
            skipped_version: Some("1.2.3".into()),
            window_size: Some((540, 780)),
            ..Settings::default()
        };
        let loaded = Settings::parse(&s.serialize());
        assert_eq!(loaded.install_prompt, s.install_prompt);
        assert_eq!(loaded.install_directory, s.install_directory);
        assert_eq!(loaded.installed_path, s.installed_path);
        assert_eq!(loaded.automatic_checks, s.automatic_checks);
        assert_eq!(loaded.last_check_attempt, s.last_check_attempt);
        assert_eq!(loaded.skipped_version, s.skipped_version);
        assert_eq!(loaded.window_size, s.window_size);
        assert_eq!(loaded.setup_complete, s.setup_complete);
        assert!(loaded.enabled);
        assert_eq!(
            Settings::parse("install_prompt=later").install_prompt,
            InstallPrompt::Later
        );
    }

    #[test]
    fn malformed_new_fields_do_not_break_old_settings() {
        let s = Settings::parse(
            "strength=NaN\nlast_check_attempt=bad\ninstall_directory=bad\nskipped_version=bad\ninstall_prompt=bad\nwindow_size=bad\nwindow_size=100\nwindow_size=1 2 3\nmic=original\n",
        );
        assert_eq!(s.strength, 100.0);
        assert_eq!(s.mic.as_deref(), Some("original"));
        assert_eq!(s.install_prompt, InstallPrompt::Ask);
        assert!(
            s.install_directory.is_none()
                && s.last_check_attempt.is_none()
                && s.skipped_version.is_none()
                && s.window_size.is_none()
        );
    }

    #[test]
    fn window_size_is_clamped_to_a_sane_range() {
        let s = Settings::parse("window_size=10 9999\n");
        assert_eq!(s.window_size, Some((480, 1400)));
        assert_eq!(
            Settings::parse("window_size=1000 400").window_size,
            Some((1000, 480))
        );
        assert_eq!(
            Settings::parse("window_size=420 740").window_size,
            Some((480, 740))
        );
    }

    #[test]
    fn setup_progress_survives_other_preference_saves() {
        let fresh = Settings::default();
        assert!(!fresh.setup_complete);
        assert!(!Settings::parse(&fresh.serialize()).setup_complete);
        let complete = Settings {
            setup_complete: true,
            ..fresh
        };
        assert!(Settings::parse(&complete.serialize()).setup_complete);
    }
}
