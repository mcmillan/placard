use std::net::{SocketAddr, UdpSocket};
use std::sync::mpsc;

use rosc::OscPacket;

use crate::command::parse_osc;
use crate::state::{Envelope, ReplyTo};

/// OSC listener: UDP datagrams → `Command`s on the state channel. Accepted
/// messages are acked by the state thread (`/placard/ok`); messages that fail
/// to parse are rejected here with `/placard/error`. Runs until the process
/// dies; nothing a client sends can make it return.
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
            Ok((_, packet)) => handle_packet(&socket, &tx, packet, from),
            Err(err) => {
                tracing::warn!(%from, %err, "dropping undecodable OSC datagram");
            }
        }
    }
}

/// Bundles are unpacked and each message handled independently (DESIGN.md §6).
fn handle_packet(
    socket: &UdpSocket,
    tx: &mpsc::Sender<Envelope>,
    packet: OscPacket,
    from: SocketAddr,
) {
    match packet {
        OscPacket::Bundle(bundle) => {
            for inner in bundle.content {
                handle_packet(socket, tx, inner, from);
            }
        }
        OscPacket::Message(msg) => match parse_osc(&msg) {
            Ok(command) => {
                let envelope = Envelope {
                    command,
                    via: "osc",
                    from: Some(from),
                    reply: ReplyTo::Osc(from),
                };
                if tx.send(envelope).is_err() {
                    tracing::error!("state thread gone, dropping OSC command");
                }
            }
            Err(err) => {
                tracing::warn!(%from, addr = %msg.addr, %err, "rejecting OSC message");
                reply_error(socket, from, &err.to_string());
            }
        },
    }
}

fn reply_error(socket: &UdpSocket, to: SocketAddr, reason: &str) {
    let msg = rosc::OscMessage {
        addr: "/placard/error".into(),
        args: vec![rosc::OscType::String(reason.into())],
    };
    match rosc::encoder::encode(&rosc::OscPacket::Message(msg)) {
        Ok(bytes) => {
            if let Err(err) = socket.send_to(&bytes, to) {
                tracing::warn!(%to, %err, "failed to send OSC error reply");
            }
        }
        Err(err) => tracing::warn!(%err, "failed to encode OSC error reply"),
    }
}
