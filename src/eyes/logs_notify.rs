use crate::daemon::ring_buffer::now_ms;
use crate::eyes::vision::runtime_dir;
use crate::protocol::types::LogEntry;
use anyhow::Result;
use serde_json::Value;
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

/// Harici komutu çalıştırır; `output()` gibi süresiz beklemez, süre dolunca süreci `kill` eder ve
/// boru dolup kilitlenmesin diye stdout'u ayrı iş parçacığında okur.
fn run_command_with_timeout(cmd: &mut Command, timeout: Duration) -> Result<Output> {
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let mut pipe = child.stdout.take();
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(p) = pipe.as_mut() {
            let _ = p.read_to_end(&mut buf);
        }
        buf
    });

    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(st) = child.try_wait()? {
            break st;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            break child.wait()?;
        }
        std::thread::sleep(Duration::from_millis(4));
    };

    Ok(Output {
        status,
        stdout: reader.join().unwrap_or_default(),
        stderr: Vec::new(),
    })
}

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
        let notify_quota = (limit / 3).clamp(1, 4);
        if let Ok(mut notify_logs) = read_dunst_notifications(notify_quota) {
            entries.append(&mut notify_logs);
        }
    }

    // 4. Çöken Süreçler (`coredumpctl list --json=short`)
    let crash_quota = (limit / 4).clamp(1, 3);
    if let Ok(mut crash_logs) = read_coredump_entries(filter_pids, crash_quota) {
        entries.append(&mut crash_logs);
    }

    entries.sort_by_key(|e| e.timestamp_ms);
    if entries.len() > limit {
        let skip = entries.len() - limit;
        entries = entries.into_iter().skip(skip).collect();
    }

    Ok(entries)
}

/// `coredumpctl list --json=short` üzerinden sistemde çöken (SIGSEGV, SIGABRT vb.) süreçleri okur.
pub fn read_coredump_entries(filter_pids: Option<&[i32]>, limit: usize) -> Result<Vec<LogEntry>> {
    // `-n` tek başına journal'ın EN ESKİ kayıtlarını verir (systemd 262); `-r` ile en yeniler önce gelir.
    // Aksi halde yeni bir çökme hiç görünmez, eski çökmeler "son çökme" gibi raporlanır.
    let out = run_command_with_timeout(
        Command::new("coredumpctl").args(["list", "--json=short", "-r", "-n", "15", "--no-pager"]),
        Duration::from_secs(5),
    )?;
    if !out.status.success() || out.stdout.is_empty() {
        return Ok(Vec::new());
    }
    let items: Vec<Value> = serde_json::from_slice(&out.stdout).unwrap_or_default();
    let mut entries = Vec::new();

    for item in items {
        let pid = item.get("pid").and_then(|v| v.as_i64()).map(|n| n as i32);
        if let Some(pids) = filter_pids {
            if let Some(p) = pid {
                if !pids.contains(&p) {
                    continue;
                }
            } else {
                continue;
            }
        }
        let ts_usec = item
            .get("time")
            .and_then(|v| v.as_u64())
            .unwrap_or_else(|| now_ms() * 1000);
        let sig = item.get("sig").and_then(|v| v.as_i64()).unwrap_or(0);
        let sig_name = match sig {
            4 => "SIGILL",
            5 => "SIGTRAP",
            6 => "SIGABRT",
            7 => "SIGBUS",
            8 => "SIGFPE",
            11 => "SIGSEGV",
            _ => "SIGNAL",
        };
        let exe = item
            .get("exe")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        let app_name = std::path::Path::new(exe)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or(exe)
            .to_string();
        let corefile = item
            .get("corefile")
            .and_then(|v| v.as_str())
            .unwrap_or("none");

        entries.push(LogEntry {
            timestamp_ms: ts_usec / 1000,
            source: "coredump".to_string(),
            level: "crit".to_string(),
            pid,
            unit_or_app: app_name,
            message: format!(
                "PROCESS CRASHED ({} / sig={}): pid={} exe={} [core={}]",
                sig_name,
                sig,
                pid.map_or("?".to_string(), |p| p.to_string()),
                exe,
                corefile
            ),
        });
        if entries.len() >= limit {
            break;
        }
    }
    Ok(entries)
}

fn read_journald_entries(filter_pids: Option<&[i32]>, limit: usize) -> Result<Vec<LogEntry>> {
    // Ajan `-n 1000000` gibi değer verebilir; journalctl tüm journal'ı üretip belleği şişirmesin.
    let limit = limit.min(500);
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

    let output = run_command_with_timeout(&mut cmd, Duration::from_secs(5))?;
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

/// Dosyanın son `max_bytes` baytını kayıplı UTF-8 ile çözer; büyük `app_logs` dosyalarının tamamını
/// belleğe almaz ve tek geçersiz bayt yüzünden dosyayı atlamaz.
fn read_file_tail_lossy(path: &std::path::Path, max_bytes: u64) -> Option<String> {
    let mut file = fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let mut buf = Vec::new();
    if len > max_bytes {
        file.seek(SeekFrom::Start(len - max_bytes)).ok()?;
    }
    file.read_to_end(&mut buf).ok()?;
    if len > max_bytes {
        // Pencere bir satırın ortasından başlayabilir; ilk yarım satırı at.
        if let Some(pos) = buf.iter().position(|&b| b == b'\n') {
            buf.drain(..=pos);
        }
    }
    Some(String::from_utf8_lossy(&buf).into_owned())
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

        // Tek geçersiz bayt dosyanın tamamını atlatıyordu; kayıplı çöz, son 256 KiB'ı oku.
        let Some(content) = read_file_tail_lossy(&path, 256 * 1024) else {
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
    let out =
        run_command_with_timeout(Command::new("dunstctl").arg("history"), Duration::from_secs(5))?;
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
