use crate::daemon::ring_buffer::now_ms;
use crate::director::input::ActionReceipt;
use crate::protocol::types::{AudioReport, AudioStreamNode, MprisMediaInfo, PcmSignalMetrics};
use anyhow::{anyhow, Result};
use serde_json::Value;
use std::io::Read;
use std::process::{Child, Command, Output, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// Göz 5: PipeWire ses grafiğini (`pw-dump`), gerçek fiziksel PCM sinyal seviyesini (`RMS`/`Peak` dBFS)
/// ve D-Bus `MPRIS` medya/tarayıcı URL durumunu tek raporda toplar.
pub fn capture_audio_report() -> Result<AudioReport> {
    let ts = now_ms();
    let (default_sink_volume, default_sink_muted) = read_default_sink_volume();
    let media_players = collect_mpris_media_players();
    let mut default_sink = resolve_default_sink();

    let output = Command::new("pw-dump").output()?;
    if !output.status.success() {
        let pcm = measure_sink_pcm_signal(default_sink.as_ref());
        return Ok(AudioReport {
            timestamp_ms: ts,
            default_sink_volume,
            default_sink_muted,
            active_playback_streams: 0,
            active_recording_streams: 0,
            pcm_signal_note: pcm.as_ref().err().cloned(),
            pcm_signal: pcm.ok(),
            media_players,
            nodes: Vec::new(),
        });
    }

    let arr: Vec<Value> = serde_json::from_slice(&output.stdout).unwrap_or_default();
    let mut nodes = Vec::new();
    let mut active_playback_streams = 0usize;
    let mut active_recording_streams = 0usize;

    for item in arr {
        let Some(obj_type) = item.get("type").and_then(|v| v.as_str()) else {
            continue;
        };
        if obj_type != "PipeWire:Interface:Node" {
            continue;
        }
        let id = item.get("id").and_then(|v| v.as_u64()).unwrap_or(0);
        let Some(info) = item.get("info") else {
            continue;
        };
        let state = info
            .get("state")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string();
        let Some(props) = info.get("props") else {
            continue;
        };
        let Some(media_class) = props.get("media.class").and_then(|v| v.as_str()) else {
            continue;
        };

        if !media_class.contains("Audio") {
            continue;
        }

        let name = props
            .get("node.name")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string();
        let app_name = props
            .get("application.name")
            .or_else(|| props.get("node.description"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        let pid = props.get("application.process.id").and_then(|v| {
            v.as_i64()
                .map(|n| n as i32)
                .or_else(|| v.as_str().and_then(|s| s.parse::<i32>().ok()))
        });

        if let Some(sink) = default_sink.as_mut() {
            if media_class == "Audio/Sink" && sink.node_name == name {
                sink.state = Some(state.clone());
            }
        }

        if state == "running" {
            if media_class == "Stream/Output/Audio" {
                active_playback_streams += 1;
            } else if media_class == "Stream/Input/Audio" {
                active_recording_streams += 1;
            }
        }

        nodes.push(AudioStreamNode {
            id,
            name,
            app_name,
            pid,
            media_class: media_class.to_string(),
            state,
            volume: None,
            muted: false,
        });
    }

    let pcm = measure_sink_pcm_signal(default_sink.as_ref());

    Ok(AudioReport {
        timestamp_ms: ts,
        default_sink_volume,
        default_sink_muted,
        active_playback_streams,
        active_recording_streams,
        pcm_signal_note: pcm.as_ref().err().cloned(),
        pcm_signal: pcm.ok(),
        media_players,
        nodes,
    })
}

/// PCM örneklemesinin katı süre bütçesi: sink saati dursa ya da hiç veri gelmese bile çağrı bu sürede döner.
const PCM_SAMPLE_TIMEOUT: Duration = Duration::from_millis(700);
/// 800 örnek (50ms @ 16kHz, s16 mono, ham PCM).
const PCM_SAMPLE_BYTES: usize = 1600;
/// `wpctl inspect` gibi yardımcı sorgular için üst süre.
const PROBE_CMD_TIMEOUT: Duration = Duration::from_millis(1500);
/// pw-record akışının mikrofona değil, hedef sink'in monitör portlarına bağlanmasını sağlar;
/// hedef kaybolursa başka bir düğüme (ör. varsayılan kaynağa) yeniden bağlanmaz.
const SINK_MONITOR_PROPS: &str = "{ stream.capture.sink=true node.dont-reconnect=true }";

/// Varsayılan ses çıkışının monitöründen ~50ms'lik gerçek 16-bit PCM kesiti okuyup RMS ve Peak (dBFS) hesaplar.
/// Sinyal alınamazsa gerekçeyi `Err` olarak döner; asla mikrofona bağlanmaz ve `PCM_SAMPLE_TIMEOUT`'u aşmaz.
pub fn measure_sink_pcm_signal(
    sink: Option<&DefaultSink>,
) -> std::result::Result<PcmSignalMetrics, String> {
    let sink = sink.ok_or_else(|| {
        "varsayılan sink çözülemedi; mikrofona düşmemek için örnekleme atlandı".to_string()
    })?;
    if let Some(state) = sink.state.as_deref() {
        if state != "running" {
            return Err(format!(
                "sink `{}` durumu `{}`; çalan ses yok",
                sink.node_name, state
            ));
        }
    }

    let mut cmd = sink_monitor_record_command(&sink.node_name, "-", true);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let child = cmd
        .spawn()
        .map_err(|e| format!("pw-record başlatılamadı: {}", e))?;

    let (bytes, timed_out) = read_child_stdout_bounded(child, PCM_SAMPLE_BYTES, PCM_SAMPLE_TIMEOUT);
    if bytes.len() < 2 {
        return Err(if timed_out {
            format!(
                "sink `{}` monitöründen {} ms içinde PCM gelmedi (sink saati durmuş veya askıda olabilir)",
                sink.node_name,
                PCM_SAMPLE_TIMEOUT.as_millis()
            )
        } else {
            format!(
                "pw-record sink `{}` için veri vermeden kapandı",
                sink.node_name
            )
        });
    }
    Ok(compute_pcm_s16le_metrics(&bytes))
}

/// Varsayılan sink'in kimliği. `@DEFAULT_AUDIO_SINK@` yalnız wpctl'nin anladığı bir takma addır;
/// pw-record'a doğrudan verilirse varsayılan kaynağa, yani mikrofona bağlanır.
#[derive(Debug, Clone, PartialEq)]
pub struct DefaultSink {
    pub node_name: String,
    /// pw-dump'tan gelen düğüm durumu (running | idle | suspended); bilinmiyorsa `None`.
    pub state: Option<String>,
}

/// `wpctl inspect @DEFAULT_AUDIO_SINK@` çıktısından `node.name` değerini çözer.
pub fn resolve_default_sink() -> Option<DefaultSink> {
    let mut cmd = Command::new("wpctl");
    cmd.args(["inspect", "@DEFAULT_AUDIO_SINK@"]);
    let out = output_with_timeout(cmd, PROBE_CMD_TIMEOUT)?;
    if !out.status.success() {
        return None;
    }
    let node_name = parse_sink_node_name(&String::from_utf8_lossy(&out.stdout))?;
    Some(DefaultSink {
        node_name,
        state: None,
    })
}

/// `wpctl inspect` çıktısındaki `node.name` değerini döner; düğüm bir `Audio/Sink` değilse `None`.
pub fn parse_sink_node_name(inspect_output: &str) -> Option<String> {
    let prop = |key: &str| {
        inspect_output.lines().find_map(|line| {
            let l = line.trim_start().trim_start_matches('*').trim();
            let v = l.strip_prefix(key)?.trim_start().strip_prefix('=')?.trim();
            Some(v.trim_matches('"').to_string())
        })
    };
    if prop("media.class")? != "Audio/Sink" {
        return None;
    }
    prop("node.name").filter(|n| !n.is_empty())
}

/// Hedef sink'in monitörünü kaydeden pw-record komutu. `raw` ise başlıksız s16le PCM yazar.
fn sink_monitor_record_command(sink_node_name: &str, out: &str, raw: bool) -> Command {
    let mut cmd = Command::new("pw-record");
    if raw {
        cmd.arg("--raw");
    }
    cmd.args([
        "--target",
        sink_node_name,
        "-P",
        SINK_MONITOR_PROPS,
        "--latency",
        "20ms",
        "--rate",
        "16000",
        "--channels",
        "1",
        "--format",
        "s16",
        out,
    ]);
    cmd
}

/// Çocuğun stdout'undan en çok `budget` bayt okur; `timeout` dolunca bırakır.
/// Her durumda çocuğu öldürür ve bekler (zombi/yetim kalmaz). Dönüş: (okunan baytlar, süre doldu mu).
fn read_child_stdout_bounded(
    mut child: Child,
    budget: usize,
    timeout: Duration,
) -> (Vec<u8>, bool) {
    let deadline = Instant::now() + timeout;
    let mut buf = Vec::with_capacity(budget);
    let mut timed_out = false;

    if let Some(mut stdout) = child.stdout.take() {
        let (tx, rx) = mpsc::channel::<Vec<u8>>();
        // Okuyucu, çocuk öldürülüp boru kapanınca EOF alır ve kendiliğinden biter.
        let _ = std::thread::Builder::new()
            .name("pc-eye-pcm-reader".into())
            .spawn(move || {
                let mut chunk = [0u8; 4096];
                loop {
                    match stdout.read(&mut chunk) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            if tx.send(chunk[..n].to_vec()).is_err() {
                                break;
                            }
                        }
                    }
                }
            });

        while buf.len() < budget {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                timed_out = true;
                break;
            }
            match rx.recv_timeout(remaining) {
                Ok(chunk) => buf.extend_from_slice(&chunk),
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    timed_out = true;
                    break;
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
    }

    let _ = child.kill();
    let _ = child.wait();
    buf.truncate(budget - budget % 2);
    (buf, timed_out)
}

/// `Command::output()` eşdeğeri, ama `timeout` dolunca çocuğu öldürüp `None` döner.
fn output_with_timeout(mut cmd: Command, timeout: Duration) -> Option<Output> {
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut v = Vec::new();
        let _ = stdout.read_to_end(&mut v);
        v
    });
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    };
    let stdout = reader.join().unwrap_or_default();
    Some(Output {
        status,
        stdout,
        stderr: Vec::new(),
    })
}

