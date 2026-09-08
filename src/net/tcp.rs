use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc;

use crate::command::{CommandError, parse_json};
use crate::state::Envelope;

use super::{dispatch, reply_json};

/// Maximum line *content* (the terminator doesn't count against it).
const MAX_LINE: usize = 64 * 1024;

/// NDJSON listener: one JSON command per line, one JSON reply line per
/// command, connections stay open. A line over 64 KiB or invalid UTF-8 gets
/// an error reply and the connection is closed.
pub fn run(listener: TcpListener, tx: mpsc::Sender<Envelope>) {
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let tx = tx.clone();
                let spawned = std::thread::Builder::new()
                    .name("tcp-conn".into())
                    .spawn(move || serve(stream, tx));
                if let Err(err) = spawned {
                    tracing::warn!(%err, "failed to spawn tcp connection thread");
                }
            }
            Err(err) => tracing::warn!(%err, "tcp accept failed"),
        }
    }
}

fn serve(stream: TcpStream, tx: mpsc::Sender<Envelope>) {
    let peer = stream.peer_addr().ok();
    let mut writer = match stream.try_clone() {
        Ok(w) => w,
        Err(err) => {
            tracing::warn!(?peer, %err, "failed to clone tcp stream");
            return;
        }
    };
    let mut reader = BufReader::new(stream);
    let mut line = Vec::with_capacity(1024);

    loop {
        line.clear();
        // Bounded read: MAX_LINE of content plus room for \r\n; anything
        // longer is over the limit even before a newline shows up.
        match (&mut reader)
            .take(MAX_LINE as u64 + 3)
            .read_until(b'\n', &mut line)
        {
            Ok(0) => return, // clean disconnect
            Ok(_) => {}
            Err(err) => {
                tracing::warn!(?peer, %err, "tcp read failed");
                return;
            }
        }
        if line.last() == Some(&b'\n') {
            line.pop();
        }
        if line.last() == Some(&b'\r') {
            line.pop();
        }
        // Rejects dispatch like everything else so they land in the history
        // and reply through the one shared path; the connection then closes.
        if line.len() > MAX_LINE {
            let raw = String::from_utf8_lossy(&line).into_owned();
            let result = dispatch(&tx, Err(CommandError::LineTooLong), raw, "tcp", peer);
            let _ = writeln!(writer, "{}", reply_json(&result));
            return;
        }
        let text = match std::str::from_utf8(&line) {
            Ok(text) => text.trim(),
            Err(_) => {
                let raw = String::from_utf8_lossy(&line).into_owned();
                let result = dispatch(&tx, Err(CommandError::InvalidUtf8), raw, "tcp", peer);
                let _ = writeln!(writer, "{}", reply_json(&result));
                return;
            }
        };
        if text.is_empty() {
            continue;
        }
        // Parse failures dispatch too: the state thread records every
        // inbound line in the history and logs the rejection.
        let result = dispatch(&tx, parse_json(text), text.to_string(), "tcp", peer);
        if writeln!(writer, "{}", reply_json(&result)).is_err() {
            return;
        }
    }
}
