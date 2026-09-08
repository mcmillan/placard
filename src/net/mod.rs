pub mod http;
pub mod osc;
pub mod tcp;

use std::net::SocketAddr;
use std::sync::mpsc;

use crate::command::{Command, CommandError};
use crate::state::{Envelope, Reply, ReplyTo};

/// Send an inbound message (parsed or not) to the state thread and wait for
/// its reply. The state thread answers in microseconds; if it's gone the
/// process is dying anyway.
pub fn dispatch(
    tx: &mpsc::Sender<Envelope>,
    command: Result<Command, CommandError>,
    raw: String,
    via: &'static str,
    from: Option<SocketAddr>,
) -> Result<Reply, CommandError> {
    let (reply_tx, reply_rx) = mpsc::channel();
    tx.send(Envelope {
        command,
        raw,
        via,
        from,
        reply: ReplyTo::Oneshot(reply_tx),
    })
    .map_err(|_| CommandError::ShuttingDown)?;
    reply_rx.recv().unwrap_or(Err(CommandError::ShuttingDown))
}

/// The one JSON reply shape shared by TCP and HTTP: `{"ok":true}`, a full
/// status report, a history listing, or `{"ok":false,"error":"…"}`.
pub fn reply_json(result: &Result<Reply, CommandError>) -> String {
    match result {
        Ok(Reply::Ok) => r#"{"ok":true}"#.to_string(),
        Ok(Reply::Status(report)) => serde_json::to_string(report)
            .unwrap_or_else(|_| r#"{"ok":false,"error":"status serialisation failed"}"#.into()),
        Ok(Reply::History(entries)) => {
            let history = serde_json::to_value(entries).unwrap_or_default();
            serde_json::json!({ "ok": true, "history": history }).to_string()
        }
        Err(err) => {
            let mut obj = serde_json::Map::new();
            obj.insert("ok".into(), false.into());
            obj.insert("error".into(), err.to_string().into());
            serde_json::Value::Object(obj).to_string()
        }
    }
}
