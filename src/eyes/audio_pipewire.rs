use crate::daemon::ring_buffer::now_ms;
use crate::protocol::types::{AudioReport, AudioStreamNode};
use anyhow::Result;
use serde_json::Value;
use std::process::Command;

/// Göz 5: PipeWire grafiğini (`pw-dump`) ve varsayılan ses yüksekliğini (`wpctl`) okur.
pub fn capture_audio_report() -> Result<AudioReport> {
    let ts = now_ms();
    let (default_sink_volume, default_sink_muted) = read_default_sink_volume();

    let output = Command::new("pw-dump").output()?;
    if !output.status.success() {
        return Ok(AudioReport {
            timestamp_ms: ts,
            default_sink_volume,
            default_sink_muted,
            active_playback_streams: 0,
            active_recording_streams: 0,
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

        // Sadece ses düğümlerini ve akışlarını dahil et
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

    Ok(AudioReport {
        timestamp_ms: ts,
        default_sink_volume,
        default_sink_muted,
        active_playback_streams,
        active_recording_streams,
        nodes,
    })
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
    // Örnek çıktı: "Volume: 0.40" veya "Volume: 0.40 [MUTED]"
    let text = String::from_utf8_lossy(&output.stdout);
    let muted = text.contains("MUTED");
    let vol = text
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse::<f64>().ok());
    (vol, muted)
}
