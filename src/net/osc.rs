use std::fmt::Write as _;
use std::net::{SocketAddr, UdpSocket};
use std::sync::mpsc;

use rosc::{OscMessage, OscPacket, OscType};

use crate::command::parse_osc;
use crate::state::{Envelope, ReplyTo};

/// OSC listener: UDP datagrams → inbound messages on the state channel,
/// parse failures included — the state thread records them in the history
/// and sends the `/placard/ok` / `/placard/error` ack either way. Runs until
/// the process dies; nothing a client sends can make it return.
pub fn run(socket: UdpSocket, tx: mpsc::Sender<Envelope>) {
    let mut buf = [0u8; 65_536];
    loop {
        let (len, from) = match socket.recv_from(&mut buf) {
            Ok(ok) => ok,
            Err(err) => {
                tracing::warn!(%err, "osc recv failed");
                continue;
            }
        };
        match rosc::decoder::decode_udp(&buf[..len]) {
            Ok((_, packet)) => handle_packet(&tx, packet, from),
            Err(err) => {
                // Not OSC at all: nothing to attribute or reply to.
                tracing::warn!(%from, %err, "dropping undecodable OSC datagram");
            }
        }
    }
}

/// Bundles are unpacked and each message handled independently; bundle
/// timestamps are ignored.
fn handle_packet(tx: &mpsc::Sender<Envelope>, packet: OscPacket, from: SocketAddr) {
    match packet {
        OscPacket::Bundle(bundle) => {
            for inner in bundle.content {
                handle_packet(tx, inner, from);
            }
        }
        OscPacket::Message(msg) => {
            let envelope = Envelope {
                command: parse_osc(&msg),
                raw: render(&msg),
                via: "osc",
                from: Some(from),
                reply: ReplyTo::Osc(from),
            };
            if tx.send(envelope).is_err() {
                tracing::error!("state thread gone, dropping OSC message");
            }
        }
    }
}

/// A readable one-line form of an OSC message for the history — the raw
/// datagram is binary and useless to a human.
fn render(msg: &OscMessage) -> String {
    let mut out = msg.addr.clone();
    for arg in &msg.args {
        let _ = match arg {
            OscType::String(s) => write!(out, " s:{s:?}"),
            OscType::Int(n) => write!(out, " i:{n}"),
            OscType::Float(f) => write!(out, " f:{f}"),
            OscType::Bool(b) => write!(out, " {b}"),
            other => write!(out, " {other:?}"),
        };
    }
    out
}
