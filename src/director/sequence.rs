use crate::director::input::{
    click_at, click_ui_element, move_cursor, press_key_combo, scroll_wheel, type_text,
    ActionReceipt,
};
use crate::director::process_ctl::manage_window;
use crate::eyes::compositor::{capture_desktop_state, wait_for_compositor_event};
use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use std::thread;
use std::time::Duration;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SequenceStep {
    pub op: String, // "click" | "click_ui" | "move" | "type" | "key" | "scroll" | "window" | "wait" | "sleep_ms"
    pub x: Option<i64>,
    pub y: Option<i64>,
    // Tekerlek miktarı ayrı alanlarda: `x`/`y` yalnız imlecin gideceği noktadır, aksi hâlde
    // koordinatlar kaydırma miktarı sanılıp sayfa yüzlerce adım kayıyordu.
    pub dx: Option<i32>,
    pub dy: Option<i32>,
    pub name: Option<String>,
    pub mark: Option<usize>,
    pub text: Option<String>,
    pub keys: Option<String>,
    pub window: Option<String>,
    pub action: Option<String>,
    pub arg: Option<String>,
    pub ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SequenceReport {
    pub completed: bool,
    pub steps_executed: usize,
    pub total_steps: usize,
    pub aborted_reason: Option<String>,
    pub receipts: Vec<ActionReceipt>,
}

/// Çok adımlı aksiyon dizisini (`sequence`) atomik olarak yürütür; araya beklenmedik bir güvenlik penceresi
/// veya kilit ekranı girerse anında durur.
pub fn execute_sequence(steps: &[SequenceStep], allow_protected: bool) -> Result<SequenceReport> {
    let initial_state = capture_desktop_state()?;
    let mut initial_workspace = initial_state
        .monitors
        .iter()
        .find(|m| m.focused)
        .map(|m| m.active_workspace_id);

    let mut receipts = Vec::new();
    let total_steps = steps.len();

    for (idx, step) in steps.iter().enumerate() {
        let current_state = capture_desktop_state()?;
        if current_state.auth_guard_tripped {
            return Ok(SequenceReport {
                completed: false,
                steps_executed: idx,
                total_steps,
                aborted_reason: Some(format!(
                    "Adım {} öncesi AuthGuard devreye girdi: {}",
                    idx + 1,
                    current_state.auth_guard_reason.unwrap_or_default()
                )),
                receipts,
            });
        }

        let op = step.op.to_ascii_lowercase();
        let receipt = match op.as_str() {
            "click" => click_at(
                step.x,
                step.y,
                step.action.as_deref().unwrap_or("left"),
                1,
                allow_protected,
            )?,
            "click_ui" => click_ui_element(
                step.window.as_deref(),
                step.name.as_deref(),
                step.mark,
                "left",
                allow_protected,
            )?,
            "move" => {
                let x = step.x.ok_or_else(|| anyhow!("move için x gerekli"))?;
                let y = step.y.ok_or_else(|| anyhow!("move için y gerekli"))?;
                move_cursor(x, y)?
            }
            "type" => {
                let txt = step
                    .text
                    .as_deref()
                    .ok_or_else(|| anyhow!("type için text gerekli"))?;
                type_text(txt, step.window.as_deref(), allow_protected)?
            }
            "key" => {
                let k = step
                    .keys
                    .as_deref()
                    .or(step.text.as_deref())
                    .ok_or_else(|| anyhow!("key için keys gerekli"))?;
                press_key_combo(k, step.window.as_deref(), allow_protected)?
            }
            "scroll" => scroll_wheel(
                step.x,
                step.y,
                step.dx.unwrap_or(0),
                step.dy.unwrap_or(-3),
                allow_protected,
            )?,
            "window" => manage_window(
                step.action.as_deref().unwrap_or("focus"),
                step.window.as_deref(),
                step.arg.as_deref(),
            )?,
            "wait" => {
                let ev = wait_for_compositor_event(
                    step.action.as_deref().unwrap_or("activewindow"),
                    step.text.as_deref(),
                    step.ms.unwrap_or(3000),
                )?;
                ActionReceipt {
                    ok: true,
                    action: "wait".to_string(),
                    target: ev.kind,
                    coordinates: None,
                    details: ev.payload,
                    verification: None,
                }
            }
            "sleep_ms" => {
                let ms = step.ms.unwrap_or(100).min(5000);
                thread::sleep(Duration::from_millis(ms));
                ActionReceipt {
                    ok: true,
                    action: "sleep_ms".to_string(),
                    target: format!("{}ms", ms),
                    coordinates: None,
                    details: "Beklendi".to_string(),
                    verification: None,
                }
            }
            other => return Err(anyhow!("Bilinmeyen sequence adımı: {}", other)),
        };

        receipts.push(receipt);

        // `window` adımı çalışma alanını bilinçli değiştirebilir: referans yeni duruma çekilir,
        // yoksa sonraki adım eski referansla karşılaştırılıp dizi haksız yere durdurulur.
        if op == "window" {
            let post = capture_desktop_state()?;
            if let Some(ws) = post
                .monitors
                .iter()
                .find(|m| m.focused)
                .map(|m| m.active_workspace_id)
            {
                initial_workspace = Some(ws);
            }
        // Eğer adım bir workspace değişimi istemediyse ama workspace beklenmedik şekilde değiştiyse diziyi durdur
        } else {
            let post = capture_desktop_state()?;
            let post_ws = post
                .monitors
                .iter()
                .find(|m| m.focused)
                .map(|m| m.active_workspace_id);
            if initial_workspace.is_some() && post_ws != initial_workspace {
                return Ok(SequenceReport {
                    completed: false,
                    steps_executed: idx + 1,
                    total_steps,
                    aborted_reason: Some(
                        "Beklenmedik çalışma alanı (workspace) değişimi tespit edildi, sequence durduruldu"
                            .to_string(),
                    ),
                    receipts,
                });
            }
        }
    }

    Ok(SequenceReport {
        completed: true,
        steps_executed: total_steps,
        total_steps,
        aborted_reason: None,
        receipts,
    })
}

#[cfg(test)]
mod tests {
    use super::SequenceStep;

    // `x`/`y` yalnız imleç konumudur; tekerlek miktarı verilmezse varsayılanda kalmalı.
    #[test]
    fn scroll_step_defaults_dx_dy() {
        let step: SequenceStep =
            serde_json::from_str(r#"{"op":"scroll","x":600,"y":400}"#).unwrap();
        assert_eq!((step.dx, step.dy), (None, None));
    }
}
