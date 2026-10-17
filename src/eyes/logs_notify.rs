use crate::daemon::ring_buffer::now_ms;
use crate::eyes::vision::runtime_dir;
use crate::protocol::types::LogEntry;
use anyhow::Result;
use serde_json::Value;
use std::fs;
use std::process::Command;

/// Göz 4: `journalctl`, PC-Eye `app_logs/*.log` (`stdout/stderr`) ve `dunstctl history` masaüstü bildirimlerini
/// tek bir kronolojik akışta birleştirir.
pub fn query_recent_journal_logs(
    filter_pids: Option<&[i32]>,
    limit: usize,
) -> Result<Vec<LogEntry>> {
    let mut entries = Vec::new();

    // 1. systemd-journald kayıtları
    if let Ok(mut j_logs) = read_journald_entries(filter_pids, limit) {
        entries.append(&mut j_logs);
    }

    // 2. PC-Eye `launch` ile başlatılan uygulamaların `app_logs/*.log` (`stdout/stderr`) çıktıları
    if let Ok(mut app_logs) = read_managed_app_logs(limit.min(20)) {
        entries.append(&mut app_logs);
    }

    // 3. Masaüstü Bildirimleri (`dunstctl history`)
    if filter_pids.is_none() {
        let notify_quota = (limit / 3).clamp(1, 5);
        if let Ok(mut notify_logs) = read_dunst_notifications(notify_quota) {
            entries.append(&mut notify_logs);
        }
    }

    entries.sort_by_key(|e| e.timestamp_ms);
    if entries.len() > limit {
        let skip = entries.len() - limit;
        entries = entries.into_iter().skip(skip).collect();
    }

    Ok(entries)
}

fn read_journald_entries(filter_pids: Option<&[i32]>, limit: usize) -> Result<Vec<LogEntry>> {
    let mut cmd = Command::new("journalctl");
    cmd.arg("--no-pager")
        .arg("-o")
        .arg("json")
        .arg("-n")
        .arg(limit.to_string());

    if let Some(pids) = filter_pids {
        if pids.is_empty() {
            return Ok(Vec::new());
        }
        for &pid in pids.iter().take(24) {
            if pid > 0 {
                cmd.arg(format!("_PID={}", pid));
            }
        }
    } else {
        cmd.arg("-p").arg("0..4");
    }

    let output = cmd.output()?;
    if !output.status.success() {
        return Ok(Vec::new());
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut entries = Vec::new();
    for line in stdout.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(val) = serde_json::from_str::<Value>(trimmed) else {
            continue;
        };
        if let Some(entry) = parse_journal_json(&val) {
            entries.push(entry);
        }
    }
    Ok(entries)
}

/// `/run/user/1000/pc-eye/app_logs/*.log` içindeki uygulama `stdout/stderr` satırlarını okur.
fn read_managed_app_logs(max_lines_per_app: usize) -> Result<Vec<LogEntry>> {
    let log_dir = runtime_dir().join("app_logs");
    if !log_dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    let ts = now_ms();

    for entry in fs::read_dir(log_dir)?.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("log") {
            continue;
        }
        let app_name = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("app")
            .to_string();
        let mtime_ms = entry
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as u64)
            .unwrap_or(ts);

        let Ok(content) = fs::read_to_string(&path) else {
            continue;
        };
        let lines: Vec<&str> = content
            .lines()
            .map(|l| l.trim())
            .filter(|l| !l.is_empty())
            .collect();
        let start = lines.len().saturating_sub(max_lines_per_app);

        for line in &lines[start..] {
            let lower = line.to_ascii_lowercase();
            let level = if lower.contains("error")
                || lower.contains("panic")
                || lower.contains("fatal")
                || lower.contains("segfault")
            {
                "err"
            } else if lower.contains("warn") {
                "warn"
            } else {
                "info"
            };
            out.push(LogEntry {
                timestamp_ms: mtime_ms,
                source: "app-stderr".to_string(),
                level: level.to_string(),
                pid: None,
                unit_or_app: app_name.clone(),
                message: truncate_str(line, 300),
            });
        }
    }
    Ok(out)
}

