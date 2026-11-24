//! gray-git — one sidecar for the git lifecycle.
//!
//! Merges gray-checkpoint, gray-autocommit, gray-git-merge and gray-review.
//! Each source lives verbatim in `mods/` with its own `~/.gray/<orig>/`
//! state and slash commands — only the wire plumbing is shared.
//!
//! Routing:
//! - `agent/before_start` → checkpoint (stash snapshot at agent start).
//! - `event/notify` (notification) → checkpoint (turn_end snapshot), then
//!   autocommit (pre_tool/post_tool tracking, turn_end commit). Checkpoint
//!   runs first so the snapshot captures the tree before it is committed.
//! - `tool/call` → `git_conflicts` → git-merge.
//! - `command/run` → routed by `params.name` to the owning module.
//! - `host/ask` round-trips (checkpoint's interactive restore) go through a
//!   reader thread that parks host answers on a pending map.

use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::sync::{Arc, Mutex, mpsc};

use serde_json::{Value, json};

mod parts;

#[allow(dead_code)]
#[path = "mods/autocommit.rs"]
mod autocommit;
#[allow(dead_code)]
#[path = "mods/checkpoint.rs"]
mod checkpoint;
#[allow(dead_code)]
#[path = "mods/gitmerge.rs"]
mod gitmerge;
#[allow(dead_code)]
#[path = "mods/review.rs"]
mod review;

const PARTS: &[parts::Part] = &[
    parts::Part {
        name: "checkpoint",
        default: true,
        description: "git stash snapshots around turns, restorable with /checkpoint",
    },
    parts::Part {
        name: "autocommit",
        default: true,
        description: "commit at turn end when tools ran",
    },
    parts::Part {
        name: "git-merge",
        default: true,
        description: "merge a branch and work through conflicts",
    },
    parts::Part {
        name: "review",
        default: true,
        description: "review or simplify the working-tree diff",
    },
];

fn manifest(parts: &parts::Parts) -> Value {
    let mut tools: Vec<Value> = Vec::new();
    if parts.on("git-merge") {
        tools.push(json!({
            "name": "git_conflicts",
            "description": "List files still conflicted after a merge, or stage a file whose conflict markers you fixed. action \"list\" shows every `git diff --diff-filter=U` path and whether conflict markers remain; action \"mark-done\" with `file` verifies the markers are gone and runs `git add` on it.",
            "parameters": {
                "type": "object",
                "properties": {
                    "action": { "type": "string", "enum": ["list", "mark-done"], "description": "\"list\" conflicts or \"mark-done\" a resolved file." },
                    "file": { "type": "string", "description": "Repo-relative path to stage. Required for mark-done." },
                    "cwd": { "type": "string", "description": "Repo root. Defaults to the cwd recorded by /merge." }
                },
                "required": ["action"]
            }
        }));
    }
    let mut commands: Vec<&str> = vec!["/git"];
    let mut descriptions = serde_json::Map::new();
    descriptions.insert("/git".into(), json!("list the git lifecycle commands"));
    if parts.on("checkpoint") {
        commands.push("/checkpoint");
        descriptions.insert(
            "/checkpoint".into(),
            json!("list or restore git stash checkpoints"),
        );
    }
    if parts.on("autocommit") {
        commands.push("/autocommit");
        descriptions.insert(
            "/autocommit".into(),
            json!("toggle committing changes at turn end"),
        );
    }
    if parts.on("git-merge") {
        commands.push("/merge");
        descriptions.insert("/merge".into(), json!("merge a branch and fix conflicts"));
    }
    if parts.on("review") {
        commands.extend(["/review", "/simplify"]);
        descriptions.insert("/review".into(), json!("review the working-tree git diff"));
        descriptions.insert("/simplify".into(), json!("simplify the changed code"));
    }
    let mut hooks: Vec<&str> = Vec::new();
    if parts.on("checkpoint") {
        hooks.push("agent/before_start");
    }
    let mut capabilities: Vec<&str> = Vec::new();
    if parts.on("checkpoint") {
        capabilities.push("host.ask");
    }
    json!({
        "name": "git",
        "version": env!("CARGO_PKG_VERSION"),
        "protocol": "2.0",
        "tools": tools,
        "commands": commands,
        "command_descriptions": descriptions,
        "hooks": hooks,
        "capabilities": capabilities,
        "parts": parts.manifest(PARTS),
    })
}

/// `/git` — merged-plugin overview.
fn git_overview(parts: &parts::Parts) -> String {
    let mut lines: Vec<String> = vec![format!(
        "gray-git {} — one sidecar, four git-lifecycle modules:",
        env!("CARGO_PKG_VERSION")
    )];
    if parts.on("checkpoint") {
        lines.push(
            "\x20 /checkpoint [list|restore <n>]  git stash snapshots per session (~/.gray/checkpoint/)"
                .into(),
        );
    }
    if parts.on("autocommit") {
        lines.push(
            "\x20 /autocommit on|off|status       commit at turn end when tools ran (~/.gray/autocommit/)"
                .into(),
        );
    }
    if parts.on("git-merge") {
        lines.push(
            "\x20 /merge <branch>                 git merge; conflicts → resolve prompt + git_conflicts tool"
                .into(),
        );
    }
    if parts.on("review") {
        lines.push(
            "\x20 /review, /simplify              review or simplify the working-tree diff".into(),
        );
    }
    lines.join("\n")
}

