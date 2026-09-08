mod command;
mod config;
mod net;
mod render;
mod scene;
mod state;
mod status;

use std::path::{Path, PathBuf};
use std::sync::mpsc;

use anyhow::Context as _;
use chrono::{DateTime, Utc};
use clap::Parser;
use serde::Deserialize;
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::util::SubscriberInitExt as _;

use crate::config::Config;
use crate::render::{Renderer, SinkKind};
use crate::state::StateThread;

#[derive(Parser)]
#[command(version, about = "Single-purpose display appliance")]
struct Args {
    /// Path to config.toml
    #[arg(long)]
    config: PathBuf,

    /// Video sink; defaults to kms on Linux, auto elsewhere
    #[arg(long, value_enum)]
    sink: Option<SinkKind>,

    /// State directory; defaults to $STATE_DIRECTORY, then ./state
    #[arg(long)]
    state_dir: Option<PathBuf>,

    /// Render one frame of a scene fixture to a PNG and exit
    #[arg(long, num_args = 2, value_names = ["SCENE_JSON", "OUT_PNG"])]
    snapshot: Option<Vec<PathBuf>>,
}

/// A snapshot fixture is the `/api/status` scene shape plus a fixed `now`, so
/// countdown and clock strings are deterministic (DESIGN.md §13).
#[derive(Deserialize)]
struct SnapshotFixture {
    #[serde(flatten)]
    scene: scene::Scene,
    now: DateTime<Utc>,
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    init_tracing();

    let config = Config::load(&args.config)?;

    if let Some(paths) = &args.snapshot {
        return snapshot(&config, &paths[0], &paths[1]);
    }

    let sink = args.sink.unwrap_or_else(render::default_sink);
    let state_dir = args
        .state_dir
        .or_else(|| std::env::var_os("STATE_DIRECTORY").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("./state"));
    std::fs::create_dir_all(&state_dir)
        .with_context(|| format!("creating state dir {}", state_dir.display()))?;

    let (env_tx, env_rx) = mpsc::channel();
    let (spec_tx, spec_rx) = mpsc::channel();

    let config_net = config.net.clone();
    let osc_addr = format!("0.0.0.0:{}", config_net.osc_port);
    let osc_socket =
        std::net::UdpSocket::bind(&osc_addr).with_context(|| format!("binding OSC {osc_addr}"))?;
    let osc_reply_socket = osc_socket.try_clone().context("cloning OSC socket")?;

    // Build the pipeline before spawning anything so a bad display setup
    // fails fast, before we claim ports.
    let renderer = Renderer::build(&config, sink, None)?;

    let state = StateThread::new(config, &state_dir, spec_tx, Some(osc_reply_socket));
    std::thread::Builder::new()
        .name("state".into())
        .spawn(move || state.run(env_rx))
        .context("spawning state thread")?;

    let osc_tx = env_tx.clone();
    std::thread::Builder::new()
        .name("osc".into())
        .spawn(move || net::osc::run(osc_socket, osc_tx))
        .context("spawning osc thread")?;

    let tcp_addr = format!("0.0.0.0:{}", config_net.tcp_port);
    let tcp_listener = std::net::TcpListener::bind(&tcp_addr)
        .with_context(|| format!("binding TCP {tcp_addr}"))?;
    let tcp_tx = env_tx.clone();
    std::thread::Builder::new()
        .name("tcp".into())
        .spawn(move || net::tcp::run(tcp_listener, tcp_tx))
        .context("spawning tcp thread")?;

    let http_addr = format!("0.0.0.0:{}", config_net.http_port);
    let http_listener = std::net::TcpListener::bind(&http_addr)
        .with_context(|| format!("binding HTTP {http_addr}"))?;
    let http_tx = env_tx.clone();
    std::thread::Builder::new()
        .name("http".into())
        .spawn(move || {
            if let Err(err) = net::http::run(http_listener, http_tx) {
                // The HTTP listener dying silently would leave a half-alive
                // appliance; die loudly and let systemd restart everything.
                tracing::error!(%err, "http listener failed");
                std::process::exit(1);
            }
        })
        .context("spawning http thread")?;

    tracing::info!(version = env!("CARGO_PKG_VERSION"), ?sink, "placard up");

    // The render loop owns this thread. It only returns when the pipeline
    // dies; either way the process must exit so systemd restarts it.
    renderer.run(spec_rx)?;
    anyhow::bail!("render loop ended unexpectedly (EOS on a live pipeline)")
}

/// Build the real pipeline with the png sink, apply the fixture scene, render
/// one frame, exit.
fn snapshot(config: &Config, scene_path: &Path, out_path: &Path) -> anyhow::Result<()> {
    let raw = std::fs::read_to_string(scene_path)
        .with_context(|| format!("reading fixture {}", scene_path.display()))?;
    let fixture: SnapshotFixture = serde_json::from_str(&raw)
        .with_context(|| format!("parsing fixture {}", scene_path.display()))?;

    let renderer = Renderer::build(config, SinkKind::Png, Some(out_path))?;
    let spec = state::derive_spec(&fixture.scene, fixture.now, config);
    renderer.apply(&spec);

    // Keep the sender alive so the render loop doesn't treat a closed channel
    // as the state thread dying; pngenc sends EOS after the first frame.
    let (_spec_tx, spec_rx) = mpsc::channel();
    renderer.run(spec_rx)?;

    anyhow::ensure!(
        out_path.exists(),
        "snapshot did not produce {}",
        out_path.display()
    );
    tracing::info!(out = %out_path.display(), "snapshot written");
    Ok(())
}

/// Journald when running under systemd, stderr otherwise (DESIGN.md §7).
/// Runtime detection, not cfg: JOURNAL_STREAM is set by systemd for services.
fn init_tracing() {
    let filter = || {
        tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"))
    };
    if std::env::var_os("JOURNAL_STREAM").is_some()
        && let Ok(journald) = tracing_journald::layer()
    {
        tracing_subscriber::registry()
            .with(filter())
            .with(journald)
            .init();
        return;
    }
    tracing_subscriber::registry()
        .with(filter())
        .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
        .init();
}
