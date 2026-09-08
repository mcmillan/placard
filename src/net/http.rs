use std::net::SocketAddr;
use std::sync::mpsc;

use axum::Router;
use axum::extract::{ConnectInfo, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};

use crate::command::{Command, parse_json};
use crate::state::Envelope;

use super::{dispatch, reply_json};

#[derive(Clone)]
struct App {
    tx: mpsc::Sender<Envelope>,
}

/// HTTP listener: `POST /api/command` and `GET /api/status`, nothing else.
/// Runs a current-thread tokio runtime on its own thread; the only await
/// points are accept/read/write, so GStreamer never sees async.
pub fn run(listener: std::net::TcpListener, tx: mpsc::Sender<Envelope>) -> anyhow::Result<()> {
    let app = Router::new()
        .route("/api/command", post(command))
        .route("/api/status", get(status))
        .with_state(App { tx });

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async {
        listener.set_nonblocking(true)?;
        let listener = tokio::net::TcpListener::from_std(listener)?;
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await?;
        Ok(())
    })
}

async fn command(
    State(app): State<App>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    body: String,
) -> Response {
    // Same parser as TCP so the two transports can't drift.
    let result = match parse_json(&body) {
        Ok(command) => run_blocking(app, command, peer).await,
        Err(err) => {
            tracing::warn!(%peer, %err, "http command rejected");
            Err(err)
        }
    };
    let code = if result.is_ok() {
        StatusCode::OK
    } else {
        StatusCode::BAD_REQUEST
    };
    json_response(code, reply_json(&result))
}

async fn status(State(app): State<App>, ConnectInfo(peer): ConnectInfo<SocketAddr>) -> Response {
    let result = run_blocking(app, Command::Status, peer).await;
    let code = if result.is_ok() {
        StatusCode::OK
    } else {
        StatusCode::INTERNAL_SERVER_ERROR
    };
    json_response(code, reply_json(&result))
}

/// The state-thread round trip blocks on an mpsc recv; keep it off the
/// current-thread runtime's core.
async fn run_blocking(
    app: App,
    command: Command,
    peer: SocketAddr,
) -> Result<crate::state::Reply, crate::command::CommandError> {
    tokio::task::spawn_blocking(move || dispatch(&app.tx, command, "http", Some(peer)))
        .await
        .unwrap_or(Err(crate::command::CommandError::ShuttingDown))
}

fn json_response(code: StatusCode, body: String) -> Response {
    (
        code,
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        body,
    )
        .into_response()
}
