//! gray-claude-mem — auto-captured session memory for gray.
//!
//! Behavioral port of thedotmack/claude-mem (Apache-2.0), deliberately
//! scoped to the delta over gray-memory (which already covers *curated*
//! memory): this plugin auto-captures. It watches `input/submit` for the
//! user's asks and `event/notify` post_tool for tool activity; on
//! `turn_end` it writes one compact entry (~200 chars: last ask + what was
//! done) to `~/.gray/claude-mem/<project-hash>.jsonl`. `prompt/context`
//! injects the last 5 entries for the session's cwd; `mem_search` and
//! `/mem search` run a substring+token scan over the jsonl.
//!
//! Extraction is heuristic — no LLM in the hot path. If the `host.turn`
//! capability is granted, `host/run` rewrites the entry into prose; every
//! failure falls back to the heuristic text (never crashes, never eats
//! input).

use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use serde_json::{Value, json};

/// How long we wait for a `host/run` reply (host caps the child turn ~28s).
const RUN_TTL: Duration = Duration::from_secs(20);
const CONTEXT_ENTRIES: usize = 5;

fn manifest() -> Value {
    json!({
        "name": "claude-mem",
        "version": env!("CARGO_PKG_VERSION"),
        "protocol": "2.0",
        "tools": [{
            "name": "mem_search",
            "description": "Search auto-captured session memories (past user asks + what was \
                done) for this project. Substring + token match over the project's memory log. \
                Set `all` to search every project.",
            "parameters": {
                "type": "object",
                "properties": {
                    "query": { "type": "string" },
                    "all": { "type": "boolean", "description": "Search all projects; default false." }
                },
                "required": ["query"]
            }
        }],
        "commands": ["/mem"],
        "hooks": ["input/submit", "prompt/context"],
        "capabilities": ["host.turn"],
    })
}

// --- host link (only used by the optional host.turn compression) --------------

type Pending = Arc<Mutex<HashMap<String, mpsc::Sender<Value>>>>;

struct HostLink {
    out: Arc<Mutex<std::io::Stdout>>,
    pending: Pending,
    counter: Arc<Mutex<u64>>,
}

fn next_id(counter: &Arc<Mutex<u64>>) -> String {
    let mut n = counter.lock().expect("id counter");
    *n += 1;
    format!("q{n}")
}

/// `host/run` one prompt, returning reply text. Every failure is an Err.
fn host_run(link: Option<&HostLink>, prompt: &str) -> Result<String, String> {
    let Some(link) = link else {
        return Err("host/run unavailable".into());
    };
    let id = next_id(&link.counter);
    let (tx, rx) = mpsc::channel();
    link.pending.lock().expect("pending").insert(id.clone(), tx);
    let req = json!({"id": id, "method": "host/run", "params": {"prompt": prompt}});
    {
        let mut o = link.out.lock().expect("stdout");
        if writeln!(o, "{req}").and_then(|_| o.flush()).is_err() {
            link.pending.lock().expect("pending").remove(&id);
            return Err("host/run: wire write failed".into());
        }
    }
    let reply = match rx.recv_timeout(RUN_TTL) {
        Ok(v) => v,
        Err(_) => {
            link.pending.lock().expect("pending").remove(&id);
            return Err("host/run did not answer in time".into());
        }
    };
    if let Some(text) = reply.pointer("/result/text").and_then(Value::as_str) {
        return Ok(text.to_string());
    }
    if let Some(e) = reply.pointer("/error/message").and_then(Value::as_str) {
        return Err(format!("host/run: {e}"));
    }
    Err("host/run: empty reply".into())
}

// --- state --------------------------------------------------------------------

fn state_dir() -> PathBuf {
    let home = std::env::var_os("GRAY_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".gray")))
        .unwrap_or_else(|| PathBuf::from("."));
    home.join("claude-mem")
}

/// Short stable hash of a canonicalized cwd → per-project jsonl file.
fn project_hash(cwd: &str) -> String {
    use std::hash::{Hash, Hasher};
    let canon = std::fs::canonicalize(cwd)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| cwd.trim_end_matches('/').to_string());
    let mut h = std::collections::hash_map::DefaultHasher::new();
    canon.hash(&mut h);
    format!("{:012x}", h.finish())
}

fn project_file(cwd: &str) -> PathBuf {
    state_dir().join(format!("{}.jsonl", project_hash(cwd)))
}

