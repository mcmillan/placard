use std::io::Write as _;
use std::net::{SocketAddr, UdpSocket};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use chrono::{DateTime, TimeDelta, Utc};
use gstreamer::glib;
use serde::{Deserialize, Serialize};

use crate::command::{Command, CommandError};
use crate::config::Config;
use crate::render::RenderSpec;
use crate::scene::{Content, Scene, format_clock};
use crate::status::{self, StatusReport};

/// Where the reply to a command goes. OSC acks are datagrams back to the
/// sender; TCP/HTTP wait on a oneshot; parse errors never get this far.
/// (DESIGN.md §5 also sketches a `None` variant; every transport replies, so
/// it had no constructor and is omitted.)
pub enum ReplyTo {
    Osc(SocketAddr),
    Oneshot(mpsc::Sender<Result<Reply, CommandError>>),
}

#[derive(Debug)]
pub enum Reply {
    Ok,
    Status(StatusReport),
}

/// A parsed command plus enough provenance to ack it and record it.
pub struct Envelope {
    pub command: Command,
    pub via: &'static str,
    pub from: Option<SocketAddr>,
    pub reply: ReplyTo,
}

/// What survives a power cycle: the scene and where it came from.
#[derive(Debug, Serialize, Deserialize)]
struct PersistedState {
    scene: Scene,
    canned_id: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct LastCommand {
    pub at: DateTime<Utc>,
    pub via: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<SocketAddr>,
}

pub struct StateThread {
    config: Config,
    state_path: PathBuf,
    scene: Scene,
    canned_id: Option<String>,
    last_command: Option<LastCommand>,
    started: Instant,
    last_pushed: Option<RenderSpec>,
    spec_tx: mpsc::Sender<RenderSpec>,
    /// Cloned OSC socket, used only to send `/placard/ok` / `/placard/error`.
    osc_socket: Option<UdpSocket>,
}

impl StateThread {
    pub fn new(
        config: Config,
        state_dir: &Path,
        spec_tx: mpsc::Sender<RenderSpec>,
        osc_socket: Option<UdpSocket>,
    ) -> StateThread {
        let state_path = state_dir.join("state.json");
        let (scene, canned_id) = match load_state(&state_path) {
            Some(p) => (p.scene, p.canned_id),
            None => (
                boot_scene(&config),
                Some(config.defaults.boot_scene.clone()),
            ),
        };
        StateThread {
            config,
            state_path,
            scene,
            canned_id,
            last_command: None,
            started: Instant::now(),
            last_pushed: None,
            spec_tx,
            osc_socket,
        }
    }

    /// The state thread proper: a recv_timeout loop whose 250 ms timeout
    /// doubles as the countdown/clock ticker (DESIGN.md §5).
    pub fn run(mut self, rx: mpsc::Receiver<Envelope>) {
        self.push_if_changed();
        loop {
            match rx.recv_timeout(Duration::from_millis(250)) {
                Ok(envelope) => self.handle(envelope),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => return,
            }
            self.push_if_changed();
        }
    }

    fn handle(&mut self, envelope: Envelope) {
        let result = self.apply(&envelope.command);
        if let Err(err) = &result {
            tracing::warn!(via = envelope.via, from = ?envelope.from, %err, "command rejected");
        } else if !matches!(envelope.command, Command::Status) {
            self.last_command = Some(LastCommand {
                at: Utc::now(),
                via: envelope.via,
                from: envelope.from,
            });
            self.persist();
        }
        self.send_reply(envelope.reply, result);
    }

