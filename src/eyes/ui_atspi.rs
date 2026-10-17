use crate::daemon::ring_buffer::now_ms;
use crate::eyes::compositor::{capture_desktop_state, normalize_window_address};
use crate::eyes::process_xray::collect_app_cluster;
use crate::protocol::types::{TerminalContext, UiElement, UiTreeReport, WindowInfo};
use anyhow::{anyhow, Result};
use serde_json::Value;
use std::collections::{HashSet, VecDeque};
use std::fs;
use std::process::Command;

const TERMINAL_CLASSES: &[&str] = &[
    "kitty",
    "alacritty",
    "foot",
    "wezterm",
    "org.wezfurlong.wezterm",
    "ghostty",
    "konsole",
    "gnome-terminal",
    "xterm",
    "urxvt",
    "terminator",
    "tilix",
];

/// K1 Kuralı: Hyprland oturumunda AT-SPI2 erişilebilirlik veri yolunu (`IsEnabled = true`) zorunlu olarak etkinleştirir.
pub fn ensure_atspi_enabled() -> bool {
    let status = Command::new("busctl")
        .args([
            "--user",
            "set-property",
            "org.a11y.Bus",
            "/org/a11y/bus",
            "org.a11y.Status",
            "IsEnabled",
            "b",
            "true",
        ])
        .output();
    status.map(|o| o.status.success()).unwrap_or(false)
}

