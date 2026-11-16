//! End-to-end tests. Built without the libtest harness so this one binary can
//! also act as the MCP server being wrapped (`--mock-server`), which keeps
//! the suite free of any external runtime.

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

/// What Claude Code 2.1.325 sends to a server that speaks the
/// initialize-based protocol, and to one that speaks the stateless one.
const CLASSIC: &str = include_str!("fixtures/claude-code-2.1.325/classic.jsonl");
const STATELESS: &str = include_str!("fixtures/claude-code-2.1.325/stateless.jsonl");

const PARK_AFTER: &str = "0.3";
const PROBE_ID: &str = "server-discover-probe-1";

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("--mock-server") {
        mock_server();
        return;
    }
    let filter = args.iter().skip(1).find(|arg| !arg.starts_with('-'));
    let tests: &[(&str, fn())] = &[
        (
            "parks_then_wakes_and_stays_awake",
            parks_then_wakes_and_stays_awake,
        ),
        (
            "listing_while_parked_is_live_then_reparks",
            listing_while_parked_is_live_then_reparks,
        ),
        (
            "stateless_handshake_parks_and_replays_listen",
            stateless_handshake_parks_and_replays_listen,
        ),
        (
            "ping_while_parked_is_answered_locally",
            ping_while_parked_is_answered_locally,
        ),
        (
            "benign_notifications_do_not_wake",
            benign_notifications_do_not_wake,
        ),
        (
            "unknown_method_wakes_for_good",
            unknown_method_wakes_for_good,
        ),
        ("slow_listing_is_not_cut_off", slow_listing_is_not_cut_off),
        (
            "server_request_blocks_park_until_answered",
            server_request_blocks_park_until_answered,
        ),
        (
            "stubborn_server_is_killed_on_park",
            stubborn_server_is_killed_on_park,
        ),
        (
            "launcher_failure_exits_without_a_reply",
            launcher_failure_exits_without_a_reply,
        ),
        (
            "server_crash_takes_the_wrapper_down",
            server_crash_takes_the_wrapper_down,
        ),
        ("sigterm_stops_a_live_server", sigterm_stops_a_live_server),
        (
            "sigkill_leaves_no_live_server",
            sigkill_leaves_no_live_server,
        ),
        (
            "listing_bytes_match_unwrapped_classic",
            listing_bytes_match_unwrapped_classic,
        ),
        (
            "listing_bytes_match_unwrapped_stateless",
            listing_bytes_match_unwrapped_stateless,
        ),
    ];
    let running: Vec<_> = tests
        .iter()
        .filter(|(name, _)| filter.is_none_or(|wanted| name.contains(wanted.as_str())))
        .map(|(name, test)| {
            let handle = thread::Builder::new()
                .name(name.to_string())
                .spawn(*test)
                .unwrap();
            (name, handle)
        })
        .collect();
    let mut failed = 0;
    for (name, handle) in running {
        match handle.join() {
            Ok(()) => println!("ok    {name}"),
            Err(_) => {
                failed += 1;
                println!("FAIL  {name}");
            }
        }
    }
    println!("{} failed", failed);
    std::process::exit(if failed == 0 { 0 } else { 1 });
}

// ---------------------------------------------------------------- tests

fn parks_then_wakes_and_stays_awake() {
    let ctx = Ctx::new("wake");
    let mut session = ctx.wrapped();
    let tools = session.handshake(CLASSIC);
    assert_eq!(tools, ["alpha", "beta"]);
    assert_eq!(ctx.starts(), 1);
    ctx.wait_parked();

    session.send(&call(2, "alpha"));
    assert_eq!(
        session.reply(json!(2))["result"]["content"][0]["text"],
        "called alpha"
    );
    assert_eq!(ctx.starts(), 2);
    assert_eq!(
        ctx.received_by_last_server(),
        ["initialize", "notifications/initialized", "tools/call"],
        "the restarted server must see the handshake before the call"
    );

    thread::sleep(Duration::from_millis(1000));
    assert!(
        ctx.server_alive(),
        "a server that took a real call must not be parked"
    );
    session.send(&call(3, "beta"));
    assert_eq!(
        session.reply(json!(3))["result"]["content"][0]["text"],
        "called beta"
    );
    assert_eq!(ctx.starts(), 2);

    let initialize_replies = session.seen.iter().filter(|m| m["id"] == json!(0)).count();
    assert_eq!(
        initialize_replies, 1,
        "the replayed initialize reply must not reach the client"
    );

    assert!(session.close().success());
    ctx.wait_server_gone();
}

