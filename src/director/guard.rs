use crate::eyes::compositor::{capture_desktop_state, normalize_window_address};
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
        // `-c`, `-lc`, `-ec` gibi birleşik kabuk bayrakları; yorumlayıcılarda `-c`/`-e`/`--eval`.
        let inline = rest.iter().any(|a| {
            (is_shell && a.starts_with('-') && !a.starts_with("--") && a.contains('c'))
                || (is_interp && matches!(a.as_str(), "-c" | "-e" | "-E" | "--eval" | "-r"))
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
        ] {
            assert!(enforce_mcp_launch_policy(&argv(bad)).is_err(), "{:?}", bad);
        }
        for ok in [
            &["firefox", "https://example.com"][..],
            &["kitty"],
            &["python3", "/home/u/app.py", "--scene", "form"],
            &["gnome-text-editor", "-c"],
            &["bash", "script.sh"],
        ] {
            assert!(enforce_mcp_launch_policy(&argv(ok)).is_ok(), "{:?}", ok);
        }
    }
}
