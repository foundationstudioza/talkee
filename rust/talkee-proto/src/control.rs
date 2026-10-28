//! Control-plane messages: newline-delimited JSON over TCP.
//!
//! Defined before they were needed, so the contract was written down exactly once
//! instead of being reverse-engineered out of two implementations later. It is the
//! pairing handshake that runs here, and that handshake is what decides whether a
//! single audio datagram is accepted — so this file is part of the security
//! boundary, not just plumbing.
//!
//! The JSON framing lives here too, so that a caller has one way to read a line and
//! one limit on how long a line may be: a peer that streams bytes without ever
//! sending a newline must not be able to grow our buffer without bound.

use crate::{SampleFormat, PROTOCOL_VERSION};
use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader, Read, Write};

/// One line of the control stream. `type` is the serde tag.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ControlMsg {
    /// Phone announces itself immediately after connecting.
    Hello {
        /// Protocol version the phone speaks.
        ver: u8,
        /// Stable per-install device id.
        dev_id: String,
        /// Human-readable device name shown in the desktop UI.
        name: String,
        /// Capability strings, e.g. `"opus"`, `"aes-gcm"`.
        caps: Vec<String>,
    },
    /// Desktop sends a nonce the phone must prove knowledge of the PSK against.
    Challenge {
        /// 32 random bytes, hex encoded. Fresh for every control connection, which
        /// is what makes a recorded `PAIR` useless to an attacker replaying it.
        nonce: String,
    },
    /// Phone proves it holds the pairing key.
    Pair {
        /// `HMAC-SHA256(psk, "talkee-pair-v1" || nonce || session_id)`, hex encoded.
        ///
        /// Computed over the challenge's raw 32 bytes, not over its hex text.
        proof: String,
        /// 4 random bytes, hex encoded, chosen by the sender for this stream.
        ///
        /// Covered by the proof, so the receiver knows it is authentic, and used to
        /// derive a fresh session key. It has to exist because the audio nonce is
        /// only `seq` plus a direction bit: a phone that restarts begins again at
        /// zero while a persisted pairing key does not change, and a repeated GCM
        /// nonce under one key is a catastrophic failure rather than a small one.
        session: String,
    },
    /// The receiver refused the handshake, and will accept no audio on this
    /// connection.
    ///
    /// Sent before closing, so a wrong key reads as a rejected pairing rather than
    /// as a timeout. `reason` never contains key material.
    Rejected {
        /// Human-readable reason, safe to show a user.
        reason: String,
    },
    /// Desktop tells the phone exactly how to encode audio.
    Config {
        /// Hz.
        sample_rate: u32,
        /// Channel count to send.
        channels: u8,
        /// Payload encoding both sides agreed on.
        format: SampleFormat,
        /// Opus frame duration in ms (M1+).
        frame_ms: u16,
        /// Target Opus bitrate (M1+).
        bitrate: u32,
        /// Request in-band FEC (M1+).
        fec: bool,
        /// Requested capture source id on the phone, e.g. `"unprocessed"`.
        source: String,
    },
    /// Begin streaming.
    Start,
    /// Stop streaming but keep the connection for a fast resume.
    Stop,
    /// The desktop's live settings, pushed to the phone on connect and on every change.
    ///
    /// The desktop is the single source of truth: a phone that wants a setting changed asks with
    /// [`ControlMsg::Set`] and learns the result from the next `State`, so the two sides cannot
    /// disagree about what is on.
    State {
        /// Audio is muted: the phone stops capturing, the PC plays nothing.
        paused: bool,
        /// Noise suppression is applied to the phone's audio.
        noise_suppression: bool,
        /// Echo cancellation is applied to the phone's audio.
        echo_cancel: bool,
    },
    /// The phone asks the desktop to change a setting. Fields left out are left alone.
    Set {
        /// Mute or unmute.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        paused: Option<bool>,
        /// Noise suppression on or off.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        noise_suppression: Option<bool>,
        /// Echo cancellation on or off.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        echo_cancel: Option<bool>,
    },
    /// Liveness probe; the peer echoes `ts`.
    Ping {
        /// Sender's timestamp in ms.
        ts: u32,
        /// The sender's last measured round trip to the peer, in ms. Only the side that sent a ping
        /// can time its echo, so this is how the other side learns the figure. Left out until there
        /// has been an echo to time, and by senders that predate it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        rtt_ms: Option<u32>,
    },
    /// Periodic telemetry from the phone, for the desktop stats panel.
    Stats {
        /// Frames the phone successfully sent.
        frames_sent: u64,
        /// Frames dropped locally (e.g. encoder backpressure).
        frames_dropped: u64,
        /// Phone's battery percentage, or -1 if unknown.
        battery_pct: i32,
    },
}

