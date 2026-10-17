use crate::director::guard::{enforce_action_guard, verify_freeze_target_safe};
use crate::director::input::ActionReceipt;
use crate::eyes::compositor::{
    capture_desktop_state, hypr_dispatch, normalize_window_address, wait_for_compositor_event,
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

    let dispatch_cmd = match action.to_ascii_lowercase().as_str() {
        "focus" => {
            let addr = normalize_window_address(
                target.ok_or_else(|| anyhow!("focus için pencere adresi gerekli"))?,
            );
            format!("focuswindow address:{}", addr)
        }
        "close" => {
            let addr = normalize_window_address(
                target.ok_or_else(|| anyhow!("close için pencere adresi gerekli"))?,
            );
            format!("closewindow address:{}", addr)
        }
        "workspace" => {
            let ws = target
                .or(arg)
                .ok_or_else(|| anyhow!("workspace numarası/adı gerekli"))?;
            format!("workspace {}", ws)
        }
        "movetoworkspace" => {
            let addr = normalize_window_address(
                target.ok_or_else(|| anyhow!("pencere adresi gerekli"))?,
            );
            let ws = arg.ok_or_else(|| anyhow!("hedef workspace gerekli"))?;
            format!("movetoworkspacesilent {},address:{}", ws, addr)
        }
        "fullscreen" => "fullscreen 0".to_string(),
        "float" => {
            if let Some(t) = target {
                format!("togglefloating address:{}", normalize_window_address(t))
            } else {
                "togglefloating".to_string()
            }
        }
        "move" => {
            let coords = arg
                .or(target)
                .ok_or_else(|| anyhow!("move için `X Y` koordinatı gerekli"))?;
            format!("moveactive exact {}", coords)
        }
        "resize" => {
            let dims = arg
                .or(target)
                .ok_or_else(|| anyhow!("resize için `W H` boyutu gerekli"))?;
            format!("resizeactive exact {}", dims)
        }
        other => return Err(anyhow!("Bilinmeyen pencere aksiyonu: {}", other)),
    };

    hypr_dispatch(&dispatch_cmd)?;
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
        details: format!("Hyprland dispatch çalıştırıldı: `{}`", dispatch_cmd),
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
        let _ = hypr_dispatch(&format!("movetoworkspacesilent {},address:{}", ws, win.address));
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
        let out = Command::new("wl-paste")
            .arg("--no-newline")
            .output()
            .context("wl-paste çalıştırılamadı")?;
        let content = String::from_utf8_lossy(&out.stdout).to_string();
        Ok(ActionReceipt {
            ok: true,
            action: "clipboard_read".to_string(),
            target: "wayland_clipboard".to_string(),
            coordinates: None,
            details: content,
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

