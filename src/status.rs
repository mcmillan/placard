use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::scene::Scene;
use crate::state::LastCommand;

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
    /// Absent when steady; otherwise mirrors the shape a command sends, so
    /// what you read back looks like what you'd write.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub flash: Option<FlashStatus>,
    pub uptime_secs: u64,
    pub version: &'static str,
    pub build: &'static str,
    pub last_command: Option<LastCommand>,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(untagged)]
pub enum FlashStatus {
    Timed { remaining_s: f64 },
    Forever { infinite: bool },
}

/// The scene as clients see it: content carries the derived `display` string.
#[derive(Debug, Serialize)]
pub struct SceneStatus {
    pub bg: crate::scene::Rgb,
    pub fg: crate::scene::Rgb,
    pub content: serde_json::Value,
}

pub fn report(
    scene: &Scene,
    canned_id: Option<String>,
    uptime_secs: u64,
    last_command: Option<LastCommand>,
    now: DateTime<Utc>,
    flash: Option<FlashStatus>,
) -> StatusReport {
    let mut content = match serde_json::to_value(&scene.content) {
        Ok(v) => v,
        Err(_) => serde_json::Value::Null,
    };
    if let Some(obj) = content.as_object_mut() {
        obj.insert("display".into(), scene.display_string(now).into());
    }
    StatusReport {
        ok: true,
        scene: SceneStatus {
            bg: scene.bg,
            fg: scene.fg,
            content,
        },
        canned_id,
        flash,
        uptime_secs,
        version: env!("CARGO_PKG_VERSION"),
        build: BUILD,
        last_command,
    }
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
        let report = report(&scene, None, 8123, None, now, None);
        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(json["ok"], true);
        assert_eq!(json["scene"]["bg"], "8a0000");
        assert_eq!(json["scene"]["content"]["kind"], "countdown");
        assert_eq!(json["scene"]["content"]["display"], "-0:42");
        assert_eq!(json["scene"]["content"]["label"], "House opens in");
        assert_eq!(json["uptime_secs"], 8123);
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
            Some(FlashStatus::Forever { infinite: true }),
        ))
        .unwrap();
        assert_eq!(json["scene"]["content"]["kind"], "text");
        assert_eq!(json["scene"]["content"]["display"], "GO");
        assert_eq!(json["canned_id"], "go");
    }
}
