//! The link between two devices.
//!
//! # Handshake (JSON lines, in the clear, nothing secret in them)
//!
//! ```text
//! dialer -> listener   {"t":"MESH_HELLO","v":1,"fp":"<key fingerprint>","nonce":"<32 bytes hex>"}
//! listener -> dialer   {"t":"MESH_CHALLENGE","nonce":"<32 bytes hex>","proof":"<hex>"}
//!                      or {"t":"MESH_DENY","reason":"..."}
//! dialer -> listener   {"t":"MESH_PROOF","proof":"<hex>"}
//! ```
//!
//! Both sides prove they hold the pair's key, each over both nonces and under its own label, so a
//! proof cannot be reflected back, and a recorded handshake is worthless against the next one:
//!
//! * listener proof = HMAC-SHA256(key, "talkee-mesh-listener-v1" | dialer nonce | listener nonce)
//! * dialer proof   = HMAC-SHA256(key, "talkee-mesh-dialer-v1"   | dialer nonce | listener nonce)
//! * link key       = HMAC-SHA256(key, "talkee-mesh-link-v1"     | dialer nonce | listener nonce)
//!
//! The link key is fresh for every connection because both nonces are, so AES-GCM nonces (the
//! sequence number and a direction bit) never repeat under one key.
//!
//! # Frames
//!
//! `[length u32 BE][seq u32 LE][nonce | ciphertext | tag]`, at most [`MAX_FRAME`] bytes. Sequence
//! numbers start at 0 in each direction and must rise by one; anything else closes the link.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use talkee_crypto::{Direction, Psk, Session, SessionId};

use crate::MESH_VERSION;

/// Largest frame either side accepts. Requests and answers are JSON; files go in pieces.
pub const MAX_FRAME: usize = 1 << 20;

/// Longest handshake line.
const MAX_LINE: usize = 1024;

/// How long a handshake may take before the connection is dropped.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

const LISTENER_LABEL: &[u8] = b"talkee-mesh-listener-v1";
const DIALER_LABEL: &[u8] = b"talkee-mesh-dialer-v1";
const LINK_LABEL: &[u8] = b"talkee-mesh-link-v1";

#[derive(Serialize, Deserialize)]
#[serde(tag = "t")]
enum Hand {
    #[serde(rename = "MESH_HELLO")]
    Hello { v: u32, fp: String, nonce: String },
    #[serde(rename = "MESH_CHALLENGE")]
    Challenge { nonce: String, proof: String },
    #[serde(rename = "MESH_DENY")]
    Deny { reason: String },
    #[serde(rename = "MESH_PROOF")]
    Proof { proof: String },
}

type HmacSha256 = Hmac<Sha256>;

fn mac(key: &Psk, label: &[u8], a: &[u8; 32], b: &[u8; 32]) -> HmacSha256 {
    let mut m = HmacSha256::new_from_slice(key.as_bytes()).expect("HMAC takes any key length");
    m.update(label);
    m.update(a);
    m.update(b);
    m
}

fn tag(key: &Psk, label: &[u8], a: &[u8; 32], b: &[u8; 32]) -> String {
    hex::encode(mac(key, label, a, b).finalize().into_bytes())
}

/// Constant-time check of a hex proof.
fn check(key: &Psk, label: &[u8], a: &[u8; 32], b: &[u8; 32], claimed: &str) -> bool {
    match hex::decode(claimed) {
        Ok(bytes) => mac(key, label, a, b).verify_slice(&bytes).is_ok(),
        Err(_) => false,
    }
}

fn nonce() -> Result<[u8; 32]> {
    let mut n = [0u8; 32];
    getrandom::getrandom(&mut n).map_err(|e| anyhow!("no randomness: {e}"))?;
    Ok(n)
}

fn parse_nonce(text: &str) -> Result<[u8; 32]> {
    let bytes = hex::decode(text).context("nonce is not hex")?;
    bytes.try_into().map_err(|_| anyhow!("nonce is not 32 bytes"))
}

fn link_key(key: &Psk, dialer: &[u8; 32], listener: &[u8; 32]) -> Psk {
    let bytes = mac(key, LINK_LABEL, dialer, listener).finalize().into_bytes();
    Psk::from_slice(&bytes).expect("HMAC-SHA256 output is 32 bytes")
}

fn write_line(stream: &mut TcpStream, msg: &Hand) -> Result<()> {
    let mut line = serde_json::to_vec(msg)?;
    line.push(b'\n');
    stream.write_all(&line)?;
    Ok(())
}

