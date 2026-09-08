use chrono::{DateTime, Utc};
use rosc::{OscMessage, OscType};
use serde::Deserialize;

use crate::scene::{FLASH_DEFAULT, Flash, Rgb};

/// The one command set all three transports (OSC, TCP, HTTP) normalise to.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case", deny_unknown_fields)]
pub enum Command {
    Show {
        text: String,
        bg: Option<Rgb>,
        fg: Option<Rgb>,
        #[serde(default)]
        flash: Flash,
    },
    Canned {
        id: String,
        bg: Option<Rgb>,
        fg: Option<Rgb>,
        /// None defers to the canned message's own `flash` config.
        flash: Option<Flash>,
    },
    Colour {
        bg: Option<Rgb>,
        fg: Option<Rgb>,
    },
    CountdownTo {
        target: DateTime<Utc>,
        label: Option<String>,
        bg: Option<Rgb>,
        fg: Option<Rgb>,
    },
    CountdownSecs {
        secs: u32,
        label: Option<String>,
        bg: Option<Rgb>,
        fg: Option<Rgb>,
    },
    Clear,
    Status,
    History,
}

#[derive(Debug, Clone, thiserror::Error, PartialEq)]
pub enum CommandError {
    #[error("unknown address {0}")]
    UnknownAddress(String),
    #[error("{addr}: expected args {expected}")]
    WrongArgs {
        addr: String,
        expected: &'static str,
    },
    #[error("{0}")]
    BadColour(String),
    #[error("bad timestamp {0:?}: expected ISO 8601, e.g. 2026-09-08T19:30:00Z")]
    BadTimestamp(String),
    #[error("unknown canned id {0:?}")]
    UnknownCanned(String),
    #[error("{0}")]
    BadFlash(String),
    #[error("invalid JSON: {0}")]
    BadJson(String),
    #[error("line exceeds 64 KiB")]
    LineTooLong,
    #[error("invalid UTF-8")]
    InvalidUtf8,
    #[error("{0} is not available over OSC")]
    QueryNotSupported(&'static str),
    #[error("shutting down")]
    ShuttingDown,
}

pub fn parse_json(line: &str) -> Result<Command, CommandError> {
    serde_json::from_str(line).map_err(|e| CommandError::BadJson(e.to_string()))
}

/// Map an OSC message onto `Command`. The address table is documented in
/// docs/protocol.md.
pub fn parse_osc(msg: &OscMessage) -> Result<Command, CommandError> {
    let args = OscArgs::new(&msg.addr, &msg.args);
    match msg.addr.as_str() {
        "/placard/show" => {
            let (text, bg, fg, flash) =
                args.string_then_colours_and_flash("s text [s bg] [s fg] [i flash | f seconds]")?;
            Ok(Command::Show {
                text,
                bg,
                fg,
                flash: flash.unwrap_or_default(),
            })
        }
        "/placard/canned" => {
            let (id, bg, fg, flash) =
                args.string_then_colours_and_flash("s id [s bg] [s fg] [i flash | f seconds]")?;
            Ok(Command::Canned { id, bg, fg, flash })
        }
        "/placard/colour" => {
            let expected = "s bg [s fg]";
            let bg = parse_colour(args.string(0, expected)?)?;
            let fg = args
                .optional_string(1, expected)?
                .map(parse_colour)
                .transpose()?;
            args.no_more(2, expected)?;
            Ok(Command::Colour { bg: Some(bg), fg })
        }
        "/placard/countdown/to" => {
            let expected = "s iso8601 [s label]";
            let raw = args.string(0, expected)?;
            let target = raw
                .parse::<DateTime<Utc>>()
                .map_err(|_| CommandError::BadTimestamp(raw.clone()))?;
            let label = args.optional_string(1, expected)?;
            args.no_more(2, expected)?;
            Ok(Command::CountdownTo {
                target,
                label,
                bg: None,
                fg: None,
            })
        }
        "/placard/countdown/secs" => {
            let expected = "i secs [s label]";
            let secs = args.int(0, expected)?;
            let secs = u32::try_from(secs).map_err(|_| CommandError::WrongArgs {
                addr: msg.addr.clone(),
                expected,
            })?;
            let label = args.optional_string(1, expected)?;
            args.no_more(2, expected)?;
            Ok(Command::CountdownSecs {
                secs,
                label,
                bg: None,
                fg: None,
            })
        }
        "/placard/clear" => {
            args.no_more(0, "no args")?;
            Ok(Command::Clear)
        }
        "/placard/status" => Err(CommandError::QueryNotSupported("status")),
        "/placard/history" => Err(CommandError::QueryNotSupported("history")),
        other => Err(CommandError::UnknownAddress(other.to_string())),
    }
}

fn parse_colour(s: String) -> Result<Rgb, CommandError> {
    s.parse().map_err(CommandError::BadColour)
}

struct OscArgs<'a> {
    addr: &'a str,
    args: &'a [OscType],
}

