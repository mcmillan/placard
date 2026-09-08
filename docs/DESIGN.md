# Placard — design doc

Status: v1, ready for implementation · Owner: Josh · Target hardware: MeLE Cyber X1 (Intel N150)

## 1. Summary

Placard is a single-purpose appliance that drives one 1080p display with a solid background colour and centred, wrapped text. It is controlled over the network (OSC, newline-delimited JSON over TCP, or plain HTTP), supports canned messages, arbitrary text and countdowns, survives power loss by resuming its last scene, and fails to black with automatic restart.

It is one Rust binary, packaged as a `.deb`, built on GitHub Actions, installed and configured by Ansible onto a stock Debian 13 install.

## 2. Goals and non-goals

Goals

- 1080p output; background colour, text colour, text centred both axes, word-wrapped, auto-shrunk to fit.
- Rendering built as composited layers from day one, so later features (fades, extra regions, a second text layer) are pad additions and property changes, not a re-architecture.
- Control via OSC (UDP), NDJSON over TCP, HTTP. All three map onto one command set.
- Canned messages (from config), arbitrary text, countdowns to an ISO 8601 UTC instant or to N seconds from receipt. Countdowns run through zero and keep counting negative.
- A wall clock (`HH:MM:SS`, local time) always present in a corner.
- Power loss → same scene back on screen with no operator action.
- Any fault → black, then automatic restart. Hung process or hung kernel → watchdog reboot.
- Debian; Rust; binary from CI; Ansible provisioning; documented deploy.

Non-goals (v1)

- Video, images, fades or animation.
- Multiple displays. Multiple regions/layers are supported by the pipeline but not exposed by any v1 command.
- Authentication on any interface (see §9).
- Any UI or operator tooling. The client is QLab or whatever else speaks OSC/TCP/HTTP.
- Read-only root filesystem (see §11, later hardening).

## 3. Hardware and OS

| Item | Decision |
|---|---|
| Box | MeLE Cyber X1 (Intel N150, fanless, BIOS auto-power-on, RTC) |
| OS | Debian 13 (trixie). Kernel ≥ 6.12 is required: the N150 iGPU (PCI 8086:46D4) is not driven by Debian 12's 6.1 |
| Display path | DRM/KMS directly via `kmssink`. No X, no Wayland |
| Output mode | Pinned on the kernel cmdline, `video=HDMI-A-1:1920x1080@50D`. The rate comes from the inventory var `hdmi_rate` (default 50), which Ansible also writes into `config.toml` as `display.fps`. **The two must match**; a mismatch means dropped or duplicated frames |
| Clock | `chrony`; system timezone stays UTC |
| Service user | root. `kmssink` wants DRM master, this is a single-purpose appliance, and a dedicated user + `video` group + udev rules buys nothing here. Do not add them |

Rationale for these choices is in `docs/decisions.md`.

Kernel cmdline additions (via Ansible, `/etc/default/grub`):

```
quiet loglevel=0 consoleblank=0 vt.global_cursor_default=0 video=HDMI-A-1:1920x1080@50D
```

`getty@tty1` is masked so nothing else contends for the console.

## 4. Language and rendering stack

**Rust + GStreamer (`gstreamer-rs`).**

Rendering is a fixed pipeline held open for the life of the process:

```
compositor name=comp background=black
  ! video/x-raw,width=1920,height=1080,framerate=50/1
  ! videoconvert ! kmssink

# layer 0: background colour
videotestsrc name=bg pattern=solid-color is-live=true
  ! video/x-raw,format=BGRA,width=1920,height=1080,framerate=50/1
  ! comp.sink_0

# layer 1: text on a transparent canvas
videotestsrc name=canvas pattern=solid-color foreground-color=0x00000000 is-live=true
  ! video/x-raw,format=BGRA,width=1920,height=1080,framerate=50/1
  ! textoverlay name=text wrap-mode=word-char line-alignment=center
                halignment=center valignment=center auto-resize=true
                xpad=96 ypad=64
  ! comp.sink_1

# layer 2: wall clock, small, bottom-right
videotestsrc name=clockcanvas pattern=solid-color foreground-color=0x00000000 is-live=true
  ! video/x-raw,format=BGRA,width=1920,height=1080,framerate=50/1
  ! textoverlay name=clock halignment=right valignment=bottom
                font-desc="Inter Semibold 40" xpad=48 ypad=32
  ! comp.sink_2
```

