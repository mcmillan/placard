use std::fmt;
use std::str::FromStr;
use std::time::Duration;

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

/// How a message flashes when it lands: not at all, for a fixed time, or
/// until the next command replaces it.
///
/// On the wire (JSON body and TOML config alike) this accepts, in order of
/// how often it's wanted:
///
/// | Value | Meaning |
/// |---|---|
/// | `true` | flash for the default 3 s |
/// | `false` | don't flash |
/// | `10` | flash for 10 seconds |
/// | `{ duration_s = 10 }` | the same, spelled out |
/// | `{ infinite = true }` | flash until the next command |
/// | `-1` | the same, in scalar form (OSC has no tables) |
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Flash {
    #[default]
    Off,
    For(Duration),
    /// Until another command replaces the scene. Unlike a timed flash this
    /// is part of the state, not a transient effect, so it survives a
    /// restart — an alarm that quietly stops alarming would be worse than
    /// one that keeps going.
    Forever,
}

/// What a bare `true` means.
pub const FLASH_DEFAULT: Duration = Duration::from_secs(3);
/// Longest explicit duration accepted; past this, say infinite and mean it.
const FLASH_MAX_SECS: f64 = 86_400.0;

impl Flash {
    /// Seconds → `Flash`, the mapping shared by every transport: zero is
    /// off, negative is forever, and anything unrepresentable is an error
    /// rather than a panic in `Duration::from_secs_f64`.
    pub fn from_secs(secs: f64) -> Result<Flash, String> {
        if secs.is_nan() {
            return Err("flash duration is not a number".into());
        }
        if secs < 0.0 {
            return Ok(Flash::Forever);
        }
        if secs == 0.0 {
            return Ok(Flash::Off);
        }
        if secs > FLASH_MAX_SECS {
            return Err(format!(
                "flash duration {secs}s is over the {FLASH_MAX_SECS}s maximum; \
                 use infinite for a flash with no end"
            ));
        }
        Ok(Flash::For(Duration::from_secs_f64(secs)))
    }
}

impl<'de> Deserialize<'de> for Flash {
    fn deserialize<D: serde::Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        de.deserialize_any(FlashVisitor)
    }
}

struct FlashVisitor;

impl<'de> serde::de::Visitor<'de> for FlashVisitor {
    type Value = Flash;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(
            "a boolean, a number of seconds (negative for no end), \
             or a table with duration_s or infinite",
        )
    }

    fn visit_bool<E: serde::de::Error>(self, v: bool) -> Result<Flash, E> {
        Ok(if v {
            Flash::For(FLASH_DEFAULT)
        } else {
            Flash::Off
        })
    }

    fn visit_i64<E: serde::de::Error>(self, v: i64) -> Result<Flash, E> {
        self.visit_f64(v as f64)
    }

    fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<Flash, E> {
        self.visit_f64(v as f64)
    }

    fn visit_f64<E: serde::de::Error>(self, v: f64) -> Result<Flash, E> {
        Flash::from_secs(v).map_err(E::custom)
    }

    /// Exactly one recognised key. Contradictions (`duration_s` *and*
    /// `infinite`) and `infinite: false` are rejected with a message that
    /// says what to write instead, rather than being silently resolved.
    fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<Flash, A::Error> {
        use serde::de::Error as _;
        let mut flash: Option<Flash> = None;
        while let Some(key) = map.next_key::<String>()? {
            let value = match key.as_str() {
                "duration_s" => {
                    Flash::from_secs(map.next_value::<Seconds>()?.0).map_err(A::Error::custom)?
                }
                "infinite" => {
                    if map.next_value::<bool>()? {
                        Flash::Forever
                    } else {
                        return Err(A::Error::custom(
                            "flash infinite = false is ambiguous; use flash = false to \
                             turn flashing off, or give duration_s",
                        ));
                    }
                }
                other => {
                    return Err(A::Error::custom(format!(
                        "unknown flash field {other:?}, expected duration_s or infinite"
                    )));
                }
            };
            if flash.replace(value).is_some() {
                return Err(A::Error::custom(
                    "flash takes duration_s or infinite, not both",
                ));
            }
        }
        flash.ok_or_else(|| A::Error::custom("flash table needs duration_s or infinite"))
    }
}

/// A number of seconds written as either an integer or a float — TOML and
/// JSON both spell `10` and `10.0` differently and both must work.
struct Seconds(f64);

