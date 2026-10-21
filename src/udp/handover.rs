//! UDP flows across a worker replacement. The replaced worker hands each
//! flow to the next generation's worker of the same index: client, reply
//! address, backend, and the flow's upstream socket itself, so the flow
//! keeps its backend and the backend sees the same source address.
//!
//! One fixed-size record per flow on a Unix stream socket, the upstream
//! socket passed with it (`SCM_RIGHTS`); an empty record ends the set.

use std::io::{IoSlice, IoSliceMut};
use std::net::SocketAddr;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use nix::sys::socket::{recvmsg, sendmsg, ControlMessage, ControlMessageOwned, MsgFlags};
use serde::{Deserialize, Serialize};

const RECORD: usize = 256;

/// One flow as it moves.
#[derive(Debug, Serialize, Deserialize, PartialEq)]
pub struct Moved {
    pub client: SocketAddr,
    pub reply_to: SocketAddr,
    pub backend: SocketAddr,
}

/// Where worker `index`'s successor listens for the flows of `address`.
pub fn path(dir: &Path, address: &str, index: usize) -> PathBuf {
    let name: String = address.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }).collect();
    dir.join(format!("udp-{name}-{index}.sock"))
}

/// The successor's end, bound before the worker can be ready, so a replaced
/// worker never finds it missing.
pub fn listen(path: &Path) -> std::io::Result<UnixListener> {
    let _ = std::fs::remove_file(path);
    let listener = UnixListener::bind(path)?;
    listener.set_nonblocking(true)?;
    Ok(listener)
}

/// The replaced worker: send every flow, then the end record. Blocking.
pub fn send(path: &Path, flows: Vec<(Moved, OwnedFd)>) -> std::io::Result<usize> {
    let stream = connect(path, Duration::from_secs(2))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    let n = flows.len();
    for (moved, fd) in flows {
        let mut record = [0u8; RECORD];
        let json = serde_json::to_vec(&moved)?;
        record[..json.len()].copy_from_slice(&json);
        let fds = [fd.as_raw_fd()];
        sendmsg::<()>(stream.as_raw_fd(), &[IoSlice::new(&record)], &[ControlMessage::ScmRights(&fds)], MsgFlags::empty(), None)?;
    }
    sendmsg::<()>(stream.as_raw_fd(), &[IoSlice::new(&[0u8; RECORD])], &[], MsgFlags::empty(), None)?;
    Ok(n)
}

fn connect(path: &Path, wait: Duration) -> std::io::Result<UnixStream> {
    let deadline = Instant::now() + wait;
    loop {
        match UnixStream::connect(path) {
            Ok(s) => return Ok(s),
            Err(e) if Instant::now() >= deadline => return Err(e),
            Err(_) => std::thread::sleep(Duration::from_millis(20)),
        }
    }
}

/// The successor: wait up to `wait` for the replaced worker, and take its
/// flows. Blocking. None when it never came.
pub fn receive(listener: &UnixListener, wait: Duration) -> std::io::Result<Option<Vec<(Moved, std::net::UdpSocket)>>> {
    let deadline = Instant::now() + wait;
    let stream = loop {
        match listener.accept() {
            Ok((s, _)) => break s,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock && Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return Ok(None),
            Err(e) => return Err(e),
        }
    };
    stream.set_nonblocking(false)?;
    // A bound only; macOS refuses it once the sender has closed (EINVAL).
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let mut flows = Vec::new();
    loop {
        let mut record = [0u8; RECORD];
        let (got, fd) = read_record(stream.as_raw_fd(), &mut record)?;
        let len = record.iter().position(|b| *b == 0).unwrap_or(RECORD);
        if got == 0 || len == 0 {
            return Ok(Some(flows));
        }
        let moved: Moved = serde_json::from_slice(&record[..len])?;
        let Some(fd) = fd else { continue };
        // SAFETY: a descriptor the kernel just installed for us.
        flows.push((moved, unsafe { std::net::UdpSocket::from_raw_fd(fd) }));
    }
}

/// One whole record and the descriptor sent with it.
fn read_record(stream: RawFd, record: &mut [u8; RECORD]) -> std::io::Result<(usize, Option<RawFd>)> {
    let mut fd = None;
    let mut got = 0;
    while got < RECORD {
        let mut space = nix::cmsg_space!([RawFd; 1]);
        let mut iov = [IoSliceMut::new(&mut record[got..])];
        let msg = recvmsg::<()>(stream, &mut iov, Some(&mut space), MsgFlags::empty())?;
        for c in msg.cmsgs()? {
            if let ControlMessageOwned::ScmRights(fds) = c {
                fd = fds.first().copied();
            }
        }
        if msg.bytes == 0 {
            return Ok((got, fd));
        }
        got += msg.bytes;
    }
    Ok((got, fd))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flows_and_their_sockets_move() {
        let dir = std::env::temp_dir().join(format!("keel-handover-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = path(&dir, "0.0.0.0:53", 1);
        let listener = listen(&path).unwrap();
        let upstream = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let local = upstream.local_addr().unwrap();
        let moved = Moved { client: "192.0.2.1:5000".parse().unwrap(), reply_to: "192.0.2.1:5000".parse().unwrap(), backend: "10.0.0.1:53".parse().unwrap() };
        let sender = {
            let path = path.clone();
            let fd: OwnedFd = upstream.into();
            std::thread::spawn(move || send(&path, vec![(moved, fd)]))
        };
        let got = receive(&listener, Duration::from_secs(5)).unwrap().expect("a predecessor");
        assert_eq!(sender.join().unwrap().unwrap(), 1);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].0.backend, "10.0.0.1:53".parse::<SocketAddr>().unwrap());
        assert_eq!(got[0].1.local_addr().unwrap(), local, "the same socket");
        assert!(receive(&listen(&path).unwrap(), Duration::from_millis(50)).unwrap().is_none(), "nobody came");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
