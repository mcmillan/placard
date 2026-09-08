use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::scene::Scene;
use crate::state::LastCommand;

/// `(synced, offset_ms)`, `None` where unknowable.
pub type NtpProbe = (Option<bool>, Option<i64>);

/// The release tag CI stamped into this binary, or "dev" for local builds.
/// This is how an operator tells which build a box actually runs — the cargo
/// version alone never changes between releases.
const BUILD: &str = match option_env!("PLACARD_BUILD") {
    Some(build) => build,
    None => "dev",
};

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
    pub build: &'static str,
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
    probe: NtpProbe,
) -> StatusReport {
    let mut content = match serde_json::to_value(&scene.content) {
        Ok(v) => v,
        Err(_) => serde_json::Value::Null,
    };
    if let Some(obj) = content.as_object_mut() {
        obj.insert("display".into(), scene.display_string(now).into());
    }
    let (synced, clock_offset_ms) = probe;
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
        build: BUILD,
        last_command,
    }
}

/// Clock sync state. Linux asks timedatectl/chrony; other platforms report
/// unknown. Runs on the state thread, so every subprocess is hard-bounded by
/// a timeout — a wedged chronyc must never freeze the ticker. This and the
/// sink factory in render.rs are deliberately the only two `cfg(target_os)`
/// sites — platform differences stay contained here.
#[cfg(target_os = "linux")]
pub fn probe_ntp() -> NtpProbe {
    use std::time::Duration;

    let timeout = Duration::from_millis(250);
    let synced = run_bounded(
        "timedatectl",
        &["show", "--property=NTPSynchronized", "--value"],
        timeout,
    )
    .map(|out| out.trim() == "yes");
    // `chronyc -c tracking` CSV field 4 is the system time offset in seconds.
    let offset_ms = run_bounded("chronyc", &["-c", "tracking"], timeout)
        .and_then(|out| out.split(',').nth(4)?.trim().parse::<f64>().ok())
        // A sane offset; anything bigger means the parse grabbed nonsense.
        .filter(|secs| secs.abs() < 3600.0)
        .map(|secs| (secs * 1000.0).round() as i64);
    (synced, offset_ms)
}

/// Run a command, returning its stdout only if it exits successfully within
/// the timeout; the child is killed otherwise. Output must stay under the
/// pipe buffer (these produce a few bytes) or the child would stall — which
/// the timeout also covers.
#[cfg(target_os = "linux")]
fn run_bounded(program: &str, args: &[&str], timeout: std::time::Duration) -> Option<String> {
    use std::io::Read as _;
    use std::process::Stdio;

    let mut child = std::process::Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(exit)) => {
                if !exit.success() {
                    return None;
                }
                let mut out = String::new();
                child.stdout.take()?.read_to_string(&mut out).ok()?;
                return Some(out);
            }
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            Err(_) => return None,
        }
    }
}

#[cfg(not(target_os = "linux"))]
pub fn probe_ntp() -> NtpProbe {
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
        let report = report(&scene, None, 8123, None, now, (Some(true), Some(3)));
        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(json["ok"], true);
        assert_eq!(json["scene"]["bg"], "8a0000");
        assert_eq!(json["scene"]["content"]["kind"], "countdown");
        assert_eq!(json["scene"]["content"]["display"], "-0:42");
        assert_eq!(json["scene"]["content"]["label"], "House opens in");
        assert_eq!(json["uptime_secs"], 8123);
        assert_eq!(json["ntp_synced"], true);
        assert_eq!(json["clock_offset_ms"], 3);
        assert_eq!(json["version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(json["build"], "dev");
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
            (None, None),
        ))
        .unwrap();
        assert_eq!(json["scene"]["content"]["kind"], "text");
        assert_eq!(json["scene"]["content"]["display"], "GO");
        assert_eq!(json["canned_id"], "go");
        assert_eq!(json["ntp_synced"], "unknown");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn bounded_run_kills_a_hung_command() {
        let started = std::time::Instant::now();
        let out = run_bounded("sleep", &["10"], std::time::Duration::from_millis(100));
        assert!(out.is_none());
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
    }
}