impl<'a> OscArgs<'a> {
    fn new(addr: &'a str, args: &'a [OscType]) -> Self {
        OscArgs { addr, args }
    }

    fn wrong(&self, expected: &'static str) -> CommandError {
        CommandError::WrongArgs {
            addr: self.addr.to_string(),
            expected,
        }
    }

    fn string(&self, i: usize, expected: &'static str) -> Result<String, CommandError> {
        match self.args.get(i) {
            Some(OscType::String(s)) => Ok(s.clone()),
            _ => Err(self.wrong(expected)),
        }
    }

    fn optional_string(
        &self,
        i: usize,
        expected: &'static str,
    ) -> Result<Option<String>, CommandError> {
        match self.args.get(i) {
            None => Ok(None),
            Some(OscType::String(s)) => Ok(Some(s.clone())),
            _ => Err(self.wrong(expected)),
        }
    }

    fn int(&self, i: usize, expected: &'static str) -> Result<i32, CommandError> {
        match self.args.get(i) {
            Some(OscType::Int(n)) => Ok(*n),
            _ => Err(self.wrong(expected)),
        }
    }

    fn no_more(&self, max: usize, expected: &'static str) -> Result<(), CommandError> {
        if self.args.len() > max {
            Err(self.wrong(expected))
        } else {
            Ok(())
        }
    }