/// AT-SPI2 özel veri yolu adresini (`unix:path=/run/user/1000/at-spi/bus_0`) çözümler.
pub fn resolve_atspi_bus_address() -> Option<String> {
    let out = Command::new("busctl")
        .args([
            "--user",
            "call",
            "org.a11y.Bus",
            "/org/a11y/bus",
            "org.a11y.Bus",
            "GetAddress",
            "--json=short",
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let val: Value = serde_json::from_slice(&out.stdout).ok()?;
    val.get("data")?
        .as_array()?
        .first()?
        .as_str()
        .map(|s| s.to_string())
}

pub fn is_terminal_window(class: &str) -> bool {
    let lower = class.to_ascii_lowercase();
    TERMINAL_CLASSES.iter().any(|t| lower.contains(t))
}

/// Göz 2a: Hedef pencere (veya odaktaki pencere) için Semantik UI Ağacı + Koordinat Çözümlemesi + Terminal PTY Bağlamı üretir.
pub fn inspect_window_ui(
    target_window_addr: Option<&str>,
    query_filter: Option<&str>,
    max_elements: usize,
) -> Result<UiTreeReport> {
    let ts = now_ms();
    let a11y_bus_enabled = ensure_atspi_enabled();
    let desktop = capture_desktop_state()?;

    let win: WindowInfo = if let Some(addr) = target_window_addr {
        let norm = normalize_window_address(addr);
        desktop
            .windows
            .into_iter()
            .find(|w| w.address == norm)
            .ok_or_else(|| anyhow!("Pencere bulunamadı: {}", norm))?
    } else if let Some(active_addr) = desktop.active_window_address.as_deref() {
        desktop
            .windows
            .into_iter()
            .find(|w| w.address == active_addr)
            .ok_or_else(|| anyhow!("Aktif pencere bulunamadı"))?
    } else {
        return Err(anyhow!("Masaüstünde seçili veya aktif pencere yok"));
    };

    let win_geom = [win.x, win.y, win.width, win.height];

    // K2 Kuralı: Terminal penceresiyse OCR veya boş AT-SPI yerine doğrudan PTY ve alt süreç durumunu çıkar
    let terminal_context = if is_terminal_window(&win.class) && win.pid > 0 {
        Some(extract_terminal_context(win.pid))
    } else {
        None
    };

    let mut elements = Vec::new();
    if let Some(bus_addr) = resolve_atspi_bus_address() {
        elements = collect_atspi_elements_for_window(
            &bus_addr,
            &win,
            query_filter,
            max_elements.max(10),
        );
    }

    let perception_mode = if !elements.is_empty() {
        "at-spi2".to_string()
    } else if terminal_context.is_some() {
        "terminal-pty".to_string()
    } else {
        "vision-fallback".to_string()
    };

    let hint = match perception_mode.as_str() {
        "at-spi2" => Some(
            "Semantik UI elemanları hazır. `click-ui --mark <N>` veya `--name <AD>` ile doğrudan tıklayabilirsiniz."
                .to_string(),
        ),
        "terminal-pty" => Some(
            "Bu pencere bir terminal emülatörüdür. Ön plandaki kabuk/süreç durumu `terminal_context` altında verilmiştir."
                .to_string(),
        ),
        _ => Some(
            "Bu uygulama AT-SPI ağacı yayınlamıyor (veya Chromium/Electron `--force-renderer-accessibility` bayrağı olmadan başlatılmış). Görsel inceleme için `pc-eye vision --window <ADDR> --marks` kullanın."
                .to_string(),
        ),
    };

    Ok(UiTreeReport {
        timestamp_ms: ts,
        window_address: win.address,
        window_class: win.class,
        window_title: win.title,
        window_pid: win.pid,
        window_geometry: win_geom,
        a11y_bus_enabled,
        perception_mode,
        element_count: elements.len(),
        elements,
        terminal_context,
        hint,
    })
}

/// Terminal pencerelerinin içindeki kabuk, çalışma dizini (`cwd`), PTY ve alt süreçleri doğrudan `/proc` üzerinden okur.
fn extract_terminal_context(term_pid: i32) -> TerminalContext {
    let (members, _) = collect_app_cluster(term_pid).unwrap_or_default();
    let mut foreground_pid = term_pid;
    let mut foreground_cmd = String::new();
    let mut recent_children = Vec::new();

    for m in &members {
        if m.pid != term_pid {
            recent_children.push(format!("PID {} ({}): {}", m.pid, m.state, m.cmdline));
            // En derin (en son başlatılan) alt süreci ön plan süreci olarak seç
            if m.pid > foreground_pid {
                foreground_pid = m.pid;
                foreground_cmd = m.cmdline.clone();
            }
        } else if foreground_cmd.is_empty() {
            foreground_cmd = m.cmdline.clone();
        }
    }

    let cwd = fs::read_link(format!("/proc/{}/cwd", foreground_pid))
        .or_else(|_| fs::read_link(format!("/proc/{}/cwd", term_pid)))
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| "unknown".to_string());

    let pty_path = fs::read_link(format!("/proc/{}/fd/0", foreground_pid))
        .ok()
        .map(|p| p.to_string_lossy().to_string())
        .filter(|s| s.contains("/dev/pts/"));

    TerminalContext {
        terminal_pid: term_pid,
        foreground_pid,
        foreground_cmd,
        cwd,
        pty_path,
        recent_child_processes: recent_children,
    }
}

fn collect_atspi_elements_for_window(
    bus_addr: &str,
    win: &WindowInfo,
    query_filter: Option<&str>,
    max_elements: usize,
) -> Vec<UiElement> {
    // 1. Kök kayıt defterinden tüm kayıtlı uygulamaları çek
    let Some(root_children) = atspi_get_children(
        bus_addr,
        "org.a11y.atspi.Registry",
        "/org/a11y/atspi/accessible/root",
    ) else {
        return Vec::new();
    };

    // Pencerenin ait olduğu PID kümesini (AppCluster) bul
    let cluster_pids: HashSet<i32> = collect_app_cluster(win.pid)
        .map(|(m, _)| m.into_iter().map(|p| p.pid).collect())
        .unwrap_or_else(|_| {
            let mut s = HashSet::new();
            if win.pid > 0 {
                s.insert(win.pid);
            }
            s
        });

    let win_class_lower = win.class.to_ascii_lowercase();
    let mut matching_services = Vec::new();

    for (service, root_path) in root_children {
        let conn_pid = atspi_get_service_pid(bus_addr, &service);
        let app_name = atspi_get_string_prop(bus_addr, &service, &root_path, "Name")
            .unwrap_or_default()
            .to_ascii_lowercase();

        let pid_matches = conn_pid.map(|p| cluster_pids.contains(&p)).unwrap_or(false);
        let name_matches = !app_name.is_empty()
            && (win_class_lower.contains(&app_name) || app_name.contains(&win_class_lower));

        if pid_matches || name_matches {
            matching_services.push((service, root_path));
        }
    }

    let mut elements = Vec::new();
    let filter_lower = query_filter.map(|q| q.to_ascii_lowercase());

    for (service, root_path) in matching_services {
        // BFS ile erişilebilirlik ağacını tara (maksimum 80 düğüm ziyareti)
        let mut queue = VecDeque::new();
        let mut visited = HashSet::new();
        queue.push_back(root_path.clone());
        visited.insert(root_path);

        let mut inspected_nodes = 0usize;
        while let Some(path) = queue.pop_front() {
            if inspected_nodes >= 80 || elements.len() >= max_elements {
                break;
            }
            inspected_nodes += 1;

            let role = atspi_call_string(bus_addr, &service, &path, "GetRoleName")
                .unwrap_or_else(|| "unknown".to_string());
            let name =
                atspi_get_string_prop(bus_addr, &service, &path, "Name").unwrap_or_default();

            if let Some(extents) = atspi_get_extents(bus_addr, &service, &path) {
                let [rx, ry, rw, rh] = extents;
                if rw > 2 && rh > 2 && (!name.is_empty() || is_interactive_role(&role)) {
                    // K1 Kuralı: Coordinate Space Resolver
                    // Eğer AT-SPI koordinatları pencere içi göreli ise (ve pencere (0,0)'da değilse) Hyprland pencere ofsetini ekle
                    let (sx, sy) = resolve_screen_coordinates(rx, ry, rw, rh, win);
                    let matches_query = filter_lower
                        .as_ref()
                        .map(|q| {
                            name.to_ascii_lowercase().contains(q)
                                || role.to_ascii_lowercase().contains(q)
                        })
                        .unwrap_or(true);

                    if matches_query {
                        let mark = elements.len() + 1;
                        let cx = sx + rw / 2;
                        let cy = sy + rh / 2;
                        elements.push(UiElement {
                            mark,
                            role: role.clone(),
                            name: name.clone(),
                            value: None,
                            description: None,
                            states: Vec::new(),
                            actions: vec!["click".to_string()],
                            rel_bounds: [rx, ry, rw, rh],
                            screen_bounds: [sx, sy, rw, rh],
                            click_point: [cx, cy],
                            dbus_service: service.clone(),
                            dbus_path: path.clone(),
                        });
                    }
                }
            }

            if let Some(children) = atspi_get_children(bus_addr, &service, &path) {
                for (child_srv, child_path) in children {
                    if child_srv == service && visited.insert(child_path.clone()) {
                        queue.push_back(child_path);
                    }
                }
            }
        }
    }

    elements
}

/// K1 Kuralı: Wayland pencere-içi `(rx, ry)` koordinatlarını Hyprland ekran `(sx, sy)` koordinatlarına dönüştürür.
pub fn resolve_screen_coordinates(
    rx: i64,
    ry: i64,
    rw: i64,
    rh: i64,
    win: &WindowInfo,
) -> (i64, i64) {
    // Eğer rx, ry zaten pencerenin ekran sınırları içindeyse ve pencere (0,0)'dan uzaktaysa doğrudan kullan;
    // aksi halde pencere-içi göreli koordinattır -> win.x + rx, win.y + ry uygula.
    let already_global = (win.x > 50 || win.y > 50)
        && rx >= win.x
        && ry >= win.y
        && (rx + rw) <= (win.x + win.width + 20)
        && (ry + rh) <= (win.y + win.height + 20);

    if already_global {
        (rx, ry)
    } else {
        (win.x + rx, win.y + ry)
    }
}

fn is_interactive_role(role: &str) -> bool {
    matches!(
        role,
        "push button"
            | "button"
            | "toggle button"
            | "check box"
            | "radio button"
            | "menu item"
            | "text"
            | "entry"
            | "password text"
            | "combo box"
            | "link"
            | "page tab"
            | "slider"
            | "spin button"
    )
}

fn atspi_get_service_pid(bus_addr: &str, service: &str) -> Option<i32> {
    let out = Command::new("busctl")
        .arg(format!("--address={}", bus_addr))
        .args([
            "call",
            "org.freedesktop.DBus",
            "/org/freedesktop/DBus",
            "org.freedesktop.DBus",
            "GetConnectionUnixProcessID",
            "s",
            service,
            "--json=short",
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let val: Value = serde_json::from_slice(&out.stdout).ok()?;
    val.get("data")?.as_array()?.first()?.as_i64().map(|n| n as i32)
}

fn atspi_get_children(bus_addr: &str, service: &str, path: &str) -> Option<Vec<(String, String)>> {
    let out = Command::new("busctl")
        .arg(format!("--address={}", bus_addr))
        .args([
            "call",
            service,
            path,
            "org.a11y.atspi.Accessible",
            "GetChildren",
            "--json=short",
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let val: Value = serde_json::from_slice(&out.stdout).ok()?;
    let outer = val.get("data")?.as_array()?.first()?.as_array()?;
    let mut res = Vec::new();
    for item in outer {
        if let Some(pair) = item.as_array() {
            if pair.len() == 2 {
                let srv = pair[0].as_str()?.to_string();
                let pth = pair[1].as_str()?.to_string();
                res.push((srv, pth));
            }
        }
    }
    Some(res)
}

fn atspi_call_string(bus_addr: &str, service: &str, path: &str, method: &str) -> Option<String> {
    let out = Command::new("busctl")
        .arg(format!("--address={}", bus_addr))
        .args([
            "call",
            service,
            path,
            "org.a11y.atspi.Accessible",
            method,
            "--json=short",
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let val: Value = serde_json::from_slice(&out.stdout).ok()?;
    val.get("data")?
        .as_array()?
        .first()?
        .as_str()
        .map(|s| s.to_string())
}

fn atspi_get_string_prop(bus_addr: &str, service: &str, path: &str, prop: &str) -> Option<String> {
    let out = Command::new("busctl")
        .arg(format!("--address={}", bus_addr))
        .args([
            "get-property",
            service,
            path,
            "org.a11y.atspi.Accessible",
            prop,
            "--json=short",
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let val: Value = serde_json::from_slice(&out.stdout).ok()?;
    val.get("data")?.as_str().map(|s| s.to_string())
}

fn atspi_get_extents(bus_addr: &str, service: &str, path: &str) -> Option<[i64; 4]> {
    let out = Command::new("busctl")
        .arg(format!("--address={}", bus_addr))
        .args([
            "call",
            service,
            path,
            "org.a11y.atspi.Component",
            "GetExtents",
            "u",
            "0",
            "--json=short",
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let val: Value = serde_json::from_slice(&out.stdout).ok()?;
    let tuple = val.get("data")?.as_array()?.first()?.as_array()?;
    if tuple.len() == 4 {
        Some([
            tuple[0].as_i64()?,
            tuple[1].as_i64()?,
            tuple[2].as_i64()?,
            tuple[3].as_i64()?,
        ])
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_coordinate_space_resolver_relative_to_global() {
        let win = WindowInfo {
            address: "0x123".to_string(),
            class: "firefox".to_string(),
            title: "Test".to_string(),
            pid: 1000,
            workspace_id: 1,
            workspace_name: "1".to_string(),
            monitor: 0,
            x: 400,
            y: 200,
            width: 1200,
            height: 800,
            floating: false,
            fullscreen: 0,
            xwayland: false,
            mapped: true,
            hidden: false,
            focused: true,
            protected_window: false,
        };

        // Pencere içi (20, 30) koordinatı -> Ekran (420, 230) olmalı
        let (sx, sy) = resolve_screen_coordinates(20, 30, 80, 32, &win);
        assert_eq!((sx, sy), (420, 230));

        // Zaten ekran koordinatıysa (450, 250) -> olduğu gibi (450, 250) kalmalı
        let (gx, gy) = resolve_screen_coordinates(450, 250, 80, 32, &win);
        assert_eq!((gx, gy), (450, 250));
    }
}