/// SIGTERM ile nazikçe durdurur (pw-record WAV başlığını kapatabilsin), `grace` içinde çıkmazsa SIGKILL.
fn terminate_child(child: &mut Child, grace: Duration) {
    unsafe {
        libc::kill(child.id() as i32, libc::SIGTERM);
    }
    let deadline = Instant::now() + grace;
    while Instant::now() < deadline {
        if let Ok(Some(_)) = child.try_wait() {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let _ = child.kill();
    let _ = child.wait();
}

pub fn compute_pcm_s16le_metrics(pcm_bytes: &[u8]) -> PcmSignalMetrics {
    let sample_count = pcm_bytes.len() / 2;
    if sample_count == 0 {
        return PcmSignalMetrics {
            sample_count: 0,
            rms_norm: 0.0,
            peak_norm: 0.0,
            rms_dbfs: -96.0,
            peak_dbfs: -96.0,
            audible_signal: false,
        };
    }

    let mut sum_sq = 0.0f64;
    let mut peak = 0.0f64;
    for chunk in pcm_bytes.chunks_exact(2) {
        let sample = i16::from_le_bytes([chunk[0], chunk[1]]) as f64 / 32768.0;
        let abs_s = sample.abs();
        if abs_s > peak {
            peak = abs_s;
        }
        sum_sq += sample * sample;
    }

    let rms = (sum_sq / (sample_count as f64)).sqrt();
    let to_db = |v: f64| -> f64 {
        if v <= 0.000015 {
            -96.0
        } else {
            (20.0 * v.log10() * 100.0).round() / 100.0
        }
    };

    let rms_dbfs = to_db(rms);
    let peak_dbfs = to_db(peak);
    let audible_signal = rms_dbfs > -55.0;

    PcmSignalMetrics {
        sample_count,
        rms_norm: (rms * 10000.0).round() / 10000.0,
        peak_norm: (peak * 10000.0).round() / 10000.0,
        rms_dbfs,
        peak_dbfs,
        audible_signal,
    }
}

/// D-Bus üzerindeki tüm `org.mpris.MediaPlayer2.*` servislerini (Firefox, Spotify, MPV, Chromium vb.) tarar.
pub fn collect_mpris_media_players() -> Vec<MprisMediaInfo> {
    let Ok(list_out) = Command::new("busctl").args(["--user", "list"]).output() else {
        return Vec::new();
    };
    let text = String::from_utf8_lossy(&list_out.stdout);
    let mut services = Vec::new();
    for line in text.lines() {
        if let Some(srv) = line.split_whitespace().next() {
            if srv.starts_with("org.mpris.MediaPlayer2.") {
                services.push(srv.to_string());
            }
        }
    }

    let mut players = Vec::new();
    for srv in services {
        let player_name = srv
            .strip_prefix("org.mpris.MediaPlayer2.")
            .unwrap_or(&srv)
            .split('.')
            .next()
            .unwrap_or(&srv)
            .to_string();

        let playback_status =
            mpris_get_string(&srv, "PlaybackStatus").unwrap_or_else(|| "Stopped".to_string());
        let (title, artist, url, length_sec) = mpris_get_metadata(&srv);

        players.push(MprisMediaInfo {
            service: srv,
            player_name,
            playback_status,
            title,
            artist,
            url,
            length_sec,
        });
    }
    players
}

fn mpris_get_string(service: &str, prop: &str) -> Option<String> {
    let out = Command::new("busctl")
        .args([
            "--user",
            "get-property",
            service,
            "/org/mpris/MediaPlayer2",
            "org.mpris.MediaPlayer2.Player",
            prop,
            "--json=short",
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let val: Value = serde_json::from_slice(&out.stdout).ok()?;
    val.get("data")?.as_str().map(|s| s.to_string())
}

fn mpris_get_metadata(
    service: &str,
) -> (Option<String>, Option<String>, Option<String>, Option<f64>) {
    let Ok(out) = Command::new("busctl")
        .args([
            "--user",
            "get-property",
            service,
            "/org/mpris/MediaPlayer2",
            "org.mpris.MediaPlayer2.Player",
            "Metadata",
            "--json=short",
        ])
        .output()
    else {
        return (None, None, None, None);
    };
    if !out.status.success() {
        return (None, None, None, None);
    }
    let Ok(val) = serde_json::from_slice::<Value>(&out.stdout) else {
        return (None, None, None, None);
    };
    let Some(map) = val.get("data").and_then(|d| d.as_object()) else {
        return (None, None, None, None);
    };

    let title = map
        .get("xesam:title")
        .and_then(|v| v.get("data"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());

    let artist = map
        .get("xesam:artist")
        .and_then(|v| v.get("data"))
        .and_then(|v| v.as_array())
        .and_then(|a| a.first())
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());

    let url = map
        .get("xesam:url")
        .and_then(|v| v.get("data"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());

    let length_sec = map
        .get("mpris:length")
        .and_then(|v| v.get("data"))
        .and_then(|v| v.as_i64().or_else(|| v.as_u64().map(|u| u as i64)))
        .map(|usec| ((usec as f64 / 1_000_000.0) * 10.0).round() / 10.0);

    (title, artist, url, length_sec)
}

/// Ses yüksekliği (`wpctl`) ve MPRIS medya oynatıcılarını (`Play`, `Pause`, `PlayPause`, `Next`, `Previous`) kontrol eder.
pub fn control_audio_and_media(op: &str, arg: Option<&str>) -> Result<ActionReceipt> {
    match op.to_ascii_lowercase().as_str() {
        "play" | "pause" | "play-pause" | "playpause" | "next" | "previous" | "stop" => {
            let method = match op.to_ascii_lowercase().as_str() {
                "play" => "Play",
                "pause" => "Pause",
                "play-pause" | "playpause" => "PlayPause",
                "next" => "Next",
                "previous" => "Previous",
                _ => "Stop",
            };
            let players = collect_mpris_media_players();
            let target_srv = if let Some(filter) = arg {
                players
                    .iter()
                    .find(|p| {
                        p.service
                            .to_ascii_lowercase()
                            .contains(&filter.to_ascii_lowercase())
                    })
                    .map(|p| p.service.clone())
            } else {
                players.first().map(|p| p.service.clone())
            }
            .ok_or_else(|| anyhow!("Aktif MPRIS medya oynatıcısı bulunamadı"))?;

            let status = Command::new("busctl")
                .args([
                    "--user",
                    "call",
                    &target_srv,
                    "/org/mpris/MediaPlayer2",
                    "org.mpris.MediaPlayer2.Player",
                    method,
                ])
                .status()?;
            if !status.success() {
                return Err(anyhow!("MPRIS `{}` çağrısı başarısız oldu", method));
            }
            Ok(ActionReceipt {
                ok: true,
                action: format!("media_{}", method.to_ascii_lowercase()),
                target: target_srv,
                coordinates: None,
                details: format!("MPRIS `{}` komutu gönderildi", method),
            })
        }
        "set-volume" | "volume" => {
            let vol = arg.ok_or_else(|| anyhow!("Ses seviyesi gerekli (ör. 0.5 veya 50%)"))?;
            Command::new("wpctl")
                .args(["set-volume", "@DEFAULT_AUDIO_SINK@", vol])
                .status()?;
            Ok(ActionReceipt {
                ok: true,
                action: "audio_set_volume".to_string(),
                target: "@DEFAULT_AUDIO_SINK@".to_string(),
                coordinates: None,
                details: format!("Ses seviyesi `{}` olarak ayarlandı", vol),
            })
        }
        "mute" | "unmute" | "toggle-mute" => {
            let mode = match op.to_ascii_lowercase().as_str() {
                "mute" => "1",
                "unmute" => "0",
                _ => "toggle",
            };
            Command::new("wpctl")
                .args(["set-mute", "@DEFAULT_AUDIO_SINK@", mode])
                .status()?;
            Ok(ActionReceipt {
                ok: true,
                action: "audio_mute".to_string(),
                target: "@DEFAULT_AUDIO_SINK@".to_string(),
                coordinates: None,
                details: format!("Ses mute durumu `{}` yapıldı", mode),
            })
        }
        "record" | "record-wav" => {
            let duration_ms = arg
                .and_then(|s| s.trim_end_matches("ms").parse::<u64>().ok())
                .unwrap_or(2000)
                .clamp(100, 15000);
            let out_path = crate::eyes::vision::runtime_dir().join("audio_capture.wav");
            let _ = std::fs::remove_file(&out_path);
            let sink = resolve_default_sink().ok_or_else(|| {
                anyhow!("Varsayılan sink çözülemedi; mikrofona düşmemek için kayıt yapılmadı")
            })?;
            let mut cmd =
                sink_monitor_record_command(&sink.node_name, &out_path.to_string_lossy(), false);
            let mut child = cmd
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()?;
            std::thread::sleep(Duration::from_millis(duration_ms));
            terminate_child(&mut child, Duration::from_millis(1000));
            let bytes = std::fs::read(&out_path).unwrap_or_default();
            let pcm_slice = if bytes.len() > 44 { &bytes[44..] } else { &[] };
            let sig = compute_pcm_s16le_metrics(pcm_slice);
            let sig_info = format!(
                "rms_dbfs={:.1}, peak_dbfs={:.1}, audible={}",
                sig.rms_dbfs, sig.peak_dbfs, sig.audible_signal
            );

            Ok(ActionReceipt {
                ok: bytes.len() > 44,
                action: "audio_record_wav".to_string(),
                target: out_path.to_string_lossy().to_string(),
                coordinates: None,
                details: if bytes.len() > 44 {
                    format!(
                        "{} ms masaüstü sesi `{}` monitöründen kaydedildi ({} bayt, {})",
                        duration_ms,
                        sink.node_name,
                        bytes.len(),
                        sig_info
                    )
                } else {
                    format!(
                        "`{}` monitöründen {} ms içinde PCM gelmedi (sink saati durmuş veya askıda olabilir)",
                        sink.node_name, duration_ms
                    )
                },
            })
        }
        other => Err(anyhow!("Bilinmeyen ses/medya komutu: {}", other)),
    }
}

fn read_default_sink_volume() -> (Option<f64>, bool) {
    let Ok(output) = Command::new("wpctl")
        .args(["get-volume", "@DEFAULT_AUDIO_SINK@"])
        .output()
    else {
        return (None, false);
    };
    if !output.status.success() {
        return (None, false);
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let muted = text.contains("MUTED");
    let vol = text
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse::<f64>().ok());
    (vol, muted)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pcm_s16le_rms_and_peak_calculation() {
        // Tam sessizlik -> -96 dBFS ve audible_signal = false
        let silence = vec![0u8; 400];
        let m_sil = compute_pcm_s16le_metrics(&silence);
        assert_eq!(m_sil.sample_count, 200);
        assert_eq!(m_sil.rms_dbfs, -96.0);
        assert!(!m_sil.audible_signal);

        // Belirgin bir ses dalgası (16384 genlik = 0.5 = -6.02 dBFS)
        let mut loud = Vec::new();
        for _ in 0..200 {
            loud.extend_from_slice(&16384i16.to_le_bytes());
        }
        let m_loud = compute_pcm_s16le_metrics(&loud);
        assert!(m_loud.rms_dbfs > -7.0 && m_loud.rms_dbfs < -5.5);
        assert!(m_loud.audible_signal);
    }

    #[test]
    fn test_parse_sink_node_name_accepts_only_sinks() {
        let sink = "id 57, type PipeWire:Interface:Node\n    media.class = \"Audio/Sink\"\n  * node.name = \"alsa_output.pci-0000_07_00.6.analog-stereo\"\n";
        assert_eq!(
            parse_sink_node_name(sink).as_deref(),
            Some("alsa_output.pci-0000_07_00.6.analog-stereo")
        );
        // Kaynak (mikrofon) düğümü asla hedef olarak döndürülmemeli.
        let source = "    media.class = \"Audio/Source\"\n  * node.name = \"alsa_input.pci-0000_07_00.6.analog-stereo\"\n";
        assert_eq!(parse_sink_node_name(source), None);
        assert_eq!(parse_sink_node_name("  * node.name = \"x\"\n"), None);
    }

    #[test]
    fn test_sink_monitor_command_never_uses_wpctl_alias() {
        let cmd = sink_monitor_record_command("alsa_output.test", "-", true);
        let args: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(!args.iter().any(|a| a.contains("@DEFAULT_AUDIO")));
        let t = args.iter().position(|a| a == "--target").unwrap();
        assert_eq!(args[t + 1], "alsa_output.test");
        assert!(args.iter().any(|a| a.contains("stream.capture.sink=true")));
        assert!(args.iter().any(|a| a == "--raw"));
    }

    #[test]
    fn test_bounded_read_kills_silent_child_within_timeout() {
        // Hiç çıktı vermeyen çocuk: sink saati durmuş pw-record'u taklit eder.
        let child = Command::new("sleep")
            .arg("30")
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let pid = child.id() as i32;
        let started = Instant::now();
        let (bytes, timed_out) =
            read_child_stdout_bounded(child, PCM_SAMPLE_BYTES, Duration::from_millis(150));
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(timed_out);
        assert!(bytes.is_empty());
        // Çocuk öldürülüp beklendi: PID artık yok (zombi de değil).
        assert_ne!(unsafe { libc::kill(pid, 0) }, 0);
    }

    #[test]
    fn test_bounded_read_stops_at_byte_budget() {
        let child = Command::new("cat")
            .arg("/dev/zero")
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let (bytes, timed_out) =
            read_child_stdout_bounded(child, PCM_SAMPLE_BYTES, Duration::from_secs(2));
        assert!(!timed_out);
        assert_eq!(bytes.len(), PCM_SAMPLE_BYTES);
        assert!(!compute_pcm_s16le_metrics(&bytes).audible_signal);
    }

    #[test]
    fn test_unknown_sink_is_not_sampled() {
        assert!(measure_sink_pcm_signal(None).is_err());
        let idle = DefaultSink {
            node_name: "alsa_output.test".into(),
            state: Some("suspended".into()),
        };
        assert!(measure_sink_pcm_signal(Some(&idle))
            .unwrap_err()
            .contains("suspended"));
    }
}
