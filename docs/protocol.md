# Placard — control protocol

Placard listens on three ports; all three map onto one command set. Nothing a
client sends can crash it: bad input gets an error reply on the same
transport and the current scene is untouched.

| Transport | Port | Format |
|---|---|---|
| OSC | 9000/udp | messages per the address table below |
| TCP | 9001 | newline-delimited JSON, one reply line per command |
| HTTP | 8080 | `POST /api/command` (JSON body), `GET /api/status` |

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
{ "cmd": "canned", "id": "go" }
{ "cmd": "canned", "id": "go", "bg": "0b6e2e" }
{ "cmd": "colour", "bg": "8a0000" }
{ "cmd": "countdown_to", "target": "2026-09-08T19:30:00Z", "label": "House opens in" }
{ "cmd": "countdown_secs", "secs": 300, "label": "House opens in" }
{ "cmd": "clear" }
{ "cmd": "status" }
```

- `bg`/`fg` are optional everywhere; omitted means "keep the current colour"
  (for `canned`, the canned message's own colours, then `[defaults]`).
- `countdown_to` takes an ISO 8601 UTC instant. `countdown_secs` is converted
  to an absolute target when received, so it survives a restart.
- Countdowns run through zero and keep counting negative (`-0:07`,
  `-1:02:15`) until another command replaces them. Format is `M:SS` under an
  hour, `H:MM:SS` from one hour.
- `clear` is black background, no text (not "back to the boot scene"). The
  wall clock stays on screen across every command, including `clear`.
- `status` is TCP/HTTP only.

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
  "uptime_secs": 8123,
  "ntp_synced": true,
  "clock_offset_ms": 3,
  "version": "0.1.0",
  "last_command": { "at": "2026-09-08T18:29:10Z", "via": "osc", "from": "192.168.10.20:53101" }
}
```

- `content.kind` is `"text"` or `"countdown"`; `display` is the string
  currently on screen.
- `canned_id` is set while the scene is an unmodified `canned` command.
- `ntp_synced` is `true`/`false`, or `"unknown"` where it cannot be probed
  (non-Linux dev machines).
- `clock_offset_ms` is null when chrony is unavailable.

### TCP specifics

Connections may stay open and send many commands. A line over 64 KiB or
invalid UTF-8 gets an error reply and the connection is closed.

## OSC

UDP port 9000. Colours are the same `rrggbb` strings (no `#`, so QLab cue
text needs no quoting).

| Address | Args |
|---|---|
| `/placard/show` | `s text [s bg] [s fg]` |
| `/placard/canned` | `s id [s bg] [s fg]` |
| `/placard/colour` | `s bg [s fg]` |
| `/placard/countdown/to` | `s iso8601 [s label]` |
| `/placard/countdown/secs` | `i secs [s label]` |
| `/placard/clear` | — |

Every accepted message is acknowledged with `/placard/ok` to the sender's
address and port; rejected ones with `/placard/error s reason`. Bundles are
unpacked and each message handled independently. Unknown addresses get
`/placard/error`. There is no `/placard/status`.

Command-line examples (`oscsend` from liblo):

```
oscsend localhost 9000 /placard/canned s house_open
oscsend localhost 9000 /placard/show s "STAND BY" s 000000 s ffffff
oscsend localhost 9000 /placard/countdown/secs i 300 s "House opens in"
oscsend localhost 9000 /placard/clear
```

## Text rendering

Text is rendered literally: `<b>` in a cue shows as `<b>`, never as markup.
Long text word-wraps and auto-shrinks to fit inside the configured padding;
the font size in `[display].font` is the maximum, used whenever the text
fits. A countdown's label renders above the digits at 60 % of their size.
