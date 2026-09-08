use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc;

use crate::command::parse_json;
use crate::state::Envelope;

use super::{dispatch, reply_json};

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
        // Bounded read: never buffer more than MAX_LINE for one line.
        match (&mut reader)
            .take(MAX_LINE as u64 + 1)
            .read_until(b'\n', &mut line)
        {
            Ok(0) => return, // clean disconnect
            Ok(_) => {}
            Err(err) => {
                tracing::warn!(?peer, %err, "tcp read failed");
                return;
            }
        }
        if line.len() > MAX_LINE {
            let _ = writeln!(writer, r#"{{"ok":false,"error":"line exceeds 64 KiB"}}"#);
            return;
        }
        let text = match std::str::from_utf8(&line) {
            Ok(text) => text.trim(),
            Err(_) => {
                let _ = writeln!(writer, r#"{{"ok":false,"error":"invalid UTF-8"}}"#);
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
