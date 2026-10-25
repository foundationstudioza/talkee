//! The `talkee://pair?...` deep link: how a key gets from a desktop to a phone.
//!
//! # Why this file is defensive
//!
//! This is the only place where *text* becomes a key. Everything else that reaches
//! the key material comes from the OS RNG. It is also the one input a user can be
//! tricked into pasting — a link in a chat message, a QR sticker on a cafe table —
//! so the parser is strict about what it must have, and refuses rather than guesses:
//!
//! * a key has to decode to exactly 32 bytes, or it is not a key;
//! * a repeated parameter is an error, never "last one wins": two parsers
//!   disagreeing about which `k` is real is a pairing failure that presents as a
//!   wrong key, and an attacker who can smuggle one past a lax parser wins;
//! * the name may hold only printable ASCII, so a hostile link cannot inject
//!   terminal escape sequences into the console that prints it;
//! * nothing here may panic. This code parses bytes a stranger chose, and a panic
//!   in a decoder is a denial of service.
//!
//! # Why base64url and not a numeric code
//!
//! A 6-digit code is 20 bits: enough to keep an honest user out, not enough to keep
//! a script out, and scripts are what actually scan LANs. The key travels in the
//! link as base64url because the link is pasted into terminals, chat windows and
//! QR codes, where `+` and `/` do not survive unescaped.

use crate::psk::{Psk, PSK_LEN};
use anyhow::{anyhow, bail, Result};

/// Version of the deep-link payload.
///
/// Independent of `PROTOCOL_VERSION`: the link is an artefact handed between apps,
/// the protocol is what goes on the wire, and the two will not always move together.
pub const LINK_VERSION: u8 = 1;

/// Scheme every pairing link starts with.
pub const LINK_SCHEME: &str = "talkee://";

/// Refuse to look at anything longer than this. A pairing link is about 80 bytes;
/// the cap exists so that a hostile 100 MB "link" is rejected by an integer
/// comparison rather than by allocating a copy of it.
pub const MAX_LINK_LEN: usize = 2048;

/// Longest host we accept: a DNS name is at most 253 characters, and an IPv6
/// literal is shorter than that.
const MAX_HOST_LEN: usize = 253;

/// Longest display name we accept. 64 characters is someone"s idea of a device
/// name; anything longer is not a name, it is a payload.
const MAX_NAME_LEN: usize = 64;

/// Longest `k=` text we will even attempt to decode: 43 base64url characters, 44
/// with padding.
const MAX_KEY_TEXT_LEN: usize = 64;

/// A parsed (or built) pairing link.
///
/// `Debug` is derived and safe to print: [`Psk`]"s own `Debug` is redacted, so the
/// key cannot leak through this type either.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairLink {
    /// Link format version, `1` today.
    pub version: u8,
    /// Host address or DNS name, as it goes in `h=`.
    pub host: String,
    /// Control port, as it goes in `p=`. Audio is the control port plus one; see
    /// [`PairLink::audio_port`].
    pub control_port: u16,
    /// The host"s fingerprint, as it goes in `fpid=`.
    pub fingerprint: String,
    /// Human-readable host name, as it goes in `name=`.
    pub name: String,
    /// The pairing key. The reason the link exists.
    pub psk: Psk,
}
impl PairLink {
    /// Builds a link for a host, deriving `fpid` from the key.
    ///
    /// Validates what it builds, so this constructor cannot produce a link that
    /// [`PairLink::parse`] would reject. A host that prints a link a phone refuses
    /// to read is a bug that costs an afternoon of network debugging.
    pub fn new(
        host: impl Into<String>,
        control_port: u16,
        name: impl Into<String>,
        psk: Psk,
    ) -> Result<Self> {
        let link = Self {
            version: LINK_VERSION,
            host: host.into(),
            control_port,
            fingerprint: psk.fingerprint(),
            name: name.into(),
            psk,
        };
        link.validate()?;
        Ok(link)
    }

    /// Parses a link, or explains why it is not one.
    ///
    /// Never panics, and never echoes the input in an error message: the input
    /// carries the key.
    pub fn parse(text: &str) -> Result<Self> {
        parse_link(text)
    }