/// `dunstctl history` üzerinden masaüstü bildirim geçmişini okur.
pub fn read_dunst_notifications(limit: usize) -> Result<Vec<LogEntry>> {
    let out = Command::new("dunstctl").arg("history").output()?;
    if !out.status.success() {
        return Ok(Vec::new());
    }
    let val: Value = serde_json::from_slice(&out.stdout)?;
    let Some(items) = val
        .get("data")
        .and_then(|d| d.as_array())
        .and_then(|a| a.first())
        .and_then(|a| a.as_array())
    else {
        return Ok(Vec::new());
    };

    let ts = now_ms();
    let uptime_ms = fs::read_to_string("/proc/uptime")
        .ok()
        .and_then(|s| s.split_whitespace().next()?.parse::<f64>().ok())
        .map(|secs| (secs * 1000.0) as u64)
        .unwrap_or(0);
    let boot_epoch_ms = ts.saturating_sub(uptime_ms);

    let mut entries = Vec::new();
    for item in items.iter().take(limit) {
        let appname = item
            .get("appname")
            .and_then(|v| v.get("data"))
            .and_then(|v| v.as_str())
            .unwrap_or("notification")
            .to_string();
        let summary = item
            .get("summary")
            .and_then(|v| v.get("data"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let body = item
            .get("body")
            .and_then(|v| v.get("data"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let urgency = item
            .get("urgency")
            .and_then(|v| v.get("data"))
            .and_then(|v| v.as_str())
            .unwrap_or("NORMAL");
        let item_ts_ms = item
            .get("timestamp")
            .and_then(|v| v.get("data"))
            .and_then(|v| v.as_u64())
            .map(|usec| boot_epoch_ms + (usec / 1000))
            .unwrap_or(ts);

        let level = if urgency.eq_ignore_ascii_case("CRITICAL") {
            "err"
        } else {
            "info"
        }
        .to_string();

        let msg = if body.is_empty() {
            summary.to_string()
        } else {
            format!("{}: {}", summary, body)
        };
        if !msg.is_empty() {
            entries.push(LogEntry {
                timestamp_ms: item_ts_ms,
                source: "desktop-notify".to_string(),
                level,
                pid: None,
                unit_or_app: appname,
                message: truncate_str(&msg, 300),
            });
        }
    }
    Ok(entries)
}

pub fn parse_journal_json(val: &Value) -> Option<LogEntry> {
    let message = match val.get("MESSAGE")? {
        Value::String(s) => s.clone(),
        Value::Array(arr) => {
            let bytes: Vec<u8> = arr
                .iter()
                .filter_map(|v| v.as_u64().map(|n| n as u8))
                .collect();
            String::from_utf8_lossy(&bytes).to_string()
        }
        _ => return None,
    };

    let ts_usec = val
        .get("__REALTIME_TIMESTAMP")
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or_else(|| now_ms() * 1000);

    let priority = val
        .get("PRIORITY")
        .and_then(|v| v.as_str())
        .unwrap_or("6");
    let level = match priority {
        "0" => "emerg",
        "1" => "alert",
        "2" => "crit",
        "3" => "err",
        "4" => "warn",
        "5" => "notice",
        _ => "info",
    }
    .to_string();

    let pid = val
        .get("_PID")
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse::<i32>().ok());

    let unit_or_app = val
        .get("SYSLOG_IDENTIFIER")
        .or_else(|| val.get("_COMM"))
        .or_else(|| val.get("_SYSTEMD_USER_UNIT"))
        .or_else(|| val.get("_SYSTEMD_UNIT"))
        .and_then(|v| v.as_str())
        .unwrap_or("system")
        .to_string();

    Some(LogEntry {
        timestamp_ms: ts_usec / 1000,
        source: "journald".to_string(),
        level,
        pid,
        unit_or_app,
        message: truncate_str(&message, 300),
    })
}

fn truncate_str(s: &str, max_chars: usize) -> String {
    if s.chars().count() > max_chars {
        format!("{}...", s.chars().take(max_chars).collect::<String>())
    } else {
        s.to_string()
    }
}