    /// Apply a command to the scene. Errors leave the scene untouched.
    fn apply(&mut self, command: &Command) -> Result<Reply, CommandError> {
        let defaults = &self.config.defaults;
        match command {
            Command::Show { text, bg, fg } => {
                self.scene.bg = bg.unwrap_or(self.scene.bg);
                self.scene.fg = fg.unwrap_or(self.scene.fg);
                self.scene.content = Content::Text { text: text.clone() };
                self.canned_id = None;
            }
            Command::Canned { id, bg, fg } => {
                let canned = self
                    .config
                    .canned
                    .get(id)
                    .ok_or_else(|| CommandError::UnknownCanned(id.clone()))?;
                self.scene = Scene {
                    bg: bg.or(canned.bg).unwrap_or(defaults.bg),
                    fg: fg.or(canned.fg).unwrap_or(defaults.fg),
                    content: Content::Text {
                        text: canned.text.clone(),
                    },
                };
                self.canned_id = Some(id.clone());
            }
            Command::Colour { bg, fg } => {
                self.scene.bg = bg.unwrap_or(self.scene.bg);
                self.scene.fg = fg.unwrap_or(self.scene.fg);
                self.canned_id = None;
            }
            Command::CountdownTo {
                target,
                label,
                bg,
                fg,
            } => {
                self.scene.bg = bg.unwrap_or(self.scene.bg);
                self.scene.fg = fg.unwrap_or(self.scene.fg);
                self.scene.content = Content::Countdown {
                    target: *target,
                    label: label.clone(),
                };
                self.canned_id = None;
            }
            Command::CountdownSecs {
                secs,
                label,
                bg,
                fg,
            } => {
                // Converted to an absolute target at receipt; identical to
                // countdown_to from here on, so it survives a restart.
                let target = Utc::now() + TimeDelta::seconds(i64::from(*secs));
                self.scene.bg = bg.unwrap_or(self.scene.bg);
                self.scene.fg = fg.unwrap_or(self.scene.fg);
                self.scene.content = Content::Countdown {
                    target,
                    label: label.clone(),
                };
                self.canned_id = None;
            }
            Command::Clear => {
                self.scene = Scene::cleared(defaults.fg);
                self.canned_id = None;
            }
            Command::Status => {
                return Ok(Reply::Status(status::report(
                    &self.scene,
                    self.canned_id.clone(),
                    self.started.elapsed().as_secs(),
                    self.last_command.clone(),
                    Utc::now(),
                )));
            }
        }
        Ok(Reply::Ok)
    }

    fn send_reply(&self, reply: ReplyTo, result: Result<Reply, CommandError>) {
        match reply {
            ReplyTo::Oneshot(tx) => {
                let _ = tx.send(result);
            }
            ReplyTo::Osc(addr) => {
                let Some(socket) = &self.osc_socket else {
                    return;
                };
                let msg = match result {
                    Ok(_) => rosc::OscMessage {
                        addr: "/placard/ok".into(),
                        args: vec![],
                    },
                    Err(err) => rosc::OscMessage {
                        addr: "/placard/error".into(),
                        args: vec![rosc::OscType::String(err.to_string())],
                    },
                };
                match rosc::encoder::encode(&rosc::OscPacket::Message(msg)) {
                    Ok(bytes) => {
                        if let Err(err) = socket.send_to(&bytes, addr) {
                            tracing::warn!(%addr, %err, "failed to send OSC reply");
                        }
                    }
                    Err(err) => tracing::warn!(%err, "failed to encode OSC reply"),
                }
            }
        }
    }

    /// Derive the displayed strings and push a spec only when one changed.
    fn push_if_changed(&mut self) {
        let spec = derive_spec(&self.scene, Utc::now(), &self.config);
        if self.last_pushed.as_ref() != Some(&spec) {
            if self.spec_tx.send(spec.clone()).is_err() {
                // Render thread is gone; the process is coming down anyway.
                return;
            }
            self.last_pushed = Some(spec);
        }
    }

    fn persist(&self) {
        let persisted = PersistedState {
            scene: self.scene.clone(),
            canned_id: self.canned_id.clone(),
        };
        if let Err(err) = write_state(&self.state_path, &persisted) {
            tracing::error!(path = %self.state_path.display(), %err, "failed to persist state");
        }
    }
}

fn boot_scene(config: &Config) -> Scene {
    // Config validation guarantees boot_scene names a canned entry.
    let defaults = &config.defaults;
    match config.canned.get(&config.defaults.boot_scene) {
        Some(canned) => Scene {
            bg: canned.bg.unwrap_or(defaults.bg),
            fg: canned.fg.unwrap_or(defaults.fg),
            content: Content::Text {
                text: canned.text.clone(),
            },
        },
        None => Scene::cleared(defaults.fg),
    }
}

fn load_state(path: &Path) -> Option<PersistedState> {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return None,
        Err(err) => {
            tracing::warn!(path = %path.display(), %err, "failed to read state, using boot scene");
            return None;
        }
    };
    match serde_json::from_str(&raw) {
        Ok(state) => Some(state),
        Err(err) => {
            tracing::warn!(path = %path.display(), %err, "corrupt state, using boot scene");
            None
        }
    }
}