    /// The port audio is sent to.
    ///
    /// The link carries one port — the control port, which is the one a peer must
    /// reach first — and audio is conventionally the next one up, matching the
    /// defaults (47800 control, 47801 audio). A sender that knows better overrides
    /// this; a sender that only has the link uses it.
    pub fn audio_port(&self) -> u16 {
        self.control_port.saturating_add(1)
    }

    /// Renders the link a user scans or pastes.
    pub fn to_uri(&self) -> String {
        format!(
            "{LINK_SCHEME}pair?v={}&h={}&p={}&fpid={}&name={}&k={}",
            self.version,
            self.host,
            self.control_port,
            self.fingerprint,
            percent_encode(&self.name),
            self.psk.to_base64url(),
        )
    }

    /// Re-checks every field, so `new` and `parse` enforce one set of rules.
    fn validate(&self) -> Result<()> {
        if self.version != LINK_VERSION {
            bail!(
                "this build speaks pairing link version {LINK_VERSION}, not {}",
                self.version
            );
        }
        validate_host(&self.host)?;
        if self.control_port == 0 {
            bail!("port 0 is not a port a peer can connect to");
        }
        validate_fingerprint(&self.fingerprint)?;
        validate_name(&self.name)?;
        Ok(())
    }
}

/// A host is an IPv4 address, an IPv6 literal or a DNS name, and nothing else.
fn validate_host(host: &str) -> Result<()> {
    if host.is_empty() {
        bail!("the link does not name a host");
    }
    if host.len() > MAX_HOST_LEN {
        bail!("the host name is {} characters, longer than the {MAX_HOST_LEN} a DNS name allows", host.len());
    }
    if !host
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b':' | b'[' | b']' | b'_'))
    {
        bail!("the host name contains a character that cannot appear in an address or a DNS name");
    }
    Ok(())
}

/// The fingerprint is hex, even length, and short. Its *value* is not checked
/// against the key: a phone must be free to treat `fpid` as an opaque label from a
/// host whose fingerprinting scheme it does not need to know.
fn validate_fingerprint(fingerprint: &str) -> Result<()> {
    if fingerprint.len() < 8 || fingerprint.len() > 64 || fingerprint.len() % 2 != 0 {
        bail!("the fpid parameter is not 8 to 64 hex characters");
    }
    if !fingerprint.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!("the fpid parameter contains a character that is not hex");
    }
    Ok(())
}

