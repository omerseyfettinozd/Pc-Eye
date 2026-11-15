#![allow(irrefutable_let_patterns)]

mod handlers;
mod input;
mod ipc;
mod state;

use std::{io::Write, path::Path, time::Duration};

use anyhow::{bail, Context, Result};
use smithay::reexports::{
    calloop::{
        timer::{TimeoutAction, Timer},
        EventLoop,
    },
    wayland_server::Display,
};
use tracing::info;

pub use state::{CalloopData, PcEyeWm};

struct CliArgs {
    socket: String,
    ipc: String,
    events: String,
    width: i32,
    height: i32,
}

fn parse_size(raw: &str) -> Result<(i32, i32)> {
    let (w_str, h_str) = raw
        .split_once('x')
        .or_else(|| raw.split_once('X'))
        .with_context(|| format!("invalid --size '{raw}', expected WxH"))?;
    let w: i32 = w_str
        .parse()
        .with_context(|| format!("invalid width in --size '{raw}'"))?;
    let h: i32 = h_str
        .parse()
        .with_context(|| format!("invalid height in --size '{raw}'"))?;
    if w <= 0 || h <= 0 {
        bail!("--size dimensions must be positive: {w}x{h}");
    }
    Ok((w, h))
}

fn parse_args() -> Result<CliArgs> {
    let mut socket: Option<String> = None;
    let mut ipc: Option<String> = None;
    let mut events: Option<String> = None;
    let mut width = 1600;
    let mut height = 900;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if let Some(val) = arg.strip_prefix("--socket=") {
            socket = Some(val.to_string());
        } else if arg == "--socket" {
            socket = Some(args.next().context("missing value for --socket")?);
        } else if let Some(val) = arg.strip_prefix("--ipc=") {
            ipc = Some(val.to_string());
        } else if arg == "--ipc" {
            ipc = Some(args.next().context("missing value for --ipc")?);
        } else if let Some(val) = arg.strip_prefix("--events=") {
            events = Some(val.to_string());
        } else if arg == "--events" {
            events = Some(args.next().context("missing value for --events")?);
        } else if let Some(val) = arg.strip_prefix("--size=") {
            let (w, h) = parse_size(val)?;
            width = w;
            height = h;
        } else if arg == "--size" {
            let val = args.next().context("missing value for --size")?;
            let (w, h) = parse_size(&val)?;
            width = w;
            height = h;
        } else {
            bail!("unknown argument: {arg}");
        }
    }

    let socket = socket.filter(|s| !s.is_empty()).context("--socket is required")?;
    let ipc = ipc.filter(|s| !s.is_empty()).context("--ipc is required")?;
    let events = events.filter(|s| !s.is_empty()).unwrap_or_else(|| {
        let ipc_path = Path::new(&ipc);
        match ipc_path.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => {
                parent.join("wm-events.sock").to_string_lossy().into_owned()
            }
            _ => "wm-events.sock".to_string(),
        }
    });

    Ok(CliArgs {
        socket,
        ipc,
        events,
        width,
        height,
    })
}

fn main() -> Result<()> {
    if let Ok(env_filter) = tracing_subscriber::EnvFilter::try_from_default_env() {
        tracing_subscriber::fmt()
            .with_env_filter(env_filter)
            .with_writer(std::io::stderr)
            .init();
    } else {
        tracing_subscriber::fmt()
            .with_writer(std::io::stderr)
            .init();
    }

    let cli = parse_args()?;

    let mut event_loop: EventLoop<CalloopData> =
        EventLoop::try_new().context("failed to create calloop EventLoop")?;
    let display: Display<PcEyeWm> =
        Display::new().context("failed to create Wayland Display")?;
    let display_handle = display.handle();

    let state = PcEyeWm::new(
        &mut event_loop,
        display,
        &cli.socket,
        (cli.width, cli.height),
    )?;

    let mut data = CalloopData {
        state,
        display_handle,
    };

    ipc::init_ipc_listener(&event_loop.handle(), &cli.ipc)?;
    ipc::init_events_listener(&event_loop.handle(), &cli.events)?;

    event_loop
        .handle()
        .insert_source(
            Timer::from_duration(Duration::from_millis(16)),
            |_, _, data| {
                let elapsed = data.state.start_time.elapsed();
                let output = data.state.output.clone();
                for window in data.state.space.elements() {
                    window.send_frame(
                        &output,
                        elapsed,
                        Some(Duration::ZERO),
                        |_, _| Some(output.clone()),
                    );
                }
                data.state.space.refresh();
                data.state.popups.cleanup();
                ipc::resolve_waiters(&mut data.state);
                data.state.drain_event_subscribers();
                data.state.reap_children();
                let _ = data.display_handle.flush_clients();
                TimeoutAction::ToDuration(Duration::from_millis(16))
            },
        )
        .map_err(|e| anyhow::anyhow!("failed to insert frame timer: {e:?}"))?;

    let ready_line = serde_json::json!({
        "ready": true,
        "socket": cli.socket,
        "ipc": cli.ipc,
        "events": cli.events,
    });
    println!("{ready_line}");
    let _ = std::io::stdout().flush();

    info!(
        socket = %cli.socket,
        ipc = %cli.ipc,
        events = %cli.events,
        size = ?(cli.width, cli.height),
        "pc-eye-wm ready"
    );

    event_loop
        .run(None, &mut data, |_| {})
        .context("event loop exited with error")?;

    let _ = std::fs::remove_file(&cli.ipc);
    let _ = std::fs::remove_file(&cli.events);
    Ok(())
}
