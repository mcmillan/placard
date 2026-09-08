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
use crate::scene::{Content, Flash, Scene, format_clock};
use crate::status::{self, StatusReport};

/// Where the reply to a command goes. OSC acks are datagrams back to the
/// sender; TCP/HTTP wait on a oneshot.
pub enum ReplyTo {
    Osc(SocketAddr),
    Oneshot(mpsc::Sender<Result<Reply, CommandError>>),
}

#[derive(Debug)]
pub enum Reply {
    Ok,
    Status(StatusReport),
    History(Vec<HistoryEntry>),
}

/// An inbound message plus enough provenance to ack and record it. Parse
/// failures travel here too (as `Err`) so they appear in the history and
/// their replies route like everyone else's.
pub struct Envelope {
    pub command: Result<Command, CommandError>,
    /// The wire text as received (OSC rendered readably), for the history.
    pub raw: String,
    pub via: &'static str,
    pub from: Option<SocketAddr>,
    pub reply: ReplyTo,
}

/// One inbound message as shown by `/api/history` and the web UI, and as one
/// line of `history.ndjson`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub at: DateTime<Utc>,
    pub via: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<SocketAddr>,
    pub raw: String,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// How many history entries survive: the ring size, the `history` reply and
/// what a restart reloads from the tail of `history.ndjson`.
const HISTORY_CAP: usize = 200;
/// Raw text is truncated for storage so a 64 KiB TCP line can't bloat the UI.
const HISTORY_RAW_CAP: usize = 512;
/// The on-disk log is rewritten from the ring after this many appends, so
/// its size stays bounded no matter how long the box runs.
const HISTORY_COMPACT_EVERY: usize = HISTORY_CAP * 10;

/// What survives a power cycle: the scene, where it came from, and whether
/// it was flashing with no end. Timed flashes are transient attention-
/// getters and are deliberately not persisted; an endless one is a state the
/// operator chose and expects to still be there after a power blip.
#[derive(Debug, Serialize, Deserialize)]
struct PersistedState {
    scene: Scene,
    canned_id: Option<String>,
    #[serde(default)]
    flash_forever: bool,
}