`compositor` from day one; v1 uses three pads (background, main text, clock). Each layer is a `compositor` sink pad with its own `alpha`, `xpos`/`ypos`, `width`/`height` and `zorder`, and the text layer is a transparent BGRA canvas that `textoverlay` draws onto. What that buys later, without touching the architecture:

- Fades: animate `sink_1.alpha` (or `sink_0` for a background crossfade) from the state thread.
- More regions (footer, corner badge): another `videotestsrc ! textoverlay ! comp.sink_N` with `xpos`/`ypos`/`height` set — the clock layer is the worked example.
- Images/video: `filesrc ! decodebin ! comp.sink_N`.

Cost: three BGRA 1080p50 sources being blended, ~1 GB/s of memory traffic. Trivial on the N150; measured during the soak (§13).

State changes are property sets on `bg` (`foreground-color`), `text` and `clock` (`text`, `color`, `font-desc`) and compositor pads. They take effect on the next frame, so every v1 command is a one-frame cut.

Alternatives considered are in `docs/decisions.md`. The binary dynamically links libgstreamer and the plugin set; those are apt dependencies declared in the `.deb`.

### Rendering rules

These are the details an implementer would otherwise discover the hard way.

- **`textoverlay` interprets `text` as Pango markup.** Every string that came from the network or config must go through `glib::markup_escape_text` before being set. A `<` in a cue must render as `<`, not break the layout. This applies to `show`, canned text and countdown labels.
- **Countdown with label** is one `textoverlay`, text set to `<span size="60%">{escaped label}</span>\n{digits}`. Without a label it's just `{digits}`.
- **Colour encoding.** `videotestsrc.foreground-color` is `u32` `0xAARRGGBB`. Background layer: alpha `0xFF`. Canvas layers: `0x00000000` exactly, or the text layer will occlude the background. `textoverlay.color` is the same `0xAARRGGBB` layout.
- **`compositor` must have `background=black`** so transparent regions of upper pads composite over the background pad and not over garbage.
- **Property sets happen on the render thread only**, via a channel; never from a network thread. GStreamer element properties are thread-safe but the render thread owning them keeps ordering deterministic.
- **The wall clock** is `textoverlay name=clock` with `font-desc` and position from `[clock]` config; `position` maps to `halignment`/`valignment` pairs.

## 5. Process architecture

One binary, `placard`, one process, three threads:

```
 OSC (UDP 9000) ─┐
 TCP NDJSON 9001 ─┼─▶ Command channel ─▶ State thread ─▶ Render thread (GStreamer main loop)
 HTTP 8080 ───────┘                            │
                                               └─▶ state.json (atomic write)
```

- **Listeners** parse their wire format into a `Command` and send it on an `mpsc` channel. Parse errors are logged and dropped; they never reach the renderer.
- **State thread** owns the single source of truth (`Scene`), applies commands, persists on every change, and pushes a `RenderSpec` to the render thread. It also owns a 250 ms ticker that re-derives the countdown string and the wall-clock string, and only pushes a new spec when a displayed string actually changes (i.e. once a second).
- **Render thread** runs the GLib main loop, applies `RenderSpec`s as property sets, watches the GStreamer bus, and sends the systemd watchdog heartbeat once per second.

Any GStreamer bus error, thread panic (`panic = "abort"`), or lost heartbeat terminates the process. Recovery is systemd's job (§7).

### Threading rules

- **No async runtime touches GStreamer.** The render thread runs a `glib::MainLoop` and nothing else. If tokio is used for the network listeners, it lives on its own thread(s) and communicates with the state thread over `std::sync::mpsc` only.
- The state thread is plain `std::thread` with a `recv_timeout` loop (250 ms) that doubles as the ticker.
- Channels: `mpsc::Sender<(Command, ReplyTo)>` into the state thread; `mpsc::Sender<RenderSpec>` into the render thread. `ReplyTo` is an enum (`Osc(SocketAddr)`, `Oneshot(Sender<Result>)`, `None`) so acks route back to the right transport.
- No `unwrap`/`expect` on network input or config-derived values in the state or render threads. `unwrap` on programmer invariants is fine.

## 6. Data model and command set

### Scene

