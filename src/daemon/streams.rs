//! Arka plan olay akışları (F8): journald, inotify dosya, systemd birimleri ve NetworkManager.
//!
//! Dört kaynaktan gelen olaylar tek satırlık token-dostu biçime indirilip `LiveDaemonState`
//! içindeki ortak `event_ring`'e akar; `Coalescer` kaynak başına hız sınırı ve yineleme birleştirmesi uygular.

use crate::daemon::ring_buffer::now_ms;
use crate::daemon::server::LiveDaemonState;
use crate::protocol::types::CompositorEvent;
use serde_json::Value;
use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::CString;
use std::fs;
use std::io::{BufRead, BufReader};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

const MAX_PAYLOAD_CHARS: usize = 300;
const RATE_LIMIT_PER_SEC: usize = 50;
const COALESCE_WINDOW_MS: u64 = 1000;

/// Kaynak başına hız sınırı (saniyede en çok 50 olay) ve ardışık özdeş olay birleştirici.
/// İlk olay gecikmesiz yazılır; 1 sn içinde ardışık gelen özdeş `kind+payload` yinelemeleri
/// sayılarak tek `"<payload> (xN)"` özetine indirgenir.
#[derive(Debug, Clone)]
pub struct Coalescer {
    max_per_sec: usize,
    coalesce_window_ms: u64,
    recent_emit_ms: VecDeque<u64>,
    last_kind: String,
    last_payload: String,
    has_last: bool,
    repeat_start_ms: u64,
    repeat_count: u64,
}

impl Coalescer {
    pub fn new(max_per_sec: usize, coalesce_window_ms: u64) -> Self {
        Self {
            max_per_sec,
            coalesce_window_ms,
            recent_emit_ms: VecDeque::with_capacity(max_per_sec.min(128)),
            last_kind: String::new(),
            last_payload: String::new(),
            has_last: false,
            repeat_start_ms: 0,
            repeat_count: 0,
        }
    }

    fn allow_emit(&mut self, ts_ms: u64) -> bool {
        while let Some(&front) = self.recent_emit_ms.front() {
            if ts_ms.saturating_sub(front) >= 1000 {
                self.recent_emit_ms.pop_front();
            } else {
                break;
            }
        }
        if self.recent_emit_ms.len() < self.max_per_sec {
            self.recent_emit_ms.push_back(ts_ms);
            true
        } else {
            false
        }
    }

    pub fn push(&mut self, kind: &str, payload: &str, ts_ms: u64) -> Vec<(String, String)> {
        if self.has_last && self.last_kind == kind && self.last_payload == payload {
            if ts_ms.saturating_sub(self.repeat_start_ms) < self.coalesce_window_ms {
                self.repeat_count += 1;
                return Vec::new();
            }
        }

        let mut out = Vec::new();
        if self.repeat_count > 0 {
            let summary = format_coalesced(&self.last_payload, self.repeat_count);
            if self.allow_emit(ts_ms) {
                out.push((self.last_kind.clone(), summary));
            }
            self.repeat_count = 0;
        }

        if self.allow_emit(ts_ms) {
            self.last_kind.clear();
            self.last_kind.push_str(kind);
            self.last_payload.clear();
            self.last_payload.push_str(payload);
            self.has_last = true;
            self.repeat_start_ms = ts_ms;
            self.repeat_count = 0;
            out.push((kind.to_string(), payload.to_string()));
        } else {
            self.has_last = false;
            self.repeat_count = 0;
        }
        out
    }

    pub fn flush_expired(&mut self, ts_ms: u64) -> Option<(String, String)> {
        if !self.has_last || ts_ms.saturating_sub(self.repeat_start_ms) < self.coalesce_window_ms {
            return None;
        }
        self.has_last = false;
        if self.repeat_count == 0 {
            return None;
        }
        let count = self.repeat_count;
        self.repeat_count = 0;
        let summary = format_coalesced(&self.last_payload, count);
        if self.allow_emit(ts_ms) {
            Some((self.last_kind.clone(), summary))
        } else {
            None
        }
    }
}

