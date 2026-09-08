use std::collections::BTreeMap;
use std::path::Path;

use anyhow::Context as _;
use serde::Deserialize;

use crate::scene::Rgb;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub display: Display,
    pub net: Net,
    pub clock: Clock,
    pub defaults: Defaults,
    #[serde(default)]
    pub canned: BTreeMap<String, Canned>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Display {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub font: String,
    pub padding_x: u32,
    pub padding_y: u32,
}

impl Display {
    /// `font` is a Pango-style description: family, optionally a trailing
    /// point size which acts as the *maximum* — text auto-shrinks below it to
    /// fit (`textoverlay` itself culls over-tall layouts, so sizing is ours).
    pub fn font_family(&self) -> &str {
        match self.font.rsplit_once(' ') {
            Some((family, size)) if size.parse::<u32>().is_ok() => family,
            _ => &self.font,
        }
    }

    /// Maximum text size in pixels (points × 4/3), default 120 pt.
    pub fn max_font_px(&self) -> u32 {
        let pt = self
            .font
            .rsplit_once(' ')
            .and_then(|(_, size)| size.parse::<u32>().ok())
            .unwrap_or(120);
        pt * 4 / 3
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Net {
    pub osc_port: u16,
    // Read by the TCP/HTTP listeners (M2); allow until they land.
    #[allow(dead_code)]
    pub tcp_port: u16,
    #[allow(dead_code)]
    pub http_port: u16,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Clock {
    pub timezone: chrono_tz::Tz,
    pub font: String,
    pub position: ClockPosition,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ClockPosition {
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Defaults {
    pub bg: Rgb,
    pub fg: Rgb,
    pub boot_scene: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Canned {
    pub text: String,
    pub bg: Option<Rgb>,
    pub fg: Option<Rgb>,
}

impl Config {
    pub fn load(path: &Path) -> anyhow::Result<Config> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("reading config {}", path.display()))?;
        let config: Config =
            toml::from_str(&raw).with_context(|| format!("parsing config {}", path.display()))?;
        if !config.canned.contains_key(&config.defaults.boot_scene) {
            anyhow::bail!(
                "defaults.boot_scene {:?} is not a canned message",
                config.defaults.boot_scene
            );
        }
        Ok(config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn example_config_parses() {
        let config = Config::load(Path::new("packaging/config.toml.example")).unwrap();
        assert_eq!(config.display.fps, 50);
        assert_eq!(config.net.osc_port, 9000);
        assert_eq!(config.clock.timezone, chrono_tz::Tz::Europe__London);
        assert_eq!(config.canned.len(), 7);
        assert_eq!(config.canned["go"].text, "GO");
        assert_eq!(config.defaults.boot_scene, "house_closed");
    }

    #[test]
    fn boot_scene_must_be_canned() {
        let err = toml::from_str::<Config>(
            r##"
            [display]
            width = 1920
            height = 1080
            fps = 50
            font = "Inter Bold"
            padding_x = 96
            padding_y = 64
            [net]
            osc_port = 9000
            tcp_port = 9001
            http_port = 8080
            [clock]
            timezone = "Europe/London"
            font = "Inter Semibold 40"
            position = "bottom-right"
            [defaults]
            bg = "#000000"
            fg = "#ffffff"
            boot_scene = "nope"
            "##,
        )
        .map_err(|e| e.to_string())
        .and_then(|c| {
            if c.canned.contains_key(&c.defaults.boot_scene) {
                Ok(())
            } else {
                Err("missing boot scene".into())
            }
        })
        .unwrap_err();
        assert!(err.contains("missing boot scene"));
    }
}