fn unwrap_result(reply: Option<Value>) -> Value {
    match reply {
        Some(r) => r.get("result").cloned().unwrap_or(json!({})),
        None => json!({}),
    }
}

fn handle(
    req: &Value,
    turn: &mut autocommit::Turn,
    ask: &mut dyn FnMut(&[Value]) -> Option<Value>,
    parts: &parts::Parts,
) -> (Option<Value>, bool) {
    let id = req.get("id").cloned();
    let method = req.get("method").and_then(Value::as_str).unwrap_or("");
    let params = req.get("params").cloned().unwrap_or(Value::Null);
    let Some(id) = id else {
        // Notifications: fan out to every module that consumes them.
        if method == "event/notify" {
            if parts.on("checkpoint") {
                let _ = checkpoint::handle_checkpoint(req, ask);
            }
            if parts.on("autocommit") {
                let _ = autocommit::handle_autocommit(req, turn);
            }
        }
        return (None, method == "plugin/shutdown");
    };
    let result = match method {
        "plugin/manifest" => manifest(parts),
        "agent/before_start" => {
            if !parts.on("checkpoint") {
                return (Some(json!({ "id": id, "result": {} })), false);
            }
            let (reply, _) = checkpoint::handle_checkpoint(req, ask);
            return (reply, false);
        }
        "tool/call" => {
            let name = params.get("name").and_then(Value::as_str).unwrap_or("");
            if name == "git_conflicts" && parts.on("git-merge") {
                let (reply, _) = gitmerge::handle_gitmerge(req);
                return (reply, false);
            }
            json!({ "content": format!("unknown tool: {name}"), "is_error": true })
        }
        "command/run" => {
            let name = params.get("name").and_then(Value::as_str).unwrap_or("");
            match name {
                "/git" => json!({ "text": git_overview(parts) }),
                "/checkpoint" if parts.on("checkpoint") => {
                    unwrap_result(checkpoint::handle_checkpoint(req, ask).0)
                }
                "/autocommit" if parts.on("autocommit") => {
                    unwrap_result(autocommit::handle_autocommit(req, turn).0)
                }
                "/merge" if parts.on("git-merge") => {
                    unwrap_result(gitmerge::handle_gitmerge(req).0)
                }
                "/review" | "/simplify" if parts.on("review") => {
                    unwrap_result(review::handle_review(req).0)
                }
                _ => json!({ "text": format!("unknown command {name} — try /git") }),
            }
        }
        "plugin/shutdown" => return (Some(json!({ "id": id, "result": {} })), true),
        _ => {
            let error = json!({ "code": -32601, "message": "method not found" });
            return (Some(json!({ "id": id, "error": error })), false);
        }
    };
    (Some(json!({ "id": id, "result": result })), false)
}

fn main() -> std::io::Result<()> {
    let parts = parts::Parts::load("git", PARTS);
    if std::env::args().nth(1).as_deref() == Some("manifest") {
        println!("{}", manifest(&parts));
        return Ok(());
    }
    let stdout = Arc::new(Mutex::new(std::io::stdout()));
    let pending: checkpoint::Pending = Arc::new(Mutex::new(HashMap::new()));
    let counter = Mutex::new(0u64);
    let mut turn = autocommit::Turn::default();

    // Reader thread: host answers (string ids, no method) go to `pending`;
    // everything else — requests and notifications — goes to the work loop.
    let (work_tx, work_rx) = mpsc::channel::<Value>();
    let reader_pending = pending.clone();
    let _reader = std::thread::spawn(move || {
        let stdin = std::io::stdin();
        for line in stdin.lock().lines() {
            let Ok(line) = line else { break };
            let Ok(v) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if let Some(id) = v.get("id").and_then(Value::as_str)
                && v.get("method").is_none()
                && let Some(tx) = reader_pending.lock().expect("pending").remove(id)
            {
                let _ = tx.send(v.get("result").cloned().unwrap_or(Value::Null));
                continue;
            }
            if work_tx.send(v).is_err() {
                break;
            }
        }
    });

    for req in work_rx {
        let mut asker =
            |questions: &[Value]| checkpoint::ask_host(&stdout, &pending, &counter, questions);
        let (reply, exit) = handle(&req, &mut turn, &mut asker, &parts);
        if let Some(reply) = reply {
            let mut o = stdout.lock().expect("stdout");
            writeln!(o, "{reply}")?;
            o.flush()?;
        }
        if exit {
            break;
        }
    }
    Ok(())
}