fn truncate_chars(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    if max_chars == 0 {
        return String::new();
    }
    let mut out: String = s.chars().take(max_chars - 1).collect();
    out.push('…');
    out
}

fn format_coalesced(payload: &str, count: u64) -> String {
    let suffix = format!(" (x{})", count);
    let max_base = MAX_PAYLOAD_CHARS.saturating_sub(suffix.chars().count());
    let base = truncate_chars(payload, max_base);
    format!("{}{}", base, suffix)
}

fn sanitize_single_line(s: &str) -> String {
    let cleaned = s.replace(['\r', '\n', '\0'], " ");
    truncate_chars(cleaned.trim(), MAX_PAYLOAD_CHARS)
}

fn emit_with_coalescer(
    coalescer: &Mutex<Coalescer>,
    state: &Mutex<LiveDaemonState>,
    kind: &str,
    payload: &str,
) {
    let events = {
        let Ok(mut c) = coalescer.lock() else {
            return;
        };
        c.push(kind, payload, now_ms())
    };
    if events.is_empty() {
        return;
    }
    if let Ok(mut guard) = state.lock() {
        for (k, p) in events {
            guard.push_event(CompositorEvent { kind: k, payload: p });
        }
    }
}

/// Dört arka plan olay akışı iş parçacığını (journal, file, unit, network) başlatır.
pub fn start(state: Arc<Mutex<LiveDaemonState>>) {
    let make_coalescer = || Arc::new(Mutex::new(Coalescer::new(RATE_LIMIT_PER_SEC, COALESCE_WINDOW_MS)));
    let c_journal = make_coalescer();
    let c_file = make_coalescer();
    let c_unit = make_coalescer();
    let c_net = make_coalescer();

    let coalescers = vec![
        Arc::clone(&c_journal),
        Arc::clone(&c_file),
        Arc::clone(&c_unit),
        Arc::clone(&c_net),
    ];

    let s_journal = Arc::clone(&state);
    thread::spawn(move || run_journal_stream(c_journal, s_journal));

    let s_file = Arc::clone(&state);
    thread::spawn(move || run_file_stream(c_file, s_file));

    let s_unit = Arc::clone(&state);
    thread::spawn(move || run_unit_stream(c_unit, s_unit));

    let s_net = Arc::clone(&state);
    thread::spawn(move || run_network_stream(c_net, s_net));

    // Kaynak sessizleştiğinde bekleyen 1 sn'lik yineleme özetlerini düşürmemek için süpürür.
    thread::spawn(move || loop {
        thread::sleep(Duration::from_millis(500));
        let ts = now_ms();
        let flushed: Vec<(String, String)> = coalescers
            .iter()
            .filter_map(|c| c.lock().ok()?.flush_expired(ts))
            .collect();
        if !flushed.is_empty() {
            if let Ok(mut guard) = state.lock() {
                for (k, p) in flushed {
                    guard.push_event(CompositorEvent { kind: k, payload: p });
                }
            }
        }
    });
}