fn read_line(reader: &mut BufReader<TcpStream>) -> Result<Hand> {
    let mut buf = Vec::new();
    let n = reader
        .by_ref()
        .take(MAX_LINE as u64 + 1)
        .read_until(b'\n', &mut buf)?;
    if n == 0 {
        bail!("the other device closed the connection");
    }
    if n > MAX_LINE {
        bail!("handshake line too long");
    }
    serde_json::from_slice(&buf).context("not a Talkee mesh handshake")
}

/// An authenticated, encrypted link to one peer.
pub struct Channel {
    reader: BufReader<TcpStream>,
    writer: TcpStream,
    seal: Session,
    open: Session,
    send_seq: u32,
    recv_seq: u32,
    buf: Vec<u8>,
}

impl Channel {
    fn new(reader: BufReader<TcpStream>, key: Psk, dialer: bool) -> Result<Self> {
        let writer = reader.get_ref().try_clone()?;
        let id = SessionId::from_bytes([0; 4]);
        let (out_dir, in_dir) = if dialer {
            (Direction::PhoneToDesktop, Direction::DesktopToPhone)
        } else {
            (Direction::DesktopToPhone, Direction::PhoneToDesktop)
        };
        Ok(Self {
            reader,
            writer,
            seal: Session::new(&key, id, out_dir),
            open: Session::new(&key, id, in_dir),
            send_seq: 0,
            recv_seq: 0,
            buf: Vec::new(),
        })
    }

    /// Sends one message.
    pub fn send(&mut self, plain: &[u8]) -> Result<()> {
        let seq = self.send_seq;
        self.send_seq = seq.checked_add(1).context("link used up; reconnect")?;
        let aad = seq.to_le_bytes();
        self.seal.seal(seq, &aad, plain, &mut self.buf)?;
        let len = 4 + self.buf.len();
        if len > MAX_FRAME {
            bail!("message too large for the mesh ({len} bytes)");
        }
        let mut frame = Vec::with_capacity(4 + len);
        frame.extend_from_slice(&(len as u32).to_be_bytes());
        frame.extend_from_slice(&aad);
        frame.extend_from_slice(&self.buf);
        self.writer.write_all(&frame)?;
        Ok(())
    }

    /// Waits for the next message. `Ok(None)` when the other side closed cleanly.
    pub fn recv(&mut self) -> Result<Option<Vec<u8>>> {
        let mut len = [0u8; 4];
        match self.reader.read_exact(&mut len) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(e.into()),
        }
        let len = u32::from_be_bytes(len) as usize;
        if !(4..=MAX_FRAME).contains(&len) {
            bail!("frame of {len} bytes refused");
        }
        let mut frame = vec![0u8; len];
        self.reader.read_exact(&mut frame)?;
        let (aad, sealed) = frame.split_at(4);
        let seq = u32::from_le_bytes(aad.try_into().expect("4 bytes"));
        if seq != self.recv_seq {
            bail!("frame {seq} out of order (expected {})", self.recv_seq);
        }
        let plain = self
            .open
            .open(seq, aad, sealed)
            .map_err(|e| anyhow!("frame refused: {e}"))?;
        self.recv_seq = seq.wrapping_add(1);
        Ok(Some(plain))
    }

    /// Limits how long [`Channel::recv`] waits.
    pub fn set_read_timeout(&self, timeout: Option<Duration>) -> Result<()> {
        self.reader.get_ref().set_read_timeout(timeout)?;
        Ok(())
    }

    pub fn peer_addr(&self) -> Option<std::net::SocketAddr> {
        self.writer.peer_addr().ok()
    }
}

/// Dials: proves we hold `key` and checks the listener does too.
pub fn dial(stream: TcpStream, key: &Psk) -> Result<Channel> {
    stream.set_read_timeout(Some(HANDSHAKE_TIMEOUT))?;
    stream.set_nodelay(true).ok();
    let mut writer = stream.try_clone()?;
    let mut reader = BufReader::new(stream);
    let ours = nonce()?;
    write_line(
        &mut writer,
        &Hand::Hello {
            v: MESH_VERSION,
            fp: key.fingerprint(),
            nonce: hex::encode(ours),
        },
    )?;
    let theirs = match read_line(&mut reader)? {
        Hand::Challenge { nonce, proof } => {
            let theirs = parse_nonce(&nonce)?;
            if !check(key, LISTENER_LABEL, &ours, &theirs, &proof) {
                bail!("the other device does not hold this pairing's key");
            }
            theirs
        }
        Hand::Deny { reason } => bail!("refused: {reason}"),
        _ => bail!("unexpected handshake message"),
    };
    write_line(
        &mut writer,
        &Hand::Proof {
            proof: tag(key, DIALER_LABEL, &ours, &theirs),
        },
    )?;
    reader.get_ref().set_read_timeout(None)?;
    Channel::new(reader, link_key(key, &ours, &theirs), true)
}