/// Atomic write: temp file in the same directory, fsync, rename (CLAUDE.md).
fn write_state(path: &Path, state: &PersistedState) -> anyhow::Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    let tmp = dir.join(".state.json.tmp");
    let mut file = std::fs::File::create(&tmp)?;
    file.write_all(&serde_json::to_vec_pretty(state)?)?;
    file.sync_all()?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Turn the scene into final on-screen strings. This is the only place
/// network/config text meets Pango markup: everything is escaped here, and
/// the `<span>` label wrapper is the only markup the code itself adds.
pub fn derive_spec(scene: &Scene, now: DateTime<Utc>, config: &Config) -> RenderSpec {
    // (plain-text lines for size fitting, final markup)
    let (fit_lines, text_markup) = match &scene.content {
        Content::Text { text } => (
            text.lines()
                .map(|l| FitLine {
                    text: l.to_string(),
                    scale: 1.0,
                })
                .collect(),
            glib::markup_escape_text(text).to_string(),
        ),
        Content::Countdown { target, label } => {
            let digits = crate::scene::format_countdown(*target, now);
            match label {
                Some(label) => (
                    vec![
                        FitLine {
                            text: label.clone(),
                            scale: 0.6,
                        },
                        FitLine {
                            text: digits.clone(),
                            scale: 1.0,
                        },
                    ],
                    format!(
                        "<span size=\"60%\">{}</span>\n{digits}",
                        glib::markup_escape_text(label)
                    ),
                ),
                None => (
                    vec![FitLine {
                        text: digits.clone(),
                        scale: 1.0,
                    }],
                    digits,
                ),
            }
        }
    };
    let d = &config.display;
    RenderSpec {
        bg_argb: scene.bg.to_argb(0xff),
        text_markup,
        text_argb: scene.fg.to_argb(0xff),
        text_px: fit_font_px(
            &fit_lines,
            d.max_font_px(),
            f64::from(d.width - 2 * d.padding_x),
            f64::from(d.height - 2 * d.padding_y),
        ),
        clock_text: format_clock(now, config.clock.timezone),
    }
}

/// One logical (pre-wrap) line of on-screen text; `scale` is relative to the
/// main size (the countdown label renders at 60%).
struct FitLine {
    text: String,
    scale: f64,
}

// Conservative Inter Bold metrics, calibrated against rendered frames:
// average advance per char and line height as fractions of the pixel size,
// plus slack because word wrap breaks early, not at exact character counts.
const AVG_CHAR_W: f64 = 0.68;
const LINE_H: f64 = 1.25;
const WRAP_SLACK: f64 = 1.1;
const MIN_FONT_PX: u32 = 8;

