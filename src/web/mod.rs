use crate::daemon::server::{
    build_context_report, build_xray_for_target, spawn_background_collectors,
};
use crate::eyes::ui_atspi::inspect_window_ui;
use crate::eyes::vision::{capture_vision, runtime_dir};
use anyhow::Result;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::thread;
use std::time::Duration;

/// İstek başlıklarını okurken aşılmayacak üst sınır (istek satırı dâhil).
const MAX_REQUEST_BYTES: usize = 16 * 1024;

/// `http://127.0.0.1:<port>` üzerinde çalışan canlı PC-Eye Gözlem Sunucusu ve Web Dashboard.
pub fn run_web_dashboard(port: u16) -> Result<()> {
    let _bg_state = spawn_background_collectors();
    let bind_addr = format!("127.0.0.1:{}", port);
    let listener = TcpListener::bind(&bind_addr)?;
    eprintln!("PC-Eye Canlı Gözlem Paneli aktif: http://{}", bind_addr);

    for stream in listener.incoming().flatten() {
        thread::spawn(move || {
            let _ = handle_http_client(stream, port);
        });
    }
    Ok(())
}

fn handle_http_client(mut stream: TcpStream, port: u16) -> Result<()> {
    // Okuma zaman aşımı ve sınırlı boyut: veri göndermeyen ya da parçalı yazan bağlantı
    // dinleyiciyi kilitlemesin; başlıklar \r\n\r\n görülene dek okunur.
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 4096];
    while buf.len() < MAX_REQUEST_BYTES && !buf.windows(4).any(|w| w == b"\r\n\r\n") {
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    if buf.is_empty() {
        return Ok(());
    }
    let req_str = String::from_utf8_lossy(&buf);
    let first_line = req_str.lines().next().unwrap_or("");
    let path = first_line.split_whitespace().nth(1).unwrap_or("/");

    // Tarayıcı içindeki uzak sayfanın DNS rebinding ile panele erişmesini engellemek için
    // yalnız beklenen yerel Host başlığı kabul edilir.
    let host = req_str.lines().skip(1).find_map(|l| {
        let (k, v) = l.split_once(':')?;
        k.trim().eq_ignore_ascii_case("host").then(|| v.trim())
    });
    if !host_allowed(host, port) {
        write_http_response(&mut stream, "403 Forbidden", "text/plain", b"Forbidden")?;
        return Ok(());
    }

    if path == "/" || path.starts_with("/index.html") {
        let html = DASHBOARD_HTML.as_bytes();
        write_http_response(&mut stream, "200 OK", "text/html; charset=utf-8", html)?;
    } else if path.starts_with("/api/v1/context") {
        let rep = build_context_report(None)?;
        let body = serde_json::to_vec(&rep)?;
        write_http_response(&mut stream, "200 OK", "application/json", &body)?;
    } else if path.starts_with("/api/v1/xray") {
        let rep = build_xray_for_target(None, None)?;
        let body = serde_json::to_vec(&rep)?;
        write_http_response(&mut stream, "200 OK", "application/json", &body)?;
    } else if path.starts_with("/api/v1/ui") {
        let rep = inspect_window_ui(None, None, 60)?;
        let body = serde_json::to_vec(&rep)?;
        write_http_response(&mut stream, "200 OK", "application/json", &body)?;
    } else if path.starts_with("/api/v1/vision") {
        let rep = capture_vision(None, None, None, true)?;
        let body = serde_json::to_vec(&rep)?;
        write_http_response(&mut stream, "200 OK", "application/json", &body)?;
    } else if path.starts_with("/api/v1/latest.png") {
        let latest = runtime_dir().join("captures").join("latest.png");
        if !latest.exists() {
            let _ = capture_vision(None, None, None, true);
        }
        if let Ok(bytes) = std::fs::read(&latest) {
            write_http_response(&mut stream, "200 OK", "image/png", &bytes)?;
        } else {
            write_http_response(&mut stream, "404 Not Found", "text/plain", b"No capture")?;
        }
    } else {
        write_http_response(&mut stream, "404 Not Found", "text/plain", b"Not Found")?;
    }

    Ok(())
}

/// Yalnız yerel panelin beklediği Host başlığı geçerlidir (`127.0.0.1:<port>` / `localhost:<port>`).
fn host_allowed(host: Option<&str>, port: u16) -> bool {
    match host {
        Some(h) => {
            h.eq_ignore_ascii_case(&format!("127.0.0.1:{}", port))
                || h.eq_ignore_ascii_case(&format!("localhost:{}", port))
        }
        None => false,
    }
}