impl<'de> Deserialize<'de> for Seconds {
    fn deserialize<D: serde::Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        struct V;
        impl serde::de::Visitor<'_> for V {
            type Value = Seconds;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a number of seconds")
            }
            fn visit_i64<E: serde::de::Error>(self, v: i64) -> Result<Seconds, E> {
                Ok(Seconds(v as f64))
            }
            fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<Seconds, E> {
                Ok(Seconds(v as f64))
            }
            fn visit_f64<E: serde::de::Error>(self, v: f64) -> Result<Seconds, E> {
                Ok(Seconds(v))
            }
        }
        de.deserialize_any(V)
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
///
/// The value shown is the *ceiling* of the remaining time, so each digit
/// change lands exactly on its second boundary: a 5-second countdown shows
/// `0:05` for a full second (truncation would flip it to `0:04` almost
/// immediately), `0:00` lasts exactly one second, and `-0:01` appears
/// exactly one second past the target.
pub fn format_countdown(target: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let ms = (target - now).num_milliseconds();
    let secs = (ms + 999).div_euclid(1000);
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

    fn fmt_at_ms(delta_ms: i64) -> String {
        format_countdown(t0() + TimeDelta::milliseconds(delta_ms), t0())
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
    fn countdown_displays_the_ceiling_of_remaining_time() {
        // A freshly cued 5-second countdown must actually show 0:05.
        assert_eq!(fmt_at_ms(5_000), "0:05");
        assert_eq!(fmt_at_ms(4_999), "0:05");
        assert_eq!(fmt_at_ms(4_000), "0:04");
        assert_eq!(fmt_at_ms(1), "0:01");
        // 0:00 spans exactly [0, -1s): one second, like every other value.
        assert_eq!(fmt_at_ms(0), "0:00");
        assert_eq!(fmt_at_ms(-999), "0:00");
        assert_eq!(fmt_at_ms(-1_000), "-0:01");
        assert_eq!(fmt_at_ms(-1_001), "-0:01");
        assert_eq!(fmt_at_ms(-2_000), "-0:02");
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

    fn flash_json(src: &str) -> Result<Flash, String> {
        serde_json::from_str::<Flash>(src).map_err(|e| e.to_string())
    }

    /// The same value written as TOML, since config and commands share the
    /// type and TOML spells integers differently from JSON.
    fn flash_toml(src: &str) -> Result<Flash, String> {
        #[derive(Deserialize)]
        struct Holder {
            flash: Flash,
        }
        toml::from_str::<Holder>(&format!("flash = {src}"))
            .map(|h| h.flash)
            .map_err(|e| e.to_string())
    }

    #[test]
    fn flash_accepts_bools_numbers_and_tables() {
        for parse in [flash_json, flash_toml] {
            assert_eq!(parse("true").unwrap(), Flash::For(FLASH_DEFAULT));
            assert_eq!(parse("false").unwrap(), Flash::Off);
            assert_eq!(parse("0").unwrap(), Flash::Off);
            assert_eq!(parse("10").unwrap(), Flash::For(Duration::from_secs(10)));
            assert_eq!(
                parse("0.5").unwrap(),
                Flash::For(Duration::from_millis(500))
            );
            assert_eq!(parse("-1").unwrap(), Flash::Forever);
        }
        // Tables: JSON objects and TOML inline tables.
        assert_eq!(
            flash_json(r#"{"duration_s": 10}"#).unwrap(),
            Flash::For(Duration::from_secs(10))
        );
        assert_eq!(
            flash_toml("{ duration_s = 10 }").unwrap(),
            Flash::For(Duration::from_secs(10))
        );
        assert_eq!(flash_json(r#"{"infinite": true}"#).unwrap(), Flash::Forever);
        assert_eq!(flash_toml("{ infinite = true }").unwrap(), Flash::Forever);
    }

    #[test]
    fn flash_rejects_ambiguity_with_a_useful_message() {
        // Contradictions and half-answers, not silently resolved.
        let err = flash_json(r#"{"duration_s": 10, "infinite": true}"#).unwrap_err();
        assert!(err.contains("not both"), "{err}");

        let err = flash_json(r#"{"infinite": false}"#).unwrap_err();
        assert!(err.contains("ambiguous"), "{err}");

        let err = flash_json("{}").unwrap_err();
        assert!(err.contains("needs duration_s or infinite"), "{err}");

        let err = flash_json(r#"{"duraton_s": 10}"#).unwrap_err();
        assert!(err.contains("unknown flash field"), "{err}");

        // Over the cap: say infinite and mean it.
        let err = flash_json("999999999").unwrap_err();
        assert!(err.contains("maximum"), "{err}");

        // Nothing here may panic Duration::from_secs_f64.
        assert!(flash_json(r#""nope""#).is_err());
        assert!(flash_json("[1]").is_err());
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
