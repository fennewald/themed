//! Wire format shared by both sockets: one JSON object per line, UTF-8.

use std::io::{self, BufRead, Write};

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;

/// Opaque last-write-wins ordering token: nanoseconds since the Unix epoch,
/// with a per-host tiebreaker in the low byte. Never interpret it beyond
/// `>` / `<=`.
pub type Version = u64;

/// The single replicated register.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Record {
    pub version: Version,
    /// Opaque to this daemon: stored, shipped, and handed to the reconcile
    /// hook, never inspected. All theme semantics live in the hook.
    pub blob: Value,
}

/// Anything a client — peer or CLI — can send us.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "lowercase")]
pub enum Request {
    /// Peer → peer: "I adopted this record."
    Announce { version: Version, blob: Value },
    /// Peer → peer, at startup only: "what do you have?"
    Query,
    /// CLI → daemon: set the theme.
    Set { blob: Value },
    /// CLI → daemon: read the theme.
    Get,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "lowercase")]
pub enum Response {
    State { version: Version, blob: Value },
    Ok,
    Err { msg: String },
}

impl From<Record> for Response {
    fn from(r: Record) -> Self {
        Response::State {
            version: r.version,
            blob: r.blob,
        }
    }
}

/// Write one message as a single `\n`-terminated line.
///
/// Serialized into a buffer first: `serde_json::to_writer` emits a write per
/// JSON token, which on a raw socket would be a syscall — and a packet — per
/// token. Messages are a few dozen bytes, so one buffer and one `write_all` is
/// both cheaper and kinder to the reader on the other end.
pub fn write_msg<W: Write>(w: &mut W, msg: &impl Serialize) -> io::Result<()> {
    let mut line = serde_json::to_vec(msg)?;
    line.push(b'\n');
    w.write_all(&line)?;
    w.flush()
}

/// Read one `\n`-terminated message. `Ok(None)` on a clean EOF.
pub fn read_msg<T: DeserializeOwned>(r: &mut impl BufRead) -> io::Result<Option<T>> {
    let mut line = String::new();
    if r.read_line(&mut line)? == 0 {
        return Ok(None);
    }
    serde_json::from_str(&line)
        .map(Some)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(version: Version, blob: &str) -> Record {
        Record {
            version,
            blob: serde_json::from_str(blob).unwrap(),
        }
    }

    #[test]
    fn versions_order_as_integers() {
        let old = record(1, "{}");
        let new = record(u64::MAX, "{}");
        assert!(new.version > old.version);
    }

    #[test]
    fn blob_equality_ignores_formatting() {
        assert_eq!(
            record(1, r#"{"mode":"dark"}"#).blob,
            record(2, r#"{ "mode": "dark" }"#).blob
        );
        assert_ne!(
            record(1, r#"{"mode":"dark"}"#).blob,
            record(2, r#"{"mode":"light"}"#).blob
        );
    }

    #[test]
    fn record_round_trips_through_json() {
        let before = record(1_700_000_000_000_000_000, r#"{"mode":"light"}"#);
        let text = serde_json::to_string(&before).unwrap();
        let after: Record = serde_json::from_str(&text).unwrap();
        assert_eq!(before.version, after.version);
        assert_eq!(before.blob, after.blob);
    }

    #[test]
    fn requests_round_trip() {
        let line = r#"{"t":"announce","version":7,"blob":{"mode":"dark"}}"#;
        let req: Request = serde_json::from_str(line).unwrap();
        let Request::Announce { version, blob } = req else {
            panic!("wrong variant");
        };
        assert_eq!(version, 7);
        assert_eq!(blob, serde_json::json!({"mode": "dark"}));
    }

    #[test]
    fn framing_reads_one_message_per_line() {
        let mut buf = Vec::new();
        write_msg(&mut buf, &Request::Query).unwrap();
        write_msg(&mut buf, &Request::Get).unwrap();
        assert_eq!(buf.iter().filter(|b| **b == b'\n').count(), 2);

        let mut reader = &buf[..];
        assert!(matches!(
            read_msg::<Request>(&mut reader).unwrap(),
            Some(Request::Query)
        ));
        assert!(matches!(
            read_msg::<Request>(&mut reader).unwrap(),
            Some(Request::Get)
        ));
        assert!(read_msg::<Request>(&mut reader).unwrap().is_none());
    }

    /// Counts writes so the single-syscall property stays a property.
    struct CountingWriter {
        bytes: Vec<u8>,
        writes: usize,
    }

    impl Write for CountingWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.writes += 1;
            self.bytes.extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn each_message_costs_one_write() {
        let mut w = CountingWriter {
            bytes: Vec::new(),
            writes: 0,
        };
        write_msg(
            &mut w,
            &Request::Announce {
                version: 7,
                blob: serde_json::json!({"mode": "dark", "accent": "#ff0000"}),
            },
        )
        .unwrap();

        assert_eq!(w.writes, 1, "message was written in pieces");
        assert!(w.bytes.ends_with(b"\n"));
        assert_eq!(w.bytes.iter().filter(|b| **b == b'\n').count(), 1);
    }

    #[test]
    fn malformed_lines_are_errors_not_panics() {
        for line in ["not json\n", "{}\n", r#"{"t":"nonsense"}"#] {
            assert!(read_msg::<Request>(&mut line.as_bytes()).is_err());
        }
    }
}
