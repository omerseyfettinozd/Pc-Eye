use crate::director::guard::{enforce_action_guard, verify_freeze_target_safe};
use crate::director::input::ActionReceipt;
use crate::eyes::compositor::{
    capture_desktop_state, hypr_dispatch, hypr_dispatch_cmd, normalize_window_address, HyprDispatch, wait_for_compositor_event,
};
use crate::eyes::process_xray::collect_app_cluster;
use crate::eyes::vision::runtime_dir;
use anyhow::{anyhow, Context, Result};
use std::fs::OpenOptions;
use std::io::Write;
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

/// Pencere ve çalışma alanı yönetimi (Hyprland IPC).
pub fn manage_window(action: &str, target: Option<&str>, arg: Option<&str>) -> Result<ActionReceipt> {
    enforce_action_guard(target, false)?;

    let dispatch_cmd: HyprDispatch = match action.to_ascii_lowercase().as_str() {
        "focus" => {
            let addr = normalize_window_address(
                target.ok_or_else(|| anyhow!("focus için pencere adresi gerekli"))?,
            );
            HyprDispatch::FocusWindow(addr)
        }
        "close" => {
            let addr = normalize_window_address(
                target.ok_or_else(|| anyhow!("close için pencere adresi gerekli"))?,
            );
            HyprDispatch::CloseWindow(addr)
        }
        "workspace" => {
            let ws = target
                .or(arg)
                .ok_or_else(|| anyhow!("workspace numarası/adı gerekli"))?;
            HyprDispatch::FocusWorkspace(ws.to_string())
        }
        "movetoworkspace" => {
            let addr = normalize_window_address(
                target.ok_or_else(|| anyhow!("pencere adresi gerekli"))?,
            );
            let ws = arg.ok_or_else(|| anyhow!("hedef workspace gerekli"))?;
            HyprDispatch::MoveToWorkspaceSilent { workspace: ws.to_string(), address: addr }
        }
        "fullscreen" => HyprDispatch::Fullscreen,
        "float" => {
            if let Some(t) = target {
                HyprDispatch::ToggleFloat(Some(normalize_window_address(t)))
            } else {
                HyprDispatch::ToggleFloat(None)
            }
        }
        "move" => {
            let coords = arg
                .or(target)
                .ok_or_else(|| anyhow!("move için `X Y` koordinatı gerekli"))?;
            HyprDispatch::MoveActiveExact(coords.to_string())
        }
        "resize" => {
            let dims = arg
                .or(target)
                .ok_or_else(|| anyhow!("resize için `W H` boyutu gerekli"))?;
            HyprDispatch::ResizeActiveExact(dims.to_string())
        }
        other => return Err(anyhow!("Bilinmeyen pencere aksiyonu: {}", other)),
    };

    hypr_dispatch_cmd(&dispatch_cmd)?;
    if action.eq_ignore_ascii_case("close") {
        if let Some(addr) = target {
            let clean = addr.trim_start_matches("0x");
            let _ = wait_for_compositor_event("closewindow", Some(clean), 800);
        }
    }

    Ok(ActionReceipt {
        ok: true,
        action: format!("window_{}", action),
        target: target.unwrap_or("active").to_string(),
        coordinates: None,
        details: format!("Hyprland dispatch çalıştırıldı: `{}`", dispatch_cmd.lua()),
    })
}

/// Uygulamayı başlatır, `stdout/stderr` çıktısını PC-Eye log dosyasına bağlar ve yeni pencere açılana kadar bekler.
pub fn launch_application(
    cmd_args: &[String],
    workspace: Option<&str>,
    wait_window_ms: u64,
) -> Result<ActionReceipt> {
    if cmd_args.is_empty() {
        return Err(anyhow!("Başlatılacak komut boş olamaz"));
    }

    let before = capture_desktop_state()?;
    let existing_addrs: std::collections::HashSet<String> =
        before.windows.into_iter().map(|w| w.address).collect();

    let log_dir = runtime_dir().join("app_logs");
    let _ = std::fs::create_dir_all(&log_dir);
    let app_name = cmd_args[0]
        .rsplit('/')
        .next()
        .unwrap_or("app")
        .replace(|c: char| !c.is_ascii_alphanumeric(), "_");
    let log_file_path = log_dir.join(format!("{}.log", app_name));

    let stdout_file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_file_path)?;
    let stderr_file = stdout_file.try_clone()?;

    let mut command = Command::new(&cmd_args[0]);
    if cmd_args.len() > 1 {
        command.args(&cmd_args[1..]);
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout_file))
        .stderr(Stdio::from(stderr_file));

    let child = command
        .spawn()
        .with_context(|| format!("Uygulama başlatılamadı: {:?}", cmd_args))?;
    let spawned_pid = child.id() as i32;

    if wait_window_ms > 0 {
        let _ = wait_for_compositor_event("openwindow", None, wait_window_ms);
    }

    let after = capture_desktop_state()?;
    let new_win = after
        .windows
        .iter()
        .find(|w| !existing_addrs.contains(&w.address))
        .cloned();

    if let (Some(ws), Some(win)) = (workspace, new_win.as_ref()) {
        let _ = hypr_dispatch_cmd(&HyprDispatch::MoveToWorkspaceSilent {
            workspace: ws.to_string(),
            address: win.address.clone(),
        });
    }

    let target_desc = if let Some(w) = new_win {
        format!("address={} pid={} title={}", w.address, w.pid, w.title)
    } else {
        format!("pid={} (pencere henüz açılmadı)", spawned_pid)
    };

    Ok(ActionReceipt {
        ok: true,
        action: "launch".to_string(),
        target: target_desc,
        coordinates: None,
        details: format!(
            "Komut başlatıldı (`{}`), stdout/stderr logu: {}",
            cmd_args.join(" "),
            log_file_path.display()
        ),
    })
}