```rust
struct Scene {
    bg: Rgb,
    fg: Rgb,
    content: Content,
}

enum Content {
    Text(String),
    Countdown {
        target: DateTime<Utc>,
        label: Option<String>,       // shown above the countdown digits
    },
}
```

Colours are `rrggbb` strings on the wire. Canned messages are named `Scene`s in config; a canned command may override `bg`/`fg`.

### Commands

| Command | Effect |
|---|---|
| `show { text, bg?, fg? }` | Arbitrary text; colours default to current |
| `canned { id, bg?, fg? }` | Load a canned scene |
| `colour { bg?, fg? }` | Change colours, keep content |
| `countdown_to { target, label?, bg?, fg? }` | Countdown to ISO 8601 UTC instant |
| `countdown_secs { secs, label?, bg?, fg? }` | Converted to an absolute target at receipt, then identical to above |
| `clear` | Black background, empty text |
| `status` | (HTTP/TCP only) returns current scene, uptime, clock sync state |

Runtime errors — unknown canned id, malformed JSON, bad colour string, unparseable ISO 8601, wrong OSC arg types — are replied to on the originating transport, logged at `warn`, and otherwise ignored. The current scene is untouched. Nothing a client sends can terminate the process.

Countdown format: `M:SS` under an hour, `H:MM:SS` otherwise. Past the target it continues with a leading minus (`-0:07`, `-1:02:15`) indefinitely; it does not stop or change colour on its own. A `show`/`canned`/`clear` ends it.

Wall clock: `HH:MM:SS` in the configured timezone, bottom-right, updated once a second. It is a layer, not part of `content` or `Scene`, so no command touches it; it is on screen from boot, across every cue, including `clear`.

### Wire formats

**JSON schema** (HTTP body and TCP lines are identical):

```rust
#[derive(Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case", deny_unknown_fields)]
enum Command {
    Show          { text: String, bg: Option<Rgb>, fg: Option<Rgb> },
    Canned        { id: String, bg: Option<Rgb>, fg: Option<Rgb> },
    Colour        { bg: Option<Rgb>, fg: Option<Rgb> },
    CountdownTo   { target: DateTime<Utc>, label: Option<String>, bg: Option<Rgb>, fg: Option<Rgb> },
    CountdownSecs { secs: u32, label: Option<String>, bg: Option<Rgb>, fg: Option<Rgb> },
    Clear,
    Status,
}
```

`Rgb` deserialises from `rrggbb` (case-insensitive, no short form, no alpha; a leading `#` is tolerated on input but never emitted — bare hex avoids shell quoting and QLab cue escaping). Unknown fields are an error, not ignored.

Examples:

```json
{ "cmd": "show", "text": "STAND BY", "bg": "000000", "fg": "ffffff" }
{ "cmd": "countdown_secs", "secs": 300, "label": "House opens in" }
{ "cmd": "canned", "id": "go" }
```

**HTTP** — port 8080. `POST /api/command` with the JSON above; `200 {"ok":true}` or `400 {"ok":false,"error":"…"}`. `GET /api/status` returns:

```json
{
  "ok": true,
  "scene": { "bg": "8a0000", "fg": "ffffff",
             "content": { "kind": "countdown", "target": "2026-09-08T18:30:00Z", "label": "House opens in", "display": "-0:42" } },
  "canned_id": null,
  "uptime_secs": 8123,
  "ntp_synced": true,
  "clock_offset_ms": 3,
  "version": "0.3.1",
  "last_command": { "at": "2026-09-08T18:29:10Z", "via": "osc", "from": "192.168.10.20:53101" }
}
```

`content.kind` is `"text"` or `"countdown"`; `canned_id` is set when the current scene came from a `canned` command unmodified. No other HTTP routes exist.

**TCP** — port 9001, one JSON object per line, one JSON reply line per command, same bodies as HTTP. Connections may stay open and send many commands. A line over 64 KiB or invalid UTF-8 gets an error reply and the connection is closed.

**OSC** — UDP 9000. Colours as `rrggbb` strings so QLab cues stay readable.

| Address | Args |
|---|---|
| `/placard/show` | `s text` `[s bg] [s fg]` |
| `/placard/canned` | `s id` `[s bg] [s fg]` |
| `/placard/colour` | `s bg` `[s fg]` |
| `/placard/countdown/to` | `s iso8601` `[s label]` |
| `/placard/countdown/secs` | `i secs` `[s label]` |
| `/placard/clear` | — |