/// epoch secs → "YYYY-MM-DD HH:MM" UTC (no chrono dep).
fn fmt_ts(secs: u64) -> String {
    let days = secs / 86400;
    let rem = secs % 86400;
    // civil-from-days (Howard Hinnant)
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02} {:02}:{:02}", rem / 3600, rem % 3600 / 60)
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    s.chars().take(max).collect::<String>() + "…"
}

#[derive(Default)]
struct SessionBuf {
    last_ask: String,
    cwd: String,
    tools: Vec<(String, u32)>,
    files: Vec<String>,
    active: bool,
}

impl SessionBuf {
    fn note_tool(&mut self, name: &str) {
        self.active = true;
        if let Some(e) = self.tools.iter_mut().find(|(n, _)| n == name) {
            e.1 += 1;
        } else {
            self.tools.push((name.to_string(), 1));
        }
    }

    fn note_file(&mut self, path: &str) {
        self.active = true;
        if !self.files.iter().any(|f| f == path) {
            self.files.push(path.to_string());
            if self.files.len() > 8 {
                self.files.remove(0);
            }
        }
    }

    /// One-line heuristic summary, ~200 chars.
    fn heuristic(&self) -> String {
        let tools = self
            .tools
            .iter()
            .map(|(n, c)| if *c > 1 { format!("{n}×{c}") } else { n.clone() })
            .collect::<Vec<_>>()
            .join(" ");
        let files = self
            .files
            .iter()
            .take(4)
            .map(|f| f.rsplit('/').next().unwrap_or(f))
            .collect::<Vec<_>>()
            .join(",");
        let did = if files.is_empty() {
            tools
        } else {
            format!("{tools} · {files}")
        };
        format!("asked: {} | did: {}", truncate_chars(&self.last_ask, 110), truncate_chars(&did, 80))
    }
}

type Sessions = Arc<Mutex<HashMap<String, SessionBuf>>>;

fn append_entry(cwd: &str, text: &str) -> Result<(), String> {
    let dir = state_dir();
    std::fs::create_dir_all(&dir).map_err(|e| format!("mkdir: {e}"))?;
    let file = project_file(cwd);
    let entry = json!({
        "ts": fmt_ts(now_secs()),
        "cwd": cwd,
        "m": text,
    });
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&file)
        .map_err(|e| format!("open {}: {e}", file.display()))?;
    writeln!(f, "{entry}").map_err(|e| format!("append: {e}"))
}

