use crate::daemon::ring_buffer::{now_ms, SeqItem, SeqRingBuffer};
use crate::eyes::audio_pipewire::capture_audio_report;
use crate::eyes::compositor::{capture_desktop_state, resolve_hyprland_socket};
use crate::eyes::logs_notify::query_recent_journal_logs;
use crate::eyes::process_xray::{inspect_process_xray, read_system_health_summary};
use crate::eyes::vision::runtime_dir;
use crate::protocol::types::{
    CompositorEvent, LogEntry, ProcessXrayReport, SystemContextReport,
};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader};
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PersistedTelemetryState {
    pub next_seq: u64,
    pub cpu_load_history: Vec<f64>,
    pub mem_used_history: Vec<f64>,
    pub pid_rss_history: HashMap<i32, Vec<f64>>,
    pub recent_events: Vec<SeqItem<CompositorEvent>>,
}

impl PersistedTelemetryState {
    pub fn load() -> Self {
        let p = runtime_dir().join("telemetry_ring.json");
        fs::read_to_string(p)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_else(|| Self {
                next_seq: 1,
                cpu_load_history: Vec::new(),
                mem_used_history: Vec::new(),
                pid_rss_history: HashMap::new(),
                recent_events: Vec::new(),
            })
    }

    pub fn save(&self) {
        let p = runtime_dir().join("telemetry_ring.json");
        if let Ok(json) = serde_json::to_string(self) {
            crate::eyes::vision::atomic_write_str(&p, &json);
        }
    }

    pub fn record_sample(&mut self, load_1m: f64, mem_used_mb: u64, window_pids: &[(i32, f64)]) {
        if self.next_seq == 0 {
            self.next_seq = 1;
        } else {
            self.next_seq += 1;
        }
        push_bounded(&mut self.cpu_load_history, load_1m * 10.0, 120);
        push_bounded(&mut self.mem_used_history, mem_used_mb as f64, 120);
        for &(pid, rss) in window_pids {
            if pid > 0 {
                let entry = self.pid_rss_history.entry(pid).or_default();
                push_bounded(entry, rss, 120);
            }
        }
    }
}

fn push_bounded(vec: &mut Vec<f64>, val: f64, cap: usize) {
    if vec.len() >= cap {
        vec.remove(0);
    }
    vec.push(val);
}

/// Canlı arka plan durum motoru (MCP sunucusu ve Web Dashboard için paylaşımlı Halka Bellek).
pub struct LiveDaemonState {
    pub event_ring: SeqRingBuffer<CompositorEvent>,
    pub log_ring: SeqRingBuffer<LogEntry>,
    pub telemetry: PersistedTelemetryState,
}

impl LiveDaemonState {
    pub fn new() -> Self {
        Self {
            event_ring: SeqRingBuffer::new(512),
            log_ring: SeqRingBuffer::new(512),
            telemetry: PersistedTelemetryState::load(),
        }
    }
}

/// Arka planda `pc-eye daemon` sürecinin aktif olup olmadığını `/run/user/1000/pc-eye/daemon.pid` üzerinden kontrol eder.
pub fn is_daemon_running() -> bool {
    let pid_file = runtime_dir().join("daemon.pid");
    let Some(pid_str) = fs::read_to_string(pid_file).ok() else {
        return false;
    };
    let Some(pid) = pid_str.trim().parse::<i32>().ok() else {
        return false;
    };
    if pid <= 1 {
        return false;
    }
    let comm_path = format!("/proc/{}/comm", pid);
    fs::read_to_string(comm_path)
        .map(|c| c.contains("pc-eye"))
        .unwrap_or(false)
}

/// Arka planda `pc-eye daemon` çalışmıyorsa otomatik olarak sessizce başlatır.
pub fn ensure_background_daemon() -> bool {
    if is_daemon_running() {
        return true;
    }
    if let Ok(exe) = std::env::current_exe() {
        let exe_str = exe.to_string_lossy();
        // Debug/test binary'si içindeyken arka plan daemon başlatma
        if exe_str.contains("/deps/") || exe_str.contains("target/debug") {
            return false;
        }
        let _ = std::process::Command::new(exe)
            .arg("daemon")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
        thread::sleep(Duration::from_millis(30));
    }
    is_daemon_running()
}

/// `pc-eye daemon` komutu için kesintisiz arka plan Kara Kutu (Flight Recorder) döngüsü.
pub fn run_daemon_forever() -> Result<()> {
    let pid_file = runtime_dir().join("daemon.pid");
    let _ = fs::write(&pid_file, std::process::id().to_string());
    let _state = spawn_background_collectors();
    loop {
        thread::sleep(Duration::from_secs(3600));
    }
}