/// Longest control line we will read.
///
/// A `CONFIG` is a few hundred bytes and a `HELLO` is smaller still, so 4 KiB is
/// generous. The cap exists because the alternative is a peer that sends bytes and
/// never a newline, which is a memory-exhaustion attack on the desktop's main loop.
pub const MAX_CONTROL_LINE: usize = 4 * 1024;

/// Why a control line could not be read or written.
#[derive(Debug)]
pub enum ControlError {
    /// The socket failed.
    Io(std::io::Error),
    /// The line was well-formed UTF-8 but not a message we know.
    Malformed(serde_json::Error),
    /// The line ran past [`MAX_CONTROL_LINE`] without a newline.
    TooLong(usize),
}

impl std::fmt::Display for ControlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ControlError::Io(e) => write!(f, "control channel I/O failed: {e}"),
            ControlError::Malformed(e) => {
                write!(f, "control message is not one this build understands: {e}")
            }
            ControlError::TooLong(len) => write!(
                f,
                "control line of at least {len} bytes exceeds the {MAX_CONTROL_LINE}-byte cap"
            ),
        }
    }
}

impl std::error::Error for ControlError {}

impl From<std::io::Error> for ControlError {
    fn from(e: std::io::Error) -> Self {
        ControlError::Io(e)
    }
}

impl From<serde_json::Error> for ControlError {
    fn from(e: serde_json::Error) -> Self {
        ControlError::Malformed(e)
    }
}

impl ControlMsg {
    /// Convenience constructor for the version-negotiation opener.
    pub fn hello(dev_id: impl Into<String>, name: impl Into<String>, caps: Vec<String>) -> Self {
        ControlMsg::Hello {
            ver: PROTOCOL_VERSION,
            dev_id: dev_id.into(),
            name: name.into(),
            caps,
        }
    }

    /// Serializes as one line, newline included.
    pub fn to_line(&self) -> Result<String, ControlError> {
        let mut line = serde_json::to_string(self)?;
        line.push('\n');
        Ok(line)
    }

    /// Parses one line, tolerating the newline and any surrounding whitespace.
    ///
    /// An unknown `type` is a hard error, as the protocol says: a newer peer
    /// speaking a message we do not understand is a version mismatch, and skipping
    /// it silently is how two sides diverge without anyone noticing.
    pub fn from_line(line: &str) -> Result<Self, ControlError> {
        Ok(serde_json::from_str(line.trim())?)
    }

    /// Writes one line and flushes it.
    ///
    /// Flushing matters: a `CHALLENGE` that sits in a buffer while the far end
    /// waits is a pairing failure that looks like a network problem.
    pub fn write_to(&self, writer: &mut impl Write) -> Result<(), ControlError> {
        writer.write_all(self.to_line()?.as_bytes())?;
        writer.flush()?;
        Ok(())
    }
}

/// Reads one control line, refusing one that never ends.
///
/// The read is capped *during* the read, not after it, so a peer that sends an
/// endless line cannot make us allocate the whole thing first.
pub fn read_line<R: BufRead>(reader: &mut R, buf: &mut String) -> Result<usize, ControlError> {
    buf.clear();
    let read = reader.take(MAX_CONTROL_LINE as u64 + 1).read_line(buf)?;
    if buf.len() > MAX_CONTROL_LINE {
        return Err(ControlError::TooLong(buf.len()));
    }
    Ok(read)
}