fn read_entries(cwd: &str) -> Vec<Value> {
    std::fs::read_to_string(project_file(cwd))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

/// substring + token scan over one or all jsonl files.
fn search(cwd: &str, query: &str, all: bool) -> Vec<String> {
    let q = query.to_lowercase();
    let tokens: Vec<&str> = q.split_whitespace().collect();
    let mut hits = Vec::new();
    let mut scan_file = |path: PathBuf, tag: &str| {
        let Ok(content) = std::fs::read_to_string(&path) else { return };
        for line in content.lines() {
            let low = line.to_lowercase();
            if low.contains(&q) || (!tokens.is_empty() && tokens.iter().all(|t| low.contains(t))) {
                let short = serde_json::from_str::<Value>(line)
                    .map(|v| {
                        format!(
                            "[{}] {}{}",
                            v["ts"].as_str().unwrap_or("?"),
                            v["m"].as_str().unwrap_or(line),
                            if all { format!(" ({tag})") } else { String::new() }
                        )
                    })
                    .unwrap_or_else(|_| line.to_string());
                hits.push(short);
            }
        }
    };
    if all {
        if let Ok(rd) = std::fs::read_dir(state_dir()) {
            for e in rd.flatten() {
                let p = e.path();
                if p.extension().is_some_and(|x| x == "jsonl") {
                    scan_file(p, &e.file_name().to_string_lossy().replace(".jsonl", ""));
                }
            }
        }
    } else {
        scan_file(project_file(cwd), "");
    }
    hits.truncate(20);
    hits
}

// --- wire handlers ------------------------------------------------------------

/// input/submit — capture the user ask for the session buffer; always pass.
fn on_input(params: &Value, sessions: &Sessions) {
    let Some(sid) = params.pointer("/session/id").and_then(Value::as_str) else { return };
    let text = params.get("text").and_then(Value::as_str).unwrap_or("").trim();
    if text.is_empty() || text.starts_with('/') {
        return;
    }
    let mut map = sessions.lock().expect("sessions");
    let buf = map.entry(sid.to_string()).or_default();
    buf.last_ask = truncate_chars(text, 400);
    buf.active = true;
    if let Some(cwd) = params.pointer("/session/cwd").and_then(Value::as_str) {
        buf.cwd = cwd.to_string();
    }
}

/// event/notify — post_tool accumulates activity; turn_end flushes.
fn on_event(params: &Value, sessions: &Sessions, link: Option<&HostLink>) {
    match params.get("type").and_then(Value::as_str) {
        Some("post_tool") | Some("pre_tool") => {
            let Some(sid) = params.pointer("/session/id").and_then(Value::as_str) else { return };
            let mut map = sessions.lock().expect("sessions");
            let buf = map.entry(sid.to_string()).or_default();
            if let Some(name) = params.get("name").and_then(Value::as_str) {
                buf.note_tool(name);
            }
            if let Some(args) = params.get("args") {
                for key in ["path", "file_path", "filename", "notebook_path"] {
                    if let Some(p) = args.get(key).and_then(Value::as_str) {
                        buf.note_file(p);
                    }
                }
            }
            if let Some(cwd) = params.pointer("/session/cwd").and_then(Value::as_str) {
                buf.cwd = cwd.to_string();
            }
        }
        Some("turn_end") => {
            let Some(sid) = params.pointer("/session/id").and_then(Value::as_str) else { return };
            let cwd = params
                .pointer("/session/cwd")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let buf = {
                let mut map = sessions.lock().expect("sessions");
                map.remove(sid)
            };
            let Some(buf) = buf else { return };
            let cwd = if cwd.is_empty() { buf.cwd.clone() } else { cwd };
            if !buf.active || cwd.is_empty() {
                return;
            }
            // Optional LLM compression; any failure falls back to heuristic.
            let text = host_run(
                link,
                &format!(
                    "Compress this coding-session activity into ONE memory line under 180 \
                    chars, plain text, no preamble. Shape: \"<what user wanted> → <what was \
                    done>\".\n\n{}\n",
                    buf.heuristic()
                ),
            )
            .map(|t| truncate_chars(t.trim().lines().next().unwrap_or(""), 200))
            .unwrap_or_else(|_| truncate_chars(&buf.heuristic(), 200));
            let _ = append_entry(&cwd, &text);
        }
        _ => {}
    }
}

/// prompt/context — last N entries for this session's project.
fn prompt_context(params: &Value) -> Option<String> {
    let cwd = params.pointer("/session/cwd").and_then(Value::as_str)?;
    let entries = read_entries(cwd);
    if entries.is_empty() {
        return None;
    }
    let mut s = String::from("claude-mem — recent sessions here:\n");
    for e in entries.iter().rev().take(CONTEXT_ENTRIES) {
        s.push_str(&format!(
            "- [{}] {}\n",
            e["ts"].as_str().unwrap_or("?"),
            e["m"].as_str().unwrap_or("")
        ));
    }
    Some(s.trim_end().to_string())
}

fn tool_cwd() -> String {
    std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| ".".into())
}

fn call_tool(name: &str, args: &Value) -> Result<String, String> {
    match name {
        "mem_search" => {
            let q = args.get("query").and_then(Value::as_str).unwrap_or("").trim();
            if q.is_empty() {
                return Err("missing required argument: query".into());
            }
            let all = args.get("all").and_then(Value::as_bool).unwrap_or(false);
            let hits = search(&tool_cwd(), q, all);
            Ok(if hits.is_empty() { "no memory entries match".into() } else { hits.join("\n") })
        }
        other => Err(format!("unknown tool: {other}")),
    }
}

