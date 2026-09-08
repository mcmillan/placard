# Placard — control protocol

Placard listens on three ports; all three map onto one command set. Nothing a
client sends can crash it: bad input gets an error reply on the same
transport and the current scene is untouched.

| Transport | Port | Format |
|---|---|---|
| OSC | 9000/udp | messages per the address table below |
| TCP | 9001 | newline-delimited JSON, one reply line per command |
| HTTP | 8080 | `POST /api/command` (JSON body), `GET /api/status`, `GET /api/history`, history page at `/` |

Ports come from `/etc/placard/config.toml` `[net]`; the values above are the
defaults.

## Colours

Colours are bare lowercase hex, `rrggbb` — e.g. `8a0000`. Case-insensitive; a
leading `#` is tolerated on input but never emitted. No short form, no alpha.

## Commands (JSON — TCP and HTTP)

Identical bodies over TCP (one object per line) and HTTP (`POST
/api/command`). Unknown fields are an error, not ignored.

```json
{ "cmd": "show", "text": "STAND BY", "bg": "000000", "fg": "ffffff" }
{ "cmd": "show", "text": "SHOW STOP", "flash": true }
{ "cmd": "show", "text": "SHOW STOP", "flash": 10 }
{ "cmd": "show", "text": "SHOW STOP", "flash": { "infinite": true } }
{ "cmd": "canned", "id": "go" }
{ "cmd": "canned", "id": "go", "bg": "0b6e2e" }
{ "cmd": "canned", "id": "show_stop", "flash": false }
{ "cmd": "colour", "bg": "8a0000" }
{ "cmd": "countdown_to", "target": "2026-09-08T19:30:00Z", "label": "House opens in" }
{ "cmd": "countdown_secs", "secs": 300, "label": "House opens in" }
{ "cmd": "clear" }
{ "cmd": "status" }
{ "cmd": "history" }
```

