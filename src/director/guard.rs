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
}