/// `journalctl --output=json` satırını `(kind, payload)` çiftine ayrıştırır.
pub fn parse_journal_json_line(line: &str) -> Option<(String, String)> {
    let v: Value = serde_json::from_str(line).ok()?;
    let prio = v.get("PRIORITY").and_then(|p| {
        p.as_u64()
            .or_else(|| p.as_str().and_then(|s| s.trim().parse::<u64>().ok()))
    });
    let level = match prio {
        Some(0..=2) => "crit",
        Some(3) => "err",
        Some(4) => "warn",
        _ => "info",
    };

    let raw_msg = match v.get("MESSAGE") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(arr)) => {
            let bytes: Vec<u8> = arr
                .iter()
                .filter_map(|x| x.as_u64().and_then(|n| u8::try_from(n).ok()))
                .collect();
            String::from_utf8_lossy(&bytes).into_owned()
        }
        _ => return None,
    };
    let msg = raw_msg.split_whitespace().collect::<Vec<_>>().join(" ");
    if msg.is_empty() {
        return None;
    }

    let str_field = |k: &str| {
        v.get(k)
            .and_then(|x| x.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    let unit = str_field("_SYSTEMD_UNIT").or_else(|| str_field("_SYSTEMD_USER_UNIT"));
    let source = str_field("SYSLOG_IDENTIFIER")
        .or_else(|| str_field("_COMM"))
        .or_else(|| unit.clone())
        .unwrap_or_else(|| "unknown".to_string());

    let pid = v.get("_PID").and_then(|p| {
        p.as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .or_else(|| p.as_u64().map(|n| n.to_string()))
    });

    let is_coredump = source == "systemd-coredump"
        || unit.as_deref().map_or(false, |u| u.starts_with("systemd-coredump"))
        || (msg.contains("Process ") && msg.contains("dumped core"));
    let kind = if is_coredump { "coredump" } else { "journal" };

    let raw_payload = match pid {
        Some(p) => format!("{} {}[{}]: {}", level, source, p, msg),
        None => format!("{} {}: {}", level, source, msg),
    };
    Some((kind.to_string(), sanitize_single_line(&raw_payload)))
}

fn run_journal_stream(coalescer: Arc<Mutex<Coalescer>>, state: Arc<Mutex<LiveDaemonState>>) {
    loop {
        if let Ok(mut child) = Command::new("journalctl")
            .args([
                "--follow",
                "--output=json",
                "--lines=0",
                "--priority=0..4",
                "--no-pager",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
        {
            if let Some(stdout) = child.stdout.take() {
                let reader = BufReader::new(stdout);
                for line in reader.lines().map_while(Result::ok) {
                    if let Some((kind, payload)) = parse_journal_json_line(&line) {
                        emit_with_coalescer(&coalescer, &state, &kind, &payload);
                    }
                }
            }
            let _ = child.wait();
        }
        thread::sleep(Duration::from_secs(2));
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawInotifyEvent {
    pub wd: i32,
    pub mask: u32,
    #[allow(dead_code)]
    pub cookie: u32,
    pub name: String,
}

/// `inotify` okuma tamponunu sınır denetimli olarak ayrıştırır.
pub fn parse_inotify_buffer(buf: &[u8]) -> Vec<RawInotifyEvent> {
    let mut events = Vec::new();
    let mut offset = 0usize;
    while offset + 16 <= buf.len() {
        let wd = i32::from_ne_bytes(buf[offset..offset + 4].try_into().unwrap());
        let mask = u32::from_ne_bytes(buf[offset + 4..offset + 8].try_into().unwrap());
        let cookie = u32::from_ne_bytes(buf[offset + 8..offset + 12].try_into().unwrap());
        let len = u32::from_ne_bytes(buf[offset + 12..offset + 16].try_into().unwrap()) as usize;
        let name_start = offset + 16;
        let Some(next_offset) = name_start.checked_add(len) else {
            break;
        };
        if next_offset > buf.len() {
            break;
        }
        let name_bytes = &buf[name_start..next_offset];
        let nul_pos = name_bytes
            .iter()
            .position(|&b| b == 0)
            .unwrap_or(name_bytes.len());
        let name = String::from_utf8_lossy(&name_bytes[..nul_pos]).into_owned();
        events.push(RawInotifyEvent {
            wd,
            mask,
            cookie,
            name,
        });
        offset = next_offset;
    }
    events
}

/// `inotify` maskesini olay eylemine çevirir; dosya `IN_CREATE` göz ardı edilir.
pub fn inotify_action(mask: u32) -> Option<&'static str> {
    if mask & libc::IN_CLOSE_WRITE != 0 {
        Some("written")
    } else if mask & libc::IN_DELETE != 0 {
        Some("deleted")
    } else if mask & libc::IN_MOVED_TO != 0 {
        Some("moved_in")
    } else if mask & libc::IN_MOVED_FROM != 0 {
        Some("moved_out")
    } else if mask & libc::IN_CREATE != 0 && mask & libc::IN_ISDIR != 0 {
        Some("dir_created")
    } else {
        None
    }
}

/// İndirme/düzenleyici geçici dosyalarını eler.
pub fn is_temp_filename(name: &str) -> bool {
    name.is_empty()
        || name.ends_with(".part")
        || name.ends_with(".crdownload")
        || name.ends_with(".tmp")
        || name.ends_with('~')
        || name.starts_with(".goutputstream-")
        || name.starts_with(".#")
}

fn expand_tilde(raw: &str, home: Option<&str>) -> PathBuf {
    if let Some(h) = home {
        if raw == "~" {
            return PathBuf::from(h);
        }
        if let Some(rest) = raw.strip_prefix("~/") {
            return PathBuf::from(h).join(rest);
        }
    }
    PathBuf::from(raw)
}

fn xdg_user_dir(kind: &str) -> Option<PathBuf> {
    let out = Command::new("xdg-user-dir")
        .arg(kind)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(PathBuf::from(s))
    }
}

fn default_watch_dirs(home: Option<&str>) -> Vec<PathBuf> {
    let download =
        xdg_user_dir("DOWNLOAD").or_else(|| home.map(|h| PathBuf::from(h).join("Downloads")));
    let desktop =
        xdg_user_dir("DESKTOP").or_else(|| home.map(|h| PathBuf::from(h).join("Desktop")));
    [download, desktop].into_iter().flatten().collect()
}

/// Yapılandırma JSON'u, varsayılan dizinler ve `PCEYE_WATCH` birleşiminden izlenecek yolları çözer.
pub fn resolve_watch_dirs_from(
    config_json: Option<&str>,
    env_watch: Option<&str>,
    home: Option<&str>,
    fallback_dirs: &[PathBuf],
) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    let configured = config_json
        .and_then(|s| serde_json::from_str::<Value>(s).ok())
        .and_then(|v| {
            v.get("watch").and_then(|w| w.as_array()).map(|arr| {
                arr.iter()
                    .filter_map(|item| item.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(|s| expand_tilde(s, home))
                    .collect::<Vec<_>>()
            })
        });

    if let Some(list) = configured {
        dirs.extend(list);
    } else {
        dirs.extend(fallback_dirs.iter().cloned());
    }

    if let Some(extra) = env_watch {
        for part in extra.split(':').map(str::trim).filter(|s| !s.is_empty()) {
            dirs.push(expand_tilde(part, home));
        }
    }

    let mut seen = HashSet::new();
    dirs.retain(|p| seen.insert(p.clone()));
    dirs
}

fn resolve_watch_dirs() -> Vec<PathBuf> {
    let home = std::env::var("HOME").ok();
    let config_json = home
        .as_deref()
        .and_then(|h| fs::read_to_string(PathBuf::from(h).join(".config/pc-eye/streams.json")).ok());
    let has_watch_key = config_json
        .as_deref()
        .and_then(|s| serde_json::from_str::<Value>(s).ok())
        .and_then(|v| v.get("watch").and_then(|w| w.as_array()).map(|_| ()))
        .is_some();
    let fallback = if has_watch_key {
        Vec::new()
    } else {
        default_watch_dirs(home.as_deref())
    };
    let env_watch = std::env::var("PCEYE_WATCH").ok();
    resolve_watch_dirs_from(
        config_json.as_deref(),
        env_watch.as_deref(),
        home.as_deref(),
        &fallback,
    )
    .into_iter()
    .filter(|p| p.is_dir())
    .collect()
}

fn run_file_stream(coalescer: Arc<Mutex<Coalescer>>, state: Arc<Mutex<LiveDaemonState>>) {
    let dirs = resolve_watch_dirs();
    if dirs.is_empty() {
        return;
    }

    let raw_fd = unsafe { libc::inotify_init1(libc::IN_CLOEXEC) };
    if raw_fd < 0 {
        return;
    }
    let fd = unsafe { OwnedFd::from_raw_fd(raw_fd) };
    let mask = libc::IN_CLOSE_WRITE
        | libc::IN_MOVED_TO
        | libc::IN_MOVED_FROM
        | libc::IN_DELETE
        | libc::IN_CREATE;

    let mut wd_map = HashMap::new();
    for dir in dirs {
        let Some(s) = dir.to_str() else { continue };
        let Ok(c_path) = CString::new(s) else { continue };
        let wd = unsafe { libc::inotify_add_watch(fd.as_raw_fd(), c_path.as_ptr(), mask) };
        if wd >= 0 {
            wd_map.insert(wd, dir);
        }
    }
    if wd_map.is_empty() {
        return;
    }

    let mut buf = [0u8; 8192];
    loop {
        let n = unsafe {
            libc::read(
                fd.as_raw_fd(),
                buf.as_mut_ptr().cast::<libc::c_void>(),
                buf.len(),
            )
        };
        if n < 0 {
            if std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            break;
        }
        if n == 0 {
            break;
        }
        for ev in parse_inotify_buffer(&buf[..n as usize]) {
            if is_temp_filename(&ev.name) {
                continue;
            }
            let Some(action) = inotify_action(ev.mask) else {
                continue;
            };
            let Some(dir) = wd_map.get(&ev.wd) else {
                continue;
            };
            let full = dir.join(&ev.name);
            let payload = sanitize_single_line(&format!("{} {}", action, full.to_string_lossy()));
            emit_with_coalescer(&coalescer, &state, "file", &payload);
        }
    }
}

/// systemd `JobRemoved` olayını biçimlendirir; yalnız `.service` birimleri ve sistemde `done` dışı sonuçlar döner.
pub fn format_unit_job_removed(is_system: bool, unit: &str, result: &str) -> Option<String> {
    if !unit.ends_with(".service") {
        return None;
    }
    if is_system && result == "done" {
        return None;
    }
    let scope = if is_system { "system" } else { "user" };
    Some(sanitize_single_line(&format!("{} {} {}", scope, unit, result)))
}

fn run_unit_stream(coalescer: Arc<Mutex<Coalescer>>, state: Arc<Mutex<LiveDaemonState>>) {
    let c_sys = Arc::clone(&coalescer);
    let s_sys = Arc::clone(&state);
    thread::spawn(move || loop {
        let _ = listen_unit_bus(true, &c_sys, &s_sys);
        thread::sleep(Duration::from_secs(5));
    });

    for is_system in [true, false] {
        let (c, s) = (Arc::clone(&coalescer), Arc::clone(&state));
        thread::spawn(move || loop {
            let _ = listen_unit_failures(is_system, &c, &s);
            thread::sleep(Duration::from_secs(5));
        });
    }

    loop {
        let _ = listen_unit_bus(false, &coalescer, &state);
        thread::sleep(Duration::from_secs(5));
    }
}

/// systemd birim nesne yolundaki kaçışları çözer: `pceye_2df8b_2eservice` → `pceye-f8b.service`.
pub fn unescape_unit_path(path: &str) -> String {
    let last = path.rsplit('/').next().unwrap_or(path);
    let b = last.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'_' && i + 2 < b.len() {
            if let Ok(v) = u8::from_str_radix(&last[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Çalışırken çöken servisler: başlatma işi "done" ile biter, çöküş ancak birimin ActiveState
/// özelliği "failed" olunca görülür (JobRemoved bunu yakalamıyordu).
fn listen_unit_failures(
    is_system: bool,
    coalescer: &Mutex<Coalescer>,
    state: &Mutex<LiveDaemonState>,
) -> zbus::Result<()> {
    let conn = if is_system {
        zbus::blocking::Connection::system()?
    } else {
        zbus::blocking::Connection::session()?
    };
    conn.call_method(
        Some("org.freedesktop.systemd1"),
        "/org/freedesktop/systemd1",
        Some("org.freedesktop.systemd1.Manager"),
        "Subscribe",
        &(),
    )?;
    let rule = zbus::MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .interface("org.freedesktop.DBus.Properties")?
        .member("PropertiesChanged")?
        .path_namespace("/org/freedesktop/systemd1/unit")?
        .build();
    let iter = zbus::blocking::MessageIterator::for_match_rule(rule, &conn, None)?;
    for msg_res in iter {
        let msg = msg_res?;
        let header = msg.header();
        let Some(path) = header.path() else { continue };
        let unit = unescape_unit_path(path.as_str());
        if !unit.ends_with(".service") {
            continue;
        }
        type Changed = std::collections::HashMap<String, zbus::zvariant::OwnedValue>;
        if let Ok((iface, changed, _inv)) = msg.body().deserialize::<(String, Changed, Vec<String>)>() {
            if iface != "org.freedesktop.systemd1.Unit" {
                continue;
            }
            let failed = changed
                .get("ActiveState")
                .and_then(|v| <&str>::try_from(v).ok())
                .map_or(false, |s| s == "failed");
            if failed {
                let bus = if is_system { "system" } else { "user" };
                emit_with_coalescer(coalescer, state, "unit", &format!("{} {} failed", bus, unit));
            }
        }
    }
    Ok(())
}

fn listen_unit_bus(
    is_system: bool,
    coalescer: &Mutex<Coalescer>,
    state: &Mutex<LiveDaemonState>,
) -> zbus::Result<()> {
    let conn = if is_system {
        zbus::blocking::Connection::system()?
    } else {
        zbus::blocking::Connection::session()?
    };
    conn.call_method(
        Some("org.freedesktop.systemd1"),
        "/org/freedesktop/systemd1",
        Some("org.freedesktop.systemd1.Manager"),
        "Subscribe",
        &(),
    )?;
    let rule = zbus::MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .interface("org.freedesktop.systemd1.Manager")?
        .member("JobRemoved")?
        .build();
    let iter = zbus::blocking::MessageIterator::for_match_rule(rule, &conn, None)?;
    for msg_res in iter {
        let msg = msg_res?;
        if let Ok((_id, _job, unit, result)) =
            msg.body()
                .deserialize::<(u32, zbus::zvariant::OwnedObjectPath, String, String)>()
        {
            if let Some(payload) = format_unit_job_removed(is_system, &unit, &result) {
                emit_with_coalescer(coalescer, state, "unit", &payload);
            }
        }
    }
    Ok(())
}

/// NetworkManager `StateChanged` durum kodunu okunur ada çevirir.
pub fn format_nm_state(code: u32) -> String {
    let name = match code {
        10 => "asleep".to_string(),
        20 => "disconnected".to_string(),
        30 => "disconnecting".to_string(),
        40 => "connecting".to_string(),
        50 => "connected_local".to_string(),
        60 => "connected_site".to_string(),
        70 => "connected_global".to_string(),
        other => other.to_string(),
    };
    format!("network {}", name)
}

fn run_network_stream(coalescer: Arc<Mutex<Coalescer>>, state: Arc<Mutex<LiveDaemonState>>) {
    let Ok(conn) = zbus::blocking::Connection::system() else {
        return;
    };
    let has_nm = conn
        .call_method(
            Some("org.freedesktop.DBus"),
            "/org/freedesktop/DBus",
            Some("org.freedesktop.DBus"),
            "NameHasOwner",
            &("org.freedesktop.NetworkManager",),
        )
        .ok()
        .and_then(|m| m.body().deserialize::<bool>().ok())
        .unwrap_or(false);
    if !has_nm {
        return;
    }
    let Ok(builder) = zbus::MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .interface("org.freedesktop.NetworkManager")
    else {
        return;
    };
    let Ok(builder) = builder.member("StateChanged") else {
        return;
    };
    let rule = builder.build();
    let Ok(iter) = zbus::blocking::MessageIterator::for_match_rule(rule, &conn, None) else {
        return;
    };
    for msg_res in iter {
        let Ok(msg) = msg_res else {
            break;
        };
        if let Ok(code) = msg.body().deserialize::<u32>() {
            let payload = format_nm_state(code);
            emit_with_coalescer(&coalescer, &state, "network", &payload);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_coalescer_merges_repeats_and_rate_limits() {
        let mut c = Coalescer::new(50, 1000);
        assert_eq!(
            c.push("journal", "err foo: boom", 100),
            vec![("journal".to_string(), "err foo: boom".to_string())]
        );
        assert!(c.push("journal", "err foo: boom", 200).is_empty());
        assert!(c.push("journal", "err foo: boom", 300).is_empty());

        // Farklı olay gelince bekleyen tekrar tek özet olarak dökülür.
        let out = c.push("journal", "warn bar: ok", 400);
        assert_eq!(
            out,
            vec![
                ("journal".to_string(), "err foo: boom (x2)".to_string()),
                ("journal".to_string(), "warn bar: ok".to_string()),
            ]
        );

        // Süre aşımı ile dökme:
        assert!(c.push("journal", "warn bar: ok", 600).is_empty());
        assert_eq!(c.flush_expired(1399), None);
        assert_eq!(
            c.flush_expired(1400),
            Some(("journal".to_string(), "warn bar: ok (x1)".to_string()))
        );

        // Hız sınırı: saniyede en çok 50 olay
        let mut rl = Coalescer::new(50, 1000);
        let mut emitted = 0usize;
        for i in 0..70 {
            emitted += rl.push("file", &format!("written /tmp/{}", i), 2000).len();
        }
        assert_eq!(emitted, 50);
        // 1 sn geçince kota yenilenir
        assert_eq!(rl.push("file", "written /tmp/next", 3000).len(), 1);
    }

    #[test]
    fn test_parse_journal_json_line() {
        let err_line = r#"{"PRIORITY":"3","MESSAGE":"disk\nfull","SYSLOG_IDENTIFIER":"kernel","_PID":"12"}"#;
        assert_eq!(
            parse_journal_json_line(err_line),
            Some(("journal".to_string(), "err kernel[12]: disk full".to_string()))
        );

        // Bayt dizisi MESSAGE + _COMM yedeği + sayı PRIORITY
        let byte_msg = r#"{"PRIORITY":4,"MESSAGE":[72,101,108,108,111,255],"_COMM":"app"}"#;
        let parsed = parse_journal_json_line(byte_msg).unwrap();
        assert_eq!(parsed.0, "journal");
        assert!(parsed.1.starts_with("warn app: Hello"));

        // systemd-coredump -> kind = "coredump"
        let core_line = r#"{"PRIORITY":2,"MESSAGE":"Process 99 (boom) of user 1000 dumped core.","SYSLOG_IDENTIFIER":"systemd-coredump","_PID":99}"#;
        let core = parse_journal_json_line(core_line).unwrap();
        assert_eq!(core.0, "coredump");
        assert!(core.1.starts_with("crit systemd-coredump[99]: Process 99"));

        // En çok 300 karakter sınırı
        let long_msg = "a".repeat(400);
        let long_line = format!(r#"{{"PRIORITY":3,"MESSAGE":"{}","_SYSTEMD_UNIT":"foo.service"}}"#, long_msg);
        let long_parsed = parse_journal_json_line(&long_line).unwrap();
        assert!(long_parsed.1.chars().count() <= 300);
    }

    #[test]
    fn test_inotify_buffer_action_and_temp_filter() {
        assert_eq!(inotify_action(libc::IN_CLOSE_WRITE), Some("written"));
        assert_eq!(inotify_action(libc::IN_DELETE), Some("deleted"));
        assert_eq!(inotify_action(libc::IN_MOVED_TO), Some("moved_in"));
        assert_eq!(inotify_action(libc::IN_MOVED_FROM), Some("moved_out"));
        assert_eq!(
            inotify_action(libc::IN_CREATE | libc::IN_ISDIR),
            Some("dir_created")
        );
        assert_eq!(inotify_action(libc::IN_CREATE), None);

        assert!(is_temp_filename("foo.part"));
        assert!(is_temp_filename("bar.crdownload"));
        assert!(is_temp_filename("baz.tmp"));
        assert!(is_temp_filename("notes~"));
        assert!(is_temp_filename(".goutputstream-XYZ"));
        assert!(is_temp_filename(".#lock"));
        assert!(!is_temp_filename("report.pdf"));

        // Sentetik 2 olay + sonda eksik/kırpılmış başlık baytları
        let mut buf = Vec::new();
        buf.extend_from_slice(&1i32.to_ne_bytes());
        buf.extend_from_slice(&libc::IN_CLOSE_WRITE.to_ne_bytes());
        buf.extend_from_slice(&0u32.to_ne_bytes());
        buf.extend_from_slice(&8u32.to_ne_bytes());
        buf.extend_from_slice(b"a.txt\0\0\0");

        buf.extend_from_slice(&2i32.to_ne_bytes());
        buf.extend_from_slice(&(libc::IN_CREATE | libc::IN_ISDIR).to_ne_bytes());
        buf.extend_from_slice(&7u32.to_ne_bytes());
        buf.extend_from_slice(&4u32.to_ne_bytes());
        buf.extend_from_slice(b"dir\0");

        // Yarım kalmış 3. olay güvenle yoksayılmalı
        buf.extend_from_slice(&3i32.to_ne_bytes());
        buf.extend_from_slice(&libc::IN_DELETE.to_ne_bytes());

        let events = parse_inotify_buffer(&buf);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].wd, 1);
        assert_eq!(events[0].name, "a.txt");
        assert_eq!(events[1].wd, 2);
        assert_eq!(events[1].cookie, 7);
        assert_eq!(events[1].name, "dir");
    }

    #[test]
    fn test_unescape_unit_path() {
        assert_eq!(
            unescape_unit_path("/org/freedesktop/systemd1/unit/pceye_2df8b_2d42_2eservice"),
            "pceye-f8b-42.service"
        );
        assert_eq!(unescape_unit_path("/org/freedesktop/systemd1/unit/dbus_2eservice"), "dbus.service");
    }

    #[test]
    fn test_unit_and_network_formatters_and_watch_dirs() {
        assert_eq!(format_unit_job_removed(true, "foo.service", "done"), None);
        assert_eq!(
            format_unit_job_removed(true, "foo.service", "failed"),
            Some("system foo.service failed".to_string())
        );
        assert_eq!(
            format_unit_job_removed(false, "bar.service", "done"),
            Some("user bar.service done".to_string())
        );
        assert_eq!(format_unit_job_removed(false, "foo.scope", "done"), None);

        assert_eq!(format_nm_state(10), "network asleep");
        assert_eq!(format_nm_state(20), "network disconnected");
        assert_eq!(format_nm_state(70), "network connected_global");
        assert_eq!(format_nm_state(99), "network 99");

        let defaults = vec![PathBuf::from("/home/u/Downloads"), PathBuf::from("/home/u/Desktop")];
        let dirs = resolve_watch_dirs_from(
            Some(r#"{"watch":["~/Work"]}"#),
            Some("/tmp/a:/tmp/b:~/Work"),
            Some("/home/u"),
            &defaults,
        );
        assert_eq!(
            dirs,
            vec![
                PathBuf::from("/home/u/Work"),
                PathBuf::from("/tmp/a"),
                PathBuf::from("/tmp/b"),
            ]
        );
    }
}
