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
    /// Saturating so an unvalidated size can never wrap; `Config::load`
    /// rejects out-of-range values with a real error message.
    pub fn max_font_px(&self) -> u32 {
        let pt = self
            .font
            .rsplit_once(' ')
            .and_then(|(_, size)| size.parse::<u32>().ok())
            .unwrap_or(120);
        pt.saturating_mul(4) / 3
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Net {
    pub osc_port: u16,
    pub tcp_port: u16,
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
    /// Invert fg/bg every 500 ms for 3 s whenever this message is cued.
    #[serde(default)]
    pub flash: bool,
}

impl Config {
    pub fn load(path: &Path) -> anyhow::Result<Config> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("reading config {}", path.display()))?;
        let config: Config =
            toml::from_str(&raw).with_context(|| format!("parsing config {}", path.display()))?;
        config
            .validate()
            .with_context(|| format!("invalid config {}", path.display()))?;
        Ok(config)
    }

    /// Reject configs the renderer can't do anything sensible with. Values
    /// that pass here are safe for the arithmetic in the fit computation.
    fn validate(&self) -> anyhow::Result<()> {
        let d = &self.display;
        anyhow::ensure!(
            (64..=7680).contains(&d.width) && (64..=4320).contains(&d.height),
            "display {}x{} is outside 64x64..7680x4320",
            d.width,
            d.height
        );
        anyhow::ensure!(
            (1..=240).contains(&d.fps),
            "display.fps {} is outside 1..240",
            d.fps
        );
        anyhow::ensure!(
            2 * u64::from(d.padding_x) < u64::from(d.width)
                && 2 * u64::from(d.padding_y) < u64::from(d.height),
            "padding {}x{} leaves no room inside {}x{}",
            d.padding_x,
            d.padding_y,
            d.width,
            d.height
        );
        anyhow::ensure!(
            (8..=2000).contains(&d.max_font_px()),
            "display.font size {:?} maps to {} px, outside 8..2000",
            d.font,
            d.max_font_px()
        );
        anyhow::ensure!(
            self.canned.contains_key(&self.defaults.boot_scene),
            "defaults.boot_scene {:?} is not a canned message",
            self.defaults.boot_scene
        );
        Ok(())
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

    fn base_config(display_overrides: &str) -> Config {
        toml::from_str(&format!(
            r##"
            [display]
            width = 1920
            height = 1080
            fps = 50
            font = "Inter Bold 120"
            padding_x = 96
            padding_y = 64
            {display_overrides}
            [net]
            osc_port = 9000
            tcp_port = 9001
            http_port = 8080
            [clock]
            timezone = "Europe/London"
            font = "Inter Semibold 40"
            position = "bottom-right"
            [defaults]
            bg = "000000"
            fg = "ffffff"
            boot_scene = "go"
            [canned.go]
            text = "GO"
            "##
        ))
        .unwrap()
    }

    #[test]
    fn boot_scene_must_be_canned() {
        let mut config = base_config("");
        config.defaults.boot_scene = "nope".into();
        let err = config.validate().unwrap_err().to_string();
        assert!(err.contains("not a canned message"), "{err}");
    }

    #[test]
    fn validation_rejects_padding_wider_than_the_frame() {
        let mut config = base_config("");
        config.display.padding_x = 1000;
        let err = config.validate().unwrap_err().to_string();
        assert!(err.contains("no room"), "{err}");
    }

    #[test]
    fn validation_rejects_silly_dimensions_fps_and_fonts() {
        let mut config = base_config("");
        config.display.width = 0;
        assert!(config.validate().is_err());

        let mut config = base_config("");
        config.display.fps = 0;
        assert!(config.validate().is_err());

        let mut config = base_config("");
        config.display.font = "Inter Bold 4000000000".into();
        assert!(config.validate().is_err());

        base_config("").validate().unwrap();
    }
}
