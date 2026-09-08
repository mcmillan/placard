use std::fmt;
use std::str::FromStr;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// A colour on the wire: `rrggbb`, case-insensitive, with a leading `#`
/// tolerated. No short form, no alpha. Serialises without the `#` — it needs
/// quoting in shells and comments in QLab OSC cues.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Rgb {
    pub const BLACK: Rgb = Rgb { r: 0, g: 0, b: 0 };

    /// Pack as `0xAARRGGBB`, the layout both `videotestsrc.foreground-color`
    /// and `textoverlay.color` use.
    pub fn to_argb(self, alpha: u8) -> u32 {
        (alpha as u32) << 24 | (self.r as u32) << 16 | (self.g as u32) << 8 | self.b as u32
    }
}

impl FromStr for Rgb {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let hex = Some(s.strip_prefix('#').unwrap_or(s))
            .filter(|h| h.len() == 6 && h.bytes().all(|b| b.is_ascii_hexdigit()))
            .ok_or_else(|| format!("bad colour {s:?}: expected rrggbb"))?;
        let parse = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).map_err(|e| e.to_string());
        Ok(Rgb {
            r: parse(0)?,
            g: parse(2)?,
            b: parse(4)?,
        })
    }
}

impl fmt::Display for Rgb {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:02x}{:02x}{:02x}", self.r, self.g, self.b)
    }
}

