use serde_json::{json, Value};
use std::io::Write;
use std::process::{Command, Stdio};

fn bin_path() -> &'static str {
    env!("CARGO_BIN_EXE_pc-eye")
}

fn call_mcp(requests: &[Value]) -> Vec<Value> {
    let mut child = Command::new(bin_path())
        .arg("mcp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("Failed to spawn pc-eye mcp");

    {
        let stdin = child.stdin.as_mut().expect("Failed to open stdin");
        for req in requests {
            writeln!(stdin, "{}", req).expect("Failed to write request");
        }
    }

    let output = child.wait_with_output().expect("Failed to wait on child");
    assert!(output.status.success());

    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str::<Value>(l).expect("Invalid JSON-RPC line"))
        .collect()
}

#[test]
fn test_mcp_initialize_and_tools_list() {
    let responses = call_mcp(&[
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {}
        }),
        json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/list",
            "params": {}
        }),
    ]);

    assert_eq!(responses.len(), 2);
    assert_eq!(responses[0]["result"]["serverInfo"]["name"], "pc-eye");

    let tools = responses[1]["result"]["tools"]
        .as_array()
        .expect("tools should be an array");
    assert_eq!(tools.len(), 16, "Expected 16 MCP tools");
    assert!(tools.iter().any(|t| t["name"] == "pc_eye_snapshot"));
    assert!(tools.iter().any(|t| t["name"] == "pc_eye_processes"));

    let names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
    assert!(names.contains(&"pc_eye_context"));
    assert!(names.contains(&"pc_eye_ui"));
    assert!(names.contains(&"pc_eye_vision"));
    assert!(names.contains(&"pc_eye_xray"));
    assert!(names.contains(&"pc_eye_logs"));
    assert!(names.contains(&"pc_eye_audio"));
    assert!(names.contains(&"pc_eye_click_ui"));
    assert!(names.contains(&"pc_eye_input"));
    assert!(names.contains(&"pc_eye_window"));
    assert!(names.contains(&"pc_eye_launch"));
    assert!(names.contains(&"pc_eye_wait"));
    assert!(names.contains(&"pc_eye_director"));
}

#[test]
fn test_mcp_context_and_binds_and_safety_denylist() {
    let my_pid = std::process::id() as i32;
    let responses = call_mcp(&[
        json!({
            "jsonrpc": "2.0",
            "id": 10,
            "method": "tools/call",
            "params": {
                "name": "pc_eye_context",
                "arguments": {}
            }
        }),
        json!({
            "jsonrpc": "2.0",
            "id": 11,
            "method": "tools/call",
            "params": {
                "name": "pc_eye_xray",
                "arguments": {
                    "pid": my_pid
                }
            }
        }),
        json!({
            "jsonrpc": "2.0",
            "id": 12,
            "method": "tools/call",
            "params": {
                "name": "pc_eye_director",
                "arguments": {
                    "op": "binds"
                }
            }
        }),
        json!({
            "jsonrpc": "2.0",
            "id": 13,
            "method": "tools/call",
            "params": {
                "name": "pc_eye_director",
                "arguments": {
                    "op": "freeze",
                    "pid": 1
                }
            }
        }),
    ]);

    assert_eq!(responses.len(), 4);

    // 1. Context response should contain system_health, desktop, and audio_summary
    let ctx_text = responses[0]["result"]["content"][0]["text"]
        .as_str()
        .expect("context text");
    let ctx_json: Value = serde_json::from_str(ctx_text).expect("valid context json");
    assert!(ctx_json.get("system_health").is_some());
    assert!(ctx_json.get("desktop").is_some());
    assert!(ctx_json.get("audio_summary").is_some());

    // 2. Xray on current test runner PID should succeed
    let xray_text = responses[1]["result"]["content"][0]["text"]
        .as_str()
        .expect("xray text");
    let xray_json: Value = serde_json::from_str(xray_text).expect("valid xray json");
    assert_eq!(xray_json["root_pid"].as_i64(), Some(my_pid as i64));

    // 3. Binds should return a JSON array of Hyprland binds
    let binds_text = responses[2]["result"]["content"][0]["text"]
        .as_str()
        .expect("binds text");
    let binds_json: Value = serde_json::from_str(binds_text).expect("valid binds json");
    assert!(binds_json.is_array());

    // 4. Freezing PID 1 (systemd/init) must be blocked by FreezeDenylist safety guard
    assert_eq!(
        responses[3]["result"]["isError"],
        Value::Bool(true),
        "Freezing PID 1 must be blocked by safety guard"
    );
}

#[test]
fn test_cli_doctor_logs_and_rich_clipboard() {
    let out = Command::new(bin_path())
        .args(["doctor", "--json"])
        .output()
        .expect("Failed to run pc-eye doctor --json");
    assert!(out.status.success());
    let doc: Value = serde_json::from_slice(&out.stdout).expect("Valid doctor JSON");
    assert_eq!(doc["status"].as_str(), Some("ready"));
    assert_eq!(doc["ocr_available"].as_bool(), Some(true));
    assert!(doc.get("hyprland_socket").is_some());
    assert!(doc.get("gpus_detected").is_some());

    let logs_out = Command::new(bin_path())
        .args(["logs", "--limit", "8", "--json"])
        .output()
        .expect("Failed to run pc-eye logs --limit 8 --json");
    assert!(logs_out.status.success());
    let logs: Value = serde_json::from_slice(&logs_out.stdout).expect("Valid logs JSON");
    assert!(logs.is_array());

    let clip_out = Command::new(bin_path())
        .args(["director", "clipboard", "--json"])
        .output()
        .expect("Failed to run pc-eye director clipboard --json");
    assert!(clip_out.status.success());
    let clip: Value = serde_json::from_slice(&clip_out.stdout).expect("Valid clipboard JSON");
    assert!(clip.get("mime_types").and_then(|m| m.as_array()).is_some());
    assert!(clip.get("history").and_then(|h| h.as_array()).is_some());
}