    /// The `s text [s bg] [s fg] [i flash | f seconds]` shape shared by show
    /// and canned. After the leading string, remaining strings are colours in
    /// bg-then-fg order and a single numeric argument anywhere is the flash —
    /// so a cue can flash without padding in colours it doesn't want to
    /// change. OSC has no tables, so the type carries the meaning: an int (or
    /// OSC true/false) is the boolean form, a float is seconds with negative
    /// meaning no end.
    #[allow(clippy::type_complexity)]
    fn string_then_colours_and_flash(
        &self,
        expected: &'static str,
    ) -> Result<(String, Option<Rgb>, Option<Rgb>, Option<Flash>), CommandError> {
        let text = self.string(0, expected)?;
        let (mut bg, mut fg, mut flash) = (None, None, None);
        let on_off = |on: bool| {
            if on {
                Flash::For(FLASH_DEFAULT)
            } else {
                Flash::Off
            }
        };
        let seconds = |secs: f64| Flash::from_secs(secs).map_err(CommandError::BadFlash);
        for arg in self.args.iter().skip(1) {
            match arg {
                OscType::String(s) if bg.is_none() => bg = Some(parse_colour(s.clone())?),
                OscType::String(s) if fg.is_none() => fg = Some(parse_colour(s.clone())?),
                OscType::Int(n) if flash.is_none() => flash = Some(on_off(*n != 0)),
                OscType::Long(n) if flash.is_none() => flash = Some(on_off(*n != 0)),
                OscType::Bool(b) if flash.is_none() => flash = Some(on_off(*b)),
                OscType::Float(f) if flash.is_none() => flash = Some(seconds(f64::from(*f))?),
                OscType::Double(d) if flash.is_none() => flash = Some(seconds(*d)?),
                _ => return Err(self.wrong(expected)),
            }
        }
        Ok((text, bg, fg, flash))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rgb(s: &str) -> Rgb {
        s.parse().unwrap()
    }

    fn osc(addr: &str, args: Vec<OscType>) -> OscMessage {
        OscMessage {
            addr: addr.to_string(),
            args,
        }
    }

    #[test]
    fn json_show_full() {
        let cmd = parse_json(
            r##"{ "cmd": "show", "text": "STAND BY", "bg": "#000000", "fg": "#ffffff" }"##,
        )
        .unwrap();
        assert_eq!(
            cmd,
            Command::Show {
                text: "STAND BY".into(),
                bg: Some(rgb("#000000")),
                fg: Some(rgb("#ffffff")),
                flash: Flash::Off,
            }
        );
    }

    #[test]
    fn json_show_minimal() {
        let cmd = parse_json(r#"{ "cmd": "show", "text": "HELLO" }"#).unwrap();
        assert_eq!(
            cmd,
            Command::Show {
                text: "HELLO".into(),
                bg: None,
                fg: None,
                flash: Flash::Off,
            }
        );
    }

    #[test]
    fn json_countdown_secs() {
        let cmd =
            parse_json(r#"{ "cmd": "countdown_secs", "secs": 300, "label": "House opens in" }"#)
                .unwrap();
        assert_eq!(
            cmd,
            Command::CountdownSecs {
                secs: 300,
                label: Some("House opens in".into()),
                bg: None,
                fg: None,
            }
        );
    }

    #[test]
    fn json_countdown_to() {
        let cmd =
            parse_json(r#"{ "cmd": "countdown_to", "target": "2026-09-08T18:30:00Z" }"#).unwrap();
        match cmd {
            Command::CountdownTo {
                target,
                label: None,
                bg: None,
                fg: None,
            } => {
                assert_eq!(
                    target,
                    "2026-09-08T18:30:00Z".parse::<DateTime<Utc>>().unwrap()
                );
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn json_clear_and_status() {
        assert_eq!(parse_json(r#"{ "cmd": "clear" }"#).unwrap(), Command::Clear);
        assert_eq!(
            parse_json(r#"{ "cmd": "status" }"#).unwrap(),
            Command::Status
        );
    }

    #[test]
    fn json_rejects_unknown_fields_and_commands() {
        assert!(parse_json(r#"{ "cmd": "show", "text": "X", "blink": true }"#).is_err());
        assert!(parse_json(r#"{ "cmd": "dance" }"#).is_err());
        assert!(parse_json(r#"{ "cmd": "show" }"#).is_err());
        assert!(parse_json("not json").is_err());
    }

    #[test]
    fn json_rejects_bad_values() {
        assert!(parse_json(r#"{ "cmd": "show", "text": "X", "bg": "red" }"#).is_err());
        assert!(parse_json(r##"{ "cmd": "show", "text": "X", "bg": "#fff" }"##).is_err());
        assert!(parse_json(r#"{ "cmd": "countdown_to", "target": "tomorrow" }"#).is_err());
        assert!(parse_json(r#"{ "cmd": "countdown_secs", "secs": -5 }"#).is_err());
    }

    #[test]
    fn osc_show_variants() {
        let m = osc("/placard/show", vec![OscType::String("HELLO".into())]);
        assert_eq!(
            parse_osc(&m).unwrap(),
            Command::Show {
                text: "HELLO".into(),
                bg: None,
                fg: None,
                flash: Flash::Off,
            }
        );

        let m = osc(
            "/placard/show",
            vec![
                OscType::String("GO".into()),
                OscType::String("#0b6e2e".into()),
                OscType::String("#ffffff".into()),
            ],
        );
        assert_eq!(
            parse_osc(&m).unwrap(),
            Command::Show {
                text: "GO".into(),
                bg: Some(rgb("#0b6e2e")),
                fg: Some(rgb("#ffffff")),
                flash: Flash::Off,
            }
        );
    }

    #[test]
    fn osc_canned_and_colour() {
        let m = osc("/placard/canned", vec![OscType::String("go".into())]);
        assert_eq!(
            parse_osc(&m).unwrap(),
            Command::Canned {
                id: "go".into(),
                bg: None,
                fg: None,
                flash: None,
            }
        );

        let m = osc("/placard/colour", vec![OscType::String("#ff0000".into())]);
        assert_eq!(
            parse_osc(&m).unwrap(),
            Command::Colour {
                bg: Some(rgb("#ff0000")),
                fg: None
            }
        );
    }

    #[test]
    fn json_flash() {
        assert_eq!(
            parse_json(r#"{ "cmd": "show", "text": "SHOW STOP", "flash": true }"#).unwrap(),
            Command::Show {
                text: "SHOW STOP".into(),
                bg: None,
                fg: None,
                flash: Flash::For(FLASH_DEFAULT),
            }
        );
        // Absent on canned means "use the canned message's own setting".
        assert_eq!(
            parse_json(r#"{ "cmd": "canned", "id": "go" }"#).unwrap(),
            Command::Canned {
                id: "go".into(),
                bg: None,
                fg: None,
                flash: None,
            }
        );
        assert_eq!(
            parse_json(r#"{ "cmd": "canned", "id": "go", "flash": false }"#).unwrap(),
            Command::Canned {
                id: "go".into(),
                bg: None,
                fg: None,
                flash: Some(Flash::Off),
            }
        );
    }

    #[test]
    fn osc_flash() {
        // Flash without colours: no padding args needed.
        let m = osc(
            "/placard/show",
            vec![OscType::String("SHOW STOP".into()), OscType::Int(1)],
        );
        assert_eq!(
            parse_osc(&m).unwrap(),
            Command::Show {
                text: "SHOW STOP".into(),
                bg: None,
                fg: None,
                flash: Flash::For(FLASH_DEFAULT),
            }
        );
        // Colours and flash together, flag last.
        let m = osc(
            "/placard/canned",
            vec![
                OscType::String("go".into()),
                OscType::String("0b6e2e".into()),
                OscType::Int(1),
            ],
        );
        assert_eq!(
            parse_osc(&m).unwrap(),
            Command::Canned {
                id: "go".into(),
                bg: Some(rgb("0b6e2e")),
                fg: None,
                flash: Some(Flash::For(FLASH_DEFAULT)),
            }
        );
        // i 0 is an explicit "don't flash" override.
        let m = osc(
            "/placard/canned",
            vec![OscType::String("show_stop".into()), OscType::Int(0)],
        );
        assert_eq!(
            parse_osc(&m).unwrap(),
            Command::Canned {
                id: "show_stop".into(),
                bg: None,
                fg: None,
                flash: Some(Flash::Off),
            }
        );
        // A float is seconds; negative is a flash with no end.
        let m = osc(
            "/placard/show",
            vec![OscType::String("X".into()), OscType::Float(10.0)],
        );
        assert_eq!(
            parse_osc(&m).unwrap(),
            Command::Show {
                text: "X".into(),
                bg: None,
                fg: None,
                flash: Flash::For(std::time::Duration::from_secs(10)),
            }
        );
        let m = osc(
            "/placard/canned",
            vec![OscType::String("show_stop".into()), OscType::Float(-1.0)],
        );
        assert_eq!(
            parse_osc(&m).unwrap(),
            Command::Canned {
                id: "show_stop".into(),
                bg: None,
                fg: None,
                flash: Some(Flash::Forever),
            }
        );
        // An out-of-range float is rejected, not rounded or panicked on.
        let m = osc(
            "/placard/show",
            vec![OscType::String("X".into()), OscType::Float(f32::NAN)],
        );
        assert!(matches!(parse_osc(&m), Err(CommandError::BadFlash(_))));

        // Two ints make no sense.
        let m = osc(
            "/placard/show",
            vec![
                OscType::String("X".into()),
                OscType::Int(1),
                OscType::Int(1),
            ],
        );
        assert!(matches!(parse_osc(&m), Err(CommandError::WrongArgs { .. })));
    }

    #[test]
    fn osc_countdowns() {
        let m = osc(
            "/placard/countdown/to",
            vec![
                OscType::String("2026-09-08T18:30:00Z".into()),
                OscType::String("Doors".into()),
            ],
        );
        match parse_osc(&m).unwrap() {
            Command::CountdownTo { label, .. } => assert_eq!(label.as_deref(), Some("Doors")),
            other => panic!("unexpected {other:?}"),
        }

        let m = osc("/placard/countdown/secs", vec![OscType::Int(90)]);
        assert_eq!(
            parse_osc(&m).unwrap(),
            Command::CountdownSecs {
                secs: 90,
                label: None,
                bg: None,
                fg: None
            }
        );
    }

    #[test]
    fn osc_clear() {
        assert_eq!(
            parse_osc(&osc("/placard/clear", vec![])).unwrap(),
            Command::Clear
        );
    }

    #[test]
    fn osc_errors() {
        // Unknown address.
        assert!(matches!(
            parse_osc(&osc("/placard/nope", vec![])),
            Err(CommandError::UnknownAddress(_))
        ));
        // Missing required arg.
        assert!(matches!(
            parse_osc(&osc("/placard/show", vec![])),
            Err(CommandError::WrongArgs { .. })
        ));
        // Wrong type: float where int expected.
        assert!(matches!(
            parse_osc(&osc("/placard/countdown/secs", vec![OscType::Float(5.0)])),
            Err(CommandError::WrongArgs { .. })
        ));
        // Negative seconds.
        assert!(matches!(
            parse_osc(&osc("/placard/countdown/secs", vec![OscType::Int(-5)])),
            Err(CommandError::WrongArgs { .. })
        ));
        // Bad colour string.
        assert!(matches!(
            parse_osc(&osc("/placard/colour", vec![OscType::String("red".into())])),
            Err(CommandError::BadColour(_))
        ));
        // Bad timestamp.
        assert!(matches!(
            parse_osc(&osc(
                "/placard/countdown/to",
                vec![OscType::String("tomorrow".into())]
            )),
            Err(CommandError::BadTimestamp(_))
        ));
        // Excess args.
        assert!(matches!(
            parse_osc(&osc("/placard/clear", vec![OscType::String("x".into())])),
            Err(CommandError::WrongArgs { .. })
        ));
    }
}