/// A flash in progress. `duration` is `None` for one that runs until the
/// next command.
#[derive(Debug, Clone, Copy)]
struct RunningFlash {
    started: Instant,
    duration: Option<Duration>,
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
    history: std::collections::VecDeque<HistoryEntry>,
    history_path: PathBuf,
    history_appends: usize,
    active_flash: Option<RunningFlash>,
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
        let history_path = state_dir.join("history.ndjson");
        let (scene, canned_id, flash_forever) = match load_state(&state_path) {
            Some(p) => (p.scene, p.canned_id, p.flash_forever),
            None => (
                boot_scene(&config),
                Some(config.defaults.boot_scene.clone()),
                false,
            ),
        };
        let (history, history_oversized) = load_history(&history_path);
        let mut state = StateThread {
            config,
            state_path,
            scene,
            canned_id,
            last_command: None,
            started: Instant::now(),
            last_pushed: None,
            history,
            history_path,
            history_appends: 0,
            // An endless flash resumes; its phase restarts, which is the
            // only thing a reboot can't preserve and nobody can perceive.
            active_flash: flash_forever.then(|| RunningFlash {
                started: Instant::now(),
                duration: None,
            }),
            spec_tx,
            osc_socket,
        };
        if history_oversized {
            state.compact_history();
        }
        state
    }

    /// The state thread proper: a recv_timeout loop whose timeout doubles as
    /// the ticker that re-derives countdown and clock strings. The tick
    /// tightens while a flash is running so the 500 ms colour swaps land
    /// close to their boundaries.
    pub fn run(mut self, rx: mpsc::Receiver<Envelope>) {
        self.push_if_changed();
        loop {
            let tick = if self.active_flash.is_some() { 50 } else { 250 };
            match rx.recv_timeout(Duration::from_millis(tick)) {
                Ok(envelope) => self.handle(envelope),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => return,
            }
            self.push_if_changed();
        }
    }

    fn handle(&mut self, envelope: Envelope) {
        let result = match &envelope.command {
            Ok(command) => self.apply(command),
            Err(err) => Err(err.clone()),
        };
        // Queries stay out of the history or the UI's own polling would
        // flood it.
        let query = matches!(envelope.command, Ok(Command::Status | Command::History));
        if !query {
            self.record(&envelope, result.as_ref().err());
        }
        if let Err(err) = &result {
            tracing::warn!(via = envelope.via, from = ?envelope.from, %err, "command rejected");
        } else if !query {
            self.last_command = Some(LastCommand {
                at: Utc::now(),
                via: envelope.via,
                from: envelope.from,
            });
            self.persist();
        }
        self.send_reply(envelope.reply, result);
    }

    fn start_flash(&mut self, flash: Flash) {
        self.active_flash = match flash {
            Flash::Off => None,
            Flash::For(duration) => Some(RunningFlash {
                started: Instant::now(),
                duration: Some(duration),
            }),
            Flash::Forever => Some(RunningFlash {
                started: Instant::now(),
                duration: None,
            }),
        };
    }

    /// The flash as clients see it: absent when steady, otherwise how much
    /// longer it runs.
    fn flash_status(&self) -> Option<status::FlashStatus> {
        let flash = self.active_flash?;
        match flash.duration {
            None => Some(status::FlashStatus::Forever { infinite: true }),
            Some(duration) => Some(status::FlashStatus::Timed {
                remaining_s: duration
                    .saturating_sub(flash.started.elapsed())
                    .as_secs_f64(),
            }),
        }
    }

    fn record(&mut self, envelope: &Envelope, error: Option<&CommandError>) {
        if self.history.len() >= HISTORY_CAP {
            self.history.pop_front();
        }
        let mut raw = envelope.raw.clone();
        if raw.len() > HISTORY_RAW_CAP {
            let mut end = HISTORY_RAW_CAP;
            while !raw.is_char_boundary(end) {
                end -= 1;
            }
            raw.truncate(end);
            raw.push('…');
        }
        let entry = HistoryEntry {
            at: Utc::now(),
            via: envelope.via.to_string(),
            from: envelope.from,
            raw,
            ok: error.is_none(),
            error: error.map(|e| e.to_string()),
        };
        // Into the ring first: compaction snapshots the ring, so the entry
        // that trips the threshold must already be in it.
        self.history.push_back(entry.clone());
        self.append_history(&entry);
    }

    /// Append one line to `history.ndjson`. No fsync: this is diagnostics,
    /// and a power cut can at worst tear the final line, which the loader
    /// skips. Failures are logged and never disturb the show.
    fn append_history(&mut self, entry: &HistoryEntry) {
        let line = match serde_json::to_string(entry) {
            Ok(line) => line,
            Err(err) => {
                tracing::warn!(%err, "failed to serialise history entry");
                return;
            }
        };
        let appended = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.history_path)
            .and_then(|mut file| writeln!(file, "{line}"));
        if let Err(err) = appended {
            tracing::warn!(path = %self.history_path.display(), %err, "failed to append history");
            return;
        }
        self.history_appends += 1;
        if self.history_appends >= HISTORY_COMPACT_EVERY {
            self.compact_history();
        }
    }

    /// Rewrite the log as just the current ring (atomically, like
    /// state.json), bounding the file to ~200 lines regardless of uptime.
    fn compact_history(&mut self) {
        self.history_appends = 0;
        let dir = self.history_path.parent().unwrap_or(Path::new("."));
        let tmp = dir.join(".history.ndjson.tmp");
        let write = || -> std::io::Result<()> {
            let mut file = std::fs::File::create(&tmp)?;
            for entry in &self.history {
                if let Ok(line) = serde_json::to_string(entry) {
                    writeln!(file, "{line}")?;
                }
            }
            file.sync_all()?;
            std::fs::rename(&tmp, &self.history_path)
        };
        if let Err(err) = write() {
            tracing::warn!(path = %self.history_path.display(), %err, "failed to compact history");
        }
    }

    /// Apply a command to the scene. Errors leave the scene untouched. Any
    /// scene-changing command ends a running flash; show/canned may start a
    /// new one.
    fn apply(&mut self, command: &Command) -> Result<Reply, CommandError> {
        let defaults = &self.config.defaults;
        if !matches!(command, Command::Status | Command::History) {
            self.active_flash = None;
        }
        match command {
            Command::Show {
                text,
                bg,
                fg,
                flash,
            } => {
                self.scene.bg = bg.unwrap_or(self.scene.bg);
                self.scene.fg = fg.unwrap_or(self.scene.fg);
                self.scene.content = Content::Text { text: text.clone() };
                self.canned_id = None;
                self.start_flash(*flash);
            }
            Command::Canned { id, bg, fg, flash } => {
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
                let flash = flash.unwrap_or(canned.flash);
                self.canned_id = Some(id.clone());
                self.start_flash(flash);
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
            Command::History => {
                // Newest first, the order a human scans it in.
                return Ok(Reply::History(self.history.iter().rev().cloned().collect()));
            }
            Command::Status => {
                return Ok(Reply::Status(status::report(
                    &self.scene,
                    self.canned_id.clone(),
                    self.started.elapsed().as_secs(),
                    self.last_command.clone(),
                    Utc::now(),
                    self.flash_status(),
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
        let invert = match self
            .active_flash
            .map(|f| flash_invert(f.started.elapsed(), f.duration))
        {
            Some(Some(invert)) => invert,
            Some(None) => {
                // Flash over; settle on the real colours and stop fast ticks.
                self.active_flash = None;
                false
            }
            None => false,
        };
        let spec = derive_spec(&self.scene, Utc::now(), &self.config, invert);
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
            flash_forever: matches!(self.active_flash, Some(RunningFlash { duration: None, .. })),
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

/// Reload the tail of `history.ndjson`. Lines that don't parse (torn by a
/// power cut, or from an older schema) are skipped, not fatal. The second
/// value reports whether the file has outgrown the cap: in-process
/// compaction only fires after enough appends in one lifetime, so a
/// restart-heavy box would otherwise grow the file forever.
fn load_history(path: &Path) -> (std::collections::VecDeque<HistoryEntry>, bool) {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(err) => {
            if err.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(path = %path.display(), %err, "failed to read history");
            }
            return (
                std::collections::VecDeque::with_capacity(HISTORY_CAP),
                false,
            );
        }
    };
    let oversized = raw.lines().count() > HISTORY_CAP;
    let mut entries: std::collections::VecDeque<HistoryEntry> = raw
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();
    while entries.len() > HISTORY_CAP {
        entries.pop_front();
    }
    (entries, oversized)
}

/// Atomic write — temp file in the same directory, fsync, rename — so a
/// power cut mid-write can never leave a truncated state file.
fn write_state(path: &Path, state: &PersistedState) -> anyhow::Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    let tmp = dir.join(".state.json.tmp");
    let mut file = std::fs::File::create(&tmp)?;
    file.write_all(&serde_json::to_vec_pretty(state)?)?;
    file.sync_all()?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

const FLASH_PERIOD_MS: u128 = 500;

/// Whether the colours are currently inverted, `elapsed` into a flash;
/// `None` once the flash is over (`duration` of `None` never is). Starts
/// inverted for immediate attention and, for a timed flash, lands back on
/// the real colours: half-second phases, even ones inverted.
fn flash_invert(elapsed: Duration, duration: Option<Duration>) -> Option<bool> {
    if duration.is_some_and(|d| elapsed >= d) {
        return None;
    }
    Some((elapsed.as_millis() / FLASH_PERIOD_MS).is_multiple_of(2))
}

/// Turn the scene into final on-screen strings. This is the only place
/// network/config text meets Pango markup: everything is escaped here, and
/// the `<span>` label wrapper is the only markup the code itself adds.
/// `invert` swaps fg/bg (the flash effect); the scene keeps its real colours.
pub fn derive_spec(scene: &Scene, now: DateTime<Utc>, config: &Config, invert: bool) -> RenderSpec {
    // (plain-text lines for size fitting, final markup)
    let (fit_lines, text_markup) = match &scene.content {
        Content::Text { text } => (
            clamp_display(text)
                .lines()
                .map(|l| FitLine {
                    text: l.to_string(),
                    scale: 1.0,
                })
                .collect(),
            glib::markup_escape_text(&clamp_display(text)).to_string(),
        ),
        Content::Countdown { target, label } => {
            let digits = crate::scene::format_countdown(*target, now);
            match label {
                Some(label) => {
                    let label = clamp_display(label);
                    // The label can contain newlines, which render as real
                    // line breaks — each one must count towards the height
                    // or the fit underestimates and textoverlay culls.
                    let mut lines: Vec<FitLine> = label
                        .lines()
                        .map(|l| FitLine {
                            text: l.to_string(),
                            scale: 0.6,
                        })
                        .collect();
                    lines.push(FitLine {
                        text: digits.clone(),
                        scale: 1.0,
                    });
                    (
                        lines,
                        format!(
                            "<span size=\"60%\">{}</span>\n{digits}",
                            glib::markup_escape_text(&label)
                        ),
                    )
                }
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
    // Float subtraction so a mis-sized config can never wrap u32 arithmetic
    // into a giant usable area; validation rejects such configs at load, this
    // is defence in depth.
    let usable_w = (f64::from(d.width) - 2.0 * f64::from(d.padding_x)).max(1.0);
    let usable_h = (f64::from(d.height) - 2.0 * f64::from(d.padding_y)).max(1.0);
    RenderSpec {
        bg_argb: if invert { scene.fg } else { scene.bg }.to_argb(0xff),
        text_markup,
        text_argb: if invert { scene.bg } else { scene.fg }.to_argb(0xff),
        text_px: fit_font_px(&fit_lines, d.max_font_px(), usable_w, usable_h),
        clock_text: format_clock(now, config.clock.timezone),
    }
}

/// One logical (pre-wrap) line of on-screen text; `scale` is relative to the
/// main size (the countdown label renders at 60%).
struct FitLine {
    text: String,
    scale: f64,
}

const LINE_H: f64 = 1.25;
const MIN_FONT_PX: u32 = 8;
const SPACE_EM: f64 = 0.30;
/// Bounds on what reaches the renderer. A 1080p frame at `MIN_FONT_PX`
/// provably fits this much even in the worst glyph case; without a bound,
/// thousands of newlines or one enormous unbroken word build a layout no
/// font size can fit — and `textoverlay` culls over-tall layouts to a blank
/// frame.
const MAX_DISPLAY_LINES: usize = 40;
const MAX_DISPLAY_CHARS: usize = 4_000;

/// Truncate degenerate input for display, with an ellipsis. The scene (and
/// so state.json and `/api/status`) keeps the full text; only the on-screen
/// string is clamped.
fn clamp_display(text: &str) -> std::borrow::Cow<'_, str> {
    let mut lines = 0usize;
    for (count, (idx, c)) in text.char_indices().enumerate() {
        if c == '\n' {
            lines += 1;
        }
        if count >= MAX_DISPLAY_CHARS || lines >= MAX_DISPLAY_LINES {
            let mut out = text[..idx].to_string();
            out.push('…');
            return std::borrow::Cow::Owned(out);
        }
    }
    std::borrow::Cow::Borrowed(text)
}

/// Per-character advance estimate for Inter Bold, in em, deliberately on the
/// wide side of reality: overestimating width only makes text smaller, while
/// underestimating risks textoverlay culling the whole layout. Anything
/// non-ASCII (CJK, emoji, symbols) is assumed very wide for the same reason.
fn char_em(c: char) -> f64 {
    match c {
        ' ' => SPACE_EM,
        'i' | 'I' | 'l' | 'j' | '!' | '.' | ',' | ':' | ';' | '\'' | '|' => 0.40,
        'f' | 't' | 'r' | '-' | '(' | ')' | '[' | ']' | '"' => 0.55,
        'm' | 'w' | 'M' | 'W' | '@' => 1.00,
        c if c.is_ascii() => 0.80,
        _ => 1.30,
    }
}

fn text_em(text: &str) -> f64 {
    text.chars().map(char_em).sum()
}

/// Conservative greedy word wrap: how many rendered lines one logical line
/// occupies at `em_px` pixels per em. Mirrors Pango's greedy breaker but with
/// the inflated widths above, so it can only over-count lines, never under.
/// Words wider than the frame char-break, as wrap-mode `wordchar` does.
fn wrapped_line_count(text: &str, em_px: f64, usable_w: f64) -> usize {
    let space = SPACE_EM * em_px;
    let mut lines = 1usize;
    let mut cur = 0.0f64;
    let mut first = true;
    // split(' ') rather than split_whitespace so runs of spaces keep their
    // width instead of collapsing (collapsing would under-estimate).
    for word in text.split(' ') {
        let w = text_em(word) * em_px;
        let gap = if first { 0.0 } else { space };
        first = false;
        if w > usable_w {
            // Pango's wordchar mode moves a too-long word to a fresh line
            // and char-breaks it there. Unreachable while fit_font_px
            // requires the longest word to fit, but modelled correctly so a
            // future relaxation of that rule doesn't inherit a wrong count.
            if cur > 0.0 {
                lines += 1;
            }
            let full = (w / usable_w).ceil().max(1.0) as usize;
            lines += full - 1;
            cur = w - (full - 1) as f64 * usable_w;
        } else if cur + gap + w <= usable_w {
            cur += gap + w;
        } else {
            lines += 1;
            cur = w;
        }
    }
    lines
}

/// Largest pixel size ≤ `max_px` whose wrapped layout fits the usable area.
/// `textoverlay` silently renders nothing when a layout is taller than the
/// frame, so erring small is always the right trade.
fn fit_font_px(lines: &[FitLine], max_px: u32, usable_w: f64, usable_h: f64) -> u32 {
    let fits = |px: u32| -> bool {
        let mut height = 0.0;
        for line in lines {
            let eff = f64::from(px) * line.scale;
            // Prefer shrinking over breaking inside a word: the widest word
            // must fit on a line of its own.
            let longest_word = line
                .text
                .split_whitespace()
                .map(text_em)
                .fold(0.0f64, f64::max);
            if longest_word * eff > usable_w {
                return false;
            }
            height += wrapped_line_count(&line.text, eff, usable_w) as f64 * LINE_H * eff;
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
        let spec = derive_spec(&scene_text("<b>&\"</b>"), t0(), &test_config(), false);
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
        let spec = derive_spec(&scene, t0(), &test_config(), false);
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
            derive_spec(&scene, t0(), &test_config(), false).text_markup,
            "-0:07"
        );
    }

    #[test]
    fn short_text_gets_max_size() {
        let spec = derive_spec(&scene_text("GO"), t0(), &test_config(), false);
        assert_eq!(spec.text_px, test_config().display.max_font_px());
    }

    #[test]
    fn long_text_shrinks_and_never_hits_the_floor() {
        let text = "KILL ALL HUMANS, KILL ALL HUMANS, MUST KILL ALL HUMANS... ".repeat(7);
        let spec = derive_spec(&scene_text(&text), t0(), &test_config(), false);
        let max = test_config().display.max_font_px();
        assert!(spec.text_px < max, "400 chars must shrink below {max}");
        assert!(spec.text_px > MIN_FONT_PX, "must stay readable");
    }

    #[test]
    fn fit_is_monotonic_in_text_length() {
        let cfg = test_config();
        let sizes: Vec<u32> = [1usize, 4, 16, 64, 256]
            .iter()
            .map(|n| derive_spec(&scene_text(&"HUMANS ".repeat(*n)), t0(), &cfg, false).text_px)
            .collect();
        let mut sorted = sizes.clone();
        sorted.sort_unstable_by(|a, b| b.cmp(a));
        assert_eq!(sizes, sorted, "more text must never mean bigger text");
    }

    #[test]
    fn unbreakable_word_is_bounded_by_width() {
        let spec = derive_spec(&scene_text(&"M".repeat(60)), t0(), &test_config(), false);
        // 60 'M's at their estimated 1.0 em advance must fit in 1728 px.
        assert!(f64::from(spec.text_px) * 60.0 <= 1728.0);
    }

    #[test]
    fn wide_glyphs_shrink_more_than_narrow_ones() {
        let cfg = test_config();
        let wide = derive_spec(&scene_text(&"WM ".repeat(60)), t0(), &cfg, false).text_px;
        let narrow = derive_spec(&scene_text(&"il ".repeat(60)), t0(), &cfg, false).text_px;
        assert!(
            wide < narrow,
            "wide-glyph text must be sized smaller ({wide} vs {narrow})"
        );
    }

    #[test]
    fn degenerate_input_is_clamped_instead_of_blanking() {
        let cfg = test_config();
        // Hundreds of newlines: without the clamp no font size fits and
        // textoverlay would cull the whole layout.
        let spec = derive_spec(&scene_text(&"a\n".repeat(300)), t0(), &cfg, false);
        assert!(spec.text_markup.lines().count() <= MAX_DISPLAY_LINES);
        assert!(spec.text_markup.ends_with('…'));

        // One enormous unbroken word.
        let spec = derive_spec(&scene_text(&"W".repeat(60_000)), t0(), &cfg, false);
        assert!(spec.text_markup.chars().count() <= MAX_DISPLAY_CHARS + 1);
        assert!(spec.text_markup.ends_with('…'));

        // A degenerate label too.
        let scene = Scene {
            bg: Rgb::BLACK,
            fg: "ffffff".parse().unwrap(),
            content: Content::Countdown {
                target: t0() + TimeDelta::seconds(90),
                label: Some("x\n".repeat(300)),
            },
        };
        let spec = derive_spec(&scene, t0(), &cfg, false);
        assert!(spec.text_markup.lines().count() <= MAX_DISPLAY_LINES + 2);

        // Ordinary text is untouched — no ellipsis, no allocation surprises.
        let spec = derive_spec(&scene_text("KILL ALL HUMANS"), t0(), &cfg, false);
        assert_eq!(spec.text_markup, "KILL ALL HUMANS");
    }

    #[test]
    fn long_words_wrap_onto_fresh_lines_in_the_model() {
        // A word wider than the frame starts on its own line, as Pango does:
        // 0.4 of a line used, then a 2.5-line word → 1 + 3 = 4 lines.
        let usable = 1000.0;
        // "aaaaa" = 5 × 0.8em × 100px = 400px; 25 W's = 25 × 1.0em × 100px
        // = 2500px = a fresh line plus 2 more.
        let text = format!("aaaaa {}", "W".repeat(25));
        assert_eq!(super::wrapped_line_count(&text, 100.0, usable), 4);
    }

    #[test]
    fn multiline_countdown_label_counts_every_line() {
        let cfg = test_config();
        let scene_with = |label: &str| Scene {
            bg: Rgb::BLACK,
            fg: "ffffff".parse().unwrap(),
            content: Content::Countdown {
                target: t0() + TimeDelta::seconds(90),
                label: Some(label.into()),
            },
        };
        let one = derive_spec(&scene_with("HOLD"), t0(), &cfg, false).text_px;
        let many = derive_spec(&scene_with(&"HOLD\n".repeat(12)), t0(), &cfg, false).text_px;
        assert!(
            many < one,
            "a 12-line label must shrink the layout ({many} vs {one})"
        );
        // And the shrunk layout must actually fit the frame estimate.
        assert!(f64::from(many) * 0.6 * 12.0 * LINE_H + f64::from(many) * LINE_H <= 952.0 * 1.01);
    }

    fn test_state(name: &str) -> (StateThread, mpsc::Receiver<RenderSpec>) {
        let dir = std::env::temp_dir().join(format!("placard-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (spec_tx, spec_rx) = mpsc::channel();
        (
            StateThread::new(test_config(), &dir, spec_tx, None),
            spec_rx,
        )
    }

    fn envelope(
        command: Result<Command, CommandError>,
        raw: &str,
    ) -> (Envelope, mpsc::Receiver<Result<Reply, CommandError>>) {
        let (reply_tx, reply_rx) = mpsc::channel();
        (
            Envelope {
                command,
                raw: raw.into(),
                via: "tcp",
                from: None,
                reply: ReplyTo::Oneshot(reply_tx),
            },
            reply_rx,
        )
    }

    fn history_of(state: &mut StateThread) -> Vec<HistoryEntry> {
        let (env, rx) = envelope(Ok(Command::History), "");
        state.handle(env);
        match rx.recv().unwrap() {
            Ok(Reply::History(entries)) => entries,
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn history_records_accepted_and_rejected_but_not_queries() {
        let (mut state, _spec_rx) = test_state("history");
        let (env, _rx) = envelope(
            Ok(Command::Show {
                text: "GO".into(),
                bg: None,
                fg: None,
                flash: Flash::Off,
            }),
            r#"{"cmd":"show","text":"GO"}"#,
        );
        state.handle(env);
        let (env, _rx) = envelope(Err(CommandError::BadJson("nope".into())), "not json");
        state.handle(env);
        let (env, _rx) = envelope(Ok(Command::Status), "");
        state.handle(env);

        let entries = history_of(&mut state);
        // Newest first; the status query and the history query are absent.
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].raw, "not json");
        assert!(!entries[0].ok);
        assert_eq!(entries[0].error.as_deref(), Some("invalid JSON: nope"));
        assert_eq!(entries[1].raw, r#"{"cmd":"show","text":"GO"}"#);
        assert!(entries[1].ok);
        assert_eq!(entries[1].via, "tcp");
    }

    #[test]
    fn history_survives_a_restart_via_ndjson_tail() {
        let dir = std::env::temp_dir().join(format!("placard-hist-persist-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (spec_tx, _spec_rx) = mpsc::channel();
        let mut state = StateThread::new(test_config(), &dir, spec_tx, None);
        for i in 0..3 {
            let (env, _rx) = envelope(
                Err(CommandError::BadJson(format!("e{i}"))),
                &format!("line {i}"),
            );
            state.handle(env);
        }
        drop(state);

        // A torn final line (power cut mid-append) must not poison the load.
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(dir.join("history.ndjson"))
            .unwrap();
        write!(file, "{{\"at\":\"2026-").unwrap();
        drop(file);

        let (spec_tx, _spec_rx) = mpsc::channel();
        let mut reborn = StateThread::new(test_config(), &dir, spec_tx, None);
        let entries = history_of(&mut reborn);
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].raw, "line 2");
        assert_eq!(entries[2].raw, "line 0");
        assert_eq!(entries[0].via, "tcp");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn oversized_history_log_is_compacted_at_load() {
        let dir = std::env::temp_dir().join(format!("placard-hist-boot-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // Simulate many short process lifetimes appending without ever
        // hitting the in-process compaction threshold.
        let mut log = String::new();
        for i in 0..(HISTORY_CAP * 3) {
            let entry = HistoryEntry {
                at: t0(),
                via: "tcp".into(),
                from: None,
                raw: format!("m{i}"),
                ok: true,
                error: None,
            };
            log.push_str(&serde_json::to_string(&entry).unwrap());
            log.push('\n');
        }
        std::fs::write(dir.join("history.ndjson"), log).unwrap();

        let (spec_tx, _spec_rx) = mpsc::channel();
        let _state = StateThread::new(test_config(), &dir, spec_tx, None);
        let lines = std::fs::read_to_string(dir.join("history.ndjson"))
            .unwrap()
            .lines()
            .count();
        assert_eq!(lines, HISTORY_CAP, "boot must compact an oversized log");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn history_log_is_compacted_to_the_ring() {
        let dir = std::env::temp_dir().join(format!("placard-hist-compact-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (spec_tx, _spec_rx) = mpsc::channel();
        let mut state = StateThread::new(test_config(), &dir, spec_tx, None);
        for i in 0..(HISTORY_COMPACT_EVERY + 3) {
            let (env, _rx) = envelope(Err(CommandError::BadJson("e".into())), &format!("m{i}"));
            state.handle(env);
        }
        drop(state);
        let lines = std::fs::read_to_string(dir.join("history.ndjson"))
            .unwrap()
            .lines()
            .count();
        // Compaction fired at the threshold; only the post-compact appends
        // sit on top of the ring snapshot.
        assert_eq!(lines, HISTORY_CAP + 3);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn history_is_a_ring_and_truncates_long_raw() {
        let (mut state, _spec_rx) = test_state("history-ring");
        for i in 0..(HISTORY_CAP + 5) {
            let (env, _rx) = envelope(
                Err(CommandError::BadJson(format!("e{i}"))),
                &format!("line {i} {}", "x".repeat(2000)),
            );
            state.handle(env);
        }
        let entries = history_of(&mut state);
        assert_eq!(entries.len(), HISTORY_CAP);
        // Oldest five fell off the front; newest is the last sent.
        assert!(
            entries[0]
                .raw
                .starts_with(&format!("line {}", HISTORY_CAP + 4))
        );
        assert!(entries[0].raw.ends_with('…'));
        assert!(entries[0].raw.len() < 600);
    }

    #[test]
    fn flash_show_toggles_specs_through_the_state_thread() {
        let dir = std::env::temp_dir().join(format!("placard-flash-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (spec_tx, spec_rx) = mpsc::channel();
        let (env_tx, env_rx) = mpsc::channel();
        let state = StateThread::new(test_config(), &dir, spec_tx, None);
        let thread = std::thread::spawn(move || state.run(env_rx));

        let (reply_tx, reply_rx) = mpsc::channel();
        env_tx
            .send(Envelope {
                command: Ok(Command::Show {
                    text: "SHOW STOP".into(),
                    bg: Some("8a0000".parse().unwrap()),
                    fg: Some("ffffff".parse().unwrap()),
                    flash: Flash::For(crate::scene::FLASH_DEFAULT),
                }),
                raw: r#"{"cmd":"show","text":"SHOW STOP","flash":true}"#.into(),
                via: "tcp",
                from: None,
                reply: ReplyTo::Oneshot(reply_tx),
            })
            .unwrap();
        assert!(matches!(reply_rx.recv().unwrap(), Ok(Reply::Ok)));

        // Collect specs across two-plus flash phases: both polarities must
        // appear for this text.
        let deadline = Instant::now() + Duration::from_millis(1200);
        let (mut saw_inverted, mut saw_normal) = (false, false);
        while Instant::now() < deadline {
            if let Ok(spec) = spec_rx.recv_timeout(Duration::from_millis(100))
                && spec.text_markup == "SHOW STOP"
            {
                match (spec.bg_argb, spec.text_argb) {
                    (0xffffffff, 0xff8a0000) => saw_inverted = true,
                    (0xff8a0000, 0xffffffff) => saw_normal = true,
                    other => panic!("unexpected colours {other:x?}"),
                }
            }
        }
        drop(env_tx);
        thread.join().unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(saw_inverted, "never saw inverted colours during the flash");
        assert!(saw_normal, "never saw normal colours during the flash");
    }

    #[test]
    fn endless_flash_never_settles_and_survives_a_restart() {
        // No duration means no end, however long it has been running.
        for ms in [0u64, 499, 500, 3_000, 86_400_000] {
            assert!(
                flash_invert(Duration::from_millis(ms), None).is_some(),
                "an endless flash must still be flashing at {ms}ms"
            );
        }

        let dir = std::env::temp_dir().join(format!("placard-flash-fvr-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (spec_tx, _spec_rx) = mpsc::channel();
        let mut state = StateThread::new(test_config(), &dir, spec_tx, None);

        // A timed flash is transient: it must not persist.
        let (env, _rx) = envelope(
            Ok(Command::Show {
                text: "BRIEF".into(),
                bg: None,
                fg: None,
                flash: Flash::For(Duration::from_secs(3)),
            }),
            "",
        );
        state.handle(env);
        let (spec_tx, _spec_rx) = mpsc::channel();
        let reborn = StateThread::new(test_config(), &dir, spec_tx, None);
        assert!(
            reborn.active_flash.is_none(),
            "a timed flash must not resume after a restart"
        );

        // An endless one is part of the state and must come back flashing.
        let (env, _rx) = envelope(
            Ok(Command::Show {
                text: "SHOW STOP".into(),
                bg: None,
                fg: None,
                flash: Flash::Forever,
            }),
            "",
        );
        state.handle(env);
        let (spec_tx, _spec_rx) = mpsc::channel();
        let mut reborn = StateThread::new(test_config(), &dir, spec_tx, None);
        assert!(matches!(
            reborn.active_flash,
            Some(RunningFlash { duration: None, .. })
        ));
        assert!(matches!(
            reborn.flash_status(),
            Some(status::FlashStatus::Forever { .. })
        ));

        // And any later command ends it, in the restarted process too.
        let (env, _rx) = envelope(Ok(Command::Clear), "");
        reborn.handle(env);
        assert!(reborn.active_flash.is_none());
        let (spec_tx, _spec_rx) = mpsc::channel();
        let after_clear = StateThread::new(test_config(), &dir, spec_tx, None);
        assert!(after_clear.active_flash.is_none());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn flash_starts_inverted_and_settles_after_three_seconds() {
        let at =
            |ms: u64| flash_invert(Duration::from_millis(ms), Some(crate::scene::FLASH_DEFAULT));
        assert_eq!(at(0), Some(true));
        assert_eq!(at(499), Some(true));
        assert_eq!(at(500), Some(false));
        assert_eq!(at(999), Some(false));
        assert_eq!(at(1000), Some(true));
        assert_eq!(at(2500), Some(false));
        assert_eq!(at(2999), Some(false));
        assert_eq!(at(3000), None);
        assert_eq!(at(60_000), None);
    }

    #[test]
    fn derive_invert_swaps_colours_only() {
        let scene = Scene {
            bg: "8a0000".parse().unwrap(),
            fg: "ffffff".parse().unwrap(),
            content: Content::Text { text: "X".into() },
        };
        let normal = derive_spec(&scene, t0(), &test_config(), false);
        let inverted = derive_spec(&scene, t0(), &test_config(), true);
        assert_eq!(inverted.bg_argb, normal.text_argb);
        assert_eq!(inverted.text_argb, normal.bg_argb);
        assert_eq!(inverted.text_markup, normal.text_markup);
        assert_eq!(inverted.text_px, normal.text_px);
    }

    #[test]
    fn derive_colours_pack_opaque() {
        let scene = Scene {
            bg: "#8a0000".parse().unwrap(),
            fg: "#ffffff".parse().unwrap(),
            content: Content::Text { text: "X".into() },
        };
        let spec = derive_spec(&scene, t0(), &test_config(), false);
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
            flash_forever: false,
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