/// A display name is printed in a terminal and in a phone UI, so it is restricted
/// to characters that cannot do anything there. A newline or an ANSI escape in a
/// device name is how a hostile link rewrites the screen of the person pairing.
fn validate_name(name: &str) -> Result<()> {
    if name.is_empty() {
        bail!("the link does not name the host");
    }
    if name.len() > MAX_NAME_LEN {
        bail!("the host name is longer than {MAX_NAME_LEN} characters");
    }
    if !name.bytes().all(|b| b.is_ascii_graphic() || b == b' ') {
        bail!("the host name contains a non-printable character");
    }
    Ok(())
}
/// Parses a link, or explains why it is not one.
///
/// Never panics and never echoes the input in its error: the input contains the key.
fn parse_link(text: &str) -> Result<PairLink> {
    let text = text.trim();
    if text.len() > MAX_LINK_LEN {
        bail!("a pairing link cannot be {} bytes long", text.len());
    }
    let rest = text
        .strip_prefix(LINK_SCHEME)
        .ok_or_else(|| anyhow!("a pairing link starts with {LINK_SCHEME}"))?;
    let (action, query) = match rest.split_once('?') {
        Some((action, query)) => (action, query),
        None => (rest, ""),
    };
    if action != "pair" {
        bail!("the only pairing link action is \"pair\", not {action:?}");
    }

    let mut version = None;
    let mut host = None;
    let mut port = None;
    let mut fingerprint = None;
    let mut name = None;
    let mut key = None;

    for field in query.split('&') {
        if field.is_empty() {
            // Tolerated: a trailing `&`, or one from a URI builder that always
            // appends a separator.
            continue;
        }
        let (parameter, value) = field
            .split_once('=')
            .ok_or_else(|| anyhow!("a pairing link parameter is not a name=value pair"))?;
        match parameter {
            "v" => set_once(&mut version, "v", value)?,
            "h" => set_once(&mut host, "h", value)?,
            "p" => set_once(&mut port, "p", value)?,
            "fpid" => set_once(&mut fingerprint, "fpid", value)?,
            "name" => set_once(&mut name, "name", value)?,
            "k" => set_once(&mut key, "k", value)?,
            // Unknown parameters are ignored rather than refused: a newer host may
            // add fields, and refusing them would mean a phone could not read a
            // link from a desktop that is one version ahead. The fields that carry
            // meaning here are all checked strictly.
            "" => bail!("a pairing link parameter has an empty name"),
            _ => {}
        }
    }

    let version_text = version.ok_or_else(|| anyhow!("the link has no v= parameter"))?;
    let version: u8 = version_text
        .parse()
        .map_err(|_| anyhow!("the v= parameter is not a small number"))?;
    if version != LINK_VERSION {
        bail!("pairing link version {version} is not the version this build speaks ({LINK_VERSION})");
    }

    let host = host.ok_or_else(|| anyhow!("the link has no h= parameter"))?;
    validate_host(&host)?;

    let port_text = port.ok_or_else(|| anyhow!("the link has no p= parameter"))?;
    let control_port: u16 = port_text
        .parse()
        .map_err(|_| anyhow!("the p= parameter is not a port number"))?;
    if control_port == 0 {
        bail!("port 0 is not a port a peer can connect to");
    }

    let fingerprint = fingerprint.ok_or_else(|| anyhow!("the link has no fpid= parameter"))?;
    validate_fingerprint(&fingerprint)?;

    let name = percent_decode(&name.ok_or_else(|| anyhow!("the link has no name= parameter"))?)?;
    validate_name(&name)?;

    let key_text = key.ok_or_else(|| anyhow!("the link has no k= parameter, so it carries no key"))?;
    if key_text.len() > MAX_KEY_TEXT_LEN {
        bail!("the k= parameter is longer than {MAX_KEY_TEXT_LEN} characters");
    }
    let psk = Psk::from_base64url(&key_text)
        .map_err(|_| anyhow!("the k= parameter is not a base64url {PSK_LEN}-byte key"))?;

    let link = PairLink {
        version,
        host,
        control_port,
        fingerprint,
        name,
        psk,
    };
    link.validate()?;
    Ok(link)
}
/// Records a parameter, refusing a second copy of one we already have.
///
/// "Last one wins" is the behaviour that lets a lenient parser and a strict one
/// disagree about a key, so both here pick the same answer: neither.
fn set_once(slot: &mut Option<String>, name: &str, value: &str) -> Result<()> {
    if slot.is_some() {
        bail!("the pairing link carries {name}= twice, and a parser must not guess which is real");
    }
    *slot = Some(value.to_string());
    Ok(())
}

/// Characters a URI may carry unescaped (RFC 3986 unreserved).
fn is_unreserved(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~')
}

/// Escapes everything outside the unreserved set, so a name with a space, an `&`
/// or a `%` in it cannot change the meaning of the query string.
fn percent_encode(input: &str) -> String {
    const HEX_UPPER: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(input.len());
    for byte in input.bytes() {
        if is_unreserved(byte) {
            out.push(byte as char);
        } else {
            out.push('%');
            out.push(HEX_UPPER[(byte >> 4) as usize] as char);
            out.push(HEX_UPPER[(byte & 0x0F) as usize] as char);
        }
    }
    out
}

/// The inverse of [`percent_encode`], strict about incomplete escapes.
///
/// `+` is *not* treated as a space: that is form encoding, not URI encoding, and a
/// decoder that guesses would let two different links mean the same name.
fn percent_decode(input: &str) -> Result<String> {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        if byte != b'%' {
            out.push(byte);
            index += 1;
            continue;
        }
        let high = bytes
            .get(index + 1)
            .and_then(|b| hex_value(*b))
            .ok_or_else(|| anyhow!("a percent escape in the link is not two hex digits"))?;
        let low = bytes
            .get(index + 2)
            .and_then(|b| hex_value(*b))
            .ok_or_else(|| anyhow!("a percent escape in the link is not two hex digits"))?;
        out.push((high << 4) | low);
        index += 3;
    }
    String::from_utf8(out).map_err(|_| anyhow!("the name in the link is not valid UTF-8"))
}

