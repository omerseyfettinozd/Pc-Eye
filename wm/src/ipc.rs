use smithay::reexports::wayland_server::Resource;
use std::{
    fs::File,
    io::{BufWriter, ErrorKind, Read, Write},
    os::unix::net::{UnixListener, UnixStream},
    path::Path,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use smithay::reexports::calloop::{
    generic::Generic, Interest, LoopHandle, Mode, PostAction,
};

use crate::state::{CalloopData, PcEyeWm, PendingWaitIdle};

#[derive(Debug, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum IpcRequest {
    Ping,
    Windows,
    Capture {
        id: u64,
        path: String,
    },
    WaitIdle {
        id: Option<u64>,
        #[serde(default = "default_quiet_ms")]
        quiet_ms: u64,
        #[serde(default = "default_timeout_ms")]
        timeout_ms: u64,
    },
    Click {
        id: u64,
        x: f64,
        y: f64,
        #[serde(default = "default_button")]
        button: String,
        #[serde(default = "default_count")]
        count: u32,
    },
    Move {
        id: u64,
        x: f64,
        y: f64,
    },
    Scroll {
        id: u64,
        x: f64,
        y: f64,
        dy: f64,
    },
    Key {
        id: Option<u64>,
        keys: String,
    },
    Type {
        id: Option<u64>,
        text: String,
    },
    Spawn {
        cmd: Vec<String>,
    },
    Close {
        id: u64,
    },
    Quit,
}

fn default_quiet_ms() -> u64 {
    100
}

fn default_timeout_ms() -> u64 {
    3000
}

fn default_button() -> String {
    "left".to_string()
}

fn default_count() -> u32 {
    1
}

#[derive(Serialize)]
struct WindowInfo {
    id: u64,
    app_id: String,
    title: String,
    size: [i32; 2],
    commits: u64,
    idle_ms: u64,
}

pub fn init_ipc_listener(
    loop_handle: &LoopHandle<'static, CalloopData>,
    ipc_path: &str,
) -> Result<()> {
    let path = Path::new(ipc_path);
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create IPC parent dir '{}'", parent.display()))?;
        }
    }
    let _ = std::fs::remove_file(path);

    let listener = UnixListener::bind(path)
        .with_context(|| format!("failed to bind IPC Unix socket '{ipc_path}'"))?;
    listener
        .set_nonblocking(true)
        .context("failed to set IPC listener nonblocking")?;

    loop_handle
        .insert_source(
            Generic::new(listener, Interest::READ, Mode::Level),
            |_, listener, data| {
                loop {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            if let Err(err) = register_ipc_client(&mut data.state, stream) {
                                tracing::warn!(?err, "failed to register IPC client");
                            }
                        }
                        Err(err) if err.kind() == ErrorKind::WouldBlock => break,
                        Err(err) => {
                            tracing::warn!(?err, "error accepting IPC connection");
                            break;
                        }
                    }
                }
                Ok(PostAction::Continue)
            },
        )
        .map_err(|e| anyhow::anyhow!("failed to register IPC listener source: {e:?}"))?;

    Ok(())
}

fn register_ipc_client(state: &mut PcEyeWm, stream: UnixStream) -> Result<()> {
    stream
        .set_nonblocking(true)
        .context("failed to set IPC client stream nonblocking")?;
    let writer = stream
        .try_clone()
        .context("failed to clone IPC client stream")?;

    let client_id = state.next_ipc_client_id;
    state.next_ipc_client_id += 1;
    state.ipc_clients.insert(client_id, writer);
    state.ipc_buffers.insert(client_id, Vec::new());

    let loop_handle = state.loop_handle.clone();
    loop_handle
        .insert_source(
            Generic::new(stream, Interest::READ, Mode::Level),
            move |_, stream, data| {
                let mut tmp = [0u8; 4096];
                let mut eof = false;

                loop {
                    match { let mut s: &std::os::unix::net::UnixStream = &*stream; s.read(&mut tmp) } {
                        Ok(0) => {
                            eof = true;
                            break;
                        }
                        Ok(n) => {
                            if let Some(buf) = data.state.ipc_buffers.get_mut(&client_id) {
                                buf.extend_from_slice(&tmp[..n]);
                            }
                        }
                        Err(err) if err.kind() == ErrorKind::WouldBlock => break,
                        Err(err) => {
                            tracing::debug!(?err, client_id, "IPC read error");
                            eof = true;
                            break;
                        }
                    }
                }

                let mut lines = Vec::new();
                if let Some(buf) = data.state.ipc_buffers.get_mut(&client_id) {
                    while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
                        let line: Vec<u8> = buf.drain(..=pos).collect();
                        lines.push(line);
                    }
                }

                for line in lines {
                    let trimmed = std::str::from_utf8(&line)
                        .map(str::trim)
                        .unwrap_or("");
                    if trimmed.is_empty() {
                        continue;
                    }
                    let reply = match serde_json::from_str::<IpcRequest>(trimmed) {
                        Ok(req) => handle_request(&mut data.state, client_id, req),
                        Err(err) => Some(err_json(&format!("invalid request: {err}"))),
                    };
                    if let Some(resp) = reply {
                        send_reply(&mut data.state, client_id, &resp);
                    }
                }

                let _ = data.display_handle.flush_clients();

                if eof {
                    cleanup_client(&mut data.state, client_id);
                    Ok(PostAction::Remove)
                } else {
                    Ok(PostAction::Continue)
                }
            },
        )
        .map_err(|e| anyhow::anyhow!("failed to insert IPC stream source: {e:?}"))?;

    Ok(())
}

