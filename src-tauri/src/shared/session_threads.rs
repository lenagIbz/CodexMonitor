use chrono::DateTime;
use serde_json::{json, Value};
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

fn normalize_windows_path(path: &str) -> String {
    let trimmed = path.trim().replace('/', "\\");
    let without_prefix = trimmed
        .strip_prefix(r"\\?\")
        .unwrap_or(trimmed.as_str());
    without_prefix.to_ascii_lowercase()
}

fn parse_timestamp_ms(value: &Value) -> Option<i64> {
    match value {
        Value::String(text) => DateTime::parse_from_rfc3339(text)
            .map(|dt| dt.timestamp_millis())
            .ok(),
        Value::Number(num) => num.as_i64().map(|raw| {
            if raw < 1_000_000_000_000 {
                raw * 1000
            } else {
                raw
            }
        }),
        _ => None,
    }
}

fn extract_message_text(content: &Value) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Value::Array(entries) = content {
        for entry in entries {
            if let Value::Object(map) = entry {
                let entry_type = map
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if entry_type == "input_text" || entry_type == "output_text" {
                    if let Some(text) = map.get("text").and_then(Value::as_str) {
                        if !text.trim().is_empty() {
                            parts.push(text.trim().to_string());
                        }
                    }
                }
            }
        }
    }
    parts.join(" ").trim().to_string()
}

fn collect_session_files(root: &Path) -> Vec<PathBuf> {
    let mut stack = vec![root.to_path_buf()];
    let mut files = Vec::new();
    while let Some(dir) = stack.pop() {
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if path.extension().and_then(|ext| ext.to_str()) == Some("jsonl") {
                files.push(path);
            }
        }
    }
    files
}

pub fn list_session_threads(
    codex_home: &Path,
    workspace_path: &str,
    limit: Option<u32>,
) -> Result<Vec<Value>, String> {
    let sessions_root = codex_home.join("sessions");
    if !sessions_root.exists() {
        return Ok(Vec::new());
    }
    let target_path = normalize_windows_path(workspace_path);
    let mut results: Vec<Value> = Vec::new();
    let files = collect_session_files(&sessions_root);
    for path in files {
        let file = File::open(&path).map_err(|err| err.to_string())?;
        let reader = BufReader::new(file);
        let mut session_id: Option<String> = None;
        let mut cwd: Option<String> = None;
        let mut created_at: Option<i64> = None;
        let mut updated_at: Option<i64> = None;
        let mut last_user: Option<String> = None;
        let mut last_assistant: Option<String> = None;
        for line in reader.lines().flatten() {
            let parsed: Value = match serde_json::from_str(&line) {
                Ok(value) => value,
                Err(_) => continue,
            };
            let timestamp = parsed.get("timestamp").and_then(parse_timestamp_ms);
            if let Some(ts) = timestamp {
                updated_at = Some(updated_at.map_or(ts, |prev| prev.max(ts)));
            }
            let entry_type = parsed.get("type").and_then(Value::as_str).unwrap_or("");
            if entry_type == "session_meta" {
                if let Some(payload) = parsed.get("payload") {
                    if let Some(id) = payload.get("id").and_then(Value::as_str) {
                        session_id = Some(id.to_string());
                    }
                    if let Some(raw_cwd) = payload.get("cwd").and_then(Value::as_str) {
                        cwd = Some(raw_cwd.to_string());
                    }
                    if let Some(meta_ts) = payload.get("timestamp").and_then(parse_timestamp_ms)
                    {
                        created_at = Some(meta_ts);
                    }
                }
                continue;
            }
            if entry_type != "response_item" {
                continue;
            }
            let payload = parsed.get("payload").unwrap_or(&Value::Null);
            let payload_type = payload.get("type").and_then(Value::as_str).unwrap_or("");
            if payload_type != "message" {
                continue;
            }
            let role = payload.get("role").and_then(Value::as_str).unwrap_or("");
            let content = payload.get("content").unwrap_or(&Value::Null);
            let text = extract_message_text(content);
            if text.is_empty() {
                continue;
            }
            if role == "assistant" {
                last_assistant = Some(text);
            } else if role == "user" {
                last_user = Some(text);
            }
        }

        let Some(cwd_value) = cwd.as_ref() else {
            continue;
        };
        if normalize_windows_path(cwd_value) != target_path {
            continue;
        }
        let Some(id_value) = session_id else {
            continue;
        };
        let preview = last_assistant.or(last_user).unwrap_or_default();
        results.push(json!({
            "id": id_value,
            "cwd": workspace_path,
            "preview": preview,
            "createdAt": created_at,
            "updatedAt": updated_at.or(created_at),
            "source": "session"
        }));
    }
    results.sort_by(|a, b| {
        let a_ts = a
            .get("updatedAt")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        let b_ts = b
            .get("updatedAt")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        b_ts.cmp(&a_ts)
    });
    if let Some(limit) = limit {
        results.truncate(limit as usize);
    }
    Ok(results)
}

