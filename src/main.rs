use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::process::CommandExt;
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::Value;

const USAGE: &str = "\
mcp-park - keep an MCP stdio server stopped until a real call needs it

Usage:
  mcp-park [--park-after <seconds>] [--] <command> [args...]
  mcp-park --version

The server is started for the handshake and tool listing, stopped once that
goes quiet, and started again (and then left running) on the first call that
is not a handshake or listing message.

Environment:
  MCP_PARK_AFTER       Idle seconds before parking (default 5)
  MCP_PARK_STOP_GRACE  Seconds to wait at each stop step (default 2)
  MCP_PARK_LOG         File to append start/park/wake events to
";

const ACKNOWLEDGED: &str = "notifications/subscriptions/acknowledged";

enum Event {
    Client(Vec<u8>),
    ClientEof,
    Server(u64, Vec<u8>),
    ServerEof(u64),
    Signal(i32),
}

#[derive(Deserialize)]
struct Envelope {
    #[serde(default)]
    id: Option<Value>,
    #[serde(default)]
    method: Option<String>,
    #[serde(default)]
    params: Option<Params>,
}

#[derive(Deserialize)]
struct Params {
    #[serde(rename = "_meta", default)]
    meta: Option<Meta>,
}

#[derive(Deserialize)]
struct Meta {
    #[serde(rename = "io.modelcontextprotocol/subscriptionId", default)]
    subscription_id: Option<Value>,
}

impl Envelope {
    fn parse(line: &[u8]) -> Option<Envelope> {
        serde_json::from_slice(line).ok()
    }

    fn id_key(&self) -> Option<String> {
        self.id.as_ref().map(Value::to_string)
    }

    fn subscription_key(&self) -> Option<String> {
        let id = self
            .params
            .as_ref()?
            .meta
            .as_ref()?
            .subscription_id
            .as_ref()?;
        Some(id.to_string())
    }
}

enum ClientMessage {
    Initialize(String),
    Initialized,
    Listen(String),
    Listing {
        id: String,
        method: String,
    },
    Ping(String),
    /// Notifications that carry nothing a stopped server needs to hear.
    Benign,
    Reply(String),
    /// Anything else, including lines that cannot be parsed: the server must
    /// run for it and stays running afterwards.
    Real {
        id: Option<String>,
        label: String,
    },
}

fn classify(line: &[u8]) -> ClientMessage {
    let Some(envelope) = Envelope::parse(line) else {
        return ClientMessage::Real {
            id: None,
            label: "unparsed message".into(),
        };
    };
    let id = envelope.id_key();
    let Some(method) = envelope.method else {
        return match id {
            Some(id) => ClientMessage::Reply(id),
            None => ClientMessage::Real {
                id: None,
                label: "message without method".into(),
            },
        };
    };
    match (method.as_str(), id) {
        ("initialize", Some(id)) => ClientMessage::Initialize(id),
        ("notifications/initialized", None) => ClientMessage::Initialized,
        ("subscriptions/listen", Some(id)) => ClientMessage::Listen(id),
        (
            "server/discover"
            | "tools/list"
            | "prompts/list"
            | "resources/list"
            | "resources/templates/list",
            Some(id),
        ) => ClientMessage::Listing { id, method },
        ("ping", Some(id)) => ClientMessage::Ping(id),
        ("notifications/cancelled" | "notifications/roots/list_changed", None) => {
            ClientMessage::Benign
        }
        (_, id) => ClientMessage::Real { id, label: method },
    }
}

struct Server {
    child: Child,
    stdin: ChildStdin,
    generation: u64,
}

struct Listen {
    id: String,
    line: Vec<u8>,
    /// Whether the client has already been shown the acknowledgement, so a
    /// replayed subscription must not produce a second one.
    acked: bool,
}

/// What the client sent to establish its session, kept verbatim so a
/// restarted server can be brought to the same state.
#[derive(Default)]
struct Setup {
    initialize: Option<(String, Vec<u8>)>,
    initialized: Option<Vec<u8>>,
    listens: Vec<Listen>,
}

struct Proxy {
    command: Vec<String>,
    park_after: Duration,
    stop_grace: Duration,
    events: Sender<Event>,
    log: Option<File>,

    server: Option<Server>,
    starts: u64,
    /// Set by the first real call; the server is never parked again.
    awake: bool,
    setup: Setup,

    /// Id of the replayed `initialize` whose reply is still outstanding.
    /// Client lines are queued until it arrives, because a server may reject
    /// requests that overtake its own handshake.
    replay_wait: Option<String>,
    queue: VecDeque<Vec<u8>>,
    swallow_acks: HashSet<String>,

    client_pending: HashMap<String, String>,
    server_pending: HashSet<String>,
    last_activity: Instant,
    blocked_logged: bool,
}