- `bg`/`fg` are optional everywhere; omitted means "keep the current colour"
  (for `canned`, the canned message's own colours, then `[defaults]`).
- `flash` inverts fg and bg every 500 ms when the message lands, then
  settles back on the real colours. It accepts:

  | Value | Meaning |
  |---|---|
  | `true` | flash for the default 3 seconds |
  | `false` | don't flash |
  | `10` | flash for 10 seconds |
  | `{"duration_s": 10}` | the same, spelled out |
  | `{"infinite": true}` | flash until the next command |
  | `-1` | the same, in scalar form (for OSC, which has no tables) |

  Canned messages take the same values in config; a command's own `flash`
  overrides the canned setting either way. Any later command ends a flash
  early. A timed flash is transient and does not survive a restart; an
  endless one is part of the state and resumes, since an alarm that quietly
  stopped alarming would be worse than one that kept going.
- `countdown_to` takes an ISO 8601 UTC instant. `countdown_secs` is converted
  to an absolute target when received, so it survives a restart.
- Countdowns run through zero and keep counting negative (`-0:07`,
  `-1:02:15`) until another command replaces them. Format is `M:SS` under an
  hour, `H:MM:SS` from one hour. The displayed value is the ceiling of the
  remaining time, so digits change exactly on second boundaries: a 5-second
  countdown shows `0:05` for a full second and `0:00` for exactly one.
- `clear` is black background, no text (not "back to the boot scene"). The
  wall clock stays on screen across every command, including `clear`.
- `status` and `history` are TCP/HTTP only. `history` returns
  `{"ok":true,"history":[…]}` — the last 200 inbound messages, newest first,
  each with `at`, `via`, `from`, the `raw` wire text (truncated at 512
  chars) and `ok`/`error`. Rejected messages are included; `status` and
  `history` queries themselves are not. History persists across restarts —
  it is appended to `history.ndjson` in the state directory, and the last
  200 entries are reloaded on boot.

### Replies

Every command gets one JSON reply (a line on TCP, the response body on HTTP):

```json
{"ok":true}
{"ok":false,"error":"unknown canned id \"nope\""}
```

HTTP uses 200 for accepted commands and 400 for rejected ones. `status`
replies with the full status object:

```json
{
  "ok": true,
  "scene": { "bg": "8a0000", "fg": "ffffff",
             "content": { "kind": "countdown", "target": "2026-09-08T18:30:00Z",
                          "label": "House opens in", "display": "-0:42" } },
  "canned_id": null,
  "flash": { "remaining_s": 1.8 },
  "uptime_secs": 8123,
  "version": "0.1.0",
  "build": "20260908053925",
  "last_command": { "at": "2026-09-08T18:29:10Z", "via": "osc", "from": "192.168.10.20:53101" }
}
```

- `content.kind` is `"text"` or `"countdown"`; `display` is the string
  currently on screen.
- `canned_id` is set while the scene is an unmodified `canned` command.
- `flash` is absent when the screen is steady, `{"remaining_s": N}` during a
  timed flash, and `{"infinite": true}` during an endless one.
- `build` is the release tag this binary was built from (`"dev"` for local
  builds) — the way to confirm an upgrade actually landed.

### TCP specifics

Connections may stay open and send many commands. A line whose content
exceeds 64 KiB, or that is not valid UTF-8, gets an error reply (and a
history entry) and the connection is closed.

## OSC

UDP port 9000. Colours are the same `rrggbb` strings (no `#`, so QLab cue
text needs no quoting).

| Address | Args |
|---|---|
| `/placard/show` | `s text [s bg] [s fg] [i flash \| f seconds]` |
| `/placard/canned` | `s id [s bg] [s fg] [i flash \| f seconds]` |
| `/placard/colour` | `s bg [s fg]` |
| `/placard/countdown/to` | `s iso8601 [s label]` |
| `/placard/countdown/secs` | `i secs [s label]` |
| `/placard/clear` | — |

After the leading string argument, colour strings are read in bg-then-fg
order and a single numeric argument anywhere in the tail is the flash — so
`s TEXT i 1` flashes without touching colours. OSC has no tables, so the
argument type carries the meaning: an **int** (or OSC true/false) is the
boolean form, a **float** is a number of seconds, and a negative float
means no end.

Every accepted message is acknowledged with `/placard/ok` to the sender's
address and port; rejected ones with `/placard/error s reason`. Bundles are
unpacked and each message handled independently. Unknown addresses get
`/placard/error`. There is no `/placard/status`.

Command-line examples (`oscsend` from liblo):

```
oscsend localhost 9000 /placard/canned s house_open
oscsend localhost 9000 /placard/show sss "STAND BY" 000000 ffffff
oscsend localhost 9000 /placard/countdown/secs is 300 "House opens in"
oscsend localhost 9000 /placard/show si "SHOW STOP" 1     # default 3 s
oscsend localhost 9000 /placard/show sf "SHOW STOP" 10    # 10 seconds
oscsend localhost 9000 /placard/show sf "SHOW STOP" -1    # until the next cue
oscsend localhost 9000 /placard/clear
```

## Web UI

`GET /` serves a read-only, self-contained history page: every inbound
message with its timestamp, transport, source address, raw wire text and
outcome, refreshing every 2 s. It is diagnostics, not operator tooling — it
sends no commands. OSC messages are shown in a rendered one-line form
(`/placard/show s:"GO" i:1`) since the raw datagram is binary.

## Text rendering

Text is rendered literally: `<b>` in a cue shows as `<b>`, never as markup.
Long text word-wraps and auto-shrinks to fit inside the configured padding;
the font size in `[display].font` is the maximum, used whenever the text
fits. A countdown's label renders above the digits at 60 % of their size.
Text beyond 4,000 characters or 40 lines is truncated with an ellipsis on
screen — no input can produce a layout that fails to render — while the
scene, `state.json` and `status` keep the full text.