fn run_command(argv: &[&str], session: Option<&Value>) -> String {
    let cwd = session
        .and_then(|s| s.get("cwd"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(tool_cwd);
    match argv.first().copied() {
        Some("list") => {
            let entries = read_entries(&cwd);
            if entries.is_empty() {
                "no memories for this project yet".into()
            } else {
                entries
                    .iter()
                    .rev()
                    .take(10)
                    .map(|e| format!("[{}] {}", e["ts"].as_str().unwrap_or("?"), e["m"].as_str().unwrap_or("")))
                    .collect::<Vec<_>>()
                    .join("\n")
            }
        }
        Some("clear") => match std::fs::remove_file(project_file(&cwd)) {
            Ok(()) => "cleared this project's claude-mem log".into(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => "nothing to clear".into(),
            Err(e) => format!("couldn't clear: {e}"),
        },
        Some("search") => {
            let q = argv[1..].join(" ");
            if q.trim().is_empty() {
                return "usage: /mem search <query>".into();
            }
            let hits = search(&cwd, &q, false);
            if hits.is_empty() { "no memory entries match".into() } else { hits.join("\n") }
        }
        _ => format!(
            "gray-claude-mem {} — auto-captures a one-line session memory on each \
            turn_end → ~/.gray/claude-mem/<project>.jsonl\n\
            /mem list · /mem search <q> · /mem clear",
            env!("CARGO_PKG_VERSION")
        ),
    }
}

/// One request → `Some(reply)`, or `None` for notifications. The bool asks
/// the loop to exit after writing the reply.
fn handle(req: &Value, sessions: &Sessions, link: Option<&HostLink>) -> (Option<Value>, bool) {
    let id = req.get("id").cloned();
    let method = req.get("method").and_then(Value::as_str).unwrap_or("");
    let params = req.get("params").cloned().unwrap_or(Value::Null);
    let Some(id) = id else {
        if method == "event/notify" {
            on_event(&params, sessions, link);
        }
        return (None, method == "plugin/shutdown");
    };
    let result = match method {
        "plugin/manifest" => manifest(),
        "input/submit" => {
            on_input(&params, sessions);
            json!({}) // always pass — never rewrite or swallow user input
        }
        "prompt/context" => match prompt_context(&params) {
            Some(text) => json!({ "text": text }),
            None => json!({}),
        },
        "tool/call" => {
            let name = params.get("name").and_then(Value::as_str).unwrap_or("");
            let args = params.get("args").cloned().unwrap_or(Value::Null);
            match call_tool(name, &args) {
                Ok(text) => json!({ "content": text }),
                Err(text) => json!({ "content": text, "is_error": true }),
            }
        }
        "command/run" => {
            let argv: Vec<&str> = params
                .get("argv")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            json!({ "text": run_command(&argv, params.get("session")) })
        }
        "plugin/shutdown" => return (Some(json!({ "id": id, "result": {} })), true),
        _ => {
            let error = json!({ "code": -32601, "message": "method not found" });
            return (Some(json!({ "id": id, "error": error })), false);
        }
    };
    (Some(json!({ "id": id, "result": result })), false)
}

fn main() {
    if std::env::args().nth(1).as_deref() == Some("manifest") {
        println!("{}", manifest());
        return;
    }
    let stdout = Arc::new(Mutex::new(std::io::stdout()));
    let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
    let counter = Arc::new(Mutex::new(0u64));
    let sessions: Sessions = Arc::new(Mutex::new(HashMap::new()));

    // Reader thread: host/run replies (string id, no method) route to
    // pending waiters; everything else queues for the main loop.
    let (work_tx, work_rx) = mpsc::channel::<Value>();
    let reader_pending = pending.clone();
    std::thread::spawn(move || {
        let stdin = std::io::stdin();
        for line in stdin.lock().lines() {
            let Ok(line) = line else { break };
            let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
            if let Some(id) = v.get("id").and_then(|i| i.as_str())
                && v.get("method").is_none()
                && let Some(tx) = reader_pending.lock().expect("pending").remove(id)
            {
                let _ = tx.send(v);
                continue;
            }
            if work_tx.send(v).is_err() {
                break;
            }
        }
    });

    for req in work_rx {
        let link = HostLink {
            out: stdout.clone(),
            pending: pending.clone(),
            counter: counter.clone(),
        };
        let (reply, exit) = handle(&req, &sessions, Some(&link));
        if let Some(reply) = reply {
            let mut o = stdout.lock().expect("stdout");
            let _ = writeln!(o, "{reply}");
            let _ = o.flush();
        }
        if exit {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// GRAY_HOME is process-global — env-touching tests run serialized.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn call(sessions: &Sessions, method: &str, params: Value) -> Value {
        handle(&json!({ "id": 1, "method": method, "params": params }), sessions, None)
            .0
            .unwrap()
    }

    #[test]
    fn manifest_claims_hooks_and_capability() {
        let s: Sessions = Arc::new(Mutex::new(HashMap::new()));
        let m = call(&s, "plugin/manifest", Value::Null)["result"].clone();
        assert_eq!(m["name"], "claude-mem");
        assert_eq!(m["protocol"], "2.0");
        assert_eq!(m["hooks"], json!(["input/submit", "prompt/context"]));
        assert_eq!(m["capabilities"], json!(["host.turn"]));
        assert_eq!(m["tools"][0]["name"], "mem_search");
    }

    #[test]
    fn input_submit_passes_and_captures() {
        let s: Sessions = Arc::new(Mutex::new(HashMap::new()));
        let r = call(&s, "input/submit", json!({
            "text": "fix the login bug", "session": {"id": "s1", "cwd": "/tmp"}
        }));
        assert_eq!(r["result"], json!({})); // pass-through
        assert_eq!(s.lock().unwrap()["s1"].last_ask, "fix the login bug");
    }

    #[test]
    fn heuristic_entry_is_compact() {
        let mut b = SessionBuf::default();
        b.last_ask = "refactor the store layer".into();
        b.note_tool("edit");
        b.note_tool("edit");
        b.note_tool("bash");
        b.note_file("/x/src/store.rs");
        let h = b.heuristic();
        assert!(h.contains("refactor the store layer") && h.contains("edit×2") && h.contains("store.rs"));
        assert!(h.len() <= 220, "{h}");
    }

    #[test]
    fn turn_end_appends_and_resets() {
        let _g = ENV_LOCK.lock().unwrap();
        let s: Sessions = Arc::new(Mutex::new(HashMap::new()));
        let tmp = std::env::temp_dir().join(format!("cm-test-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        // isolate state via GRAY_HOME
        unsafe { std::env::set_var("GRAY_HOME", &tmp) };
        let cwd = tmp.join("proj");
        std::fs::create_dir_all(&cwd).unwrap();
        let cwds = cwd.to_str().unwrap().to_string();
        call(&s, "input/submit", json!({
            "text": "add tests", "session": {"id": "a", "cwd": cwds}}));
        handle(&json!({"method":"event/notify","params":{
            "type":"post_tool","name":"write","args":{"path":format!("{cwds}/x.rs")},
            "session":{"id":"a","cwd":cwds}}}), &s, None);
        let (r, _) = handle(&json!({"method":"event/notify","params":{
            "type":"turn_end","usage":{},"session":{"id":"a","cwd":cwds}}}), &s, None);
        assert!(r.is_none());
        // no host link → heuristic fallback must still write
        let entries = read_entries(&cwds);
        assert_eq!(entries.len(), 1);
        assert!(entries[0]["m"].as_str().unwrap().contains("add tests"));
        // buffer flushed: a second turn_end writes nothing
        handle(&json!({"method":"event/notify","params":{
            "type":"turn_end","usage":{},"session":{"id":"a","cwd":cwds}}}), &s, None);
        assert_eq!(read_entries(&cwds).len(), 1);
        unsafe { std::env::remove_var("GRAY_HOME") };
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn context_returns_last_entries() {
        let _g = ENV_LOCK.lock().unwrap();
        let s: Sessions = Arc::new(Mutex::new(HashMap::new()));
        let tmp = std::env::temp_dir().join(format!("cm-ctx-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        unsafe { std::env::set_var("GRAY_HOME", &tmp) };
        let cwds = tmp.to_str().unwrap().to_string();
        append_entry(&cwds, "older memory").unwrap();
        append_entry(&cwds, "newer memory").unwrap();
        let r = call(&s, "prompt/context", json!({"session": {"cwd": cwds}}));
        let t = r["result"]["text"].as_str().unwrap();
        assert!(t.contains("older memory") && t.contains("newer memory"));
        unsafe { std::env::remove_var("GRAY_HOME") };
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn mem_search_finds_substring() {
        let _g = ENV_LOCK.lock().unwrap();
        let tmp = std::env::temp_dir().join(format!("cm-search-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        unsafe { std::env::set_var("GRAY_HOME", &tmp) };
        let cwds = tmp.to_str().unwrap().to_string();
        append_entry(&cwds, "asked: fix login | did: edit×2").unwrap();
        let hits = search(&cwds, "login", false);
        assert_eq!(hits.len(), 1);
        assert!(search(&cwds, "nomatch-token", false).is_empty());
        unsafe { std::env::remove_var("GRAY_HOME") };
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
