//! Outbound peer traffic: one-shot TCP connections, one message each.

use std::io::{self, BufReader};
use std::net::{TcpStream, ToSocketAddrs};
use std::thread;
use std::time::Duration;

use log::{debug, warn};

use crate::proto::{Record, Request, Response, read_msg, write_msg};

/// Per-peer budget for connect + write + read.
pub const PEER_TIMEOUT: Duration = Duration::from_secs(2);

/// Push a record to every peer in parallel. Failures are logged, never fatal.
pub fn announce_all(peers: &[String], record: &Record) {
    if peers.is_empty() {
        return;
    }
    debug!(
        "announcing version {} to {} peers",
        record.version,
        peers.len()
    );
    thread::scope(|s| {
        for peer in peers {
            s.spawn(move || {
                let req = Request::Announce {
                    version: record.version,
                    blob: record.blob.clone(),
                };
                if let Err(e) = send(peer, &req, false) {
                    debug!("announce to {peer} failed: {e}");
                }
            });
        }
    });
}

/// Ask every peer for its current record. Unreachable peers are simply absent
/// from the result.
pub fn query_all(peers: &[String]) -> Vec<Record> {
    if peers.is_empty() {
        return Vec::new();
    }
    thread::scope(|s| {
        let handles: Vec<_> = peers
            .iter()
            .map(|peer| {
                s.spawn(move || match send(peer, &Request::Query, true) {
                    Ok(Some(Response::State { version, blob })) => Some(Record { version, blob }),
                    Ok(_) => None,
                    Err(e) => {
                        debug!("query to {peer} failed: {e}");
                        None
                    }
                })
            })
            .collect();
        handles
            .into_iter()
            .filter_map(|h| h.join().unwrap_or(None))
            .collect()
    })
}

fn send(peer: &str, req: &Request, want_reply: bool) -> io::Result<Option<Response>> {
    let addr = peer
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no address for peer"))?;
    let stream = TcpStream::connect_timeout(&addr, PEER_TIMEOUT)?;
    stream.set_read_timeout(Some(PEER_TIMEOUT))?;
    stream.set_write_timeout(Some(PEER_TIMEOUT))?;

    let mut w = &stream;
    write_msg(&mut w, req)?;
    if !want_reply {
        return Ok(None);
    }
    read_msg(&mut BufReader::new(&stream))
}

/// Resolve `addr`, retrying with backoff — tailscaled may not be up yet.
pub fn resolve_forever(addr: &str) -> std::net::SocketAddr {
    let mut delay = Duration::from_secs(1);
    loop {
        match addr.to_socket_addrs().map(|mut a| a.next()) {
            Ok(Some(sa)) => return sa,
            Ok(None) => warn!("{addr} resolved to nothing; retrying in {delay:?}"),
            Err(e) => warn!("cannot resolve {addr}: {e}; retrying in {delay:?}"),
        }
        thread::sleep(delay);
        delay = (delay * 2).min(Duration::from_secs(30));
    }
}