fn listing_while_parked_is_live_then_reparks() {
    let ctx = Ctx::new("relist");
    let mut session = ctx.wrapped();
    session.handshake(CLASSIC);
    ctx.wait_parked();

    ctx.set_tools(&["alpha", "beta", "gamma"]);
    session.send(r#"{"method":"tools/list","jsonrpc":"2.0","id":5}"#);
    assert_eq!(
        tool_names(&session.reply(json!(5))),
        ["alpha", "beta", "gamma"]
    );
    assert_eq!(ctx.starts(), 2);
    ctx.wait_parked();

    session.send(&call(6, "gamma"));
    assert_eq!(
        session.reply(json!(6))["result"]["content"][0]["text"],
        "called gamma"
    );
    assert_eq!(ctx.starts(), 3);
    session.close();
}

fn stateless_handshake_parks_and_replays_listen() {
    let ctx = Ctx::new("stateless").env("MOCK_MODE", "stateless");
    let mut session = ctx.wrapped();
    assert_eq!(session.handshake(STATELESS), ["alpha", "beta"]);
    ctx.wait_parked();

    session.send(&call(2, "alpha"));
    assert_eq!(
        session.reply(json!(2))["result"]["content"][0]["text"],
        "called alpha"
    );
    assert_eq!(ctx.starts(), 2);
    assert_eq!(
        ctx.received_by_last_server(),
        ["subscriptions/listen", "tools/call"]
    );

    session.drain(Duration::from_millis(500));
    let acks = session
        .seen
        .iter()
        .filter(|m| m["method"] == "notifications/subscriptions/acknowledged")
        .count();
    assert_eq!(
        acks, 1,
        "the replayed subscription must not be acknowledged twice"
    );
    assert!(ctx.server_alive());
    session.close();
}

fn ping_while_parked_is_answered_locally() {
    let ctx = Ctx::new("ping");
    let mut session = ctx.wrapped();
    session.handshake(CLASSIC);
    ctx.wait_parked();
    session.send(r#"{"jsonrpc":"2.0","id":9,"method":"ping"}"#);
    assert_eq!(session.reply(json!(9))["result"], json!({}));
    assert_eq!(ctx.starts(), 1);
    session.close();
}

fn benign_notifications_do_not_wake() {
    let ctx = Ctx::new("benign");
    let mut session = ctx.wrapped();
    session.handshake(CLASSIC);
    ctx.wait_parked();
    session
        .send(r#"{"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":1}}"#);
    session.send(r#"{"jsonrpc":"2.0","method":"notifications/roots/list_changed"}"#);
    thread::sleep(Duration::from_millis(600));
    assert_eq!(ctx.starts(), 1);
    session.close();
}

fn unknown_method_wakes_for_good() {
    let ctx = Ctx::new("unknown");
    let mut session = ctx.wrapped();
    session.handshake(CLASSIC);
    ctx.wait_parked();
    session.send(r#"{"jsonrpc":"2.0","id":7,"method":"frobnicate/now"}"#);
    assert_eq!(session.reply(json!(7))["error"]["code"], -32601);
    thread::sleep(Duration::from_millis(1000));
    assert!(ctx.server_alive());
    session.close();
}

fn slow_listing_is_not_cut_off() {
    let ctx = Ctx::new("slow").env("MOCK_LIST_DELAY_MS", "1200");
    let mut session = ctx.wrapped();
    assert_eq!(session.handshake(CLASSIC), ["alpha", "beta"]);
    assert_eq!(ctx.starts(), 1);
    session.close();
}

fn server_request_blocks_park_until_answered() {
    let ctx = Ctx::new("roots").env("MOCK_ROOTS", "1");
    let mut session = ctx.wrapped();
    session.handshake(CLASSIC);
    let request = session.message_where(|m| m["method"] == "roots/list");
    assert_eq!(request["id"], "srv-1");

    thread::sleep(Duration::from_millis(1000));
    assert!(
        ctx.server_alive(),
        "must not park while the server awaits the client"
    );
    assert!(ctx
        .park_log()
        .contains("park blocked by unanswered server request"));

    session.send(r#"{"jsonrpc":"2.0","id":"srv-1","result":{"roots":[]}}"#);
    ctx.wait_parked();
    assert!(ctx
        .received()
        .iter()
        .any(|line| line.ends_with(r#"reply "srv-1""#)));
    session.close();
}

fn stubborn_server_is_killed_on_park() {
    let ctx = Ctx::new("stubborn")
        .env("MOCK_STUBBORN", "1")
        .env("MCP_PARK_STOP_GRACE", "0.3");
    let mut session = ctx.wrapped();
    session.handshake(CLASSIC);
    ctx.wait_parked();
    session.close();
}

fn launcher_failure_exits_without_a_reply() {
    let ctx = Ctx::new("fail").env("MOCK_FAIL", "1");
    let mut session = ctx.wrapped();
    for line in CLASSIC.lines() {
        session.send(line);
    }
    let status = session
        .wait_exit(Duration::from_secs(5))
        .expect("wrapper must not hang");
    assert_eq!(status.code(), Some(1));
    assert!(
        session.drain(Duration::from_millis(200)).is_empty(),
        "no reply may be invented"
    );
}

fn server_crash_takes_the_wrapper_down() {
    let ctx = Ctx::new("crash").env("MOCK_CRASH_ON_CALL", "1");
    let mut session = ctx.wrapped();
    session.handshake(CLASSIC);
    session.send(&call(2, "alpha"));
    let status = session
        .wait_exit(Duration::from_secs(5))
        .expect("wrapper must not hang");
    assert_eq!(status.code(), Some(3));
    assert!(session.seen.iter().all(|m| m["id"] != json!(2)));
}

fn sigterm_stops_a_live_server() {
    wrapper_signal_leaves_no_server("term", libc::SIGTERM);
}

fn sigkill_leaves_no_live_server() {
    wrapper_signal_leaves_no_server("kill", libc::SIGKILL);
}

fn wrapper_signal_leaves_no_server(name: &str, signal: i32) {
    let ctx = Ctx::new(name);
    let mut session = ctx.wrapped();
    session.handshake(CLASSIC);
    session.send(&call(2, "alpha"));
    session.reply(json!(2));
    assert!(ctx.server_alive());
    unsafe { libc::kill(session.child.id() as i32, signal) };
    session
        .wait_exit(Duration::from_secs(5))
        .expect("wrapper must exit");
    ctx.wait_server_gone();
}

fn listing_bytes_match_unwrapped_classic() {
    listing_bytes_match_unwrapped("bytes-classic", "classic", CLASSIC);
}

fn listing_bytes_match_unwrapped_stateless() {
    listing_bytes_match_unwrapped("bytes-stateless", "stateless", STATELESS);
}

fn listing_bytes_match_unwrapped(name: &str, mode: &str, handshake: &str) {
    let ctx = Ctx::new(name).env("MOCK_MODE", mode);
    let mut direct = ctx.unwrapped();
    direct.handshake(handshake);
    let mut wrapped = ctx.wrapped();
    wrapped.handshake(handshake);
    assert_eq!(direct.raw, wrapped.raw);
    direct.close();
    wrapped.close();
}

// ---------------------------------------------------------------- client side

fn call(id: u64, tool: &str) -> String {
    json!({"method":"tools/call","params":{"name":tool,"arguments":{}},"jsonrpc":"2.0","id":id})
        .to_string()
}

fn tool_names(reply: &Value) -> Vec<String> {
    reply["result"]["tools"]
        .as_array()
        .expect("tools/list result")
        .iter()
        .map(|tool| tool["name"].as_str().unwrap().to_string())
        .collect()
}

fn wait_until(what: &str, timeout: Duration, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting until {what}");
        thread::sleep(Duration::from_millis(20));
    }
}

struct Ctx {
    dir: PathBuf,
    env: Vec<(String, String)>,
}

impl Ctx {
    fn new(name: &str) -> Ctx {
        let dir = std::env::temp_dir().join(format!("mcp-park-test-{}-{name}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = |file: &str| dir.join(file).to_string_lossy().into_owned();
        let env = vec![
            ("MOCK_TOOLS_FILE".to_string(), path("tools")),
            ("MOCK_STARTS_LOG".to_string(), path("starts")),
            ("MOCK_RECV_LOG".to_string(), path("received")),
            ("MCP_PARK_LOG".to_string(), path("park.log")),
        ];
        let ctx = Ctx { dir, env };
        ctx.set_tools(&["alpha", "beta"]);
        ctx
    }

    fn env(mut self, key: &str, value: &str) -> Ctx {
        self.env.push((key.to_string(), value.to_string()));
        self
    }

    fn set_tools(&self, names: &[&str]) {
        fs::write(self.dir.join("tools"), names.join("\n")).unwrap();
    }

    fn read(&self, file: &str) -> String {
        fs::read_to_string(self.dir.join(file)).unwrap_or_default()
    }

    fn server_pids(&self) -> Vec<i32> {
        self.read("starts")
            .lines()
            .map(|pid| pid.parse().unwrap())
            .collect()
    }

    fn starts(&self) -> usize {
        self.server_pids().len()
    }

    fn server_alive(&self) -> bool {
        let pid = *self.server_pids().last().expect("a server was started");
        unsafe { libc::kill(pid, 0) == 0 }
    }

    fn wait_server_gone(&self) {
        wait_until("the server is gone", Duration::from_secs(5), || {
            !self.server_alive()
        });
    }

    fn wait_parked(&self) {
        self.wait_server_gone();
        wait_until("the park is logged", Duration::from_secs(2), || {
            self.park_log()
                .lines()
                .filter(|line| line.ends_with("parked"))
                .count()
                == self.starts()
        });
    }

    fn park_log(&self) -> String {
        self.read("park.log")
    }

    /// Lines of the form `<pid> <method>`, in arrival order.
    fn received(&self) -> Vec<String> {
        self.read("received").lines().map(str::to_string).collect()
    }

    fn received_by_last_server(&self) -> Vec<String> {
        let prefix = format!("{} ", self.server_pids().last().unwrap());
        self.received()
            .iter()
            .filter_map(|line| line.strip_prefix(&prefix).map(str::to_string))
            .collect()
    }

    fn mock_command(&self) -> Vec<String> {
        vec![
            std::env::current_exe()
                .unwrap()
                .to_string_lossy()
                .into_owned(),
            "--mock-server".into(),
        ]
    }

    fn wrapped(&self) -> Session {
        let mut command = Command::new(env!("CARGO_BIN_EXE_mcp-park"));
        command
            .args(["--park-after", PARK_AFTER, "--"])
            .args(self.mock_command());
        self.spawn(command)
    }

    fn unwrapped(&self) -> Session {
        let mock = self.mock_command();
        let mut command = Command::new(&mock[0]);
        command.args(&mock[1..]);
        self.spawn(command)
    }

    fn spawn(&self, mut command: Command) -> Session {
        command
            .envs(self.env.iter().cloned())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = command.spawn().unwrap();
        let stdin = child.stdin.take();
        let stdout = child.stdout.take().unwrap();
        let (lines, inbox) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if lines.send(line).is_err() {
                    break;
                }
            }
        });
        Session {
            child,
            stdin,
            inbox,
            raw: Vec::new(),
            seen: Vec::new(),
        }
    }
}

impl Drop for Ctx {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

struct Session {
    child: Child,
    stdin: Option<ChildStdin>,
    inbox: Receiver<String>,
    raw: Vec<String>,
    seen: Vec<Value>,
}

impl Session {
    fn send(&mut self, line: &str) {
        let stdin = self.stdin.as_mut().unwrap();
        let _ = writeln!(stdin, "{line}");
    }

    fn next(&mut self, timeout: Duration) -> Option<Value> {
        let line = self.inbox.recv_timeout(timeout).ok()?;
        let message: Value = serde_json::from_str(&line).expect("server output is JSON");
        self.raw.push(line);
        self.seen.push(message.clone());
        Some(message)
    }

    fn message_where(&mut self, wanted: impl Fn(&Value) -> bool) -> Value {
        if let Some(found) = self.seen.iter().find(|m| wanted(m)) {
            return found.clone();
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let message = self.next(left).expect("timed out waiting for a message");
            if wanted(&message) {
                return message;
            }
        }
    }

    fn reply(&mut self, id: Value) -> Value {
        self.message_where(|m| m["id"] == id && m.get("method").is_none())
    }

    fn drain(&mut self, duration: Duration) -> Vec<Value> {
        let deadline = Instant::now() + duration;
        let mut drained = Vec::new();
        while let Some(message) = self.next(deadline.saturating_duration_since(Instant::now())) {
            drained.push(message);
        }
        drained
    }

    /// Sends a captured handshake and returns the tool names it listed.
    fn handshake(&mut self, lines: &str) -> Vec<String> {
        for line in lines.lines() {
            self.send(line);
        }
        self.reply(json!(PROBE_ID));
        tool_names(&self.reply(json!(1)))
    }

    fn wait_exit(&mut self, timeout: Duration) -> Option<ExitStatus> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return Some(status);
            }
            if Instant::now() >= deadline {
                return None;
            }
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn close(&mut self) -> ExitStatus {
        self.stdin = None;
        self.wait_exit(Duration::from_secs(10))
            .expect("exits once its input closes")
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// ---------------------------------------------------------------- server side

/// A strict MCP server: it refuses listings and calls that arrive before the
/// handshake its protocol requires, so a wrapper that restarts it without
/// replaying that handshake is caught.
fn mock_server() {
    let env = |name: &str| std::env::var(name).ok();
    let flag = |name: &str| env(name).as_deref() == Some("1");
    let pid = std::process::id();
    let append = |file: Option<String>, line: String| {
        if let Some(file) = file {
            let mut file = fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(file)
                .unwrap();
            writeln!(file, "{line}").unwrap();
        }
    };
    append(env("MOCK_STARTS_LOG"), pid.to_string());
    if flag("MOCK_FAIL") {
        std::process::exit(1);
    }
    let stubborn = flag("MOCK_STUBBORN");
    if stubborn {
        unsafe { libc::signal(libc::SIGTERM, libc::SIG_IGN) };
    }
    let stateless = env("MOCK_MODE").as_deref() == Some("stateless");
    let list_delay = env("MOCK_LIST_DELAY_MS")
        .and_then(|ms| ms.parse().ok())
        .unwrap_or(0);

    // Key order matches the TypeScript SDK, which writes the result first.
    let reply =
        |id: &Value, result: Value| println!(r#"{{"result":{result},"jsonrpc":"2.0","id":{id}}}"#);
    let error = |id: &Value, code: i32, message: &str| {
        println!(
            "{}",
            json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message}})
        )
    };
    let tools = || -> Vec<String> {
        let names = fs::read_to_string(env("MOCK_TOOLS_FILE").unwrap()).unwrap();
        names
            .lines()
            .filter(|name| !name.is_empty())
            .map(str::to_string)
            .collect()
    };

    let mut ready = false;
    for line in std::io::stdin().lock().lines().map_while(Result::ok) {
        let Ok(message) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let id = message.get("id").cloned().unwrap_or(Value::Null);
        let Some(method) = message["method"].as_str() else {
            append(env("MOCK_RECV_LOG"), format!("{pid} reply {id}"));
            continue;
        };
        append(env("MOCK_RECV_LOG"), format!("{pid} {method}"));
        match method {
            "server/discover" if stateless => reply(
                &id,
                json!({"supportedVersions":["2026-07-28"],"capabilities":{"tools":{"listChanged":true}}}),
            ),
            "initialize" if !stateless => reply(
                &id,
                json!({
                    "protocolVersion": message["params"]["protocolVersion"],
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": "mock", "version": "1"},
                }),
            ),
            "notifications/initialized" if !stateless => {
                ready = true;
                if flag("MOCK_ROOTS") {
                    println!(r#"{{"jsonrpc":"2.0","id":"srv-1","method":"roots/list"}}"#);
                }
            }
            "subscriptions/listen" if stateless => {
                ready = true;
                println!(
                    "{}",
                    json!({
                        "jsonrpc": "2.0",
                        "method": "notifications/subscriptions/acknowledged",
                        "params": {
                            "notifications": message["params"]["notifications"],
                            "_meta": {"io.modelcontextprotocol/subscriptionId": id},
                        },
                    })
                );
            }
            "tools/list" | "tools/call" if !ready => error(&id, -32002, "handshake not completed"),
            "tools/list" => {
                thread::sleep(Duration::from_millis(list_delay));
                let listed: Vec<Value> = tools()
                    .iter()
                    .map(|name| {
                        json!({
                            "name": name,
                            "description": format!("mock tool {name}"),
                            "inputSchema": {"type":"object","properties":{"id":{"type":"string"}}},
                        })
                    })
                    .collect();
                reply(&id, json!({"tools": listed}));
            }
            "tools/call" => {
                if flag("MOCK_CRASH_ON_CALL") {
                    std::process::exit(3);
                }
                let name = message["params"]["name"].as_str().unwrap_or_default();
                if tools().iter().any(|tool| tool == name) {
                    reply(
                        &id,
                        json!({"content":[{"type":"text","text":format!("called {name}")}]}),
                    );
                } else {
                    error(&id, -32602, "unknown tool");
                }
            }
            _ if !id.is_null() => error(&id, -32601, "Method not found"),
            _ => {}
        }
    }
    if stubborn {
        loop {
            thread::sleep(Duration::from_secs(60));
        }
    }
}