/// Arka plan toplayıcı iş parçacıklarını (Hyprland `.socket2.sock` olay akışı + 1000ms sistem telemetri halka belleği) başlatır.
pub fn spawn_background_collectors() -> Arc<Mutex<LiveDaemonState>> {
    let state = Arc::new(Mutex::new(LiveDaemonState::new()));

    // 1. Hyprland .socket2.sock gerçek zamanlı olay dinleyicisi (her olayı anında diske de yazar)
    let state_ev = Arc::clone(&state);
    thread::spawn(move || loop {
        if let Ok(sock_path) = resolve_hyprland_socket(".socket2.sock") {
            if let Ok(stream) = UnixStream::connect(&sock_path) {
                let reader = BufReader::new(stream);
                for line in reader.lines().map_while(Result::ok) {
                    let trimmed = line.trim();
                    if let Some((kind, payload)) = trimmed.split_once(">>") {
                        let ev = CompositorEvent {
                            kind: kind.to_string(),
                            payload: payload.to_string(),
                        };
                        if let Ok(mut guard) = state_ev.lock() {
                            let seq = guard.event_ring.push(ev.clone());
                            let ts = now_ms();
                            guard.telemetry.next_seq = guard.telemetry.next_seq.max(seq);
                            guard.telemetry.recent_events.push(SeqItem {
                                seq,
                                timestamp_ms: ts,
                                data: ev,
                            });
                            if guard.telemetry.recent_events.len() > 128 {
                                guard.telemetry.recent_events.remove(0);
                            }
                            guard.telemetry.save();
                        }
                    }
                }
            }
        }
        thread::sleep(Duration::from_secs(1));
    });

    // 2. 1000ms aralıklarla çalışan Sistem & Süreç Halka Bellek Toplayıcısı
    let state_tel = Arc::clone(&state);
    thread::spawn(move || loop {
        let health = read_system_health_summary(&[], &[]);
        if let Ok(mut guard) = state_tel.lock() {
            guard
                .telemetry
                .record_sample(health.load_avg_1m, health.mem_used_mb, &[]);
            guard.telemetry.save();
        }
        thread::sleep(Duration::from_millis(1000));
    });

    state
}

/// Tek çağrıda birleşik `SystemContextReport` (`pc_eye_context`) üretir.
pub fn build_context_report(seq_since: Option<u64>) -> Result<SystemContextReport> {
    let ts = now_ms();
    let daemon_running = ensure_background_daemon();
    let desktop = capture_desktop_state()?;
    let mut telemetry = PersistedTelemetryState::load();

    let health_now =
        read_system_health_summary(&telemetry.cpu_load_history, &telemetry.mem_used_history);
    telemetry.record_sample(health_now.load_avg_1m, health_now.mem_used_mb, &[]);
    telemetry.save();

    let system_health =
        read_system_health_summary(&telemetry.cpu_load_history, &telemetry.mem_used_history);
    let audio_summary = capture_audio_report().unwrap_or_else(|_| crate::protocol::types::AudioReport {
        timestamp_ms: ts,
        default_sink_volume: None,
        default_sink_muted: false,
        active_playback_streams: 0,
        active_recording_streams: 0,
        pcm_signal: None,
        media_players: Vec::new(),
        nodes: Vec::new(),
    });

    let since = seq_since.unwrap_or(0);
    let recent_events: Vec<SeqItem<CompositorEvent>> = telemetry
        .recent_events
        .iter()
        .filter(|e| e.seq > since)
        .cloned()
        .collect();

    let window_pids: Vec<i32> = desktop
        .windows
        .iter()
        .map(|w| w.pid)
        .filter(|&p| p > 0)
        .collect();
    let logs_raw = query_recent_journal_logs(
        if window_pids.is_empty() {
            None
        } else {
            Some(&window_pids)
        },
        15,
    )
    .unwrap_or_default();

    let mut log_ring = SeqRingBuffer::new(32);
    for l in logs_raw {
        log_ring.push_at(l.clone(), l.timestamp_ms);
    }
    let (recent_logs, dropped_events) = log_ring.since_seq(0, 15);

    Ok(SystemContextReport {
        seq: telemetry.next_seq,
        timestamp_ms: ts,
        daemon_running,
        desktop,
        system_health,
        audio_summary,
        recent_events,
        recent_logs,
        dropped_events,
    })
}

/// Pencere adresinden veya PID'den halka bellek geçmişiyle zenginleştirilmiş `ProcessXrayReport` üretir.
pub fn build_xray_for_target(
    window_addr: Option<&str>,
    pid: Option<i32>,
) -> Result<ProcessXrayReport> {
    let desktop = capture_desktop_state()?;
    let (target_pid, win_addr, win_title) = if let Some(p) = pid {
        let matched_win = desktop.windows.iter().find(|w| w.pid == p);
        (
            p,
            matched_win.map(|w| w.address.clone()),
            matched_win.map(|w| w.title.clone()),
        )
    } else if let Some(addr) = window_addr {
        let norm = crate::eyes::compositor::normalize_window_address(addr);
        let win = desktop
            .windows
            .iter()
            .find(|w| w.address == norm)
            .ok_or_else(|| anyhow::anyhow!("Pencere bulunamadı: {}", norm))?;
        (win.pid, Some(win.address.clone()), Some(win.title.clone()))
    } else if let Some(active_pid) = desktop.active_window_pid {
        (
            active_pid,
            desktop.active_window_address,
            desktop.active_window_title,
        )
    } else {
        return Err(anyhow::anyhow!(
            "X-Ray için `--pid` veya `--window` belirtilmeli ya da aktif bir pencere olmalı"
        ));
    };

    let mut telemetry = PersistedTelemetryState::load();
    let rss_hist = telemetry.pid_rss_history.get(&target_pid).cloned();
    let mut report = inspect_process_xray(
        target_pid,
        win_addr,
        win_title,
        Some(&telemetry.cpu_load_history),
        rss_hist.as_deref(),
    )?;

    // Yeni RSS örneğini halka belleğe kaydet
    telemetry.record_sample(
        0.0,
        0,
        &[(target_pid, report.total_rss_mb)],
    );
    telemetry.save();
    if let Some(updated_hist) = telemetry.pid_rss_history.get(&target_pid) {
        report.cluster_rss_mb_stats =
            crate::daemon::ring_buffer::PercentileStats::from_slice(updated_hist, 1.5);
    }

    Ok(report)
}
