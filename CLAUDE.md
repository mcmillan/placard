# CLAUDE.md — Placard

Read `DESIGN.md` first. It is the specification; this file is the set of rules you follow while implementing it. Where they conflict, ask.

## What this is

A Rust binary that drives one 1080p display via GStreamer (`compositor` + `textoverlay` + `kmssink`) and takes commands over OSC, NDJSON/TCP and HTTP. It runs as root under systemd on Debian 13 on a fanless mini PC in a theatre. Reliability beats everything else.

## Non-negotiables

1. **No async runtime touches GStreamer.** The render thread runs a `glib::MainLoop` and nothing else. tokio, if used, stays on the HTTP thread. Threads talk over `std::sync::mpsc` only.
2. **All text reaching `textoverlay` is escaped with `glib::markup_escape_text`.** It's Pango markup. `<` in a cue must render as `<`. The only unescaped markup is the `<span size="60%">` wrapper the code itself adds around countdown labels.
3. **Nothing a client sends can terminate the process.** Bad input → error reply, `warn` log, scene unchanged. No `unwrap`/`expect` on anything derived from the network, config, or `state.json`.
4. **`cfg(target_os)` appears in exactly two places:** the sink factory in `render.rs` and the `ntp_synced` probe in `status.rs`. If you think you need a third, stop and say so.
5. **No new crates** beyond the list in `DESIGN.md` §11 without stating why in the PR description. No `image`, no config-reload crates, no glib↔async bridges.
6. **Colours:** `videotestsrc.foreground-color` and `textoverlay.color` are `0xAARRGGBB`. Background alpha `0xFF`; canvas layers exactly `0x00000000`. `compositor background=black`.
7. **`display.fps` in config must equal the kernel `video=` rate.** Don't derive one from the other in code; both come from Ansible.
8. **State writes are atomic:** write to a temp file in the same directory, `fsync`, `rename`.
9. **Property sets on GStreamer elements happen on the render thread only.**
10. **Ports:** OSC 9000/udp, TCP 9001, HTTP 8080. Not 80.

## Definition of done

Before you say a task is complete:

- `make test` passes (`cargo fmt --check`, `cargo clippy --all-targets -D warnings`, `cargo test`).
- The relevant "done when" lines from `DESIGN.md` §16 are actually true, and you've run the command that proves it (`oscsend`, `curl`, `--snapshot`, etc.), not reasoned that it would be.
- No `TODO`, `unimplemented!()` or `todo!()` in committed code.
- If you changed a wire format, `docs/protocol.md` changed in the same commit.

## Working style

- **Code comments must stand alone.** No references to `DESIGN.md` sections, milestones, or the conversation that produced the code ("per the spec", "M2 will use this", "as discussed"). A comment earns its place only by telling a future reader something the code can't. Pointing at living reference docs a client would read (`docs/protocol.md`) is fine.
- **Commit messages describe the change, not the process.** No milestone numbers, no design-doc section references, no session narrative — none of that means anything in five years. Say what changed and why it changed.
- Small commits, one concern each. Milestone order from `DESIGN.md` §16; don't start M2 features while M1 is red.
- Don't restructure files or rename things in `DESIGN.md` §11 without asking. The layout is part of the spec.
- Prefer boring code. A `match` over a trait object; a `struct` over a builder; a plain loop over an iterator chain if it's clearer.
- When something in `DESIGN.md` is ambiguous or wrong, say so and propose a fix. Don't silently pick an interpretation.
- Never run `deploy-dev` or anything against a real box unless asked in that message.

## Dev environment

macOS with Homebrew GStreamer; see `DESIGN.md` §14. `make run` gives you a window. The Linux box is a verification target, not somewhere you develop.