/// Exit code to terminate the wrapper with.
type Exit = i32;

impl Proxy {
    fn run(&mut self, events: Receiver<Event>) -> Exit {
        loop {
            let event = match self.park_deadline() {
                Some(deadline) => {
                    let wait = deadline.saturating_duration_since(Instant::now());
                    match events.recv_timeout(wait) {
                        Ok(event) => event,
                        Err(RecvTimeoutError::Timeout) => {
                            self.on_idle();
                            continue;
                        }
                        Err(RecvTimeoutError::Disconnected) => return self.shutdown(1),
                    }
                }
                None => match events.recv() {
                    Ok(event) => event,
                    Err(_) => return self.shutdown(1),
                },
            };
            let outcome = match event {
                Event::Client(line) => self.on_client(line),
                Event::Server(generation, line) => self.on_server(generation, line),
                Event::ServerEof(generation) => self.on_server_eof(generation),
                Event::ClientEof => Err(0),
                Event::Signal(signal) => Err(128 + signal),
            };
            if let Err(code) = outcome {
                return self.shutdown(code);
            }
        }
    }

    fn park_deadline(&self) -> Option<Instant> {
        if self.server.is_none() || self.awake || self.blocked_logged {
            return None;
        }
        Some(self.last_activity + self.park_after)
    }

    fn on_idle(&mut self) {
        if let Some(blocker) = self.park_blocker() {
            self.log(format!("park blocked by {blocker}"));
            self.blocked_logged = true;
            return;
        }
        self.stop_server();
        self.log("parked");
    }

    fn park_blocker(&self) -> Option<String> {
        if self.replay_wait.is_some() {
            return Some("handshake replay".into());
        }
        if let Some((id, method)) = self.client_pending.iter().next() {
            return Some(format!("pending {method} (id {id})"));
        }
        let id = self.server_pending.iter().next()?;
        Some(format!("unanswered server request (id {id})"))
    }