/// K5 Kuralı: Watchdog korumalı ve Denylist denetimli süreç dondurma (`SIGSTOP` + otomatik `SIGCONT`).
pub fn freeze_process_safe(pid: i32, max_freeze_ms: u64) -> Result<ActionReceipt> {
    let clamped_ms = max_freeze_ms.clamp(10, 5000);
    let (members, _) = collect_app_cluster(pid)?;
    for m in &members {
        verify_freeze_target_safe(m.pid, &format!("{} {}", m.name, m.cmdline))?;
    }

    unsafe {
        if libc::kill(pid, libc::SIGSTOP) != 0 {
            return Err(anyhow!("PID {} için SIGSTOP gönderilemedi", pid));
        }
    }

    // K5 Kuralı: Masaüstünün asla kilitli kalmaması için arka planda bağımsız bir Watchdog başlat
    let _ = Command::new("sh")
        .arg("-c")
        .arg(format!(
            "sleep {:.3} && kill -CONT {} 2>/dev/null || true",
            (clamped_ms as f64) / 1000.0,
            pid
        ))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();

    Ok(ActionReceipt {
        ok: true,
        action: "freeze_process".to_string(),
        target: format!("PID {}", pid),
        coordinates: None,
        details: format!(
            "PID {} donduruldu (SIGSTOP). Watchdog en geç {} ms sonra otomatik SIGCONT gönderecek.",
            pid, clamped_ms
        ),
    })
}

/// Dondurulmuş süreci anında çözer (`SIGCONT`).
pub fn thaw_process(pid: i32) -> Result<ActionReceipt> {
    unsafe {
        if libc::kill(pid, libc::SIGCONT) != 0 {
            return Err(anyhow!("PID {} için SIGCONT gönderilemedi", pid));
        }
    }
    Ok(ActionReceipt {
        ok: true,
        action: "thaw_process".to_string(),
        target: format!("PID {}", pid),
        coordinates: None,
        details: format!("PID {} çözüldü (SIGCONT)", pid),
    })
}

/// Zengin Çok-Formatlı Pano Gözlemi (`MIME` türleri, metin, tarayıcı `source_url`, görsel varlığı ve `cliphist` geçmişi).
pub fn inspect_clipboard_rich() -> Result<crate::protocol::types::ClipboardReport> {
    let ts = crate::daemon::ring_buffer::now_ms();
    let types_out = Command::new("wl-paste").arg("--list-types").output().ok();
    let mime_types: Vec<String> = types_out
        .filter(|o| o.status.success())
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .map(|l| l.trim().to_string())
                .filter(|l| !l.is_empty())
                .collect()
        })
        .unwrap_or_default();

    let has_image = mime_types.iter().any(|m| m.starts_with("image/"));

    let source_url = if mime_types.iter().any(|m| m == "chromium/x-source-url") {
        Command::new("wl-paste")
            .args(["--no-newline", "--type", "chromium/x-source-url"])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .filter(|s| !s.is_empty())
    } else {
        None
    };

    let current_text = if mime_types
        .iter()
        .any(|m| m.starts_with("text/plain") || m == "UTF8_STRING" || m == "TEXT")
    {
        Command::new("wl-paste")
            .arg("--no-newline")
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
    } else {
        None
    };

    let mut history = Vec::new();
    if let Ok(ch_out) = Command::new("cliphist").arg("list").output() {
        if ch_out.status.success() {
            let stdout = String::from_utf8_lossy(&ch_out.stdout);
            for line in stdout.lines().take(15) {
                if let Some((id, preview)) = line.split_once('\t') {
                    let prev_trim = preview.trim().to_string();
                    let is_binary = prev_trim.starts_with("[[ binary data");
                    history.push(crate::protocol::types::ClipboardHistoryItem {
                        id: id.trim().to_string(),
                        preview: prev_trim,
                        is_binary,
                    });
                }
            }
        }
    }

    Ok(crate::protocol::types::ClipboardReport {
        timestamp_ms: ts,
        mime_types,
        current_text,
        source_url,
        has_image,
        history,
    })
}