fn cleanup_client(state: &mut PcEyeWm, client_id: u64) {
    state.ipc_clients.remove(&client_id);
    state.ipc_buffers.remove(&client_id);
    state.waiters.retain(|w| w.client_id != client_id);
}

fn send_reply(state: &mut PcEyeWm, client_id: u64, value: &Value) {
    let Some(mut stream) = state.ipc_clients.get(&client_id) else {
        return;
    };
    let Ok(mut payload) = serde_json::to_vec(value) else {
        return;
    };
    payload.push(b'\n');
    if let Err(err) = stream.write_all(&payload) {
        if err.kind() != ErrorKind::WouldBlock {
            tracing::debug!(?err, client_id, "IPC write error");
        }
    }
}

fn err_json(msg: &str) -> Value {
    json!({
        "ok": false,
        "error": msg,
    })
}

fn handle_request(state: &mut PcEyeWm, client_id: u64, req: IpcRequest) -> Option<Value> {
    match req {
        IpcRequest::Ping => Some(json!({ "ok": true })),

        IpcRequest::Windows => {
            let mut list: Vec<WindowInfo> = state
                .windows
                .values()
                .map(|meta| {
                    let geo_size = meta.window.geometry().size;
                    let (w, h) = if geo_size.w > 0 && geo_size.h > 0 {
                        (geo_size.w, geo_size.h)
                    } else if let Some(img) = meta
                        .window
                        .toplevel()
                        .and_then(|t| state.surface_images.get(&t.wl_surface().id()))
                    {
                        (img.width as i32, img.height as i32)
                    } else {
                        (0, 0)
                    };
                    WindowInfo {
                        id: meta.id,
                        app_id: meta.app_id.clone().unwrap_or_default(),
                        title: meta.title.clone().unwrap_or_default(),
                        size: [w, h],
                        commits: meta.commits,
                        idle_ms: meta.last_commit.elapsed().as_millis() as u64,
                    }
                })
                .collect();
            list.sort_by_key(|w| w.id);
            Some(json!({
                "ok": true,
                "windows": list,
            }))
        }

        IpcRequest::Capture { id, path } => {
            let started = Instant::now();
            match state.composite_window_rgb(id) {
                Ok((width, height, rgb)) => {
                    if let Err(err) = write_ppm_p6(&path, width, height, &rgb) {
                        return Some(err_json(&format!("failed to write ppm '{path}': {err}")));
                    }
                    let ms = started.elapsed().as_millis() as u64;
                    Some(json!({
                        "ok": true,
                        "width": width,
                        "height": height,
                        "path": path,
                        "ms": ms,
                    }))
                }
                Err(err) => Some(err_json(&err)),
            }
        }

        IpcRequest::WaitIdle {
            id,
            quiet_ms,
            timeout_ms,
        } => {
            if let Some(win_id) = id {
                if !state.windows.contains_key(&win_id) {
                    return Some(err_json(&format!("unknown window id: {win_id}")));
                }
            }
            let now = Instant::now();
            state.waiters.push(PendingWaitIdle {
                client_id,
                window_id: id,
                quiet_ms,
                started: now,
                deadline: now + Duration::from_millis(timeout_ms),
            });
            None
        }

        IpcRequest::Click {
            id,
            x,
            y,
            button,
            count,
        } => match state.inject_click(id, x, y, &button, count) {
            Ok(surface_found) => Some(json!({
                "ok": true,
                "surface_found": surface_found,
            })),
            Err(err) => Some(err_json(&err)),
        },

        IpcRequest::Move { id, x, y } => match state.inject_move(id, x, y) {
            Ok(surface_found) => Some(json!({
                "ok": true,
                "surface_found": surface_found,
            })),
            Err(err) => Some(err_json(&err)),
        },

        IpcRequest::Scroll { id, x, y, dy } => match state.inject_scroll(id, x, y, dy) {
            Ok(()) => Some(json!({ "ok": true })),
            Err(err) => Some(err_json(&err)),
        },

        IpcRequest::Key { id, keys } => match state.inject_keys(id, &keys) {
            Ok(()) => Some(json!({ "ok": true })),
            Err(err) => Some(err_json(&err)),
        },

        IpcRequest::Type { id, text } => match state.inject_text(id, &text) {
            Ok(skipped) => Some(json!({
                "ok": true,
                "skipped": skipped,
            })),
            Err(err) => Some(err_json(&err)),
        },

        IpcRequest::Spawn { cmd } => {
            let Some((program, args)) = cmd.split_first() else {
                return Some(err_json("spawn cmd must not be empty"));
            };
            let mut command = Command::new(program);
            command
                .args(args)
                .env("WAYLAND_DISPLAY", &state.socket_name)
                .env("GDK_BACKEND", "wayland")
                .env("QT_QPA_PLATFORM", "wayland")
                .env("MOZ_ENABLE_WAYLAND", "1")
                .env("GSK_RENDERER", "cairo")
                .env("LIBGL_ALWAYS_SOFTWARE", "1")
                .env_remove("DISPLAY")
                .stdin(Stdio::null());

            match command.spawn() {
                Ok(child) => {
                    let pid = child.id();
                    state.children.push(child);
                    Some(json!({
                        "ok": true,
                        "pid": pid,
                    }))
                }
                Err(err) => Some(err_json(&format!("failed to spawn '{program}': {err}"))),
            }
        }

        IpcRequest::Close { id } => {
            let Some(meta) = state.windows.get(&id) else {
                return Some(err_json(&format!("unknown window id: {id}")));
            };
            if let Some(toplevel) = meta.window.toplevel() {
                toplevel.send_close();
                Some(json!({ "ok": true }))
            } else {
                Some(err_json(&format!("window {id} has no toplevel")))
            }
        }

        IpcRequest::Quit => {
            state.loop_signal.stop();
            Some(json!({ "ok": true }))
        }
    }
}

