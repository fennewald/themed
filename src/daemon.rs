//! Daemon lifecycle: state, persistence, accept loops.

use std::fs;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use log::{debug, error, info, warn};

use crate::peer;
use crate::proto::{Record, Request, Response, Version, read_msg, write_msg};
use crate::reconcile;

/// Used when no state file exists yet. The daemon never looks inside a blob;
/// this is only a seed so peers have something to converge on.
pub const DEFAULT_BLOB: &str = r#"{"mode":"dark"}"#;

pub struct Config {
    pub self_name: String,
    pub listen: String,
    pub state_file: PathBuf,
    pub socket: PathBuf,
    pub reconcile_cmd: Option<String>,
    pub peers: Vec<String>,
}

/// Which socket a request arrived on — peers and the local CLI may say
/// different things.
#[derive(Clone, Copy, PartialEq)]
enum Channel {
    Peer,
    Control,
}

/// Work to do after a record was adopted, handed to a single worker thread so
/// hooks and fan-out stay ordered and off the accept threads.
struct Job {
    record: Record,
    reconcile: bool,
    announce: bool,
}

pub struct Daemon {
    cfg: Config,
    tiebreak: Version,
    state: Mutex<Record>,
    jobs: Sender<Job>,
}

pub fn run(cfg: Config) -> std::io::Result<()> {
    // Bind first: tailscaled may not have configured our address yet.
    let addr = peer::resolve_forever(&cfg.listen);
    let tcp = bind_forever(addr);
    info!("listening for peers on {addr}");

    let record = load_state(&cfg.state_file);
    debug!("loaded version {}", record.version);

    let unix = bind_control(&cfg.socket)?;
    info!("control socket at {}", cfg.socket.display());

    let (tx, rx) = channel();
    let daemon = Arc::new(Daemon {
        tiebreak: tiebreak(&cfg.self_name),
        state: Mutex::new(record),
        jobs: tx,
        cfg,
    });

    {
        let daemon = Arc::clone(&daemon);
        thread::spawn(move || daemon.work(rx));
    }

    daemon.catch_up();

    {
        let daemon = Arc::clone(&daemon);
        thread::spawn(move || daemon.accept_peers(tcp));
    }
    {
        let daemon = Arc::clone(&daemon);
        thread::spawn(move || daemon.accept_control(unix));
    }

    wait_for_shutdown(&daemon.cfg.socket)
}

impl Daemon {
    /// Startup convergence: whoever holds the highest version wins, then
    /// reconcile once — the hook is responsible for being idempotent, since we
    /// cannot know what the system had actually applied before we started.
    fn catch_up(&self) {
        let mut best = self.state.lock().unwrap().clone();
        for reply in peer::query_all(&self.cfg.peers) {
            if reply.version > best.version {
                best = reply;
            }
        }

        let mut state = self.state.lock().unwrap();
        if best.version > state.version {
            info!("caught up to version {} from a peer", best.version);
            *state = best.clone();
            self.persist(&best);
        }
        drop(state);

        self.enqueue(Job {
            record: best,
            reconcile: true,
            announce: false,
        });
    }

    /// Adopt `incoming` if it is newer. A newer record carrying the *same*
    /// theme only refreshes the version: no hook, no fan-out.
    fn adopt(&self, incoming: Record, local: bool) {
        let mut state = self.state.lock().unwrap();
        if incoming.version <= state.version {
            debug!(
                "ignoring version {} (have {})",
                incoming.version, state.version
            );
            return;
        }
        let changed = state.blob != incoming.blob;
        *state = incoming.clone();
        self.persist(&incoming);
        drop(state);

        if changed {
            info!("adopted version {}: {}", incoming.version, incoming.blob);
        } else {
            debug!("version {} carries no change", incoming.version);
        }

        self.enqueue(Job {
            record: incoming,
            reconcile: changed,
            announce: local || changed,
        });
    }

    fn enqueue(&self, job: Job) {
        if self.jobs.send(job).is_err() {
            error!("worker thread is gone");
        }
    }

    /// Serializes reconcile hooks and peer fan-out in adoption order.
    fn work(&self, jobs: Receiver<Job>) {
        for job in jobs {
            if job.reconcile
                && let Some(cmd) = &self.cfg.reconcile_cmd
            {
                reconcile::run(cmd, &job.record.blob);
            }
            if job.announce {
                peer::announce_all(&self.cfg.peers, &job.record);
            }
        }
    }

    /// Wall-clock ns, truncated to make room for the host tiebreaker, and
    /// forced past `prev` so a local set always wins locally even under skew.
    fn next_version(&self, prev: Version) -> Version {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        ((now & !0xff) | self.tiebreak).max(prev + 1)
    }

    fn persist(&self, record: &Record) {
        if let Err(e) = write_state(&self.cfg.state_file, record) {
            error!("persisting {}: {e}", self.cfg.state_file.display());
        }
    }

    fn handle(&self, req: Request, ch: Channel) -> Option<Response> {
        match (req, ch) {
            (Request::Announce { version, blob }, Channel::Peer) => {
                self.adopt(Record { version, blob }, false);
                None
            }
            (Request::Query, Channel::Peer) | (Request::Get, Channel::Control) => {
                Some(self.state.lock().unwrap().clone().into())
            }
            (Request::Set { blob }, Channel::Control) => {
                let version = self.next_version(self.state.lock().unwrap().version);
                self.adopt(Record { version, blob }, true);
                Some(Response::Ok)
            }
            _ => Some(Response::Err {
                msg: "command not accepted on this socket".into(),
            }),
        }
    }

