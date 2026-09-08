use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::scene::Scene;
use crate::state::LastCommand;

/// `GET /api/status` / `{"cmd":"status"}` reply body; the schema is part of
/// the wire contract (docs/protocol.md).
#[derive(Debug, Serialize)]
pub struct StatusReport {
    pub ok: bool,
    pub scene: SceneStatus,
    pub canned_id: Option<String>,
    pub uptime_secs: u64,
    pub ntp_synced: NtpSynced,
    pub clock_offset_ms: Option<i64>,
    pub version: &'static str,
    pub last_command: Option<LastCommand>,
}

/// The scene as clients see it: content carries the derived `display` string.
#[derive(Debug, Serialize)]
pub struct SceneStatus {
    pub bg: crate::scene::Rgb,
    pub fg: crate::scene::Rgb,
    pub content: serde_json::Value,
}

#[derive(Debug, Clone, Copy)]
pub enum NtpSynced {
    Known(bool),
    Unknown,
}

impl Serialize for NtpSynced {
    fn serialize<S: serde::Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        match self {
            NtpSynced::Known(b) => ser.serialize_bool(*b),
            NtpSynced::Unknown => ser.serialize_str("unknown"),
        }
    }
}

pub fn report(
    scene: &Scene,
    canned_id: Option<String>,
    uptime_secs: u64,
    last_command: Option<LastCommand>,
    now: DateTime<Utc>,
) -> StatusReport {
    let mut content = match serde_json::to_value(&scene.content) {
        Ok(v) => v,
        Err(_) => serde_json::Value::Null,
    };
    if let Some(obj) = content.as_object_mut() {
        obj.insert("display".into(), scene.display_string(now).into());
    }
    let (synced, clock_offset_ms) = probe_ntp();
    let ntp_synced = match synced {
        Some(b) => NtpSynced::Known(b),
        None => NtpSynced::Unknown,
    };
    StatusReport {
        ok: true,
        scene: SceneStatus {
            bg: scene.bg,
            fg: scene.fg,
            content,
        },
        canned_id,
        uptime_secs,
        ntp_synced,
        clock_offset_ms,
        version: env!("CARGO_PKG_VERSION"),
        last_command,
    }
}

/// Clock sync state as `(synced, offset_ms)`, `None` where unknowable.
/// Linux asks timedatectl/chrony; other platforms report unknown. This and
/// the sink factory in render.rs are deliberately the only two
/// `cfg(target_os)` sites — platform differences stay contained here.
#[cfg(target_os = "linux")]
fn probe_ntp() -> (Option<bool>, Option<i64>) {
    let synced = std::process::Command::new("timedatectl")
        .args(["show", "--property=NTPSynchronized", "--value"])
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|out| String::from_utf8_lossy(&out.stdout).trim() == "yes");
    // `chronyc -c tracking` CSV field 4 is the system time offset in seconds.
    let offset_ms = std::process::Command::new("chronyc")
        .args(["-c", "tracking"])
        .output()
        .ok()
        .filter(|out| out.status.success())
        .and_then(|out| {
            String::from_utf8_lossy(&out.stdout)
                .split(',')
                .nth(4)?
                .trim()
                .parse::<f64>()
                .ok()
        })
        .map(|secs| (secs * 1000.0).round() as i64);
    (synced, offset_ms)
}

#[cfg(not(target_os = "linux"))]
fn probe_ntp() -> (Option<bool>, Option<i64>) {
    (None, None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scene::Content;

    #[test]
    fn status_serialises_countdown_with_display() {
        let scene = Scene {
            bg: "8a0000".parse().unwrap(),
            fg: "ffffff".parse().unwrap(),
            content: Content::Countdown {
                target: "2026-09-08T18:30:00Z".parse().unwrap(),
                label: Some("House opens in".into()),
            },
        };
        let now = "2026-09-08T18:30:42Z".parse().unwrap();
        let report = report(&scene, None, 8123, None, now);
        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(json["ok"], true);
        assert_eq!(json["scene"]["bg"], "8a0000");
        assert_eq!(json["scene"]["content"]["kind"], "countdown");
        assert_eq!(json["scene"]["content"]["display"], "-0:42");
        assert_eq!(json["scene"]["content"]["label"], "House opens in");
        assert_eq!(json["uptime_secs"], 8123);
        assert_eq!(json["version"], env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn status_serialises_text_scene() {
        let scene = Scene {
            bg: "000000".parse().unwrap(),
            fg: "ffffff".parse().unwrap(),
            content: Content::Text { text: "GO".into() },
        };
        let json = serde_json::to_value(report(
            &scene,
            Some("go".into()),
            1,
            None,
            "2026-09-08T18:30:00Z".parse().unwrap(),
        ))
        .unwrap();
        assert_eq!(json["scene"]["content"]["kind"], "text");
        assert_eq!(json["scene"]["content"]["display"], "GO");
        assert_eq!(json["canned_id"], "go");
    }
}
