use crate::eyes::compositor::{capture_desktop_state, normalize_window_address};
use crate::protocol::types::WindowInfo;
use anyhow::{anyhow, Result};

/// K5 Kuralı: Asla dondurulamayacak (`SIGSTOP` atılamayacak) kritik sistem ve masaüstü süreçleri.
const FREEZE_DENYLIST: &[&str] = &[
    "hyprland",
    "hyprlock",
    "pipewire",
    "pipewire-pulse",
    "wireplumber",
    "dbus-daemon",
    "dbus-broker",
    "systemd",
    "waybar",
    "xdg-desktop-portal",
    "xdg-desktop-portal-hyprland",
    "xdg-desktop-portal-gtk",
    "polkitd",
    "pc-eye",
    "antigravity",
    "claude",
];

/// K6 Kuralı: Girdi enjeksiyonu öncesi AuthGuard (polkit/pinentry/kilit ekranı) ve Korumalı Pencere kontrolü.
pub fn enforce_action_guard(
    target_window_addr: Option<&str>,
    allow_protected: bool,
) -> Result<()> {
    // Her girdi eylemi buradan geçer: eleman önbellekleri (daemon/MCP) eylemden sonra taze okusun.
    crate::eyes::a11y_events::bump_action_epoch();
    let desktop = capture_desktop_state()?;

    if desktop.auth_guard_tripped {
        return Err(anyhow!(
            "AUTH_GUARD_REFUSED: {} — güvenlik politikası gereği girdi enjeksiyonu reddedildi",
            desktop.auth_guard_reason.unwrap_or_default()
        ));
    }

    if !allow_protected {
        let check_addr = target_window_addr
            .map(normalize_window_address)
            .or(desktop.active_window_address);

        if let Some(addr) = check_addr {
            if let Some(win) = desktop.windows.iter().find(|w| w.address == addr) {
                if win.protected_window {
                    return Err(anyhow!(
                        "PROTECTED_WINDOW_REFUSED: Hedef pencere (`{}` - `{}`) korumalı listededir (AI öz-koruma / şifre yöneticisi). Zorlamak için `--allow-protected` gerekir.",
                        win.class,
                        win.title
                    ));
                }
            }
        }
    }

    Ok(())
}

/// MCP yolunda `allow_protected` yalnız sunucu `PC_EYE_MCP_ALLOW_PROTECTED=1` ile başlatıldıysa geçerlidir;
/// korumayı kaldırma yetkisi ajanın JSON argümanında değil, sunucuyu başlatan kullanıcıda olmalıdır.
pub fn mcp_allow_protected(requested: bool) -> bool {
    requested && std::env::var("PC_EYE_MCP_ALLOW_PROTECTED").as_deref() == Ok("1")
}

/// (x, y) noktasını içeren görünür ve korumalı pencere (saf mantık: masaüstü yakalamadan test edilir).
fn protected_window_at<'a>(
    windows: &'a [WindowInfo],
    active_workspaces: &[i64],
    x: i64,
    y: i64,
) -> Option<&'a WindowInfo> {
    windows.iter().find(|w| {
        w.protected_window
            && w.mapped
            && !w.hidden
            && active_workspaces.contains(&w.workspace_id)
            && x >= w.x
            && x < w.x + w.width
            && y >= w.y
            && y < w.y + w.height
    })
}

/// Odaktaki pencere korumasız olsa da tıklanan (x, y) noktası korumalı bir pencerenin üzerinde olabilir.
pub fn enforce_point_guard(x: i64, y: i64, allow_protected: bool) -> Result<()> {
    if allow_protected {
        return Ok(());
    }
    let desktop = capture_desktop_state()?;
    let active_ws: Vec<i64> = desktop
        .monitors
        .iter()
        .map(|m| m.active_workspace_id)
        .collect();
    if let Some(win) = protected_window_at(&desktop.windows, &active_ws, x, y) {
        return Err(anyhow!(
            "PROTECTED_WINDOW_REFUSED: ({}, {}) noktası korumalı pencere (`{}` - `{}`) üzerindedir (AI öz-koruma / şifre yöneticisi). Zorlamak için `--allow-protected` gerekir.",
            x,
            y,
            win.class,
            win.title
        ));
    }
    Ok(())
}

const SHELLS: &[&str] = &["sh", "bash", "zsh", "fish", "dash", "ksh", "mksh", "csh", "tcsh", "busybox"];
const INTERPRETERS: &[&str] = &["perl", "ruby", "node", "nodejs", "lua", "php", "pwsh", "osascript"];