pub fn load_session_thread(
    codex_home: &Path,
    session_id: &str,
) -> Result<Option<Value>, String> {
    let sessions_root = codex_home.join("sessions");
    if !sessions_root.exists() {
        return Ok(None);
    }
    let files = collect_session_files(&sessions_root);
    let mut target_file: Option<PathBuf> = None;
    for path in files {
        if let Some(name) = path.file_name().and_then(|value| value.to_str()) {
            if name.contains(session_id) {
                target_file = Some(path);
                break;
            }
        }
    }
    let Some(path) = target_file else {
        return Ok(None);
    };

    let file = File::open(&path).map_err(|err| err.to_string())?;
    let reader = BufReader::new(file);
    let mut cwd: Option<String> = None;
    let mut created_at: Option<i64> = None;
    let mut updated_at: Option<i64> = None;
    let mut preview: Option<String> = None;
    let mut items: Vec<Value> = Vec::new();
    let mut index: usize = 0;
    for line in reader.lines().flatten() {
        let parsed: Value = match serde_json::from_str(&line) {
            Ok(value) => value,
            Err(_) => continue,
        };
        let timestamp = parsed.get("timestamp").and_then(parse_timestamp_ms);
        if let Some(ts) = timestamp {
            updated_at = Some(updated_at.map_or(ts, |prev| prev.max(ts)));
        }
        let entry_type = parsed.get("type").and_then(Value::as_str).unwrap_or("");
        if entry_type == "session_meta" {
            if let Some(payload) = parsed.get("payload") {
                if let Some(raw_cwd) = payload.get("cwd").and_then(Value::as_str) {
                    cwd = Some(raw_cwd.to_string());
                }
                if let Some(meta_ts) = payload.get("timestamp").and_then(parse_timestamp_ms)
                {
                    created_at = Some(meta_ts);
                }
            }
            continue;
        }
        if entry_type != "response_item" {
            continue;
        }
        let payload = parsed.get("payload").unwrap_or(&Value::Null);
        let payload_type = payload.get("type").and_then(Value::as_str).unwrap_or("");
        let id = payload
            .get("id")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| format!("session-{session_id}-{index}"));
        index += 1;
        if payload_type == "message" {
            let role = payload.get("role").and_then(Value::as_str).unwrap_or("");
            let content = payload.get("content").unwrap_or(&Value::Null);
            let text = extract_message_text(content);
            if text.is_empty() {
                continue;
            }
            if role == "assistant" {
                preview = Some(text.clone());
                items.push(json!({
                    "id": id,
                    "type": "agentMessage",
                    "text": text
                }));
            } else if role == "user" {
                items.push(json!({
                    "id": id,
                    "type": "userMessage",
                    "content": [{"type": "text", "text": text}]
                }));
            }
            continue;
        }
        if payload_type == "reasoning" {
            let summary = payload
                .get("summary")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let content = payload
                .get("content")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            items.push(json!({
                "id": id,
                "type": "reasoning",
                "summary": summary,
                "content": content
            }));
            continue;
        }
    }

    let thread = json!({
        "id": session_id,
        "cwd": cwd,
        "preview": preview.unwrap_or_default(),
        "createdAt": created_at,
        "updatedAt": updated_at.or(created_at),
        "turns": [{
            "id": format!("turn-{session_id}-0"),
            "items": items
        }]
    });
    Ok(Some(thread))
}