/// Zengin pano kontrolü: metin yazma, görsel/dosya (`image/png` veya `text/uri-list`) kopyalama, `cliphist` ID geri yükleme veya zengin okuma.
pub fn clipboard_action_ext(
    write_text: Option<&str>,
    file_path: Option<&str>,
    restore_id: Option<&str>,
) -> Result<serde_json::Value> {
    if let Some(id) = restore_id {
        let dec = Command::new("cliphist")
            .args(["decode", id.trim()])
            .output()
            .context("cliphist decode çalıştırılamadı")?;
        if !dec.status.success() {
            return Err(anyhow!("cliphist ID bulunamadı: {}", id));
        }
        let mut child = Command::new("wl-copy")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(&dec.stdout)?;
        }
        thread::sleep(Duration::from_millis(20));
        return Ok(serde_json::to_value(ActionReceipt {
            ok: true,
            action: "clipboard_restore".to_string(),
            target: format!("cliphist:{}", id),
            coordinates: None,
            details: format!("cliphist #{} öğesi panoya geri yüklendi", id),
        })?);
    }

    if let Some(fp) = file_path {
        let path = std::path::Path::new(fp);
        if !path.exists() {
            return Err(anyhow!("Panoya kopyalanacak dosya bulunamadı: {}", fp));
        }
        let abs = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        let ext = abs
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();

        if ext == "png" || ext == "jpg" || ext == "jpeg" || ext == "webp" {
            let mime = if ext == "png" {
                "image/png"
            } else if ext == "webp" {
                "image/webp"
            } else {
                "image/jpeg"
            };
            let bytes = std::fs::read(&abs)?;
            let mut child = Command::new("wl-copy")
                .args(["--type", mime])
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()?;
            if let Some(mut stdin) = child.stdin.take() {
                stdin.write_all(&bytes)?;
            }
            thread::sleep(Duration::from_millis(25));
            return Ok(serde_json::to_value(ActionReceipt {
                ok: true,
                action: "clipboard_copy_image".to_string(),
                target: abs.to_string_lossy().to_string(),
                coordinates: None,
                details: format!("Görsel ({}, {} bayt) panoya kopyalandı", mime, bytes.len()),
            })?);
        } else {
            let uri = format!("file://{}\r\n", abs.to_string_lossy());
            let mut child = Command::new("wl-copy")
                .args(["--type", "text/uri-list"])
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()?;
            if let Some(mut stdin) = child.stdin.take() {
                stdin.write_all(uri.as_bytes())?;
            }
            thread::sleep(Duration::from_millis(20));
            return Ok(serde_json::to_value(ActionReceipt {
                ok: true,
                action: "clipboard_copy_file_uri".to_string(),
                target: abs.to_string_lossy().to_string(),
                coordinates: None,
                details: format!("Dosya URI (`text/uri-list`) panoya kopyalandı: {}", uri.trim()),
            })?);
        }
    }

    if let Some(txt) = write_text {
        let rec = clipboard_action(Some(txt))?;
        return Ok(serde_json::to_value(rec)?);
    }

    let rep = inspect_clipboard_rich()?;
    Ok(serde_json::to_value(rep)?)
}

/// Wayland panosunu (`wl-paste` / `wl-copy`) okur veya yazar.
pub fn clipboard_action(write_text: Option<&str>) -> Result<ActionReceipt> {
    if let Some(txt) = write_text {
        let mut child = Command::new("wl-copy")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context("wl-copy başlatılamadı")?;
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(txt.as_bytes())?;
        }
        thread::sleep(Duration::from_millis(20));
        Ok(ActionReceipt {
            ok: true,
            action: "clipboard_write".to_string(),
            target: "wayland_clipboard".to_string(),
            coordinates: None,
            details: format!("Panoya {} bayt yazıldı", txt.len()),
        })
    } else {
        let rep = inspect_clipboard_rich()?;
        Ok(ActionReceipt {
            ok: true,
            action: "clipboard_read".to_string(),
            target: "wayland_clipboard".to_string(),
            coordinates: None,
            details: serde_json::to_string(&rep).unwrap_or_default(),
        })
    }
}

/// Kullanıcının Hyprland yapılandırmasındaki bir kısayolu (`SUPER+Return`, `SUPER+D` vb.) doğrudan çalıştırır.
pub fn trigger_hyprland_bind(combo: &str) -> Result<ActionReceipt> {
    enforce_action_guard(None, false)?;
    let binds = crate::eyes::compositor::list_hyprland_binds()?;
    let target = binds
        .into_iter()
        .find(|b| b.combo.eq_ignore_ascii_case(combo.trim()))
        .ok_or_else(|| anyhow!("Hyprland kısayolu bulunamadı: `{}`", combo))?;

    if target.dispatcher.eq_ignore_ascii_case("lua") {
        return Err(anyhow!(
            "`{}` kısayolu anonim bir Lua kapanışıdır (hyprland.lua), IPC üzerinden tetiklenemez",
            combo
        ));
    }

    let dispatch_str = if target.arg.is_empty() {
        target.dispatcher.clone()
    } else {
        format!("{} {}", target.dispatcher, target.arg)
    };
    hypr_dispatch(&dispatch_str)?;

    Ok(ActionReceipt {
        ok: true,
        action: "use_bind".to_string(),
        target: target.combo,
        coordinates: None,
        details: format!("Hyprland kısayolu çalıştırıldı: `dispatch {}`", dispatch_str),
    })
}