Every accepted OSC message is acknowledged with `/placard/ok` to the message's source address and port; rejected ones with `/placard/error s reason`. Bundles are unpacked and each message handled independently. Unknown addresses get `/placard/error`.

## 7. Failure and recovery

| Failure | Behaviour |
|---|---|
| Power loss | BIOS auto-power-on → Debian boots → `placard.service` starts → pipeline comes up black → `state.json` is loaded → last scene applied. Countdowns are absolute targets so they resume correctly. ~15 s cold to picture |
| Process panic / GStreamer error | Process exits non-zero. systemd `Restart=always`, `RestartSec=1`. On start the pipeline is black until state is loaded, so the visible fault is ≤ 2 s of black |
| Render loop hangs | No heartbeat → systemd `WatchdogSec=10` kills and restarts |
| Kernel hang | `RuntimeWatchdogSec=30` in `/etc/systemd/system.conf` arms the iTCO hardware watchdog; a wedged kernel reboots the box |
| Display unplugged / replugged | kmssink handles hotplug; the pinned mode means it comes back at 1080p |
| Missing or corrupt `state.json` | Fails to parse → log, show `defaults.boot_scene`, carry on. Writes are tmpfile + `rename`, so this needs a filesystem-level corruption to happen |
| Wrong clock | `/api/status` shows `ntp_synced: false`. Relative countdowns are unaffected |

`placard.service`:

```ini
[Unit]
After=network-online.target
Wants=network-online.target

[Service]
ExecStart=/usr/bin/placard --config /etc/placard/config.toml
Restart=always
RestartSec=1
WatchdogSec=10
Type=notify
StateDirectory=placard

[Install]
WantedBy=multi-user.target
```

No `User=` line: the service runs as root (§3). Journald is capped (`SystemMaxUse=200M`) so logs can't fill the disk. Logging is via `tracing` with the `tracing-journald` layer under systemd and a plain stderr layer otherwise; default level `info`, overridable with `RUST_LOG`.

## 8. Time