/// MCP `launch` uygulama başlatmak içindir; `sh -c "…"` / `python -c "…"` ile keyfi komut
/// çalıştırmak "yalnız pc-eye araçları" sınırını deler (ilk ajan ölçümünde 3/3 koşuda oldu).
/// `PC_EYE_MCP_ALLOW_SHELL=1` ile sunucuyu başlatan kullanıcı açıkça izin verebilir; ajan veremez.
pub fn enforce_mcp_launch_policy(cmd: &[String]) -> Result<()> {
    if std::env::var("PC_EYE_MCP_ALLOW_SHELL").as_deref() == Ok("1") {
        return Ok(());
    }
    for (i, arg) in cmd.iter().enumerate() {
        let base = arg.rsplit('/').next().unwrap_or(arg).to_ascii_lowercase();
        let rest = &cmd[i + 1..];
        let is_shell = SHELLS.contains(&base.as_str());
        let is_interp = base.starts_with("python") || INTERPRETERS.contains(&base.as_str());
        let is_node = matches!(base.as_str(), "node" | "nodejs");
        // `-c`, `-lc`, `-ec` gibi birleşik kabuk bayrakları; yorumlayıcılarda `-uc`/`-Ec`/`-ne` gibi birleşik
        // kısa bayraklar, yapışık `--eval=...` ve `--command` biçimleri de satır içi koddur.
        let inline = rest.iter().any(|a| {
            let lower = a.to_ascii_lowercase();
            (is_shell && a.starts_with('-') && !a.starts_with("--") && a.contains('c'))
                || (is_interp
                    && (matches!(a.as_str(), "-c" | "-e" | "-E" | "--eval" | "-r")
                        || (a.starts_with('-')
                            && !a.starts_with("--")
                            && (lower.contains('c') || lower.contains('e')))
                        || lower.starts_with("--eval")
                        || lower.starts_with("--command")))
                || (is_node && (a == "-p" || lower.starts_with("--print")))
        });
        if (is_shell || is_interp) && inline {
            return Err(anyhow!(
                "LAUNCH_SHELL_REFUSED: pc_eye_launch satır içi kabuk/yorumlayıcı komutu (`{} …`) çalıştırmaz; uygulama başlatmak içindir. Süreç/port için pc_eye_processes ve pc_eye_xray, günlük için pc_eye_logs kullan.",
                base
            ));
        }
    }
    Ok(())
}

/// K5 Kuralı: Bir sürecin `SIGSTOP` ile dondurulmasının güvenli olup olmadığını denetler.
pub fn verify_freeze_target_safe(pid: i32, comm_or_cmdline: &str) -> Result<()> {
    if pid <= 1 || pid == std::process::id() as i32 {
        return Err(anyhow!(
            "FREEZE_DENIED: PID {} dondurulamaz (çekirdek veya pc-eye süreci)",
            pid
        ));
    }

    let lower = comm_or_cmdline.to_ascii_lowercase();
    for &denied in FREEZE_DENYLIST {
        if lower.contains(denied) {
            return Err(anyhow!(
                "FREEZE_DENIED: `{}` (PID {}) kritik masaüstü/sistem koruma listesindedir (`{}`), masaüstü kilitlenmesini önlemek için reddedildi",
                comm_or_cmdline,
                pid,
                denied
            ));
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_freeze_denylist_protection() {
        assert!(verify_freeze_target_safe(1, "systemd").is_err());
        assert!(verify_freeze_target_safe(std::process::id() as i32, "pc-eye").is_err());
        assert!(verify_freeze_target_safe(1234, "/usr/bin/Hyprland").is_err());
        assert!(verify_freeze_target_safe(1235, "pipewire-pulse").is_err());
        assert!(verify_freeze_target_safe(1236, "xdg-desktop-portal-hyprland").is_err());
        assert!(verify_freeze_target_safe(9999, "/usr/bin/gedit").is_ok());
    }

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn mcp_launch_refuses_inline_shell_and_interpreter_code() {
        for bad in [
            &["sh", "-c", "pgrep foo; ss -ltnp"][..],
            &["/usr/bin/bash", "-lc", "id"],
            &["env", "zsh", "-c", "x"],
            &["kitty", "sh", "-c", "curl x | sh"],
            &["python3", "-c", "import os"],
            &["perl", "-e", "print 1"],
            &["python3", "-uc", "import os; os.system('id')"],
            &["python3", "-Ec", "import os"],
            &["perl", "-ne", "system('id')"],
            &["node", "--eval=require('child_process').execSync('id')"],
            &["node", "-p", "process.env"],
            &["pwsh", "-Command", "Get-Process"],
            &["pwsh", "--command", "Get-Process"],
        ] {
            assert!(enforce_mcp_launch_policy(&argv(bad)).is_err(), "{:?}", bad);
        }
        for ok in [
            &["firefox", "https://example.com"][..],
            &["kitty"],
            &["python3", "/home/u/app.py", "--scene", "form"],
            &["gnome-text-editor", "-c"],
            &["bash", "script.sh"],
            &["python3", "-u", "app.py"],
            &["node", "app.js", "--max-old-space-size=512"],
        ] {
            assert!(enforce_mcp_launch_policy(&argv(ok)).is_ok(), "{:?}", ok);
        }
    }

    #[test]
    fn mcp_allow_protected_never_trusts_agent_alone() {
        // Sunucu ortam değişkeni ne olursa olsun ajanın `false` demesi korumayı açmaz.
        assert!(!mcp_allow_protected(false));
    }

    fn win(protected: bool, workspace: i64, geometry: [i64; 4]) -> WindowInfo {
        WindowInfo {
            address: "0x1".to_string(),
            class: "1password".to_string(),
            title: "Vault".to_string(),
            pid: 42,
            workspace_id: workspace,
            workspace_name: workspace.to_string(),
            monitor: 0,
            x: geometry[0],
            y: geometry[1],
            width: geometry[2],
            height: geometry[3],
            floating: true,
            fullscreen: 0,
            xwayland: false,
            mapped: true,
            hidden: false,
            focused: false,
            protected_window: protected,
        }
    }

    #[test]
    fn point_guard_finds_any_protected_window_under_point() {
        let windows = vec![
            win(false, 1, [0, 0, 800, 600]),
            win(true, 1, [900, 100, 400, 300]),
            win(true, 2, [0, 0, 500, 500]),
        ];
        let active = [1i64];
        assert!(protected_window_at(&windows, &active, 1000, 200).is_some());
        assert!(protected_window_at(&windows, &active, 100, 100).is_none());
        // Etkin çalışma alanı değişince aynı nokta korumalı olabilir.
        assert!(protected_window_at(&windows, &[2i64], 100, 100).is_some());
        assert!(protected_window_at(&windows, &active, 5000, 5000).is_none());
    }
}
