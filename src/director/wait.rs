use crate::eyes::compositor::{capture_desktop_state, wait_for_compositor_event};
use crate::eyes::logs_notify::query_recent_journal_logs;
use crate::eyes::process_xray::is_tcp_port_open;
use crate::eyes::ui_atspi::inspect_window_ui;
use crate::eyes::vision::{compute_dhash64, grab_region_ppm};
use crate::protocol::types::WaitReport;
use anyhow::{anyhow, Result};
use std::thread;
use std::time::{Duration, Instant};

/// Çok-Modlu Deterministik Bekleme Motoru (`pc-eye wait`).
/// Desteklenen modlar:
/// - `visual_settle`: Ekran/pencere animasyonu durulana kadar (ardışık 2 kare dHash farkı <= 1) bekler.
/// - `log_match`: `journald`, `app_logs` veya bildirimlerde `match_str` metni belirene kadar bekler.
/// - `ui_match`: Hedef pencerede `match_str` adında/rolünde bir AT-SPI kontrolü belirene kadar bekler.
/// - `port_open`: Belirtilen TCP portunun (`match_str` içinde port numarası) açılmasını bekler.
/// - `openwindow` / `closewindow` / `activewindow` / `workspace`: Hyprland `.socket2.sock` olayını bekler.
pub fn wait_for_condition(
    mode: &str,
    match_str: Option<&str>,
    window_addr: Option<&str>,
    timeout_ms: u64,
) -> Result<WaitReport> {
    let start = Instant::now();
    let deadline = start + Duration::from_millis(timeout_ms.max(100));
    let mode_lower = mode.to_ascii_lowercase();

    match mode_lower.as_str() {
        "visual_settle" | "settle" | "stable" => {
            let desktop = capture_desktop_state()?;
            let geom = if let Some(addr) = window_addr {
                let norm = crate::eyes::compositor::normalize_window_address(addr);
                desktop
                    .windows
                    .iter()
                    .find(|w| w.address == norm)
                    .map(|w| [w.x.max(0), w.y.max(0), w.width.max(10), w.height.max(10)])
                    .unwrap_or([0, 0, 1280, 800])
            } else {
                let mon = desktop.monitors.first();
                [
                    0,
                    0,
                    mon.map(|m| m.width).unwrap_or(2560),
                    mon.map(|m| m.height).unwrap_or(1600),
                ]
            };

            let mut prev_hash: Option<u64> = None;
            let mut stable_streak = 0u32;

            while Instant::now() < deadline {
                if let Ok(img) = grab_region_ppm(geom) {
                    let h = compute_dhash64(&img);
                    if let Some(prev) = prev_hash {
                        let hamming = (prev ^ h).count_ones();
                        if hamming <= 1 {
                            stable_streak += 1;
                            if stable_streak >= 2 {
                                return Ok(WaitReport {
                                    condition: "visual_settle".to_string(),
                                    matched: true,
                                    elapsed_ms: start.elapsed().as_millis() as u64,
                                    details: format!(
                                        "Görüntü duruldu (dHash={:016x}, hamming={})",
                                        h, hamming
                                    ),
                                });
                            }
                        } else {
                            stable_streak = 0;
                        }
                    }
                    prev_hash = Some(h);
                }
                thread::sleep(Duration::from_millis(90));
            }

            Err(anyhow!(
                "Zaman aşımı ({} ms): ekran görüntüsü durulmadı (visual_settle)",
                timeout_ms
            ))
        }
        "log_match" | "log" => {
            let needle = match_str
                .ok_or_else(|| anyhow!("`log_match` için `--match <METİN>` gerekli"))?
                .to_ascii_lowercase();

            while Instant::now() < deadline {
                if let Ok(logs) = query_recent_journal_logs(None, 40) {
                    if let Some(hit) = logs.iter().find(|l| {
                        l.message.to_ascii_lowercase().contains(&needle)
                            || l.unit_or_app.to_ascii_lowercase().contains(&needle)
                    }) {
                        return Ok(WaitReport {
                            condition: "log_match".to_string(),
                            matched: true,
                            elapsed_ms: start.elapsed().as_millis() as u64,
                            details: format!(
                                "[{} / {}] {}",
                                hit.source, hit.unit_or_app, hit.message
                            ),
                        });
                    }
                }
                thread::sleep(Duration::from_millis(150));
            }

            Err(anyhow!(
                "Zaman aşımı ({} ms): loglarda `{}` bulunamadı",
                timeout_ms,
                needle
            ))
        }
        "ui_match" | "ui" => {
            let needle = match_str.ok_or_else(|| anyhow!("`ui_match` için `--match <AD>` gerekli"))?;
            while Instant::now() < deadline {
                if let Ok(ui) = crate::eyes::a11y_events::with_fresh_reads(|| {
                    inspect_window_ui(window_addr, Some(needle), 40)
                }) {
                    if let Some(first) = ui.elements.first() {
                        return Ok(WaitReport {
                            condition: "ui_match".to_string(),
                            matched: true,
                            elapsed_ms: start.elapsed().as_millis() as u64,
                            details: format!(
                                "UI elemanı bulundu: [{}] {} ({}) @{:?}",
                                first.mark, first.name, first.role, first.click_point
                            ),
                        });
                    }
                }
                thread::sleep(Duration::from_millis(180));
            }

            Err(anyhow!(
                "Zaman aşımı ({} ms): UI ağacında `{}` bulunamadı",
                timeout_ms,
                needle
            ))
        }
        "port_open" | "port" => {
            let port_str =
                match_str.ok_or_else(|| anyhow!("`port_open` için `--match <PORT>` gerekli"))?;
            let port: u16 = port_str
                .trim()
                .parse()
                .map_err(|_| anyhow!("Geçersiz port numarası: {}", port_str))?;

            while Instant::now() < deadline {
                if let Some(info) = is_tcp_port_open(port) {
                    return Ok(WaitReport {
                        condition: "port_open".to_string(),
                        matched: true,
                        elapsed_ms: start.elapsed().as_millis() as u64,
                        details: format!("Port {} aktif: {}", port, info),
                    });
                }
                thread::sleep(Duration::from_millis(120));
            }

            Err(anyhow!(
                "Zaman aşımı ({} ms): TCP port {} açılmadı",
                timeout_ms,
                port
            ))
        }
        _ => {
            let ev = wait_for_compositor_event(mode, match_str, timeout_ms)?;
            Ok(WaitReport {
                condition: ev.kind,
                matched: true,
                elapsed_ms: start.elapsed().as_millis() as u64,
                details: ev.payload,
            })
        }
    }
}
