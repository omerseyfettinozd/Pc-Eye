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
/// Daemon olay akışındaki türler (`event` beklemesinde "tür:metin" kısaltması için).
const EVENT_KINDS: &[&str] = &["file", "journal", "coredump", "unit", "network"];

pub fn wait_for_condition(
    mode: &str,
    match_str: Option<&str>,
    window_addr: Option<&str>,
    timeout_ms: u64,
) -> Result<WaitReport> {
    let start = Instant::now();
    let deadline = start + Duration::from_millis(timeout_ms.max(100));
    let mut mode_lower = mode.to_ascii_lowercase();
    // Ajanlar olay beklemesini sık sık `{"event": "file:rapor.pdf"}` diye yazıyor (match yerine event'e):
    // bilinen bir akış türüyle başlayan event, olay beklemesi sayılır (ölçüm: Haiku 2/3 koşuda).
    let shorthand;
    let match_str = match mode.split_once(':') {
        Some((k, _)) if EVENT_KINDS.contains(&k.to_ascii_lowercase().as_str()) && match_str.is_none() => {
            shorthand = mode.to_string();
            mode_lower = "event".to_string();
            Some(shorthand.as_str())
        }
        // `{"event": "file", "match": ".pdf"}`: tür event'te, metin match'te.
        None if EVENT_KINDS.contains(&mode.to_ascii_lowercase().as_str()) => {
            shorthand = format!("{}:{}", mode.to_ascii_lowercase(), match_str.unwrap_or(""));
            mode_lower = "event".to_string();
            Some(shorthand.as_str())
        }
        _ => match_str,
    };

    match mode_lower.as_str() {
        "visual_settle" | "settle" | "stable" => {
            let desktop = capture_desktop_state()?;
            let geom = if let Some(addr) = window_addr {
                let norm = crate::eyes::compositor::normalize_window_address(addr);
                // Pencere bulunamazsa sabit bir bölgeye düşmek yanlış yeri "duruldu" sayardı.
                desktop
                    .windows
                    .iter()
                    .find(|w| w.address == norm)
                    .map(|w| [w.x.max(0), w.y.max(0), w.width.max(10), w.height.max(10)])
                    .ok_or_else(|| anyhow!("pencere bulunamadı: {}", addr))?
            } else {
                let mon = desktop.monitors.first();
                [
                    0,
                    0,
                    mon.map(|m| m.width).unwrap_or(2560),
                    mon.map(|m| m.height).unwrap_or(1600),
                ]
            };

            // pc-eye-wm: "durdu" kompozitörün kare sessizliğidir (piksel yoklaması yok).
            if crate::wm_backend::active() {
                let left = deadline.saturating_duration_since(Instant::now()).as_millis() as u64;
                let r = crate::wm_backend::wait_idle(80, left, 0)?;
                if r["idle"].as_bool() == Some(true) {
                    return Ok(WaitReport {
                        condition: "visual_settle".to_string(),
                        matched: true,
                        elapsed_ms: start.elapsed().as_millis() as u64,
                        details: format!("kare sessizliği (pc-eye-wm, {} kare)", r["commits"]),
                    });
                }
                return Err(anyhow!("Zaman aşımı: pencereler kare göndermeyi kesmedi (pc-eye-wm)"));
            }
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
        // Daemon olay akışında (dosya, günlük, servis, ağ, pencere) `match_str` geçen yeni bir olay.
        // `match_str` "tür:metin" biçiminde türle daraltılabilir: "file:rapor.pdf", "unit:failed".
        "event" | "event_match" => {
            crate::daemon::server::ensure_background_daemon();
            let raw = match_str.ok_or_else(|| anyhow!("event beklemesi için match gerekli (ör. file:rapor.pdf)"))?;
            let (kinds, needle) = match raw.split_once(':') {
                Some((k, n)) if !k.contains(' ') && !k.is_empty() => (Some(vec![k.to_string()]), n.to_string()),
                _ => (None, raw.to_string()),
            };
            let needle = crate::eyes::fold(&needle);
            let ask = |since: u64| {
                crate::daemon::rpc::request("events", serde_json::json!({ "since": since, "kinds": kinds, "limit": 2000 }))
                    .ok_or_else(|| anyhow!("daemon olay akışına ulaşılamadı"))?
                    .map_err(|e| anyhow!(e))
            };
            // Yalnız bundan SONRA gelen olaylar: önceki eşleşmeler beklemeyi yanlışlıkla bitirmesin.
            let mut since = crate::daemon::rpc::request("events", serde_json::json!({ "since": 0, "kinds": ["-"], "limit": 1 }))
                .and_then(|r| r.ok())
                .and_then(|r| r["last_seq"].as_u64())
                .unwrap_or(0);
            while Instant::now() < deadline {
                let rep = ask(since)?;
                for e in rep["events"].as_array().into_iter().flatten() {
                    let payload = e["payload"].as_str().unwrap_or_default();
                    if crate::eyes::fold(payload).contains(&needle) {
                        return Ok(WaitReport {
                            condition: format!("event({})", raw),
                            matched: true,
                            elapsed_ms: start.elapsed().as_millis() as u64,
                            details: format!("{} {}", e["kind"].as_str().unwrap_or("?"), payload),
                        });
                    }
                    since = since.max(e["seq"].as_u64().unwrap_or(since));
                }
                since = since.max(rep["last_seq"].as_u64().unwrap_or(since));
                thread::sleep(Duration::from_millis(100));
            }
            Ok(WaitReport {
                condition: format!("event({})", raw),
                matched: false,
                elapsed_ms: start.elapsed().as_millis() as u64,
                details: "zaman aşımı".to_string(),
            })
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