/// Wraps a stream for line-oriented reading.
pub fn buffered_reader<R: Read>(reader: R) -> BufReader<R> {
    BufReader::new(reader)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hello_roundtrips_as_json() {
        let msg = ControlMsg::hello("abc", "Pixel", vec!["opus".into()]);
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("\"type\":\"HELLO\""), "got {json}");
        assert_eq!(serde_json::from_str::<ControlMsg>(&json).unwrap(), msg);
    }

    #[test]
    fn config_roundtrips_as_json() {
        let msg = ControlMsg::Config {
            sample_rate: 48_000,
            channels: 1,
            format: SampleFormat::F32,
            frame_ms: 10,
            bitrate: 64_000,
            fec: true,
            source: "unprocessed".into(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert_eq!(serde_json::from_str::<ControlMsg>(&json).unwrap(), msg);
    }

    #[test]
    fn unknown_message_type_is_rejected() {
        // Forward compatibility: a newer peer sending a message type we do not
        // know must be a hard error we can log, not a silent misparse.
        assert!(serde_json::from_str::<ControlMsg>(r#"{"type":"TELEPATHY"}"#).is_err());
    }

    #[test]
    fn unit_variants_render_as_bare_tags() {
        assert_eq!(serde_json::to_string(&ControlMsg::Start).unwrap(), r#"{"type":"START"}"#);
    }

    #[test]
    fn a_pair_message_carries_the_session_id_the_proof_covers() {
        let msg = ControlMsg::Pair {
            proof: "ab".repeat(32),
            session: "01020304".into(),
        };
        let line = msg.to_line().unwrap();
        assert!(line.ends_with('\n'), "a control line ends with a newline");
        assert!(line.contains("\"type\":\"PAIR\""), "got {line}");
        assert_eq!(ControlMsg::from_line(&line).unwrap(), msg);
    }

    #[test]
    fn a_rejection_round_trips() {
        let msg = ControlMsg::Rejected {
            reason: "the pairing proof did not verify".into(),
        };
        assert_eq!(ControlMsg::from_line(&msg.to_line().unwrap()).unwrap(), msg);
        assert!(msg.to_line().unwrap().contains("\"type\":\"REJECTED\""));
    }

    #[test]
    fn writing_a_message_flushes_it() {
        // A CHALLENGE left in a buffer is a pairing that never happens, and it would
        // be diagnosed as a network fault.
        let mut sink = Vec::new();
        ControlMsg::Challenge {
            nonce: "00".repeat(32),
        }
        .write_to(&mut sink)
        .unwrap();
        assert_eq!(sink.last(), Some(&b'\n'));
        assert_eq!(
            ControlMsg::from_line(std::str::from_utf8(&sink).unwrap()).unwrap(),
            ControlMsg::Challenge { nonce: "00".repeat(32) }
        );
    }

    #[test]
    fn a_line_that_never_ends_is_cut_off() {
        // The attack this closes: a peer that sends bytes and never a newline, on a
        // connection we are holding open waiting for a handshake.
        let flood = "x".repeat(MAX_CONTROL_LINE * 2);
        let mut reader = BufReader::new(flood.as_bytes());
        let mut buf = String::new();
        let error = read_line(&mut reader, &mut buf).unwrap_err();
        assert!(matches!(error, ControlError::TooLong(_)), "got {error}");

        // ...and two normal lines still come back one at a time.
        let script = b"{\"type\":\"START\"}\n{\"type\":\"STOP\"}\n".as_slice();
        let mut reader = BufReader::new(script);
        let mut buf = String::new();
        assert!(read_line(&mut reader, &mut buf).unwrap() > 0);
        assert_eq!(ControlMsg::from_line(&buf).unwrap(), ControlMsg::Start);
        assert!(read_line(&mut reader, &mut buf).unwrap() > 0);
        assert_eq!(ControlMsg::from_line(&buf).unwrap(), ControlMsg::Stop);
        assert_eq!(read_line(&mut reader, &mut buf).unwrap(), 0, "then end of input");
        assert!(matches!(
            ControlMsg::from_line("not json").unwrap_err(),
            ControlError::Malformed(_)
        ));
    }
}
