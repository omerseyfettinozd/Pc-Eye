use crate::daemon::ring_buffer::{PercentileStats, SeqItem};
use serde::{Deserialize, Serialize};

/// Monitör ve ölçek bilgisi (Hyprland IPC).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MonitorInfo {
    pub id: i64,
    pub name: String,
    pub width: i64,
    pub height: i64,
    pub x: i64,
    pub y: i64,
    pub scale: f64,
    pub active_workspace_id: i64,
    pub active_workspace_name: String,
    pub focused: bool,
}

/// Çalışma alanı (Workspace) özeti.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceInfo {
    pub id: i64,
    pub name: String,
    pub monitor: String,
    pub windows: i64,
    pub has_fullscreen: bool,
    pub last_window: String,
    pub last_window_title: String,
}

/// Pencere (Client) bilgisi ve güvenlik koruma durumu.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WindowInfo {
    pub address: String,
    pub class: String,
    pub title: String,
    pub pid: i32,
    pub workspace_id: i64,
    pub workspace_name: String,
    pub monitor: i64,
    pub x: i64,
    pub y: i64,
    pub width: i64,
    pub height: i64,
    pub floating: bool,
    pub fullscreen: i64,
    pub xwayland: bool,
    pub mapped: bool,
    pub hidden: bool,
    pub focused: bool,
    pub protected_window: bool,
}

/// Katman yüzeyi (Waybar, bildirimler, polkit, rofi/wofi, kilit ekranı vb.).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LayerSurfaceInfo {
    pub monitor: String,
    pub level: String,
    pub address: String,
    pub namespace: String,
    pub x: i64,
    pub y: i64,
    pub width: i64,
    pub height: i64,
    pub pid: i32,
    pub is_auth_or_lock: bool,
}

/// Hyprland kullanıcı kısayolu (`j/binds`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeybindInfo {
    pub combo: String,
    pub modmask: u32,
    pub key: String,
    pub dispatcher: String,
    pub arg: String,
    pub description: String,
    pub submap: String,
}

/// Kompozitör canlı olayı (.socket2.sock).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompositorEvent {
    pub kind: String,
    pub payload: String,
}

/// Göz 1: Kompozitör ve Masaüstü Topolojisi Durumu.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DesktopState {
    pub timestamp_ms: u64,
    pub cursor_x: i64,
    pub cursor_y: i64,
    pub active_window_address: Option<String>,
    pub active_window_class: Option<String>,
    pub active_window_title: Option<String>,
    pub active_window_pid: Option<i32>,
    pub auth_guard_tripped: bool,
    pub auth_guard_reason: Option<String>,
    pub monitors: Vec<MonitorInfo>,
    pub workspaces: Vec<WorkspaceInfo>,
    pub windows: Vec<WindowInfo>,
    pub layers: Vec<LayerSurfaceInfo>,
}

/// Göz 2a: Semantik UI Elemanı (AT-SPI2 + Hyprland Ofset Çözümlü + Durum & Değer).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UiElement {
    pub mark: usize,
    pub role: String,
    pub name: String,
    pub value: Option<String>,
    pub description: Option<String>,
    pub states: Vec<String>,
    pub actions: Vec<String>,
    /// Pencere içi göreli koordinatlar (AT-SPI ham çıktı).
    pub rel_bounds: [i64; 4],
    /// Hyprland pencere ofsetiyle düzeltilmiş gerçek ekran koordinatları `[x, y, w, h]`.
    pub screen_bounds: [i64; 4],
    /// Tıklama için merkez ekran koordinatı `[cx, cy]`.
    pub click_point: [i64; 2],
    pub dbus_service: String,
    pub dbus_path: String,
}

/// Göz 2a: Pencere Semantik UI ve Terminal Raporu.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UiTreeReport {
    pub timestamp_ms: u64,
    pub window_address: String,
    pub window_class: String,
    pub window_title: String,
    pub window_pid: i32,
    pub window_geometry: [i64; 4],
    pub a11y_bus_enabled: bool,
    pub perception_mode: String, // "at-spi2" | "terminal-pty" | "vision-fallback"
    pub element_count: usize,
    pub elements: Vec<UiElement>,
    pub terminal_context: Option<TerminalContext>,
    pub hint: Option<String>,
}

/// Terminal pencereleri için doğrudan PTY / Kabuk ve görünür ekran metni durumu.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TerminalContext {
    pub terminal_pid: i32,
    pub foreground_pid: i32,
    pub foreground_cmd: String,
    pub cwd: String,
    pub pty_path: Option<String>,
    pub recent_child_processes: Vec<String>,
    #[serde(default)]
    pub visible_lines: Vec<String>,
}

/// Göz 2b: Görsel Göz (Vision + Set-of-Marks + Pixel-Diff) Raporu.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VisionReport {
    pub timestamp_ms: u64,
    pub capture_mode: String, // "monitor" | "window" | "region" | "zoom"
    pub target_window: Option<String>,
    pub file_path: String,
    pub format: String,
    pub image_width: u32,
    pub image_height: u32,
    /// Görüntü üzerindeki `(px, py)` pikselini gerçek masaüstü `(gx, gy)` koordinatına çevirmek için `[origin_x, origin_y, width, height]`.
    pub desktop_geometry: [i64; 4],
    pub scale: f64,
    pub dhash_hex: String,
    pub pixel_diff_pct: Option<f64>,
    pub visual_stable: bool,
    pub marks_drawn: usize,
    pub marks_legend: Vec<UiElement>,
}

/// Göz 3: Alt süreç öğesi.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessMember {
    pub pid: i32,
    pub ppid: i32,
    pub name: String,
    pub state: String,
    pub cmdline: String,
    pub threads: usize,
    pub rss_mb: f64,
    pub cpu_time_ticks: u64,
    pub io_read_bytes: u64,
    pub io_write_bytes: u64,
}