    fn on_client(&mut self, line: Vec<u8>) -> Result<(), Exit> {
        match classify(&line) {
            ClientMessage::Ping(id) if !self.awake => {
                let reply = format!(r#"{{"jsonrpc":"2.0","id":{id},"result":{{}}}}"#);
                self.write_client(reply.as_bytes())
            }
            ClientMessage::Ping(id) => {
                self.client_pending.insert(id, "ping".into());
                self.send(line);
                Ok(())
            }
            ClientMessage::Benign => {
                if self.server.is_some() {
                    self.send(line);
                }
                Ok(())
            }
            ClientMessage::Reply(id) => {
                // A reply addressed to a server instance that has since been
                // stopped has no recipient; the restarted one never asked.
                if self.server_pending.remove(&id) {
                    self.send(line);
                } else {
                    self.log(format!(
                        "dropped client reply (id {id}): no such server request"
                    ));
                }
                Ok(())
            }
            ClientMessage::Initialize(id) => {
                self.setup = Setup::default();
                self.ensure_up("initialize")?;
                self.client_pending.insert(id.clone(), "initialize".into());
                self.send(line.clone());
                self.setup.initialize = Some((id, line));
                Ok(())
            }
            ClientMessage::Initialized => {
                if self.server.is_some() {
                    self.send(line.clone());
                }
                self.setup.initialized = Some(line);
                Ok(())
            }
            ClientMessage::Listen(id) => {
                self.ensure_up("subscriptions/listen")?;
                self.send(line.clone());
                self.setup.listens.retain(|listen| listen.id != id);
                self.setup.listens.push(Listen {
                    id,
                    line,
                    acked: false,
                });
                Ok(())
            }
            ClientMessage::Listing { id, method } => {
                self.ensure_up(&method)?;
                self.client_pending.insert(id, method);
                self.send(line);
                Ok(())
            }
            ClientMessage::Real { id, label } => {
                if !self.awake {
                    self.awake = true;
                    self.log(format!("staying awake: {label}"));
                }
                self.ensure_up(&label)?;
                if let Some(id) = id {
                    self.client_pending.insert(id, label);
                }
                self.send(line);
                Ok(())
            }
        }
    }

    fn on_server(&mut self, generation: u64, line: Vec<u8>) -> Result<(), Exit> {
        if self.server.as_ref().map(|server| server.generation) != Some(generation) {
            return Ok(());
        }
        if let Some(envelope) = Envelope::parse(&line) {
            match (envelope.method.as_deref(), envelope.id_key()) {
                (None, Some(id)) => {
                    if self.replay_wait.as_deref() == Some(id.as_str()) {
                        self.finish_replay();
                        self.touch();
                        return Ok(());
                    }
                    self.client_pending.remove(&id);
                    self.setup.listens.retain(|listen| listen.id != id);
                }
                (Some(_), Some(id)) => {
                    self.server_pending.insert(id);
                }
                (Some(ACKNOWLEDGED), None) => {
                    if let Some(subscription) = envelope.subscription_key() {
                        if self.swallow_acks.remove(&subscription) {
                            return Ok(());
                        }
                        if let Some(listen) = self
                            .setup
                            .listens
                            .iter_mut()
                            .find(|listen| listen.id == subscription)
                        {
                            listen.acked = true;
                        }
                    }
                }
                _ => {}
            }
        }
        self.touch();
        self.write_client(&line)
    }

    /// The server went away without being asked to. The wrapper follows it
    /// rather than answering on its behalf, so the client sees the same
    /// failure it would see without the wrapper.
    fn on_server_eof(&mut self, generation: u64) -> Result<(), Exit> {
        if self.server.as_ref().map(|server| server.generation) != Some(generation) {
            return Ok(());
        }
        let status = self.stop_server();
        let code = status.and_then(|status| status.code()).unwrap_or(1);
        self.log(format!("server exited on its own (code {code})"));
        Err(code)
    }

    fn ensure_up(&mut self, reason: &str) -> Result<(), Exit> {
        if self.server.is_some() {
            return Ok(());
        }
        self.start_server(reason)?;
        match self.setup.initialize.clone() {
            Some((id, line)) => {
                self.write_server(&line);
                self.replay_wait = Some(id);
            }
            None => self.finish_replay(),
        }
        Ok(())
    }

    fn finish_replay(&mut self) {
        self.replay_wait = None;
        let mut lines = Vec::new();
        lines.extend(self.setup.initialized.clone());
        for listen in &self.setup.listens {
            lines.push(listen.line.clone());
            if listen.acked {
                self.swallow_acks.insert(listen.id.clone());
            }
        }
        lines.extend(self.queue.drain(..));
        for line in lines {
            self.write_server(&line);
        }
    }

    fn start_server(&mut self, reason: &str) -> Result<(), Exit> {
        let mut command = Command::new(&self.command[0]);
        command
            .args(&self.command[1..])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            // Own group, so a launcher and the server it spawns can be
            // signalled together when a stop has to be forced.
            .process_group(0);
        let mut child = command.spawn().map_err(|error| {
            eprintln!("mcp-park: cannot start {}: {error}", self.command[0]);
            127
        })?;
        let stdin = child.stdin.take().expect("child stdin is piped");
        let stdout = child.stdout.take().expect("child stdout is piped");

        self.starts += 1;
        let generation = self.starts;
        let events = self.events.clone();
        thread::spawn(move || {
            read_lines(stdout, |line| {
                events.send(Event::Server(generation, line)).is_ok()
            });
            let _ = events.send(Event::ServerEof(generation));
        });

        self.log(format!("start #{generation} pid {} ({reason})", child.id()));
        self.server = Some(Server {
            child,
            stdin,
            generation,
        });
        self.touch();
        Ok(())
    }

    /// Asks the server to leave by closing its input, then escalates to
    /// SIGTERM and SIGKILL on its process group.
    fn stop_server(&mut self) -> Option<ExitStatus> {
        let Server {
            mut child, stdin, ..
        } = self.server.take()?;
        drop(stdin);
        self.replay_wait = None;
        self.queue.clear();
        self.swallow_acks.clear();
        self.client_pending.clear();
        self.server_pending.clear();
        self.blocked_logged = false;

        let group = child.id() as libc::pid_t;
        for signal in [None, Some(libc::SIGTERM), Some(libc::SIGKILL)] {
            if let Some(signal) = signal {
                // Only reached while the leader is unreaped, so the group id
                // cannot have been recycled. Group 0 or 1 would be ours or
                // everyone's.
                if group > 1 {
                    unsafe { libc::kill(-group, signal) };
                }
            }
            let deadline = Instant::now() + self.stop_grace;
            loop {
                match child.try_wait() {
                    Ok(Some(status)) => return Some(status),
                    Ok(None) => {}
                    Err(_) => return None,
                }
                if signal != Some(libc::SIGKILL) && Instant::now() >= deadline {
                    break;
                }
                thread::sleep(Duration::from_millis(10));
            }
        }
        None
    }

    fn shutdown(&mut self, code: Exit) -> Exit {
        self.stop_server();
        code
    }

    fn send(&mut self, line: Vec<u8>) {
        if self.replay_wait.is_some() {
            self.queue.push_back(line);
        } else {
            self.write_server(&line);
        }
        self.touch();
    }

    fn write_server(&mut self, line: &[u8]) {
        if let Some(server) = self.server.as_mut() {
            // A failed write means the server is gone; its end-of-output
            // event reports that.
            let _ = server
                .stdin
                .write_all(line)
                .and_then(|()| server.stdin.write_all(b"\n"));
        }
    }

    fn write_client(&mut self, line: &[u8]) -> Result<(), Exit> {
        let mut out = io::stdout().lock();
        out.write_all(line)
            .and_then(|()| out.write_all(b"\n"))
            .and_then(|()| out.flush())
            .map_err(|_| 0)
    }

    fn touch(&mut self) {
        self.last_activity = Instant::now();
        self.blocked_logged = false;
    }

    fn log(&mut self, message: impl AsRef<str>) {
        if let Some(file) = self.log.as_mut() {
            // One write per entry: several wrappers usually append to the
            // same file, and only whole writes stay intact.
            let entry = format!(
                "{} [{}] {}\n",
                timestamp(),
                std::process::id(),
                message.as_ref()
            );
            let _ = file.write_all(entry.as_bytes());
        }
    }
}

fn timestamp() -> String {
    let mut buffer = [0u8; 32];
    let written = unsafe {
        let now = libc::time(std::ptr::null_mut());
        let mut local: libc::tm = std::mem::zeroed();
        libc::localtime_r(&now, &mut local);
        libc::strftime(
            buffer.as_mut_ptr().cast(),
            buffer.len(),
            c"%Y-%m-%d %H:%M:%S".as_ptr(),
            &local,
        )
    };
    String::from_utf8_lossy(&buffer[..written]).into_owned()
}

/// Calls `deliver` with each non-empty line, without its line ending, until
/// the input ends or `deliver` returns false.
fn read_lines(input: impl Read, mut deliver: impl FnMut(Vec<u8>) -> bool) {
    let mut reader = BufReader::new(input);
    loop {
        let mut line = Vec::new();
        match reader.read_until(b'\n', &mut line) {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
        while matches!(line.last(), Some(b'\n' | b'\r')) {
            line.pop();
        }
        if !line.is_empty() && !deliver(line) {
            return;
        }
    }
}

fn seconds_from_env(name: &str, default: f64) -> Duration {
    let seconds = std::env::var(name)
        .ok()
        .and_then(|value| value.parse::<f64>().ok());
    Duration::from_secs_f64(
        seconds
            .filter(|s| s.is_finite() && *s >= 0.0)
            .unwrap_or(default),
    )
}

fn main() {
    let mut args: VecDeque<String> = std::env::args().skip(1).collect();
    let mut park_after = seconds_from_env("MCP_PARK_AFTER", 5.0);
    loop {
        match args.front().map(String::as_str) {
            Some("--version") => {
                println!("mcp-park {}", env!("CARGO_PKG_VERSION"));
                return;
            }
            Some("-h" | "--help") => {
                print!("{USAGE}");
                return;
            }
            Some("--park-after") => {
                args.pop_front();
                let seconds = args.pop_front().and_then(|value| value.parse::<f64>().ok());
                match seconds.filter(|s| s.is_finite() && *s >= 0.0) {
                    Some(seconds) => park_after = Duration::from_secs_f64(seconds),
                    None => {
                        eprintln!("mcp-park: --park-after needs a number of seconds");
                        std::process::exit(2);
                    }
                }
            }
            Some("--") => {
                args.pop_front();
                break;
            }
            _ => break,
        }
    }
    if args.is_empty() {
        eprint!("{USAGE}");
        std::process::exit(2);
    }

    let (events, inbox) = mpsc::channel();

    let client = events.clone();
    thread::spawn(move || {
        read_lines(io::stdin(), |line| client.send(Event::Client(line)).is_ok());
        let _ = client.send(Event::ClientEof);
    });

    let signals = events.clone();
    let mut caught =
        signal_hook::iterator::Signals::new([libc::SIGTERM, libc::SIGINT, libc::SIGHUP])
            .expect("signal handlers can be installed");
    thread::spawn(move || {
        for signal in caught.forever() {
            if signals.send(Event::Signal(signal)).is_err() {
                return;
            }
        }
    });

    let log = std::env::var_os("MCP_PARK_LOG")
        .and_then(|path| OpenOptions::new().create(true).append(true).open(path).ok());

    let mut proxy = Proxy {
        command: args.into(),
        park_after,
        stop_grace: seconds_from_env("MCP_PARK_STOP_GRACE", 2.0),
        events,
        log,
        server: None,
        starts: 0,
        awake: false,
        setup: Setup::default(),
        replay_wait: None,
        queue: VecDeque::new(),
        swallow_acks: HashSet::new(),
        client_pending: HashMap::new(),
        server_pending: HashSet::new(),
        last_activity: Instant::now(),
        blocked_logged: false,
    };
    let code = proxy.run(inbox);
    std::process::exit(code);
}
