use crate::daemon::ring_buffer::now_ms;
use crate::protocol::types::LogEntry;
use anyhow::Result;
use serde_json::Value;
use std::process::Command;

/// `journalctl -o json` üzerinden sistem/kullanıcı loglarını (isteğe bağlı olarak PID kümesine göre filtrelenmiş) okur.
pub fn query_recent_journal_logs(
    filter_pids: Option<&[i32]>,
    limit: usize,
) -> Result<Vec<LogEntry>> {
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
        // Çok fazla PID varsa ilk 24 tanesini journalctl filtresine ekle
        for &pid in pids.iter().take(24) {
            if pid > 0 {
                cmd.arg(format!("_PID={}", pid));
            }
        }
    } else {
        // Genel sorguda sadece warning (4) ve daha kritik (0..=4) kayıtları getir
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

    let truncated_msg = if message.chars().count() > 300 {
        format!("{}...", message.chars().take(300).collect::<String>())
    } else {
        message
    };

    Some(LogEntry {
        timestamp_ms: ts_usec / 1000,
        source: "journald".to_string(),
        level,
        pid,
        unit_or_app,
        message: truncated_msg,
    })
}
