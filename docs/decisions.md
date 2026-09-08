# Placard — decisions and rationale

Companion to `DESIGN.md`. This file records *why*; `DESIGN.md` records *what*. Nothing here is needed to implement the system.

## Hardware

**MeLE Cyber X1 (N150).** Fanless, BIOS auto-power-on, RTC with coin cell, 24/7 rated, ~£200. 8 GB/128 GB eMMC is sufficient — the whole process is under 300 MB resident — but the 16 GB/512 GB variant has a replaceable NVMe stick, which is easier to image and swap than eMMC. Buy whichever is in stock; don't pay extra for RAM.

**USB-C power.** Not a locking connector. Cable-tie it at the box in the flight case.

**Why not a Raspberry Pi.** Would work for this load, but the N150 has an x86 GStreamer/NDI ecosystem, a proper BIOS with restore-on-AC, and a sane NVMe path. The price difference is under £100.

**Why not a BirdDog/Kiloview-style appliance.** They decode NDI; they don't run your code.

## OS

**Debian 13, not 12.** The N150's iGPU (PCI 8086:46D4) needs kernel ≥ 6.11/6.12 for `i915` to bind. Debian 12 ships 6.1. Bookworm + backports kernel works but there's no reason to.

**Not Alpine.** `libgstreamer` is fine on musl, but if NDI or any other glibc-only binary ever enters the picture, `gcompat` is a bad place to be. Debian-minimal is already small enough that a smaller distro gains seconds of boot and nothing else.

**Not NixOS / immutable ostree.** Strong choice for a fleet; over-engineering for one or two boxes. Revisit if the count grows.

**Root filesystem read-write.** ext4 with journaling plus atomic state writes is adequate for a device that gets power-pulled. A read-only root with `overlayroot` is the next hardening step if a filesystem ever does get corrupted.

**Service runs as root.** `kmssink` needs to become DRM master. A non-root user in `video` can usually do that if nothing else has the device, but "usually" is the wrong word for a show box, and there's no attack surface to protect on a single-purpose appliance on an isolated network.

## Language and rendering

| Option | Verdict |
|---|---|
| Rust + `gstreamer-rs` | Pango does wrapping/centring/shrink-to-fit; `kmssink` does DRM, modes, hotplug; `compositor` does layers. Application code is a state machine plus property sets. Bindings are maintained by the GStreamer project. **Chosen.** |
| Rust, direct DRM + pangocairo | Fewer runtime deps, but we own page-flipping, hotplug and mode handling. More bespoke code for the same picture. |
| Go + go-gst | cgo, third-party bindings, thinner maintenance. Go's fine for the network half but the rendering half is the risky part. |
| Zig | No usable GStreamer or Pango story. |
| Chromium kiosk + WebSocket bridge | Best typography, worst boot time and failure surface for a show box. |

**`compositor` from day one** even though v1 needs only background + text + clock. The marginal cost is ~1 GB/s of blending on a chip that doesn't notice, and it means fades, extra regions and media are pad additions rather than a rewrite.

**Text on a transparent BGRA canvas** rather than `textrender`. `textoverlay` is property-driven and easy; `textrender` wants a text *stream*. If glyph-edge alpha fringes on the real display, switch to `textrender` fed by `appsrc` with the same pad layout.

**Three transports.** OSC because QLab. HTTP because everything else. NDJSON over TCP because the brief said "some other TCP thing" and this is the least surprising answer at ~40 lines. All three normalise to one `Command` enum so the cost of the third is small.

**No auth, no host firewall.** OSC can't have auth, so adding it to HTTP alone protects nothing. The show control network is isolated; that is the access control. If that ever stops being true, the fix is a VLAN, not passwords.

**No web UI.** The client is QLab. A UI is a second product with its own failure modes; canned messages cover the "non-engineer needs to change the screen" case.

## Packaging and deployment

**`.deb` rather than a raw binary.** The binary is dynamically linked against trixie's GStreamer; the package declares those dependencies and `apt` enforces them. It also carries the unit file, config skeleton and state directory. Upgrade/downgrade/remove are one `apt` action, and the Ansible task is one line. `cargo deb` makes producing it nearly free.

**Build in a `debian:trixie` container**, including for local `linux-bin`. Cross-compiling Rust against Linux GStreamer headers from macOS is possible and miserable, and CI already does it correctly in two minutes.

**Ansible tracks the latest release** (revisited from "pin a version"): every push to main releases, so the newest release is by definition what's meant to be deployed, and boxes shouldn't lag it silently. Rollback and show-run freezes still work by setting `placard_release` to a release tag. Hand-editing config on the box works but drifts; documented as such.

## Behaviour

**Countdowns go negative.** A stage manager wants to know "we're two minutes over", not "it's done". No `on_zero` text; a swap at zero is a QLab cue at that time.

**Clock is always on, no command.** One less thing to get wrong in a cue stack; if it's ever in the way, hide it in config and restart.

**`clear` is blank screen.** Black background, no text, clock still visible. It is not "back to boot scene"; that's `canned house_closed`.

**Boot scene is `house_closed`** when there is no saved state. Safest default for a house-facing display.

## Development

**Develop on macOS, verify on the box.** Homebrew's GStreamer bundle has every plugin used. The only platform seam is the sink factory. (Revisited: the ntp probe was removed with the rest of time management — the OS clock is trusted.) QLab being on the same Mac is the real advantage: the cue stack gets built against `localhost` before the hardware exists.

**Golden images are Linux-only.** Homebrew and Debian font stacks differ enough to fail pixel comparisons; the Mac is for eyeballing, the trixie container is for asserting.