/// Answers a dialer. `lookup` maps the fingerprint it presents to a key (and whatever the caller
/// wants back, such as which peer or invite it was).
pub fn accept<T>(
    stream: TcpStream,
    lookup: impl FnOnce(&str) -> Option<(Psk, T)>,
) -> Result<(Channel, T)> {
    stream.set_read_timeout(Some(HANDSHAKE_TIMEOUT))?;
    stream.set_nodelay(true).ok();
    let mut writer = stream.try_clone()?;
    let mut reader = BufReader::new(stream);
    let (v, fp, theirs) = match read_line(&mut reader)? {
        Hand::Hello { v, fp, nonce } => (v, fp, parse_nonce(&nonce)?),
        _ => bail!("expected MESH_HELLO"),
    };
    if v != MESH_VERSION {
        let reason =
            format!("this device speaks mesh version {MESH_VERSION}, not {v}; update Talkee");
        let _ = write_line(&mut writer, &Hand::Deny { reason: reason.clone() });
        bail!(reason);
    }
    let Some((key, who)) = lookup(&fp) else {
        // Same answer for "unknown" and "expired": nothing to learn by probing.
        let _ = write_line(
            &mut writer,
            &Hand::Deny {
                reason: "not paired with this device (or the invite expired)".into(),
            },
        );
        bail!("a device presented a key this one does not know");
    };
    let ours = nonce()?;
    write_line(
        &mut writer,
        &Hand::Challenge {
            nonce: hex::encode(ours),
            proof: tag(&key, LISTENER_LABEL, &theirs, &ours),
        },
    )?;
    match read_line(&mut reader)? {
        Hand::Proof { proof } if check(&key, DIALER_LABEL, &theirs, &ours, &proof) => {}
        _ => bail!("the dialing device could not prove it holds the key"),
    }
    reader.get_ref().set_read_timeout(None)?;
    Ok((Channel::new(reader, link_key(&key, &theirs, &ours), false)?, who))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    fn pair() -> (TcpStream, TcpStream) {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let a = TcpStream::connect(l.local_addr().unwrap()).unwrap();
        let (b, _) = l.accept().unwrap();
        (a, b)
    }

    #[test]
    fn both_sides_talk_after_a_handshake() {
        let key = Psk::generate().unwrap();
        let k2 = key.clone();
        let (a, b) = pair();
        let server = std::thread::spawn(move || {
            let (mut ch, who) =
                accept(b, |fp| (fp == k2.fingerprint()).then(|| (k2.clone(), 7))).unwrap();
            assert_eq!(who, 7);
            let msg = ch.recv().unwrap().unwrap();
            ch.send(&[msg.as_slice(), b" back"].concat()).unwrap();
            ch.send(b"second").unwrap();
        });
        let mut ch = dial(a, &key).unwrap();
        ch.send(b"hello").unwrap();
        assert_eq!(ch.recv().unwrap().unwrap(), b"hello back");
        assert_eq!(ch.recv().unwrap().unwrap(), b"second");
        server.join().unwrap();
        assert!(ch.recv().unwrap().is_none(), "a clean close reads as None");
    }

    #[test]
    fn a_wrong_key_is_refused_on_both_sides() {
        let real = Psk::generate().unwrap();
        let fake = Psk::generate().unwrap();
        let (a, b) = pair();
        // The listener accepts the fingerprint but holds a different key: the dialer must refuse.
        let server = std::thread::spawn(move || accept(b, |_| Some((real, ()))).map(|_| ()));
        assert!(dial(a, &fake).is_err());
        assert!(server.join().unwrap().is_err());
    }

    #[test]
    fn an_unknown_key_is_denied() {
        let key = Psk::generate().unwrap();
        let (a, b) = pair();
        let server = std::thread::spawn(move || accept::<()>(b, |_| None).map(|_| ()));
        let err = dial(a, &key).err().unwrap().to_string();
        assert!(err.contains("not paired"), "{err}");
        assert!(server.join().unwrap().is_err());
    }
}