Absolute countdowns and the on-screen wall clock both need a correct clock, and the wall clock additionally needs the right timezone (config, default `Europe/London`; the box's system TZ stays UTC). The venue provides both internet and a LAN NTP server; `chrony` uses the LAN server (from inventory) with `pool.ntp.org` as fallback, and the RTC coin cell covers the gap between power-on and first sync. The status endpoint exposes `ntp_synced` and `clock_offset_ms` so it's visible before the show, not during.

Relative countdowns (`countdown_secs`) never depend on wall-clock correctness.

## 9. Security

LAN appliance; threat model is "stop the wrong person on the venue Wi-Fi changing the screen by accident."

- No authentication on any control port. OSC can't have it, and bolting it onto HTTP alone protects nothing.
- No host firewall. The box lives on the isolated show control network; that isolation is the access control.
- SSH: key-only, from Ansible.

## 10. Configuration

`/etc/placard/config.toml`, templated by Ansible from inventory vars:

```toml
[display]
width = 1920
height = 1080
fps = 50                     # must equal the kernel video= rate; Ansible sets both from hdmi_rate
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
position = "bottom-right"   # top-left | top-right | bottom-left | bottom-right

[defaults]
bg = "000000"
fg = "ffffff"
boot_scene = "house_closed"   # shown when there is no saved state

[canned.house_closed]
text = "HOUSE CLOSED"
bg = "8a0000"

[canned.house_open]
text = "HOUSE OPEN"
bg = "0b6e2e"

[canned.five_minute]
text = "5 MINUTE WARNING"
bg = "b36b00"

[canned.awaiting_clearance]
text = "AWAITING CLEARANCE"
bg = "b36b00"

[canned.go]
text = "GO"
bg = "0b6e2e"

[canned.show_stop]
text = "SHOW STOP"
bg = "8a0000"

[canned.cans_on]
text = "PUT YOUR CANS ON"
bg = "1f4e9e"
```

Colours above are placeholders; the ids are the contract, since they're what QLab cues reference.

Config is read once at start. Changing canned messages is an Ansible run (§12); editing the file by hand and `systemctl restart placard` also works but drifts from inventory.

## 11. Repository and build

```
placard/
  Cargo.toml
  src/
    main.rs          # arg parsing, thread spawn
    command.rs       # Command enum, JSON + OSC parsing, tests
    scene.rs         # Scene, Content, countdown formatting, tests
    state.rs         # state thread, persistence
    render.rs        # GStreamer pipeline, property application, bus watch
    net/{osc,tcp,http}.rs
  packaging/
    placard.service
    config.toml.example
  ansible/
    inventory/hosts.yml
    group_vars/placard.yml
    site.yml
    roles/base/      # kernel cmdline, chrony, watchdog, journald, sshd
    roles/placard/   # apt deps, .deb install, config template, service
  tests/
    scenes/*.json      # snapshot fixtures
    golden/*.png       # generated in the trixie container only
  docs/
    deploy.md, protocol.md, decisions.md
  CLAUDE.md            # implementer rules; see below
  Makefile
  Dockerfile.build     # debian:trixie + rust toolchain, used by linux-bin, goldens and CI
  .github/workflows/release.yml
```

### Crates

| Purpose | Crate | Notes |
|---|---|---|
| Media | `gstreamer`, `gstreamer-video` | Official bindings |
| OSC | `rosc` | Decode/encode; UDP socket is `std::net` |
| HTTP | `axum` on `tokio` | Confined to the HTTP thread; see §5 threading rules |
| TCP | `std::net` | Blocking, one thread per connection; no runtime needed |
| Serialisation | `serde`, `serde_json`, `toml` | |
| Time | `chrono`, `chrono-tz` | |
| Watchdog | `sd-notify` | Pure Rust, no-op without `NOTIFY_SOCKET` |
| Logging | `tracing`, `tracing-subscriber`, `tracing-journald` | |
| CLI | `clap` (derive) | `--config`, `--sink`, `--state-dir`, `--snapshot` |
| Errors | `anyhow` at the edges, `thiserror` for `CommandError` | |
| Packaging | `cargo-deb` | Dev tool, not a dependency |

No other crates without a stated reason in the PR. In particular: no `glib`-async bridges, no `image` crate (PNG output is `pngenc` in the pipeline), no config-reloading libraries.

### Makefile

| Target | Does |
|---|---|
| `run` | `cargo run -- --config packaging/config.toml.example --sink auto --state-dir ./state` |
| `test` | `cargo fmt --check && cargo clippy --all-targets -D warnings && cargo test` |
| `goldens` | Regenerate `tests/golden/*.png` inside the trixie container |
| `linux-bin` | Release build inside `debian:trixie` via Docker; output `target/linux/placard` |
| `deploy-dev` | `scp` that binary to `$BOX`, `systemctl restart placard`, tail the journal |
| `release` | `git push origin main`; every push to main releases, CI does the rest |

### CI

`release.yml`, on every push to `main` (versioned `<cargo-version>-<run-number>`, e.g. `0.1.0-37`, released under the tag `v0.1.0-37`):

1. Runs in a `debian:trixie` container so glibc and GStreamer headers match the target exactly.
2. `cargo test`, `cargo clippy -D warnings`.
3. `cargo deb` → `placard_<ver>_amd64.deb`, with `Depends:` on the GStreamer runtime packages and `gstreamer1.0-plugins-base`, `gstreamer1.0-plugins-good`, `gstreamer1.0-plugins-bad` (kmssink), `gstreamer1.0-x` (pango textoverlay), `fonts-inter`.
4. Uploads the `.deb` and its `sha256` to a GitHub Release.

`cargo deb` also installs the unit file, the example config and creates `/var/lib/placard`.

Local development is on macOS; see §14.

## 12. Deployment process

### First-time provisioning of a box

1. Flash Debian 13 netinst to USB. Install with: no desktop, SSH server, hostname `placard-01`, static IP or DHCP reservation. Add your SSH key. (A preseed file in `docs/` is optional but makes this repeatable.)
2. In BIOS: Auto Power On → enabled. Boot order → internal disk first.
3. Add the box to `ansible/inventory/hosts.yml`:
   ```yaml
   placard:
     hosts:
       placard-01:
         ansible_host: 192.168.10.50
         placard_version: "0.3.1"
         hdmi_rate: 50
   ```
4. `ansible-playbook -i inventory site.yml`. The playbook:
   - installs base packages and chrony;
   - sets the kernel cmdline and runs `update-grub`;
   - masks `getty@tty1`, sets the systemd hardware watchdog, caps journald;
   - downloads the pinned `.deb` from GitHub Releases, verifies the checksum, installs it;
   - templates `config.toml` (canned messages come from `group_vars`);
   - enables and starts `placard.service`;
   - reboots if the cmdline changed.
5. Verify: screen shows the `house_closed` canned scene (the boot default); `curl http://placard-01:8080/api/status` returns `ntp_synced: true`.
6. Pull the power. Confirm it comes back to the same scene unattended. This step is not optional.
7. `dd` the disk to a golden image and keep it with the spare.

### Upgrading

Bump `placard_version` in inventory, run the playbook. Downgrades are the same operation with an older version. The playbook is idempotent; running it with no changes does nothing.

### Changing canned messages

Edit `group_vars/placard.yml`, run the playbook. It re-templates the config and restarts the service (≈1 s of black).

## 13. Testing plan

- Unit: parsing (every command, every wire format, malformed input), countdown formatting boundaries (59 s, 1 h, zero crossing, `-0:01`, `-1:00:00`), clock formatting across DST change, persistence.
- Bench (macOS, §14): `--sink auto`; QLab on the same Mac, `oscsend`, `curl`, `nc`.
- Headless render: `placard --snapshot <scene.json> <out.png>` builds the real pipeline with `--sink png`, applies the scene, renders one frame and exits. Scene fixtures live in `tests/scenes/*.json` (same JSON as `/api/status`'s `scene` field, plus a fixed `now` so countdown and clock strings are deterministic). Goldens live in `tests/golden/<name>.png`, generated only inside the trixie container, compared with a per-pixel tolerance of 2/255 and an allowed differing-pixel fraction of 0.1 %. Initial fixture set: `text_short`, `text_wrapping`, `text_overflow_shrinks`, `countdown_positive`, `countdown_negative_with_label`, `clear`, `clock_only`.
- Hardware soak before first show: 24 h on the actual box driving the actual display, a countdown running, `journalctl -f` checked for restarts. Three power pulls at random points.
- Thermal: `sensors` after the soak; N150 should sit well under 70 °C in a fanless case at this load.

## 14. Developing and testing on macOS

Everything except `kmssink` and systemd is portable, so the day-to-day loop is on the Mac and the Linux box is a verification target, not a dev environment.

### Toolchain

```
brew install rustup pkg-config gstreamer liblo ansible
brew install --cask font-inter
rustup default stable
```

Homebrew's `gstreamer` formula bundles base/good/bad/ugly, so `compositor`, `textoverlay` (pango) and `autovideosink` are all present; `pkg-config` finds it and `gstreamer-rs` builds with no further setup. `liblo` provides `oscsend` for the command line. No Docker needed for the inner loop.

### Portability seams

| Concern | Linux (target) | macOS (dev) | How it's handled |
|---|---|---|---|
| Video sink | `kmssink` | `autovideosink` → `glimagesink` window | `--sink kms|auto|png`. One factory function; nothing else in `render.rs` knows which |
| Watchdog heartbeat | `sd_notify` via `NOTIFY_SOCKET` | no systemd | `sd-notify` crate is pure Rust and a no-op when the env var is absent; no `cfg` needed |
| `ntp_synced` in status | `adjtimex`/`timedatectl` | not applicable | `cfg(target_os)`; macOS returns `"unknown"` |
| State dir | `/var/lib/placard` via `StateDirectory` | `./state/` | `--state-dir` flag, default from `$STATE_DIRECTORY` |
| Fonts | `fonts-inter` deb | `font-inter` cask | Same family name in config; Pango resolves it via fontconfig on both |
| Font metrics | Debian freetype/fontconfig | Homebrew freetype/fontconfig | Close but not identical. Layout is eyeballed on the Mac and *asserted* only against Linux-rendered goldens (below) |
| Mode/timing | pinned 1080p50 | window at 1080p, vsync'd to the Mac's display | Irrelevant to logic; render timing is verified on the box |

Rule: no `cfg(target_os = "macos")` outside `status.rs` and the sink factory. If a third one appears, that's a design smell to fix rather than paper over.

### Inner loop

```
cargo run -- --config packaging/config.toml.example --sink auto --state-dir ./state
oscsend localhost 9000 /placard/canned s house_open
oscsend localhost 9000 /placard/countdown/secs i 90 s "House opens in"
curl -d '{"cmd":"show","text":"A rather longer line to check wrapping behaves"}' localhost:8080/api/command
printf '{"cmd":"clear"}\n' | nc localhost 9001
```

QLab runs on the same Mac, so the real cue stack can be built and fired at `localhost` before the box exists. This is the main reason the Mac is a good dev environment for this project rather than a compromise.

### Test tiers

| Tier | Where | What |
|---|---|---|
| Unit | Mac and CI | Command parsing for all three wire formats, malformed input, countdown/clock formatting, state round-trip. `cargo test`, no GStreamer needed for most of it |
| Pipeline smoke | Mac and CI | Construct the pipeline with `--sink png`, render one frame, assert it's 1920×1080 and not all one colour. Catches missing plugins and caps negotiation failures |
| Golden images | CI only (`debian:trixie` container) | `--snapshot` for a fixed set of scenes; compare to committed PNGs with a small tolerance. Goldens are generated in the same container image, so font rendering is deterministic. Regenerate deliberately with `make goldens` |
| Interactive | Mac | Window + QLab. Eyeball wrapping, auto-resize, long strings, negative countdowns |
| Target | The box on the bench | Everything in §13: real display, mode, watchdog, power pulls, soak |

### Building for the target

Not from the Mac. Cross-compiling Rust against Linux GStreamer headers is possible and miserable; the payoff is nil because CI does it in the right container in under two minutes. For a faster-than-release loop when working on the box:

```
make linux-bin     # cargo build --release inside debian:trixie via Docker/OrbStack, cargo cache in a volume
make deploy-dev    # scp target/linux/placard to the box, systemctl restart placard
```

`deploy-dev` is for bench iteration only. Anything that goes to a venue goes through a CI release and Ansible (§12).

### Bench setup

Box, its PSU, a 1080p monitor, one Ethernet cable into the same switch as the Mac. Static IP in inventory. That's the whole rig; keep it assembled in a box under the desk for the life of the project.

## 15. Open questions

1. `textoverlay` onto a fully transparent BGRA canvas is a well-trodden trick but the resulting alpha at glyph edges should be eyeballed on the real display during milestone 1; if it fringes, the fallback is `textrender` fed from an `appsrc`, same pad layout.

## 16. Milestones

Each milestone is done when every line in its "done when" list is true. `make test` clean is implied for all of them.

**M1 — Pipeline and OSC**
- `make run` opens a 1920×1080 window showing `defaults.boot_scene` with the clock bottom-right (no saved state on first run).
- `oscsend localhost 9000 /placard/show s HELLO` changes the text within one frame; `/placard/colour s "ff0000"` changes the background; `/placard/clear` blacks it. Sender receives `/placard/ok`.
- A 400-character string wraps and shrinks to fit inside the padding; `<b>` in a string renders literally.
- `placard --snapshot tests/scenes/text_short.json /tmp/a.png` writes a 1920×1080 PNG.
- On the real box: the same binary under `--sink kms` shows the same picture; text edges have no visible fringing.

**M2 — Full command set and persistence**
- All seven commands work over all three transports; every error case in §6 produces the documented reply and leaves the scene unchanged.
- `countdown_secs 5` reaches `0:00` and continues to `-0:01`, `-0:02`… `countdown_to` with a past target starts negative immediately.
- Kill the process mid-countdown, restart it: the countdown resumes at the correct value.
- `GET /api/status` matches the schema in §6 field for field.
- Golden tests pass in the trixie container.

**M3 — Packaging**
- `cargo deb` produces a `.deb` that installs on a clean trixie with `apt install ./placard_*.deb` and nothing else.
- `systemctl status placard` shows `Type=notify` healthy; `kill -STOP` on the process gets it restarted by the watchdog within 15 s.
- Pushing to `main` produces a GitHub Release with the `.deb` and checksum.

**M4 — Provisioning**
- `ansible-playbook site.yml` against a fresh netinst box brings it to a working appliance with no manual steps after the SSH key.
- Running the playbook a second time reports zero changes.
- Pull the power mid-countdown; the box returns to the same countdown unattended, within 30 s.

**M5 — Soak and docs**
- 24 h on the box: zero service restarts in the journal, CPU temperature under 70 °C, no dropped-frame warnings from `kmssink`.
- `docs/protocol.md` and `docs/deploy.md` are sufficient for someone else to fire cues and provision a second box.