    /// One request per connection, then close.
    fn serve(&self, mut r: impl Read, mut w: impl Write, ch: Channel) {
        let mut r = std::io::BufReader::new(&mut r);
        match read_msg(&mut r) {
            Ok(Some(req)) => {
                if let Some(reply) = self.handle(req, ch)
                    && let Err(e) = write_msg(&mut w, &reply)
                {
                    debug!("writing reply: {e}");
                }
            }
            Ok(None) => debug!("connection closed without a request"),
            Err(e) => debug!("bad request: {e}"),
        }
    }

    fn accept_peers(&self, listener: TcpListener) {
        accept_loop(listener.incoming(), |stream: TcpStream| {
            let _ = stream.set_read_timeout(Some(peer::PEER_TIMEOUT));
            let _ = stream.set_write_timeout(Some(peer::PEER_TIMEOUT));
            self.serve(&stream, &stream, Channel::Peer);
        });
    }

    fn accept_control(&self, listener: UnixListener) {
        accept_loop(listener.incoming(), |stream: UnixStream| {
            let _ = stream.set_read_timeout(Some(peer::PEER_TIMEOUT));
            let _ = stream.set_write_timeout(Some(peer::PEER_TIMEOUT));
            self.serve(&stream, &stream, Channel::Control);
        });
    }
}

fn accept_loop<S: Send>(
    incoming: impl Iterator<Item = std::io::Result<S>>,
    handle: impl Fn(S) + Sync,
) {
    thread::scope(|scope| {
        for stream in incoming {
            match stream {
                Ok(stream) => {
                    scope.spawn(|| handle(stream));
                }
                Err(e) => debug!("accept failed: {e}"),
            }
        }
    });
}

fn bind_forever(addr: SocketAddr) -> TcpListener {
    let mut delay = Duration::from_secs(1);
    loop {
        match TcpListener::bind(addr) {
            Ok(l) => return l,
            Err(e) => warn!("cannot bind {addr}: {e}; retrying in {delay:?}"),
        }
        thread::sleep(delay);
        delay = (delay * 2).min(Duration::from_secs(30));
    }
}

fn bind_control(path: &Path) -> std::io::Result<UnixListener> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    // A stale socket from an unclean exit would block the bind.
    if path.exists() {
        fs::remove_file(path)?;
    }
    UnixListener::bind(path)
}

fn load_state(path: &Path) -> Record {
    let default = || Record {
        version: 0,
        blob: serde_json::from_str(DEFAULT_BLOB).expect("valid default blob"),
    };
    match fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text).unwrap_or_else(|e| {
            warn!("{} is unreadable ({e}); starting fresh", path.display());
            default()
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => default(),
        Err(e) => {
            warn!("{}: {e}; starting fresh", path.display());
            default()
        }
    }
}

/// Written in place rather than through a temp file and rename: the rename
/// would swap the inode out from under anything watching the path. Callers hold
/// the state lock, and the daemon is the file's only writer, so a single
/// truncating write is enough.
fn write_state(path: &Path, record: &Record) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, serde_json::to_vec(record)?)
}

fn tiebreak(name: &str) -> Version {
    // FNV-1a, truncated: only needs to differ between fleet members.
    let hash = name.bytes().fold(0xcbf29ce484222325u64, |h, b| {
        (h ^ b as u64).wrapping_mul(0x100000001b3)
    });
    hash & 0xff
}

fn wait_for_shutdown(socket: &Path) -> std::io::Result<()> {
    use signal_hook::consts::{SIGINT, SIGTERM};
    use signal_hook::iterator::Signals;

    let mut signals = Signals::new([SIGTERM, SIGINT])?;
    if let Some(sig) = signals.forever().next() {
        info!("signal {sig}, shutting down");
    }
    let _ = fs::remove_file(socket);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_state_file_yields_the_default_blob() {
        let record = load_state(Path::new("/nonexistent/themed/state.json"));
        assert_eq!(record.version, 0);
        assert_eq!(record.blob, serde_json::json!({"mode": "dark"}));
    }

    #[test]
    fn state_file_round_trips() {
        let dir = std::env::temp_dir().join(format!("themed-test-{}", std::process::id()));
        let path = dir.join("state.json");
        let record = Record {
            version: 42,
            blob: serde_json::json!({"mode": "light"}),
        };

        write_state(&path, &record).unwrap();
        let loaded = load_state(&path);

        assert_eq!(loaded.version, 42);
        assert_eq!(loaded.blob, record.blob);

        // Rewriting must keep the same inode so watchers survive it.
        let inode = std::os::unix::fs::MetadataExt::ino(&fs::metadata(&path).unwrap());
        write_state(&path, &record).unwrap();
        assert_eq!(
            inode,
            std::os::unix::fs::MetadataExt::ino(&fs::metadata(&path).unwrap()),
            "state file was replaced instead of rewritten"
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn corrupt_state_file_falls_back_to_the_default() {
        let path = std::env::temp_dir().join(format!("themed-corrupt-{}.json", std::process::id()));
        fs::write(&path, "{ this is not json").unwrap();
        assert_eq!(load_state(&path).blob, serde_json::json!({"mode": "dark"}));
        fs::remove_file(&path).unwrap();
    }

    #[test]
    fn tiebreaks_differ_between_hosts() {
        let names = ["fezzik", "vizzini", "fire-swamp", "xanadu", "max"];
        let breaks: std::collections::HashSet<Version> =
            names.iter().map(|n| tiebreak(n)).collect();
        assert_eq!(breaks.len(), names.len());
    }
}