impl Serialize for Rgb {
    fn serialize<S: serde::Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        ser.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Rgb {
    fn deserialize<D: serde::Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        let s = String::deserialize(de)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

/// What is on screen. Serialises in the shape `/api/status` uses
/// (`{"kind": "text"|"countdown", ...}`); the derived `display` field in
/// status output and snapshot fixtures is ignored on the way in.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Content {
    Text {
        text: String,
    },
    Countdown {
        target: DateTime<Utc>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        label: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Scene {
    pub bg: Rgb,
    pub fg: Rgb,
    pub content: Content,
}

impl Scene {
    /// The `clear` scene: black background, no text. Colours reset so a later
    /// `show` without colours comes up white-on-black, not whatever preceded it.
    pub fn cleared(fg: Rgb) -> Scene {
        Scene {
            bg: Rgb::BLACK,
            fg,
            content: Content::Text {
                text: String::new(),
            },
        }
    }

    /// The string currently displayed for this scene's content.
    pub fn display_string(&self, now: DateTime<Utc>) -> String {
        match &self.content {
            Content::Text { text } => text.clone(),
            Content::Countdown { target, .. } => format_countdown(*target, now),
        }
    }
}

/// Countdown display: `M:SS` under an hour, `H:MM:SS` otherwise. Past the
/// target it continues with a leading minus (`-0:07`, `-1:02:15`) indefinitely.
pub fn format_countdown(target: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let secs = (target - now).num_seconds();
    let sign = if secs < 0 { "-" } else { "" };
    let s = secs.abs();
    if s < 3600 {
        format!("{sign}{}:{:02}", s / 60, s % 60)
    } else {
        format!("{sign}{}:{:02}:{:02}", s / 3600, (s % 3600) / 60, s % 60)
    }
}

/// Wall clock: `HH:MM:SS` in the configured timezone.
pub fn format_clock(now: DateTime<Utc>, tz: chrono_tz::Tz) -> String {
    now.with_timezone(&tz).format("%H:%M:%S").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeDelta;

    fn t0() -> DateTime<Utc> {
        "2026-09-08T18:30:00Z".parse().unwrap()
    }

    fn fmt_at(delta_secs: i64) -> String {
        // Positive delta: target is delta_secs in the future.
        format_countdown(t0() + TimeDelta::seconds(delta_secs), t0())
    }

    #[test]
    fn rgb_parses_bare_hex_and_tolerates_hash() {
        assert_eq!(
            "8a0000".parse::<Rgb>().unwrap(),
            Rgb {
                r: 0x8a,
                g: 0,
                b: 0
            }
        );
        assert_eq!(
            "#8a0000".parse::<Rgb>().unwrap(),
            Rgb {
                r: 0x8a,
                g: 0,
                b: 0
            }
        );
        assert_eq!(
            "FFffFF".parse::<Rgb>().unwrap(),
            Rgb {
                r: 255,
                g: 255,
                b: 255
            }
        );
    }

    #[test]
    fn rgb_rejects_bad_forms() {
        for bad in ["8a000", "8a00000", "#8a000", "8g0000", "", "#", "red"] {
            assert!(bad.parse::<Rgb>().is_err(), "{bad:?} should fail");
        }
    }

    #[test]
    fn rgb_argb_packing() {
        let c = Rgb {
            r: 0x8a,
            g: 0x12,
            b: 0x34,
        };
        assert_eq!(c.to_argb(0xff), 0xff8a1234);
        assert_eq!(c.to_argb(0x00), 0x008a1234);
    }

    #[test]
    fn rgb_display_roundtrip() {
        assert_eq!("8a0000".parse::<Rgb>().unwrap().to_string(), "8a0000");
        assert_eq!("#8a0000".parse::<Rgb>().unwrap().to_string(), "8a0000");
    }

    #[test]
    fn countdown_under_an_hour() {
        assert_eq!(fmt_at(5), "0:05");
        assert_eq!(fmt_at(59), "0:59");
        assert_eq!(fmt_at(60), "1:00");
        assert_eq!(fmt_at(3599), "59:59");
    }

    #[test]
    fn countdown_hour_and_up() {
        assert_eq!(fmt_at(3600), "1:00:00");
        assert_eq!(fmt_at(3735), "1:02:15");
        assert_eq!(fmt_at(36_000), "10:00:00");
    }

    #[test]
    fn countdown_zero_crossing_and_negative() {
        assert_eq!(fmt_at(0), "0:00");
        assert_eq!(fmt_at(-1), "-0:01");
        assert_eq!(fmt_at(-7), "-0:07");
        assert_eq!(fmt_at(-3600), "-1:00:00");
        assert_eq!(fmt_at(-3735), "-1:02:15");
    }

    #[test]
    fn clock_formats_in_timezone() {
        let now: DateTime<Utc> = "2026-01-15T12:00:00Z".parse().unwrap();
        assert_eq!(format_clock(now, chrono_tz::Tz::Europe__London), "12:00:00");
        // BST: UTC+1 in summer.
        let summer: DateTime<Utc> = "2026-07-15T12:00:00Z".parse().unwrap();
        assert_eq!(
            format_clock(summer, chrono_tz::Tz::Europe__London),
            "13:00:00"
        );
    }

    #[test]
    fn clock_across_dst_change() {
        // Europe/London: clocks go forward 2026-03-29 01:00 UTC -> 02:00 BST.
        let before: DateTime<Utc> = "2026-03-29T00:59:59Z".parse().unwrap();
        let after: DateTime<Utc> = "2026-03-29T01:00:00Z".parse().unwrap();
        assert_eq!(
            format_clock(before, chrono_tz::Tz::Europe__London),
            "00:59:59"
        );
        assert_eq!(
            format_clock(after, chrono_tz::Tz::Europe__London),
            "02:00:00"
        );
    }

    #[test]
    fn scene_serde_roundtrip() {
        let scene = Scene {
            bg: "#8a0000".parse().unwrap(),
            fg: "#ffffff".parse().unwrap(),
            content: Content::Countdown {
                target: t0(),
                label: Some("House opens in".into()),
            },
        };
        let json = serde_json::to_string(&scene).unwrap();
        assert_eq!(serde_json::from_str::<Scene>(&json).unwrap(), scene);
    }

    #[test]
    fn scene_deserialises_status_shape_ignoring_display() {
        let json = r##"{ "bg": "#8a0000", "fg": "#ffffff",
            "content": { "kind": "countdown", "target": "2026-09-08T18:30:00Z",
                         "label": "House opens in", "display": "-0:42" } }"##;
        let scene: Scene = serde_json::from_str(json).unwrap();
        assert_eq!(scene.display_string(t0() + TimeDelta::seconds(42)), "-0:42");
    }
}