/// Göz 3: Ağ bağlantısı (TCP/UDP soket).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetConnection {
    pub proto: String,
    pub local_addr: String,
    pub remote_addr: String,
    pub state: String,
    pub inode: u64,
    pub owner_pid: Option<i32>,
}

/// Göz 3: GPU Telemetrisi (NVIDIA NVML/SMI + AMD/Intel DRM sysfs).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GpuTelemetry {
    pub name: String,
    pub utilization_pct: f64,
    pub vram_used_mb: Option<u64>,
    pub vram_total_mb: Option<u64>,
    pub temperature_c: Option<f64>,
}

/// Göz 3: Sistem Basınç, GPU ve Yük Özeti (PSI + Mem + Load + GPU).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemHealthSummary {
    pub load_avg_1m: f64,
    pub load_avg_5m: f64,
    pub mem_total_mb: u64,
    pub mem_used_mb: u64,
    pub mem_available_mb: u64,
    pub swap_used_mb: u64,
    pub psi_cpu_some_avg10: f64,
    pub psi_mem_some_avg10: f64,
    pub psi_io_some_avg10: f64,
    pub gpus: Vec<GpuTelemetry>,
    pub cpu_percent_stats: PercentileStats,
    pub mem_used_mb_stats: PercentileStats,
}

/// Göz 3: Uygulama Kümesi Derin Röntgen Raporu (`pc_eye_xray`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessXrayReport {
    pub timestamp_ms: u64,
    pub root_pid: i32,
    pub window_address: Option<String>,
    pub window_title: Option<String>,
    pub cgroup_path: Option<String>,
    pub total_processes_in_cluster: usize,
    pub total_threads: usize,
    pub total_rss_mb: f64,
    pub has_zombie_or_uninterruptible: bool,
    pub cluster_cpu_stats: PercentileStats,
    pub cluster_rss_mb_stats: PercentileStats,
    pub members: Vec<ProcessMember>,
    pub open_files: Vec<String>,
    pub network_connections: Vec<NetConnection>,
    pub recent_logs: Vec<LogEntry>,
}

/// Göz 4: Log, Hata, Uygulama Çıktısı (`app_logs`), Çökme (`coredump`) ve Masaüstü Bildirim (`dunst`) Kaydı.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogEntry {
    pub timestamp_ms: u64,
    pub source: String, // "journald" | "app-stderr" | "desktop-notify" | "coredump"
    pub level: String,  // "emerg" | "alert" | "crit" | "err" | "warn" | "info"
    pub pid: Option<i32>,
    pub unit_or_app: String,
    pub message: String,
}

/// Göz 5: PipeWire Ses Düğümü / Akışı.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioStreamNode {
    pub id: u64,
    pub name: String,
    pub app_name: Option<String>,
    pub pid: Option<i32>,
    pub media_class: String,
    pub state: String,
    pub volume: Option<f64>,
    pub muted: bool,
}

/// Göz 5: Gerçek Fiziksel PCM Ses Sinyali Ölçümü (RMS / Peak dBFS).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PcmSignalMetrics {
    pub sample_count: usize,
    pub rms_norm: f64,
    pub peak_norm: f64,
    pub rms_dbfs: f64,
    pub peak_dbfs: f64,
    pub audible_signal: bool,
}

/// Göz 5: D-Bus MPRIS Medya ve Tarayıcı Sekme/Video Telemetrisi.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MprisMediaInfo {
    pub service: String,
    pub player_name: String,
    pub playback_status: String, // "Playing" | "Paused" | "Stopped"
    pub title: Option<String>,
    pub artist: Option<String>,
    pub url: Option<String>,
    pub length_sec: Option<f64>,
}

/// Göz 5: Ses, Fiziksel PCM Sinyali ve Medya Gözlemlenebilirlik Raporu.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioReport {
    pub timestamp_ms: u64,
    pub default_sink_volume: Option<f64>,
    pub default_sink_muted: bool,
    pub active_playback_streams: usize,
    pub active_recording_streams: usize,
    pub pcm_signal: Option<PcmSignalMetrics>,
    /// `pcm_signal` boşsa nedeni (sink çözülemedi, sink boşta, süre doldu vb.).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pcm_signal_note: Option<String>,
    pub media_players: Vec<MprisMediaInfo>,
    pub nodes: Vec<AudioStreamNode>,
}

/// Zengin Pano Geçmişi Öğesi (`cliphist`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClipboardHistoryItem {
    pub id: String,
    pub preview: String,
    pub is_binary: bool,
}

/// Zengin Çok-Formatlı Pano Raporu (`pc_eye_director` -> `clipboard`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClipboardReport {
    pub timestamp_ms: u64,
    pub mime_types: Vec<String>,
    pub current_text: Option<String>,
    pub source_url: Option<String>,
    pub has_image: bool,
    pub history: Vec<ClipboardHistoryItem>,
}

/// Çok-Modlu Deterministik Bekleme (`pc_eye_wait`) Sonucu.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WaitReport {
    pub condition: String,
    pub matched: bool,
    pub elapsed_ms: u64,
    pub details: String,
}

/// Birleşik Beyaz-Kutu Sistem Bağlamı (`pc_eye_context`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemContextReport {
    pub seq: u64,
    pub timestamp_ms: u64,
    pub daemon_running: bool,
    pub desktop: DesktopState,
    pub system_health: SystemHealthSummary,
    pub audio_summary: AudioReport,
    pub recent_events: Vec<SeqItem<CompositorEvent>>,
    pub recent_logs: Vec<SeqItem<LogEntry>>,
    pub dropped_events: u64,
}