/// Largest pixel size ≤ `max_px` whose wrapped layout fits the usable area.
/// `textoverlay` silently renders nothing when a layout is taller than the
/// frame, so erring small is always the right trade.
fn fit_font_px(lines: &[FitLine], max_px: u32, usable_w: f64, usable_h: f64) -> u32 {
    let fits = |px: u32| -> bool {
        let mut height = 0.0;
        for line in lines {
            let eff = f64::from(px) * line.scale;
            let longest_word = line
                .text
                .split_whitespace()
                .map(|w| w.chars().count())
                .max()
                .unwrap_or(0);
            if longest_word as f64 * AVG_CHAR_W * eff > usable_w {
                return false;
            }
            let line_w = line.text.chars().count() as f64 * AVG_CHAR_W * eff * WRAP_SLACK;
            let wrapped = (line_w / usable_w).ceil().max(1.0);
            height += wrapped * LINE_H * eff;
        }
        height <= usable_h
    };
    let (mut lo, mut hi) = (MIN_FONT_PX, max_px.max(MIN_FONT_PX));
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        if fits(mid) {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    lo
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scene::Rgb;

    fn t0() -> DateTime<Utc> {
        "2026-09-08T18:30:00Z".parse().unwrap()
    }

    fn test_config() -> Config {
        toml::from_str(
            r#"
            [display]
            width = 1920
            height = 1080
            fps = 50
            font = "Inter Bold 120"
            padding_x = 96
            padding_y = 64
            [net]
            osc_port = 9000
            tcp_port = 9001
            http_port = 8080
            [clock]
            timezone = "UTC"
            font = "Inter Semibold 40"
            position = "bottom-right"
            [defaults]
            bg = "000000"
            fg = "ffffff"
            boot_scene = "go"
            [canned.go]
            text = "GO"
            bg = "0b6e2e"
            "#,
        )
        .unwrap()
    }

    fn scene_text(text: &str) -> Scene {
        Scene {
            bg: Rgb::BLACK,
            fg: "#ffffff".parse().unwrap(),
            content: Content::Text { text: text.into() },
        }
    }

    #[test]
    fn derive_escapes_markup_in_text() {
        let spec = derive_spec(&scene_text("<b>&\"</b>"), t0(), &test_config());
        assert_eq!(spec.text_markup, "&lt;b&gt;&amp;&quot;&lt;/b&gt;");
    }

    #[test]
    fn derive_wraps_countdown_label_and_escapes_it() {
        let scene = Scene {
            bg: Rgb::BLACK,
            fg: "#ffffff".parse().unwrap(),
            content: Content::Countdown {
                target: t0() + TimeDelta::seconds(90),
                label: Some("Doors <open>".into()),
            },
        };
        let spec = derive_spec(&scene, t0(), &test_config());
        assert_eq!(
            spec.text_markup,
            "<span size=\"60%\">Doors &lt;open&gt;</span>\n1:30"
        );
    }

    #[test]
    fn derive_countdown_without_label_is_just_digits() {
        let scene = Scene {
            bg: Rgb::BLACK,
            fg: "#ffffff".parse().unwrap(),
            content: Content::Countdown {
                target: t0() - TimeDelta::seconds(7),
                label: None,
            },
        };
        assert_eq!(
            derive_spec(&scene, t0(), &test_config()).text_markup,
            "-0:07"
        );
    }

    #[test]
    fn short_text_gets_max_size() {
        let spec = derive_spec(&scene_text("GO"), t0(), &test_config());
        assert_eq!(spec.text_px, test_config().display.max_font_px());
    }

    #[test]
    fn long_text_shrinks_and_never_hits_the_floor() {
        let text = "KILL ALL HUMANS, KILL ALL HUMANS, MUST KILL ALL HUMANS... ".repeat(7);
        let spec = derive_spec(&scene_text(&text), t0(), &test_config());
        let max = test_config().display.max_font_px();
        assert!(spec.text_px < max, "400 chars must shrink below {max}");
        assert!(spec.text_px > MIN_FONT_PX, "must stay readable");
    }

    #[test]
    fn fit_is_monotonic_in_text_length() {
        let cfg = test_config();
        let sizes: Vec<u32> = [1usize, 4, 16, 64, 256]
            .iter()
            .map(|n| derive_spec(&scene_text(&"HUMANS ".repeat(*n)), t0(), &cfg).text_px)
            .collect();
        let mut sorted = sizes.clone();
        sorted.sort_unstable_by(|a, b| b.cmp(a));
        assert_eq!(sizes, sorted, "more text must never mean bigger text");
    }

    #[test]
    fn unbreakable_word_is_bounded_by_width() {
        let spec = derive_spec(&scene_text(&"M".repeat(60)), t0(), &test_config());
        // 60 chars at 0.68 advance must fit in 1728 usable px.
        assert!(f64::from(spec.text_px) * AVG_CHAR_W * 60.0 <= 1728.0);
    }

    #[test]
    fn derive_colours_pack_opaque() {
        let scene = Scene {
            bg: "#8a0000".parse().unwrap(),
            fg: "#ffffff".parse().unwrap(),
            content: Content::Text { text: "X".into() },
        };
        let spec = derive_spec(&scene, t0(), &test_config());
        assert_eq!(spec.bg_argb, 0xff8a0000);
        assert_eq!(spec.text_argb, 0xffffffff);
    }

    #[test]
    fn state_roundtrips_through_disk() {
        let dir = std::env::temp_dir().join(format!("placard-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("state.json");
        let state = PersistedState {
            scene: scene_text("STAND BY"),
            canned_id: None,
        };
        write_state(&path, &state).unwrap();
        let loaded = load_state(&path).unwrap();
        assert_eq!(loaded.scene, state.scene);
        assert_eq!(loaded.canned_id, None);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn corrupt_state_is_none() {
        let dir = std::env::temp_dir().join(format!("placard-corrupt-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("state.json");
        std::fs::write(&path, "{ not json").unwrap();
        assert!(load_state(&path).is_none());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn missing_state_is_none() {
        assert!(load_state(Path::new("/nonexistent/state.json")).is_none());
    }
}
