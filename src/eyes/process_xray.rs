use crate::daemon::ring_buffer::{now_ms, PercentileStats};
use crate::eyes::logs_notify::query_recent_journal_logs;
use crate::protocol::types::{
    GpuTelemetry, NetConnection, ProcessMember, ProcessXrayReport, SystemHealthSummary,
};
use anyhow::{anyhow, Result};
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::net::{Ipv4Addr, Ipv6Addr};
use std::path::Path;
use std::process::Command;

/// Verilen `root_pid` sürecinden türeyen tüm alt süreçleri (children/descendants)
/// ve aynı cgroup içindeki kardeş süreçleri bulur (K3 Kuralı: AppCluster).
pub fn collect_app_cluster(root_pid: i32) -> Result<(Vec<ProcessMember>, Option<String>)> {
    if root_pid <= 0 || !Path::new(&format!("/proc/{}", root_pid)).exists() {
        return Err(anyhow!("Geçersiz veya bulunamayan PID: {}", root_pid));
    }

    let root_cgroup = read_cgroup_path(root_pid);
    let mut all_procs: HashMap<i32, ProcessMember> = HashMap::new();
    let mut children_map: HashMap<i32, Vec<i32>> = HashMap::new();

    if let Ok(entries) = fs::read_dir("/proc") {
        for entry in entries.flatten() {
            let fname = entry.file_name();
            let Some(pid_str) = fname.to_str() else {
                continue;
            };
            let Ok(pid) = pid_str.parse::<i32>() else {
                continue;
            };
            if let Some(member) = read_process_member(pid) {
                children_map.entry(member.ppid).or_default().push(pid);
                all_procs.insert(pid, member);
            }
        }
    }

    let mut visited = HashSet::new();
    let mut queue = VecDeque::new();
    queue.push_back(root_pid);
    visited.insert(root_pid);

    while let Some(curr) = queue.pop_front() {
        if let Some(kids) = children_map.get(&curr) {
            for &kid in kids {
                if visited.insert(kid) {
                    queue.push_back(kid);
                }
            }
        }
    }

    let mut members: Vec<ProcessMember> = visited
        .into_iter()
        .filter_map(|pid| all_procs.remove(&pid))
        .collect();
    members.sort_by(|a, b| {
        b.rss_mb
            .partial_cmp(&a.rss_mb)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    Ok((members, root_cgroup))
}

fn read_cgroup_path(pid: i32) -> Option<String> {
    let content = fs::read_to_string(format!("/proc/{}/cgroup", pid)).ok()?;
    for line in content.lines() {
        if let Some(rest) = line.strip_prefix("0::") {
            return Some(rest.trim().to_string());
        }
    }
    content.lines().next().map(|s| s.to_string())
}

fn read_process_member(pid: i32) -> Option<ProcessMember> {
    let stat_str = fs::read_to_string(format!("/proc/{}/stat", pid)).ok()?;
    let open_paren = stat_str.find('(')?;
    let close_paren = stat_str.rfind(')')?;
    let name = stat_str[open_paren + 1..close_paren].to_string();
    let after = stat_str.get(close_paren + 2..)?;
    let fields: Vec<&str> = after.split_whitespace().collect();
    if fields.len() < 22 {
        return None;
    }

    let state = fields[0].to_string();
    let ppid = fields[1].parse::<i32>().unwrap_or(0);
    let utime = fields[11].parse::<u64>().unwrap_or(0);
    let stime = fields[12].parse::<u64>().unwrap_or(0);
    let num_threads = fields[17].parse::<usize>().unwrap_or(1);
    let rss_pages = fields[21].parse::<i64>().unwrap_or(0).max(0) as u64;
    let page_size_kb = 4.0;
    let rss_mb = ((rss_pages as f64) * page_size_kb / 1024.0 * 100.0).round() / 100.0;

    let cmdline = fs::read(format!("/proc/{}/cmdline", pid))
        .ok()
        .map(|bytes| {
            let parts: Vec<String> = bytes
                .split(|&b| b == 0)
                .filter(|s| !s.is_empty())
                .map(|s| String::from_utf8_lossy(s).to_string())
                .collect();
            if parts.is_empty() {
                format!("[{}]", name)
            } else {
                let joined = parts.join(" ");
                if joined.chars().count() > 160 {
                    format!("{}...", joined.chars().take(160).collect::<String>())
                } else {
                    joined
                }
            }
        })
        .unwrap_or_else(|| name.clone());

    let (io_read_bytes, io_write_bytes) = read_proc_io(pid);

    Some(ProcessMember {
        pid,
        ppid,
        name,
        state,
        cmdline,
        threads: num_threads,
        rss_mb,
        cpu_time_ticks: utime + stime,
        io_read_bytes,
        io_write_bytes,
    })
}

fn read_proc_io(pid: i32) -> (u64, u64) {
    let Ok(content) = fs::read_to_string(format!("/proc/{}/io", pid)) else {
        return (0, 0);
    };
    let mut rbytes = 0;
    let mut wbytes = 0;
    for line in content.lines() {
        if let Some(v) = line.strip_prefix("read_bytes:") {
            rbytes = v.trim().parse().unwrap_or(0);
        } else if let Some(v) = line.strip_prefix("write_bytes:") {
            wbytes = v.trim().parse().unwrap_or(0);
        }
    }
    (rbytes, wbytes)
}

/// Kümedeki tüm süreçlerin açık dosya tanıtıcılarını (`fd`) ve soket inode'larını toplar.
pub fn collect_open_files_and_socket_inodes(
    pids: &[i32],
    max_files: usize,
) -> (Vec<String>, HashMap<u64, i32>) {
    let mut files_set = HashSet::new();
    let mut files = Vec::new();
    let mut socket_inodes: HashMap<u64, i32> = HashMap::new();

    for &pid in pids {
        let fd_dir = format!("/proc/{}/fd", pid);
        let Ok(entries) = fs::read_dir(&fd_dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(target) = fs::read_link(entry.path()) else {
                continue;
            };
            let target_str = target.to_string_lossy().to_string();
            if let Some(inode_str) = target_str
                .strip_prefix("socket:[")
                .and_then(|s| s.strip_suffix(']'))
            {
                if let Ok(inode) = inode_str.parse::<u64>() {
                    socket_inodes.insert(inode, pid);
                }
                continue;
            }
            if target_str.starts_with("pipe:[")
                || target_str.starts_with("anon_inode:")
                || target_str == "/dev/null"
            {
                continue;
            }
            if files.len() < max_files && files_set.insert(target_str.clone()) {
                files.push(target_str);
            }
        }
    }

    files.sort();
    (files, socket_inodes)
}

/// Verilen soket inode haritasına ait aktif TCP/UDP bağlantılarını çözümler.
pub fn resolve_network_connections(socket_inodes: &HashMap<u64, i32>) -> Vec<NetConnection> {
    if socket_inodes.is_empty() {
        return Vec::new();
    }
    let mut conns = Vec::new();
    parse_proc_net_table("/proc/net/tcp", "tcp", false, socket_inodes, &mut conns);
    parse_proc_net_table("/proc/net/tcp6", "tcp6", true, socket_inodes, &mut conns);
    parse_proc_net_table("/proc/net/udp", "udp", false, socket_inodes, &mut conns);
    parse_proc_net_table("/proc/net/udp6", "udp6", true, socket_inodes, &mut conns);
    conns
}

fn parse_proc_net_table(
    path: &str,
    proto: &str,
    is_ipv6: bool,
    socket_inodes: &HashMap<u64, i32>,
    out: &mut Vec<NetConnection>,
) {
    let Ok(content) = fs::read_to_string(path) else {
        return;
    };
    for line in content.lines().skip(1) {
        let cols: Vec<&str> = line.split_whitespace().collect();
        if cols.len() < 10 {
            continue;
        }
        let Ok(inode) = cols[9].parse::<u64>() else {
            continue;
        };
        let Some(&owner_pid) = socket_inodes.get(&inode) else {
            continue;
        };
        let local_addr = decode_hex_endpoint(cols[1], is_ipv6);
        let remote_addr = decode_hex_endpoint(cols[2], is_ipv6);
        let state = decode_tcp_state(cols[3]);
        out.push(NetConnection {
            proto: proto.to_string(),
            local_addr,
            remote_addr,
            state,
            inode,
            owner_pid: Some(owner_pid),
        });
    }
}

fn decode_hex_endpoint(raw: &str, is_ipv6: bool) -> String {
    let Some((ip_hex, port_hex)) = raw.split_once(':') else {
        return raw.to_string();
    };
    let port = u16::from_str_radix(port_hex, 16).unwrap_or(0);
    if !is_ipv6 && ip_hex.len() == 8 {
        if let Ok(num) = u32::from_str_radix(ip_hex, 16) {
            let ip = Ipv4Addr::from(num.swap_bytes());
            return format!("{}:{}", ip, port);
        }
    } else if is_ipv6 && ip_hex.len() == 32 {
        let mut bytes = [0u8; 16];
        for chunk in 0..4 {
            if let Ok(w) = u32::from_str_radix(&ip_hex[chunk * 8..(chunk + 1) * 8], 16) {
                let b = w.swap_bytes().to_be_bytes();
                bytes[chunk * 4..chunk * 4 + 4].copy_from_slice(&b);
            }
        }
        let ip = Ipv6Addr::from(bytes);
        return format!("[{}]:{}", ip, port);
    }
    format!("{}:{}", ip_hex, port)
}

fn decode_tcp_state(hex_state: &str) -> String {
    match hex_state {
        "01" => "ESTABLISHED",
        "02" => "SYN_SENT",
        "03" => "SYN_RECV",
        "04" => "FIN_WAIT1",
        "05" => "FIN_WAIT2",
        "06" => "TIME_WAIT",
        "07" => "CLOSE",
        "08" => "CLOSE_WAIT",
        "09" => "LAST_ACK",
        "0A" => "LISTEN",
        "0B" => "CLOSING",
        _ => hex_state,
    }
    .to_string()
}

/// Sistemdeki NVIDIA (`nvidia-smi`) ve AMD/Intel (`/sys/class/drm`) GPU kullanımını okur.
pub fn read_gpu_telemetry() -> Vec<GpuTelemetry> {
    let mut gpus = Vec::new();

    // 1. NVIDIA GPU (`nvidia-smi`)
    if let Ok(out) = Command::new("nvidia-smi")
        .args([
            "--query-gpu=name,utilization.gpu,memory.used,memory.total,temperature.gpu",
            "--format=csv,noheader,nounits",
        ])
        .output()
    {
        if out.status.success() {
            let s = String::from_utf8_lossy(&out.stdout);
            for line in s.lines() {
                let parts: Vec<&str> = line.split(',').map(|p| p.trim()).collect();
                if parts.len() >= 5 {
                    gpus.push(GpuTelemetry {
                        name: parts[0].to_string(),
                        utilization_pct: parts[1].parse::<f64>().unwrap_or(0.0),
                        vram_used_mb: parts[2].parse::<u64>().ok(),
                        vram_total_mb: parts[3].parse::<u64>().ok(),
                        temperature_c: parts[4].parse::<f64>().ok(),
                    });
                }
            }
        }
    }

    // 2. AMD / Intel DRM sysfs (`gpu_busy_percent`)
    if let Ok(entries) = fs::read_dir("/sys/class/drm") {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if !name.starts_with("card") || name.contains('-') {
                continue;
            }
            let busy_path = entry.path().join("device").join("gpu_busy_percent");
            if let Ok(busy_str) = fs::read_to_string(&busy_path) {
                if let Ok(util) = busy_str.trim().parse::<f64>() {
                    let vram_used = fs::read_to_string(entry.path().join("device/mem_info_vram_used"))
                        .ok()
                        .and_then(|v| v.trim().parse::<u64>().ok())
                        .map(|b| b / (1024 * 1024));
                    let vram_total = fs::read_to_string(entry.path().join("device/mem_info_vram_total"))
                        .ok()
                        .and_then(|v| v.trim().parse::<u64>().ok())
                        .map(|b| b / (1024 * 1024));
                    gpus.push(GpuTelemetry {
                        name: format!("DRM {} (iGPU/AMD)", name),
                        utilization_pct: util,
                        vram_used_mb: vram_used,
                        vram_total_mb: vram_total,
                        temperature_c: None,
                    });
                }
            }
        }
    }

    gpus
}

/// Sistem geneli yük, bellek, GPU ve Linux PSI (Pressure Stall Information) özetini okur.
pub fn read_system_health_summary(
    cpu_history: &[f64],
    mem_history: &[f64],
) -> SystemHealthSummary {
    let (load_avg_1m, load_avg_5m) = fs::read_to_string("/proc/loadavg")
        .ok()
        .map(|s| {
            let parts: Vec<&str> = s.split_whitespace().collect();
            let l1 = parts
                .first()
                .and_then(|v| v.parse::<f64>().ok())
                .unwrap_or(0.0);
            let l5 = parts
                .get(1)
                .and_then(|v| v.parse::<f64>().ok())
                .unwrap_or(0.0);
            (l1, l5)
        })
        .unwrap_or((0.0, 0.0));

    let mut mem_total_kb = 0u64;
    let mut mem_avail_kb = 0u64;
    let mut swap_total_kb = 0u64;
    let mut swap_free_kb = 0u64;

    if let Ok(meminfo) = fs::read_to_string("/proc/meminfo") {
        for line in meminfo.lines() {
            if let Some(v) = line.strip_prefix("MemTotal:") {
                mem_total_kb = parse_kb(v);
            } else if let Some(v) = line.strip_prefix("MemAvailable:") {
                mem_avail_kb = parse_kb(v);
            } else if let Some(v) = line.strip_prefix("SwapTotal:") {
                swap_total_kb = parse_kb(v);
            } else if let Some(v) = line.strip_prefix("SwapFree:") {
                swap_free_kb = parse_kb(v);
            }
        }
    }

    let mem_total_mb = mem_total_kb / 1024;
    let mem_available_mb = mem_avail_kb / 1024;
    let mem_used_mb = mem_total_mb.saturating_sub(mem_available_mb);
    let swap_used_mb = swap_total_kb.saturating_sub(swap_free_kb) / 1024;

    let psi_cpu_some_avg10 = read_psi_avg10("/proc/pressure/cpu");
    let psi_mem_some_avg10 = read_psi_avg10("/proc/pressure/memory");
    let psi_io_some_avg10 = read_psi_avg10("/proc/pressure/io");

    let cpu_samples = if cpu_history.is_empty() {
        vec![load_avg_1m * 10.0]
    } else {
        cpu_history.to_vec()
    };
    let mem_samples = if mem_history.is_empty() {
        vec![mem_used_mb as f64]
    } else {
        mem_history.to_vec()
    };

    SystemHealthSummary {
        load_avg_1m,
        load_avg_5m,
        mem_total_mb,
        mem_used_mb,
        mem_available_mb,
        swap_used_mb,
        psi_cpu_some_avg10,
        psi_mem_some_avg10,
        psi_io_some_avg10,
        gpus: read_gpu_telemetry(),
        cpu_percent_stats: PercentileStats::from_slice(&cpu_samples, 2.5),
        mem_used_mb_stats: PercentileStats::from_slice(&mem_samples, 1.5),
    }
}

fn parse_kb(raw: &str) -> u64 {
    raw.split_whitespace()
        .next()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(0)
}

fn read_psi_avg10(path: &str) -> f64 {
    let Ok(content) = fs::read_to_string(path) else {
        return 0.0;
    };
    for line in content.lines() {
        if line.starts_with("some ") {
            for part in line.split_whitespace() {
                if let Some(val) = part.strip_prefix("avg10=") {
                    return val.parse::<f64>().unwrap_or(0.0);
                }
            }
        }
    }
    0.0
}

/// Sistemde belirli bir TCP portunun açık (LISTEN veya ESTABLISHED) olup olmadığını kontrol eder.
pub fn is_tcp_port_open(port: u16) -> Option<String> {
    let port_hex = format!(":{:04X}", port);
    for (path, proto, is_v6) in [
        ("/proc/net/tcp", "tcp", false),
        ("/proc/net/tcp6", "tcp6", true),
    ] {
        if let Ok(content) = fs::read_to_string(path) {
            for line in content.lines().skip(1) {
                let cols: Vec<&str> = line.split_whitespace().collect();
                if cols.len() >= 4
                    && (cols[1].ends_with(&port_hex) || cols[2].ends_with(&port_hex))
                {
                    let local = decode_hex_endpoint(cols[1], is_v6);
                    let remote = decode_hex_endpoint(cols[2], is_v6);
                    let state = decode_tcp_state(cols[3]);
                    return Some(format!("{} {} -> {} ({})", proto, local, remote, state));
                }
            }
        }
    }
    None
}

/// Göz 3: Bir pencere veya kök PID için tam kapsamlı AppCluster Röntgeni üretir.
pub fn inspect_process_xray(
    root_pid: i32,
    window_address: Option<String>,
    window_title: Option<String>,
    cpu_history: Option<&[f64]>,
    rss_history: Option<&[f64]>,
) -> Result<ProcessXrayReport> {
    let ts = now_ms();
    let (members, cgroup_path) = collect_app_cluster(root_pid)?;
    let pids: Vec<i32> = members.iter().map(|m| m.pid).collect();

    let total_processes_in_cluster = members.len();
    let total_threads: usize = members.iter().map(|m| m.threads).sum();
    let total_rss_mb: f64 =
        (members.iter().map(|m| m.rss_mb).sum::<f64>() * 100.0).round() / 100.0;
    let has_zombie_or_uninterruptible = members
        .iter()
        .any(|m| m.state == "Z" || m.state == "D" || m.state == "T");

    let (open_files, socket_inodes) = collect_open_files_and_socket_inodes(&pids, 80);
    let network_connections = resolve_network_connections(&socket_inodes);

    let rss_slice = rss_history
        .map(|s| s.to_vec())
        .unwrap_or_else(|| vec![total_rss_mb]);
    let cpu_slice = cpu_history.map(|s| s.to_vec()).unwrap_or_else(|| vec![0.0]);

    let recent_logs = query_recent_journal_logs(Some(&pids), 25).unwrap_or_default();

    Ok(ProcessXrayReport {
        timestamp_ms: ts,
        root_pid,
        window_address,
        window_title,
        cgroup_path,
        total_processes_in_cluster,
        total_threads,
        total_rss_mb,
        has_zombie_or_uninterruptible,
        cluster_cpu_stats: PercentileStats::from_slice(&cpu_slice, 2.5),
        cluster_rss_mb_stats: PercentileStats::from_slice(&rss_slice, 1.5),
        members,
        open_files,
        network_connections,
        recent_logs,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_self_process_cluster_xray() {
        let my_pid = std::process::id() as i32;
        let report = inspect_process_xray(my_pid, None, Some("self-test".to_string()), None, None)
            .expect("Kendi PID röntgenimiz başarılı olmalı");
        assert!(report.total_processes_in_cluster >= 1);
        assert!(report.total_rss_mb > 0.0);
        assert!(report.members.iter().any(|m| m.pid == my_pid));
    }

    #[test]
    fn test_hex_endpoint_decoding() {
        assert_eq!(decode_hex_endpoint("0100007F:1F90", false), "127.0.0.1:8080");
        assert_eq!(decode_tcp_state("0A"), "LISTEN");
        assert_eq!(decode_tcp_state("01"), "ESTABLISHED");
    }
}