/// Value of one hex digit, or `None`.
fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    /// The key from the crate"s known-answer vectors: bytes 0x00..0x1F.
    fn psk() -> Psk {
        let mut bytes = [0u8; PSK_LEN];
        for (i, b) in bytes.iter_mut().enumerate() {
            *b = i as u8;
        }
        Psk::from_slice(&bytes).unwrap()
    }

    /// The link a host running on 192.168.1.42 would print, with the key above.
    fn example() -> PairLink {
        PairLink::new("192.168.1.42", 47_800, "DESKTOP-ABC", psk()).unwrap()
    }

    #[test]
    fn a_link_round_trips_through_its_text_form() {
        let link = example();
        let text = link.to_uri();
        assert!(text.starts_with("talkee://pair?v=1&h=192.168.1.42&p=47800&"));
        assert_eq!(PairLink::parse(&text).unwrap(), link);
        // Stable: rendering a parsed link gives back the same bytes, so a
        // fingerprint comparison downstream is not fooled by formatting.
        assert_eq!(PairLink::parse(&text).unwrap().to_uri(), text);
        assert_eq!(link.audio_port(), 47_801);
    }

    #[test]
    fn a_name_with_a_space_and_an_ampersand_survives_the_round_trip() {
        let link = PairLink::new("desk.local", 41_000, "Talkee on DESKTOP & co", psk()).unwrap();
        let text = link.to_uri();
        assert!(text.contains("name=Talkee%20on%20DESKTOP%20%26%20co"), "got {text}");
        assert_eq!(PairLink::parse(&text).unwrap().name, "Talkee on DESKTOP & co");
        assert_eq!(PairLink::parse(&text).unwrap(), link);
    }

    #[test]
    fn the_documented_example_parses() {
        // Literally the line in docs/PROTOCOL.md, so the document and the parser
        // cannot drift apart unnoticed. `fpid` is shape-checked rather than
        // recomputed, so any 8 hex characters work here.
        let text = "talkee://pair?v=1&h=192.168.1.42&p=47800&fpid=66687aad\
                    &name=DESKTOP&k=AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8";
        let link = PairLink::parse(text).unwrap();
        assert_eq!(link.host, "192.168.1.42");
        assert_eq!(link.control_port, 47_800);
        assert_eq!(link.name, "DESKTOP");
        assert_eq!(link.fingerprint, "66687aad");
        assert_eq!(link.psk, psk());
    }

    #[test]
    fn unknown_parameters_are_ignored_so_a_newer_host_still_pairs() {
        let text = format!("{}&future=1", example().to_uri());
        assert_eq!(PairLink::parse(&text).unwrap(), example());
    }

    #[test]
    fn a_repeated_parameter_is_refused() {
        // An attacker who can append a second k= to a link a user pastes would
        // otherwise choose which key the phone stores, in whichever parser is the
        // more forgiving.
        let text = example().to_uri();
        let doubled = text.replace("&k=", "&k=AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA&k=");
        assert!(PairLink::parse(&doubled).is_err());
        for parameter in ["v", "h", "p", "fpid", "name", "k"] {
            let with_duplicate = format!("{text}&{parameter}=1");
            assert!(
                PairLink::parse(&with_duplicate).is_err(),
                "a repeated {parameter}= must be refused"
            );
        }
    }
    #[test]
    fn malformed_links_are_refused_rather_than_guessed_at() {
        let text = example().to_uri();
        let mut cases: Vec<String> = vec![
            "".into(),
            "talkee".into(),
            "talkee:".into(),
            "talkee://".into(),
            "talkee://pair".into(),
            "talkee://pair?".into(),
            "talkee://PAIR?v=1".into(),
            "talkee://pairing?v=1".into(),
            "http://pair?v=1&h=1.2.3.4&p=1&fpid=00000000&name=x&k=AAAA".into(),
            " talkee://pair?v=1".into(),
        ];
        // Every required parameter, removed in turn.
        for parameter in ["v", "h", "p", "fpid", "name", "k"] {
            cases.push(remove_parameter(&text, parameter));
        }
        // Values that are the right shape and the wrong thing.
        cases.extend([
            set_parameter(&text, "v", "2"),
            set_parameter(&text, "v", "256"),
            set_parameter(&text, "v", ""),
            set_parameter(&text, "v", "one"),
            set_parameter(&text, "h", ""),
            set_parameter(&text, "h", "192.168.1.42/../../etc"),
            set_parameter(&text, "h", "host with spaces"),
            set_parameter(&text, "h", "DESKTOP&#58;"),
            set_parameter(&text, "p", "0"),
            set_parameter(&text, "p", "65536"),
            set_parameter(&text, "p", "-1"),
            set_parameter(&text, "p", "forty-seven-thousand"),
            set_parameter(&text, "p", ""),
            set_parameter(&text, "fpid", ""),
            set_parameter(&text, "fpid", "0123"),
            set_parameter(&text, "fpid", "0123456"),
            set_parameter(&text, "fpid", "zzzzzzzz"),
            set_parameter(&text, "name", ""),
            set_parameter(&text, "name", "%00"),
            set_parameter(&text, "name", "%0A"),
            set_parameter(&text, "name", "%1B%5B31m"),
            set_parameter(&text, "name", "%E2%80%A8"),
            set_parameter(&text, "name", &"x".repeat(MAX_NAME_LEN + 1)),
            set_parameter(&text, "name", "%zz"),
            set_parameter(&text, "name", "%4"),
            set_parameter(&text, "name", "%"),
            set_parameter(&text, "k", ""),
            set_parameter(&text, "k", "!!!!"),
            set_parameter(&text, "k", "AAAA"),
            set_parameter(&text, "k", &"A".repeat(42)),
            set_parameter(&text, "k", &"A".repeat(MAX_KEY_TEXT_LEN + 1)),
            set_parameter(&text, "k", "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=="),
        ]);
        // A parameter with no `=` at all, and one that is only a name.
        cases.push(format!("{text}&name"));
        cases.push(format!("{text}&=5"));
        // Absurdly long, and interminable-parameter attacks.
        cases.push(format!("talkee://pair?{}", "a".repeat(MAX_LINK_LEN)));

        for case in &cases {
            assert!(PairLink::parse(case).is_err(), "parsed a malformed link: {case}");
        }
    }
    /// Rebuilds a link with one parameter replaced.
    fn set_parameter(link: &str, parameter: &str, value: &str) -> String {
        let (head, query) = link.split_once('?').expect("the example link has a query string");
        let needle = format!("{parameter}=");
        let kept: Vec<&str> = query.split('&').filter(|f| !f.starts_with(&needle)).collect();
        format!("{head}?{}&{parameter}={value}", kept.join("&"))
    }

    /// Rebuilds a link without one parameter.
    fn remove_parameter(link: &str, parameter: &str) -> String {
        let (head, query) = link.split_once('?').expect("the example link has a query string");
        let needle = format!("{parameter}=");
        let kept: Vec<&str> = query.split('&').filter(|f| !f.starts_with(&needle)).collect();
        format!("{head}?{}", kept.join("&"))
    }

    #[test]
    fn every_prefix_of_a_valid_link_is_handled_without_panicking() {
        // A truncated link is what arrives when a chat application wraps a long
        // paste, and a decoder that panics on it takes the phone app down.
        let text = example().to_uri();
        let mut parsed = 0;
        for end in 0..=text.len() {
            if PairLink::parse(&text[..end]).is_ok() {
                parsed += 1;
                assert_eq!(end, text.len(), "only the complete link may parse");
            }
        }
        assert_eq!(parsed, 1);
    }

    #[test]
    fn arbitrary_text_is_handled_without_panicking() {
        // Not a replacement for a fuzzer, but the shapes a fuzzer finds first:
        // truncated, over-long, escape-heavy, and structurally plausible but wrong.
        // A decoder fed by a QR code somebody else printed has to survive all of it.
        const ALPHABET: &[u8] = b"talkee:/?&=%ABCabc0123.-_~+[] ";
        let mut state: u64 = 0x2545_F491_4F6C_DD1D;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let mut parsed_ok = 0;
        for round in 0..3_000 {
            let len = (next() % 160) as usize;
            let mut candidate = String::with_capacity(len + 16);
            if round % 2 == 0 {
                candidate.push_str("talkee://pair?");
            }
            for _ in 0..len {
                candidate.push(ALPHABET[(next() % ALPHABET.len() as u64) as usize] as char);
            }
            if let Ok(link) = PairLink::parse(&candidate) {
                parsed_ok += 1;
                // Anything that parses must survive a round trip, or a phone and a
                // host would disagree about what the link said.
                let text = link.to_uri();
                assert_eq!(PairLink::parse(&text).unwrap(), link);
            }
        }
        assert!(parsed_ok < 3_000, "the generator should not produce only valid links");
    }
    #[test]
    fn errors_never_echo_the_key() {
        // A link pasted into a chat window and then into a bug report must not turn
        // a syntax error into a key disclosure.
        let link = example();
        let key_text = link.psk.to_base64url();
        let broken = format!("{}&p=0", link.to_uri());
        let err = PairLink::parse(&broken).unwrap_err().to_string();
        assert!(!err.contains(&key_text), "the error echoed the key: {err}");

        let truncated = format!("talkee://pair?k={key_text}");
        let err = PairLink::parse(&truncated).unwrap_err().to_string();
        assert!(!err.contains(&key_text), "the error echoed the key: {err}");

        let err = PairLink::parse(&format!("{truncated}nonsense")).unwrap_err().to_string();
        assert!(!err.contains(&key_text), "the error echoed the key: {err}");
    }

    #[test]
    fn building_a_link_validates_what_it_builds() {
        // The host prints what this returns, so a link the phone cannot read has to
        // be impossible to construct rather than merely unlikely.
        assert!(PairLink::new("", 47_800, "DESKTOP", psk()).is_err());
        assert!(PairLink::new("192.168.1.42", 0, "DESKTOP", psk()).is_err());
        assert!(PairLink::new("192.168.1.42", 47_800, "", psk()).is_err());
        assert!(PairLink::new("192.168.1.42", 47_800, "bad\nname", psk()).is_err());
        assert!(PairLink::new("192.168.1.42", 47_800, "bad\u{1b}[31m", psk()).is_err());
        assert!(PairLink::new("192.168.1.42", 47_800, &"x".repeat(MAX_NAME_LEN + 1), psk()).is_err());
        assert!(PairLink::new("has a space", 47_800, "DESKTOP", psk()).is_err());
        assert!(PairLink::new("::1", 47_800, "loopback", psk()).is_ok());
        assert!(PairLink::new("[fe80::1]", 47_800, "link-local", psk()).is_ok());
    }

    #[test]
    fn debug_output_does_not_leak_the_key() {
        let link = example();
        let text = format!("{link:?}");
        assert!(text.contains("DESKTOP-ABC"), "got {text}");
        assert!(!text.contains(&link.psk.to_base64url()), "leaked the key: {text}");
        assert!(text.contains("redacted"), "got {text}");
    }

    #[test]
    fn percent_decoding_is_strict_about_escapes() {
        assert_eq!(percent_decode("a%20b").unwrap(), "a b");
        assert_eq!(percent_decode("a+b").unwrap(), "a+b", "+ is not a space");
        assert_eq!(percent_decode("100%25").unwrap(), "100%");
        assert_eq!(percent_decode("%41%42").unwrap(), "AB");
        assert!(percent_decode("%").is_err());
        assert!(percent_decode("%4").is_err());
        assert!(percent_decode("%zz").is_err());
        // Invalid UTF-8 must be an error, not a lossy replacement character.
        assert!(percent_decode("%FF").is_err());
        assert_eq!(percent_encode("a b"), "a%20b");
        assert_eq!(percent_encode("100%"), "100%25");
        assert_eq!(percent_encode("A-Z.a_z~9"), "A-Z.a_z~9");
    }
}