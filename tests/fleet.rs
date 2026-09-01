//! End-to-end tests: real daemons on 127.0.0.1, talking over real sockets.

use std::fs;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::atomic::{AtomicU32, Ordering};
use std::thread::sleep;
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_themed");
const DARK: &str = r#"{"mode":"dark"}"#;
const LIGHT: &str = r#"{"mode":"light"}"#;

/// One daemon plus the paths it was told to use. Killed on drop.
struct Node {
    child: Child,
    port: u16,
    socket: PathBuf,
    applied: PathBuf,
}

impl Drop for Node {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Node {
    /// Start a daemon whose reconcile hook appends the blob it was given to a
    /// file, so tests can see exactly what was applied and how often.
    fn start(dir: &Path, name: &str, peers: &[u16]) -> Node {
        let port = free_port();
        let socket = dir.join(format!("{name}.sock"));
        let applied = dir.join(format!("{name}.applied"));

        let mut cmd = Command::new(BIN);
        cmd.args(["--self", name])
            .arg("--listen")
            .arg(format!("127.0.0.1:{port}"))
            .arg("--state-file")
            .arg(dir.join(format!("{name}.json")))
            .arg("--socket")
            .arg(&socket)
            .arg("--reconcile-cmd")
            .arg(format!("cat >> {}", applied.display()));
        for peer in peers {
            cmd.arg("--peer").arg(format!("127.0.0.1:{peer}"));
        }

        let child = cmd.spawn().expect("spawn themed");
        let node = Node {
            child,
            port,
            socket,
            applied,
        };
        wait_until(|| node.socket.exists());
        node
    }

    fn set(&self, blob: &str) -> bool {
        Command::new(BIN)
            .args(["--socket"])
            .arg(&self.socket)
            .args(["set", blob])
            .status()
            .expect("run themed set")
            .success()
    }

    fn get(&self) -> String {
        let out = Command::new(BIN)
            .args(["--socket"])
            .arg(&self.socket)
            .arg("get")
            .output()
            .expect("run themed get");
        assert!(out.status.success(), "themed get failed");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn applied(&self) -> String {
        fs::read_to_string(&self.applied).unwrap_or_default()
    }

    fn wait_applied(&self, blob: &str) {
        wait_until(|| self.applied().contains(blob));
        assert!(
            self.applied().contains(blob),
            "{} never applied {blob}; saw {:?}",
            self.applied.display(),
            self.applied()
        );
    }
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .expect("bind ephemeral port")
        .local_addr()
        .unwrap()
        .port()
}

/// Poll for up to five seconds; callers assert afterwards.
fn wait_until(mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if done() {
            return;
        }
        sleep(Duration::from_millis(20));
    }
}

fn test_dir(name: &str) -> PathBuf {
    static N: AtomicU32 = AtomicU32::new(0);
    let dir = std::env::temp_dir().join(format!(
        "themed-{name}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create test dir");
    dir
}

#[test]
fn set_propagates_to_every_peer() {
    let dir = test_dir("fanout");
    let c = Node::start(&dir, "c", &[]);
    let b = Node::start(&dir, "b", &[c.port]);
    let a = Node::start(&dir, "a", &[b.port, c.port]);

    assert!(a.set(LIGHT));

    a.wait_applied(LIGHT);
    b.wait_applied(LIGHT);
    c.wait_applied(LIGHT);
    assert_eq!(a.get(), LIGHT);
    assert_eq!(b.get(), LIGHT);
}

#[test]
fn a_late_starter_catches_up_by_querying() {
    let dir = test_dir("catchup");
    let a = Node::start(&dir, "a", &[]);
    assert!(a.set(LIGHT));
    a.wait_applied(LIGHT);

    // C was "powered off" during the set and learns on startup.
    let c = Node::start(&dir, "c", &[a.port]);
    c.wait_applied(LIGHT);
    assert_eq!(c.get(), LIGHT);
}

#[test]
fn one_hop_rebroadcast_covers_a_partition() {
    let dir = test_dir("partition");
    let c = Node::start(&dir, "c", &[]);
    let b = Node::start(&dir, "b", &[c.port]);
    // A cannot see C: its entry for C points at a port nobody listens on.
    let a = Node::start(&dir, "a", &[b.port, free_port()]);

    assert!(a.set(LIGHT));

    // B adopted it and re-announced, which is C's only path to the change.
    c.wait_applied(LIGHT);
    assert_eq!(c.get(), LIGHT);
}

#[test]
fn set_succeeds_with_every_peer_unreachable() {
    let dir = test_dir("isolated");
    let a = Node::start(&dir, "a", &[free_port(), free_port()]);

    assert!(a.set(LIGHT), "fan-out failures must not fail the set");
    a.wait_applied(LIGHT);
    assert_eq!(a.get(), LIGHT);
}

#[test]
fn redundant_sets_bump_the_version_without_reconciling() {
    let dir = test_dir("dedup");
    let b = Node::start(&dir, "b", &[]);
    let a = Node::start(&dir, "a", &[b.port]);

    assert!(a.set(LIGHT));
    a.wait_applied(LIGHT);
    b.wait_applied(LIGHT);
    let (before_a, before_b) = (a.applied(), b.applied());
    let version_before = version_of(&b);

    // Same theme again: both ends must record the newer version but leave the
    // reconcile hook alone.
    assert!(a.set(LIGHT));
    wait_until(|| version_of(&b) > version_before);

    assert!(version_of(&a) > version_before, "a should bump its version");
    assert!(
        version_of(&b) > version_before,
        "b should adopt the new version"
    );
    assert_eq!(a.applied(), before_a, "a reconciled a no-op change");
    assert_eq!(b.applied(), before_b, "b reconciled a no-op change");
}

#[test]
fn stale_and_malformed_input_is_ignored_not_fatal() {
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpStream;

    let dir = test_dir("junk");
    let a = Node::start(&dir, "a", &[]);
    assert!(a.set(LIGHT));
    a.wait_applied(LIGHT);
    let applied = a.applied();

    let send = |line: &str| {
        let stream = TcpStream::connect(("127.0.0.1", a.port)).expect("connect");
        (&stream).write_all(line.as_bytes()).expect("write");
    };
    send("this is not json\n");
    send("{\"t\":\"nonsense\"}\n");
    // Version 1 is older than any real set, so this must not be adopted.
    send(&format!(
        "{{\"t\":\"announce\",\"version\":1,\"blob\":{DARK}}}\n"
    ));

    // The daemon is still serving, and still holds the light theme.
    let stream = TcpStream::connect(("127.0.0.1", a.port)).expect("connect");
    (&stream).write_all(b"{\"t\":\"query\"}\n").expect("write");
    let mut reply = String::new();
    BufReader::new(&stream)
        .read_line(&mut reply)
        .expect("reply");
    assert!(reply.contains("light"), "unexpected reply: {reply}");
    assert_eq!(a.applied(), applied, "junk triggered a reconcile");
}

fn version_of(node: &Node) -> u64 {
    let state: serde_json::Value = {
        let stream = std::os::unix::net::UnixStream::connect(&node.socket).expect("connect");
        use std::io::{BufRead, BufReader, Write};
        (&stream).write_all(b"{\"t\":\"get\"}\n").expect("write");
        let mut line = String::new();
        BufReader::new(&stream).read_line(&mut line).expect("reply");
        serde_json::from_str(&line).expect("parse reply")
    };
    state["version"].as_u64().expect("version in reply")
}
