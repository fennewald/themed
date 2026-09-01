//! Client side of the control socket, used by `themed set` / `themed get`.

use std::io::{self, BufReader};
use std::os::unix::net::UnixStream;
use std::path::Path;

use serde_json::Value;

use crate::proto::{Record, Request, Response, read_msg, write_msg};

/// Send one request to the local daemon and read its reply.
fn request(socket: &Path, req: &Request) -> io::Result<Response> {
    let stream = UnixStream::connect(socket)
        .map_err(|e| io::Error::new(e.kind(), format!("{}: {e}", socket.display())))?;
    write_msg(&mut &stream, req)?;
    read_msg(&mut BufReader::new(&stream))?
        .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "daemon closed the connection"))
}

pub fn set(socket: &Path, blob: Value) -> io::Result<()> {
    match request(socket, &Request::Set { blob })? {
        Response::Ok => Ok(()),
        Response::Err { msg } => Err(io::Error::other(msg)),
        other => Err(io::Error::other(format!("unexpected reply: {other:?}"))),
    }
}

pub fn get(socket: &Path) -> io::Result<Record> {
    match request(socket, &Request::Get)? {
        Response::State { version, blob } => Ok(Record { version, blob }),
        Response::Err { msg } => Err(io::Error::other(msg)),
        other => Err(io::Error::other(format!("unexpected reply: {other:?}"))),
    }
}