fn write_http_response(
    stream: &mut TcpStream,
    status: &str,
    content_type: &str,
    body: &[u8],
) -> Result<()> {
    let header = format!(
        "HTTP/1.1 {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        status,
        content_type,
        body.len()
    );
    stream.write_all(header.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()?;
    Ok(())
}

const DASHBOARD_HTML: &str = r#"<!DOCTYPE html>
<html lang="tr">
<head>
<meta charset="utf-8"/>
<title>pc-eye · Tam Beyaz Kutu Gözlem Paneli</title>
<style>
  :root {
    --bg: #0d0f14;
    --panel: #151922;
    --border: #252b3b;
    --text: #e4e8f0;
    --muted: #8b95a8;
    --accent: #00e676;
    --warn: #ffb400;
    --danger: #ff5252;
  }
  * { box-sizing: border-box; margin: 0; padding: 0; }
  body { background: var(--bg); color: var(--text); font-family: 'JetBrains Mono', monospace, sans-serif; padding: 16px; font-size: 13px; }
  header { display: flex; justify-content: space-between; align-items: center; padding: 12px 16px; background: var(--panel); border: 1px solid var(--border); border-radius: 8px; margin-bottom: 16px; }
  .badge { padding: 4px 10px; border-radius: 99px; background: rgba(0,230,118,0.14); color: var(--accent); font-weight: 700; }
  .grid { display: grid; grid-template-columns: 1fr 1fr; gap: 16px; }
  .card { background: var(--panel); border: 1px solid var(--border); border-radius: 8px; padding: 14px; overflow: hidden; }
  .card h2 { font-size: 13px; text-transform: uppercase; letter-spacing: 0.06em; color: var(--accent); margin-bottom: 10px; display: flex; justify-content: space-between; }
  pre { background: #090b0f; padding: 10px; border-radius: 6px; overflow: auto; max-height: 290px; color: #c9d4e8; font-size: 12px; line-height: 1.45; }
  img { width: 100%; border-radius: 6px; border: 1px solid var(--border); }
  button { background: var(--accent); color: #05150c; border: none; padding: 5px 12px; border-radius: 5px; font-weight: 700; cursor: pointer; }
</style>
</head>
<body>
<header>
  <div><strong>PC-EYE V2</strong> · Tam Beyaz Kutu İşletim Sistemi Telemetri & Kontrol Çekirdeği</div>
  <div><span id="seq" class="badge">SEQ #0</span></div>
</header>
<div class="grid">
  <div class="card">
    <h2><span>Göz 1 & 5 · Masaüstü, Sistem PSI & Ses</span></h2>
    <pre id="context-box">Yükleniyor...</pre>
  </div>
  <div class="card">
    <h2><span>Göz 3 · Aktif Pencere Süreç Ağacı & Ağ Röntgeni (X-Ray)</span></h2>
    <pre id="xray-box">Yükleniyor...</pre>
  </div>
  <div class="card">
    <h2><span>Göz 2b · Görsel Göz & Set-of-Marks</span><button onclick="refreshVision()">Set-of-Marks Yakala</button></h2>
    <img id="vision-img" src="/api/v1/latest.png" alt="PC-Eye Capture"/>
  </div>
  <div class="card">
    <h2><span>Göz 4 · Canlı Olaylar & Hata Akışı</span></h2>
    <pre id="events-box">Yükleniyor...</pre>
  </div>
</div>
<script>
async function poll() {
  try {
    const [ctxRes, xrayRes] = await Promise.all([fetch('/api/v1/context'), fetch('/api/v1/xray')]);
    const ctx = await ctxRes.json();
    const xray = await xrayRes.json();
    document.getElementById('seq').textContent = 'SEQ #' + ctx.seq + ' | Cursor: ' + ctx.desktop.cursor_x + ',' + ctx.desktop.cursor_y;
    document.getElementById('context-box').textContent = JSON.stringify({
      active_window: ctx.desktop.active_window_title,
      active_pid: ctx.desktop.active_window_pid,
      windows_count: ctx.desktop.windows.length,
      system_health: ctx.system_health,
      audio: ctx.audio_summary,
      windows: ctx.desktop.windows.map(w => ({ addr: w.address, class: w.class, title: w.title, pid: w.pid, geom: [w.x,w.y,w.width,w.height], protected: w.protected_window }))
    }, null, 2);
    document.getElementById('xray-box').textContent = JSON.stringify({
      root_pid: xray.root_pid,
      window: xray.window_title,
      cgroup: xray.cgroup_path,
      cluster_processes: xray.total_processes_in_cluster,
      total_threads: xray.total_threads,
      total_rss_mb: xray.total_rss_mb,
      rss_stats: xray.cluster_rss_mb_stats,
      network_connections: xray.network_connections,
      open_files_sample: (xray.open_files || []).slice(0, 15)
    }, null, 2);
    document.getElementById('events-box').textContent = JSON.stringify({
      recent_events: ctx.recent_events.slice(-10),
      recent_logs: ctx.recent_logs.slice(-10)
    }, null, 2);
  } catch (e) {}
}
async function refreshVision() {
  await fetch('/api/v1/vision');
  document.getElementById('vision-img').src = '/api/v1/latest.png?t=' + Date.now();
}
setInterval(poll, 1500);
poll();
</script>
</body>
</html>
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_header_is_restricted_to_loopback() {
        assert!(host_allowed(Some("127.0.0.1:7777"), 7777));
        assert!(host_allowed(Some("localhost:7777"), 7777));
        assert!(!host_allowed(Some("evil.example"), 7777));
        assert!(!host_allowed(Some("127.0.0.1:80"), 7777));
        assert!(!host_allowed(None, 7777));
    }
}