pub fn resolve_waiters(state: &mut PcEyeWm) {
    if state.waiters.is_empty() {
        return;
    }

    let now = Instant::now();
    let waiters = std::mem::take(&mut state.waiters);
    let mut remaining = Vec::new();

    for waiter in waiters {
        let quiet = Duration::from_millis(waiter.quiet_ms);

        let resolution: Option<Value> = match waiter.window_id {
            Some(win_id) => match state.windows.get(&win_id) {
                None => Some(err_json(&format!("window {win_id} closed"))),
                Some(meta) => {
                    let is_idle = now.saturating_duration_since(meta.last_commit) >= quiet;
                    if is_idle || now >= waiter.deadline {
                        Some(json!({
                            "ok": true,
                            "idle": is_idle,
                            "waited_ms": now.saturating_duration_since(waiter.started).as_millis() as u64,
                            "commits": meta.commits,
                        }))
                    } else {
                        None
                    }
                }
            },
            None => {
                let is_idle = state
                    .windows
                    .values()
                    .all(|meta| now.saturating_duration_since(meta.last_commit) >= quiet);
                if is_idle || now >= waiter.deadline {
                    let total_commits: u64 = state.windows.values().map(|m| m.commits).sum();
                    Some(json!({
                        "ok": true,
                        "idle": is_idle,
                        "waited_ms": now.saturating_duration_since(waiter.started).as_millis() as u64,
                        "commits": total_commits,
                    }))
                } else {
                    None
                }
            }
        };

        if let Some(reply) = resolution {
            send_reply(state, waiter.client_id, &reply);
        } else {
            remaining.push(waiter);
        }
    }

    state.waiters = remaining;
}

fn write_ppm_p6(path: &str, width: u32, height: u32, rgb: &[u8]) -> std::io::Result<()> {
    let p = Path::new(path);
    if let Some(parent) = p.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    let file = File::create(p)?;
    let mut writer = BufWriter::new(file);
    write!(writer, "P6\n{width} {height}\n255\n")?;
    writer.write_all(rgb)?;
    writer.flush()
}
