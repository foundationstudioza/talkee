//! Requests and answers between paired devices.
//!
//! Every message is one JSON object: a [`Request`] `{"id":1,"op":"device.info","body":{}}` and its
//! [`Response`] `{"id":1,"ok":true,"body":{...}}` (or `"ok":false,"error":"..."`). New features add
//! operations, not sockets. A connection made with an invite may only send `pair.complete`.
//!
//! Built-in operations, answered by every node:
//!
//! * `pair.complete`: body is the dialer's [`Device`]; the answer is `{"device": ours, "key": new}`:
//!   a fresh key for the pair, so the link's key is dead once used. Turns the invite into a
//!   peer on both sides.
//! * `device.info`: this device.
//! * `peers.list`: the devices this one is paired with (names, kinds, capabilities; never keys).
//! * `ping`: an empty answer, for measuring the round trip.
//!
//! Anything else goes to the [`Handler`].

use std::net::{SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use talkee_crypto::{PairLink, Psk};
use talkee_store::Store;

use crate::identity::{valid_id, Device};
use crate::peers::{now, Intro, KeyMatch, Peer, Peers};
use crate::wire::{self, Channel};
use crate::MESH_PORT;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    pub id: u64,
    pub op: String,
    #[serde(default)]
    pub body: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub id: u64,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub body: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Operations a program adds on top of the built-in ones.
pub trait Handler: Send + Sync {
    /// Answers `op` from `peer`. `Ok(None)` means the operation is not known here.
    fn handle(&self, peer: &Peer, op: &str, body: &Value) -> Result<Option<Value>>;
}

/// A handler that knows nothing beyond the built-ins.
pub struct NoHandler;

impl Handler for NoHandler {
    fn handle(&self, _: &Peer, _: &str, _: &Value) -> Result<Option<Value>> {
        Ok(None)
    }
}

/// Where a peer is now, by id, looking for at most the given time (mDNS in practice).
pub type FindPeer = dyn Fn(&str, Duration) -> Vec<String> + Send + Sync;

/// This device, its peers, and where they are kept.
pub struct Node {
    pub store: Store,
    pub device: Device,
    pub peers: Mutex<Peers>,
    /// Finds a peer on the network when its saved addresses fail (mDNS, from `talkee-discovery`).
    pub resolver: Option<Box<FindPeer>>,
}

impl Node {
    pub fn new(store: Store, device: Device) -> Result<Self> {
        let peers = Peers::load(&store)?;
        Ok(Self {
            store,
            device,
            peers: Mutex::new(peers),
            resolver: None,
        })
    }

    /// Re-reads `mesh.dat`, so an invite made by another process (`talkeed pair`) is seen.
    fn reload(&self) {
        if let Ok(fresh) = Peers::load(&self.store) {
            *self.peers.lock().unwrap() = fresh;
        }
    }

    /// Changes the peer list atomically: under one lock, read `mesh.dat` fresh, apply `f`, save.
    ///
    /// Every change goes through here. Reloading and saving separately let one connection's reload
    /// land between another's change and its save, and the save then wrote the list without it.
    pub fn update<R>(&self, f: impl FnOnce(&mut Peers) -> R) -> Result<R> {
        let mut memory = self.peers.lock().unwrap();
        let mut copy = None;
        let r = Peers::update(&self.store, |peers| {
            let r = f(peers);
            copy = Some(peers.clone());
            r
        })?;
        if let Some(c) = copy {
            *memory = c;
        }
        Ok(r)
    }

    /// Makes an invite and the link that carries it.
    pub fn invite_link(&self, host: &str, port: u16) -> Result<String> {
        self.reload();
        let psk = self.update(|p| p.invite())??;
        Ok(PairLink::new(host, port, &self.device.name, psk)?.to_uri())
    }
}

/// Accepts connections forever, one thread each.
pub fn serve(node: Arc<Node>, listener: TcpListener, handler: Arc<dyn Handler>) -> Result<()> {
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let node = node.clone();
        let handler = handler.clone();
        std::thread::spawn(move || {
            let from = stream.peer_addr().ok();
            if let Err(e) = connection(&node, stream, handler.as_ref()) {
                eprintln!("mesh: connection from {from:?} ended: {e:#}");
            }
        });
    }
    Ok(())
}

fn connection(node: &Node, stream: TcpStream, handler: &dyn Handler) -> Result<()> {
    node.reload();
    let (mut ch, matched) = wire::accept(stream, |fp| {
        node.peers.lock().unwrap().find_key(fp).map(|m| match &m {
            KeyMatch::Peer(k, _) | KeyMatch::Invite(k) => (k.clone(), m),
        })
    })?;
    let mut peer_id = match &matched {
        KeyMatch::Peer(_, id) => Some(id.clone()),
        KeyMatch::Invite(_) => None,
    };
    if let Some(id) = &peer_id {
        let _ = node.update(|peers| {
            if let Some(p) = peers.get_mut(id) {
                p.last_seen = now();
            }
        });
    }
    while let Some(bytes) = ch.recv()? {
        let req: Request = match serde_json::from_slice(&bytes) {
            Ok(r) => r,
            Err(_) => bail!("not a request"),
        };
        let id = req.id;
        let answer = match &peer_id {
            None if req.op == "pair.complete" => complete(node, &matched, &ch, &req.body)
                .map(|(peer, body)| {
                    peer_id = Some(peer);
                    body
                }),
            None => Err(anyhow!("finish pairing first")),
            Some(pid) => {
                let peer = node.peers.lock().unwrap().get(pid).cloned();
                match peer {
                    None => Err(anyhow!("this device was forgotten")),
                    Some(_) if req.op == "pair.complete" => Err(anyhow!(
                        "this link was already used; run `talkeed pair` for a new one"
                    )),
                    Some(peer) => builtin(node, &peer, &req).or_else(|| {
                        handler
                            .handle(&peer, &req.op, &req.body)
                            .transpose()
                    })
                    .unwrap_or_else(|| Err(anyhow!("unknown operation {}", req.op))),
                }
            }
        };
        let resp = match answer {
            Ok(body) => Response { id, ok: true, body, error: None },
            Err(e) => Response { id, ok: false, body: Value::Null, error: Some(format!("{e:#}")) },
        };
        ch.send(&serde_json::to_vec(&resp)?)?;
    }
    Ok(())
}

/// Finishes pairing over an invite.
///
/// The invite's key was in a link, and a link can sit in a terminal's scrollback or a chat history,
/// so it is not kept: a fresh key is made for the pair and handed over inside the encrypted link.
/// Once this returns, the link is worth nothing.
fn complete(node: &Node, matched: &KeyMatch, ch: &Channel, body: &Value) -> Result<(String, Value)> {
    let KeyMatch::Invite(invite) = matched else { bail!("already paired") };
    let them: Device = serde_json::from_value(body.clone()).context("pair.complete needs a device")?;
    if !valid_id(&them.id) || them.id == node.device.id {
        bail!("that device id is not usable");
    }
    let name = crate::identity::clean_name(&them.name);
    let key = Psk::generate()?;
    let _ = ch.peer_addr();
    let mut peer =
        Peer::new(them.id.clone(), name, them.kind, them.caps, &key, them.addrs.first().cloned());
    peer.alt_addrs = them.addrs.iter().skip(1).cloned().collect();
    let intros = node.update(|peers| {
        peers.accept(invite, peer);
        // Joining one member joins the mesh: introduce the newcomer to everyone this node knows.
        peers.introduce(&them.id, them.addrs.first().cloned())
    })??;
    // Tell the others now if they answer; whoever does not hears at the next sync.
    deliver_soon(node);
    Ok((
        them.id,
        json!({"device": node.device, "key": key.to_base64url(), "introductions": intros}),
    ))
}

/// Runs one sync on a thread, so a new pairing reaches the other peers without waiting for the timer.
fn deliver_soon(node: &Node) {
    let store = node.store.clone();
    let device = node.device.clone();
    std::thread::spawn(move || {
        if let Ok(fresh) = Node::new(store, device) {
            fresh.sync();
        }
    });
}

/// Updates what a peer says about itself (name, capabilities, addresses).
fn refresh_peer(peers: &mut Peers, peer_id: &str, body: &Value) {
    let Ok(them) = serde_json::from_value::<Device>(body.clone()) else { return };
    if them.id != peer_id {
        return;
    }
    if let Some(p) = peers.get_mut(peer_id) {
        p.name = crate::identity::clean_name(&them.name);
        p.caps = them.caps;
        if !them.addrs.is_empty() {
            if !p.addr.as_ref().is_some_and(|a| them.addrs.contains(a)) {
                p.addr = them.addrs.first().cloned();
            }
            p.alt_addrs = them.addrs;
        }
    }
}

fn builtin(node: &Node, peer: &Peer, req: &Request) -> Option<Result<Value>> {
    Some(match req.op.as_str() {
        "device.info" => serde_json::to_value(&node.device).map_err(Into::into),
        "ping" => Ok(json!({})),
        "peers.list" => {
            let peers = node.peers.lock().unwrap();
            Ok(Value::Array(
                peers
                    .peers
                    .iter()
                    .map(|p| json!({"id": p.id, "name": p.name, "kind": p.kind, "caps": p.caps, "last_seen": p.last_seen}))
                    .collect(),
            ))
        }
        // The caller says who it is now and collects the introductions waiting for it.
        "mesh.sync" => {
            node.update(|peers| {
                refresh_peer(peers, &peer.id, &req.body);
                peers.take_pending(&peer.id)
            })
            .map(|intros| json!({"device": node.device, "introductions": intros}))
        }
        // The caller introduces us to devices it knows.
        "mesh.introduce" => (|| {
            let intros: Vec<Intro> = serde_json::from_value(req.body["introductions"].clone())
                .context("mesh.introduce needs introductions")?;
            node.update(|peers| {
                for i in &intros {
                    peers.apply(i, &node.device.id);
                }
            })?;
            Ok(json!({}))
        })(),
        // One hop for a device that cannot reach the target itself (a phone on Wi-Fi and a server
        // behind this PC). Only to this node's own peers, never another relay or a pairing.
        "mesh.relay" => (|| {
            let to = req.body["to"].as_str().context("mesh.relay needs `to`")?;
            let op = req.body["op"].as_str().context("mesh.relay needs `op`")?;
            if op.starts_with("mesh.") || op.starts_with("pair.") {
                bail!("{op} cannot be relayed");
            }
            if to == peer.id {
                bail!("that is you");
            }
            let target = node
                .peers
                .lock()
                .unwrap()
                .get(to)
                .cloned()
                .ok_or_else(|| anyhow!("this device is not paired with {to}"))?;
            let mut client = dial_peer(&target, node.resolver.as_deref(), DIRECT_TIMEOUT)?.1;
            client.call(op, req.body["body"].clone())
        })(),
        _ => return None,
    })
}

impl Node {
    /// One round with every peer that answers: deliver introductions waiting for it, tell it who we
    /// are (so it learns our current addresses), and take what it has waiting for us. Peers that do
    /// not answer are tried again next time. Call it every minute or so.
    pub fn sync(&self) {
        self.reload();
        let peers: Vec<Peer> = self.peers.lock().unwrap().peers.clone();
        for p in peers {
            let Ok((addr, mut client)) = dial_peer(&p, None, DIRECT_TIMEOUT) else { continue };
            let waiting = self.update(|peers| peers.take_pending(&p.id)).unwrap_or_default();
            if !waiting.is_empty()
                && client.call("mesh.introduce", json!({"introductions": waiting.clone()})).is_err()
            {
                let _ = self.update(|peers| peers.pending.extend(waiting));
            }
            let me = serde_json::to_value(&self.device).unwrap_or_default();
            let Ok(answer) = client.call("mesh.sync", me) else { continue };
            let intros: Vec<Intro> =
                serde_json::from_value(answer["introductions"].clone()).unwrap_or_default();
            let saved = self.update(|peers| {
                for i in &intros {
                    peers.apply(i, &self.device.id);
                }
                if let Some(q) = peers.get_mut(&p.id) {
                    q.last_seen = now();
                    q.addr = Some(addr);
                }
                refresh_peer(peers, &p.id, &answer["device"]);
            });
            if let Err(e) = saved {
                eprintln!("mesh: saving after sync: {e:#}");
            }
        }
    }
}

/// Runs [`Node::sync`] now and then every `every`, on a thread of its own.
pub fn spawn_sync(node: Arc<Node>, every: Duration) {
    std::thread::spawn(move || loop {
        node.sync();
        std::thread::sleep(every);
    });
}

/// How long one direct dial may take before the next address (or a relay) is tried. On a LAN a
/// refused or unreachable address fails at once; a dead one should not hold up the rest.
pub const DIRECT_TIMEOUT: Duration = Duration::from_millis(1500);

/// Dials a peer: every address it has (saved first) with every key the pair may be using, then
/// whatever `resolve` finds. Returns the address that worked.
pub fn dial_peer(
    peer: &Peer,
    resolve: Option<&FindPeer>,
    timeout: Duration,
) -> Result<(String, Client)> {
    let keys = peer.keys();
    let try_addr = |addr: &str| -> Result<Client> {
        let mut last = anyhow!("no usable key");
        for k in &keys {
            match Client::connect(addr, k, timeout) {
                Ok(c) => return Ok(c),
                Err(e) => last = e,
            }
        }
        Err(last)
    };
    let mut first_err = None;
    let addrs = Peers::dial_addrs(peer);
    for addr in &addrs {
        match try_addr(addr) {
            Ok(c) => return Ok((addr.clone(), c)),
            Err(e) => {
                first_err.get_or_insert(e);
            }
        }
    }
    if let Some(resolve) = resolve {
        for addr in resolve(&peer.id, FALLBACK_BROWSE) {
            if !addrs.contains(&addr) {
                if let Ok(c) = try_addr(&addr) {
                    return Ok((addr, c));
                }
            }
        }
    }
    Err(first_err.unwrap_or_else(|| anyhow!("{} has no address to call it on", peer.name)))
}

/// Calls `op` on a paired device: directly if it answers, otherwise through another paired device
/// that can reach it (`mesh.relay`). This is what lets a phone on Wi-Fi talk to a server only the
/// PC can see.
pub fn call_peer(store: &Store, who: &str, op: &str, body: Value, resolve: Resolver) -> Result<Value> {
    let err = match connect_peer_with(store, who, resolve) {
        Ok((_, mut client)) => return client.call(op, body),
        Err(e) => e,
    };
    let peers = Peers::load(store)?;
    let target = peers.get(who).cloned().ok_or_else(|| anyhow!("no paired device called {who}"))?;
    for via in peers.peers.iter().filter(|p| p.id != target.id) {
        let Ok((_, mut client)) = dial_peer(via, None, DIRECT_TIMEOUT) else { continue };
        match client.call("mesh.relay", json!({"to": target.id, "op": op, "body": body})) {
            Ok(v) => return Ok(v),
            // This one cannot reach it either; try the next.
            Err(e) if format!("{e:#}").contains("not paired with")
                || format!("{e:#}").contains("connecting to")
                || format!("{e:#}").contains("no address") => continue,
            Err(e) => return Err(e.context(format!("through {}", via.name))),
        }
    }
    Err(err)
}

/// A connection to one peer, for making requests.
pub struct Client {
    ch: Channel,
    next: u64,
}

impl Client {
    /// Dials `addr` and authenticates with `psk`.
    pub fn connect(addr: &str, psk: &Psk, timeout: Duration) -> Result<Self> {
        let target: SocketAddr = addr
            .to_socket_addrs()
            .with_context(|| format!("resolving {addr}"))?
            .next()
            .ok_or_else(|| anyhow!("{addr} has no address"))?;
        let stream = TcpStream::connect_timeout(&target, timeout)
            .with_context(|| format!("connecting to {addr}"))?;
        let ch = wire::dial(stream, psk)?;
        ch.set_read_timeout(Some(Duration::from_secs(30)))?;
        Ok(Self { ch, next: 1 })
    }

    /// Sends a request and waits for its answer.
    pub fn call(&mut self, op: &str, body: Value) -> Result<Value> {
        let id = self.next;
        self.next += 1;
        let req = Request { id, op: op.to_owned(), body };
        self.ch.send(&serde_json::to_vec(&req)?)?;
        let bytes = self.ch.recv()?.ok_or_else(|| anyhow!("the other device hung up"))?;
        let resp: Response = serde_json::from_slice(&bytes).context("not a response")?;
        if resp.id != id {
            bail!("answer {} for request {id}", resp.id);
        }
        if resp.ok {
            Ok(resp.body)
        } else {
            bail!(resp.error.unwrap_or_else(|| "failed".into()))
        }
    }
}

/// Uses a pairing link: connects, completes pairing, and saves the new peer. Returns it.
pub fn join(store: &Store, me: &Device, link: &str) -> Result<Peer> {
    let link = PairLink::parse(link.trim())?;
    let addr = host_port(&link.host, link.control_port);
    let mut client = Client::connect(&addr, &link.psk, Duration::from_secs(5))?;
    let answer = client.call("pair.complete", serde_json::to_value(me)?)?;
    let them: Device = serde_json::from_value(answer["device"].clone())
        .context("the other device's answer")?;
    let key = Psk::from_base64url(answer["key"].as_str().unwrap_or_default())
        .context("the other device sent no usable key")?;
    if !valid_id(&them.id) {
        bail!("the other device sent an unusable id");
    }
    let peer = Peer::new(
        them.id,
        crate::identity::clean_name(&them.name),
        them.kind,
        them.caps,
        &key,
        Some(addr),
    );
    let intros: Vec<Intro> =
        serde_json::from_value(answer["introductions"].clone()).unwrap_or_default();
    Peers::update(store, |peers| {
        peers.upsert(peer.clone());
        for i in &intros {
            peers.apply(i, &me.id);
        }
    })?;
    Ok(peer)
}

/// Connects to a saved peer by id or name.
pub fn connect_peer(store: &Store, who: &str) -> Result<(Peer, Client)> {
    let peers = Peers::load(store)?;
    let peer = peers
        .get(who)
        .cloned()
        .ok_or_else(|| anyhow!("no paired device called {who}"))?;
    let addr = peer
        .addr
        .clone()
        .ok_or_else(|| anyhow!("{} dials this device; there is no address to call it on", peer.name))?;
    let client = Client::connect(&addr, &peer.psk()?, Duration::from_secs(5))?;
    Ok((peer, client))
}

/// How long a fallback look on the network may take.
pub const FALLBACK_BROWSE: Duration = Duration::from_secs(2);

/// Finds the addresses a device may now be at, given its id and how long to look. The mDNS one
/// lives in `talkee-discovery` (this crate has no network-discovery code); tests pass a closure.
pub type Resolver<'a> = &'a mut dyn FnMut(&str, Duration) -> Vec<String>;

/// Tries the saved address first, then whatever `resolve` finds. Returns the address that worked
/// and what `dial` made of it. Dialling a wrong device fails in the handshake (the key does not
/// match), so a stranger at a recycled address is never mistaken for the peer.
fn dial_with_fallback<T>(
    id: &str,
    saved: Option<&str>,
    resolve: Option<Resolver>,
    mut dial: impl FnMut(&str) -> Result<T>,
) -> Result<(String, T)> {
    let mut first_err = None;
    if let Some(addr) = saved {
        match dial(addr) {
            Ok(t) => return Ok((addr.to_owned(), t)),
            Err(e) => first_err = Some(e),
        }
    }
    if let Some(resolve) = resolve {
        for addr in resolve(id, FALLBACK_BROWSE) {
            if Some(addr.as_str()) == saved {
                continue;
            }
            if let Ok(t) = dial(&addr) {
                return Ok((addr, t));
            }
        }
    }
    Err(first_err.unwrap_or_else(|| anyhow!("no address to call it on")))
}

/// Like [`connect_peer`], but when the saved address does not answer, asks `resolve` where the
/// device is now and, if that works, saves the new address in `mesh.dat`.
pub fn connect_peer_with(store: &Store, who: &str, resolve: Resolver) -> Result<(Peer, Client)> {
    let peers = Peers::load(store)?;
    let mut peer = peers
        .get(who)
        .cloned()
        .ok_or_else(|| anyhow!("no paired device called {who}"))?;
    let keys = peer.keys();
    // The saved address first; then the others it reported (LAN, Tailscale), then the network.
    let alts: Vec<String> = Peers::dial_addrs(&peer).into_iter().skip(1).collect();
    let mut resolve_all = |id: &str, t: Duration| -> Vec<String> {
        let mut out = alts.clone();
        out.extend(resolve(id, t));
        out
    };
    let saved = Peers::dial_addrs(&peer).into_iter().next();
    let (addr, client) = dial_with_fallback(&peer.id, saved.as_deref(), Some(&mut resolve_all), |a| {
        let mut last = anyhow!("no usable key");
        for k in &keys {
            match Client::connect(a, k, DIRECT_TIMEOUT) {
                Ok(c) => return Ok(c),
                Err(e) => last = e,
            }
        }
        Err(last)
    })
    .with_context(|| format!("could not reach {}", peer.name))?;
    if peer.addr.as_deref() != Some(addr.as_str()) {
        // Re-read before writing, so a change made meanwhile (a forget) is not undone.
        let a = addr.clone();
        Peers::update(store, |fresh| {
            if let Some(p) = fresh.get_mut(&peer.id) {
                p.addr = Some(a);
            }
        })?;
        peer.addr = Some(addr);
    }
    Ok((peer, client))
}

/// Sets the address a saved peer is dialled on (a Tailscale name, a fixed IP). `host` may omit
/// the port, which then defaults to the mesh port.
pub fn set_peer_addr(store: &Store, who: &str, host: &str) -> Result<(Peer, String)> {
    let host = host.trim();
    if host.is_empty() || host.contains(char::is_whitespace) {
        bail!("that is not an address");
    }
    let addr = normalize_addr(host)?;
    let peer = Peers::update(store, |peers| {
        let id = peers.get(who).map(|p| p.id.clone())?;
        let p = peers.get_mut(&id)?;
        p.addr = Some(addr.clone());
        Some(p.clone())
    })?
    .ok_or_else(|| anyhow!("no paired device called {who}"))?;
    Ok((peer, addr))
}

/// `host` or `host:port` (or `[v6]` / `[v6]:port`, or a bare IPv6 literal) to `host:port`.
fn normalize_addr(host: &str) -> Result<String> {
    if let Some(rest) = host.strip_prefix('[') {
        let (h, tail) = rest.split_once(']').ok_or_else(|| anyhow!("unclosed [ in address"))?;
        return match tail.strip_prefix(':') {
            Some(p) => Ok(host_port(h, p.parse().context("port")?)),
            None if tail.is_empty() => Ok(host_port(h, MESH_PORT)),
            None => bail!("that is not an address"),
        };
    }
    match host.matches(':').count() {
        0 => Ok(host_port(host, MESH_PORT)),
        1 => {
            let (h, p) = host.split_once(':').unwrap();
            if h.is_empty() {
                bail!("that is not an address");
            }
            Ok(host_port(h, p.parse().context("port")?))
        }
        _ => Ok(host_port(host, MESH_PORT)),
    }
}

/// `host:port`, bracketing an IPv6 literal.
pub fn host_port(host: &str, port: u16) -> String {
    if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

/// The default port, for callers that build links.
pub const fn default_port() -> u16 {
    MESH_PORT
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::{Capability, Kind};

    fn node(name: &str, kind: Kind) -> (Store, Device) {
        let root = std::env::temp_dir().join(format!("talkee-mesh-test-node-{name}"));
        let _ = std::fs::remove_dir_all(&root);
        let store = Store::at(&root);
        let mut d = Device::load_or_create(&store, kind, &[Capability::Agents]).unwrap();
        d.name = name.into();
        (store, d)
    }

    struct Echo;
    impl Handler for Echo {
        fn handle(&self, peer: &Peer, op: &str, body: &Value) -> Result<Option<Value>> {
            Ok((op == "echo").then(|| json!({"from": peer.name, "body": body})))
        }
    }

    #[test]
    fn fast_path_does_not_browse() {
        let mut asked = false;
        let mut resolve = |_: &str, _: Duration| {
            asked = true;
            vec![]
        };
        let (addr, v) =
            dial_with_fallback("id", Some("1.1.1.1:1"), Some(&mut resolve), |a| Ok(a.len())).unwrap();
        assert_eq!((addr.as_str(), v), ("1.1.1.1:1", 9));
        assert!(!asked);
    }

    #[test]
    fn falls_back_to_resolver_and_skips_the_dead_address() {
        let mut dialled = vec![];
        let mut resolve = |id: &str, _: Duration| {
            assert_eq!(id, "id");
            vec!["1.1.1.1:1".to_owned(), "2.2.2.2:2".to_owned(), "3.3.3.3:3".to_owned()]
        };
        let (addr, ()) = dial_with_fallback("id", Some("1.1.1.1:1"), Some(&mut resolve), |a| {
            dialled.push(a.to_owned());
            if a == "3.3.3.3:3" { Ok(()) } else { bail!("down") }
        })
        .unwrap();
        assert_eq!(addr, "3.3.3.3:3");
        assert_eq!(dialled, ["1.1.1.1:1", "2.2.2.2:2", "3.3.3.3:3"], "saved address is not retried");
    }

    #[test]
    fn no_saved_address_still_uses_the_resolver_and_failure_reports_the_first_error() {
        let mut resolve = |_: &str, _: Duration| vec!["2.2.2.2:2".to_owned()];
        assert!(dial_with_fallback("id", None, Some(&mut resolve), |_| Ok(())).is_ok());
        let mut none = |_: &str, _: Duration| vec![];
        let e = dial_with_fallback::<()>("id", Some("1.1.1.1:1"), Some(&mut none), |_| bail!("refused"))
            .unwrap_err();
        assert_eq!(e.to_string(), "refused");
    }

    #[test]
    fn addresses_normalise() {
        assert_eq!(normalize_addr("compaq.tail1234.ts.net").unwrap(), "compaq.tail1234.ts.net:47810");
        assert_eq!(normalize_addr("100.64.0.5:9000").unwrap(), "100.64.0.5:9000");
        assert_eq!(normalize_addr("fd7a::1").unwrap(), "[fd7a::1]:47810");
        assert_eq!(normalize_addr("[fd7a::1]:1234").unwrap(), "[fd7a::1]:1234");
        assert!(normalize_addr(":80").is_err());
        assert!(normalize_addr("host:notaport").is_err());
    }

    #[test]
    fn fallback_saves_the_new_address_and_set_addr_works() {
        let (s_store, s_dev) = node("fb-server", Kind::Server);
        let (c_store, c_dev) = node("fb-pc", Kind::Pc);
        let server = Arc::new(Node::new(s_store, s_dev).unwrap());
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let link = server.invite_link("127.0.0.1", port).unwrap();
        {
            let server = server.clone();
            std::thread::spawn(move || serve(server, listener, Arc::new(Echo)));
        }
        let peer = join(&c_store, &c_dev, &link).unwrap();
        // The saved address goes stale (a port nothing listens on).
        let (_, stale) = set_peer_addr(&c_store, "fb-server", "127.0.0.1:1").unwrap();
        assert_eq!(stale, "127.0.0.1:1");
        let good = format!("127.0.0.1:{port}");
        let mut resolve = |id: &str, _: Duration| {
            assert_eq!(id, peer.id);
            vec![good.clone()]
        };
        let (p, mut client) = connect_peer_with(&c_store, "fb-server", &mut resolve).unwrap();
        assert_eq!(p.addr.as_deref(), Some(good.as_str()));
        client.call("ping", Value::Null).unwrap();
        assert_eq!(Peers::load(&c_store).unwrap().get("fb-server").unwrap().addr.as_deref(), Some(good.as_str()));
    }

    #[test]
    fn join_then_call_then_forget() {
        let (s_store, s_dev) = node("server", Kind::Server);
        let (c_store, c_dev) = node("pc", Kind::Pc);
        let server = Arc::new(Node::new(s_store.clone(), s_dev).unwrap());
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let link = server.invite_link("127.0.0.1", port).unwrap();
        {
            let server = server.clone();
            std::thread::spawn(move || serve(server, listener, Arc::new(Echo)));
        }

        let peer = join(&c_store, &c_dev, &link).unwrap();
        assert_eq!(peer.name, "server");
        assert_eq!(peer.kind, Kind::Server);

        // The invite is spent and its key was replaced: a second device with the same link gets
        // nowhere, and neither does anyone holding the link's key.
        let (o_store, o_dev) = node("other", Kind::Pc);
        assert!(join(&o_store, &o_dev, &link).is_err());
        let link_key = PairLink::parse(&link).unwrap().psk;
        assert!(Client::connect(&format!("127.0.0.1:{port}"), &link_key, Duration::from_secs(2)).is_err());

        let (_, mut client) = connect_peer(&c_store, "server").unwrap();
        let info = client.call("device.info", Value::Null).unwrap();
        assert_eq!(info["kind"], "server");
        assert_eq!(client.call("echo", json!(5)).unwrap(), json!({"from": "pc", "body": 5}));
        let list = client.call("peers.list", Value::Null).unwrap();
        assert_eq!(list[0]["name"], "pc");
        assert!(!list.to_string().contains("key"), "peers.list must not carry keys");
        assert!(client.call("nope", Value::Null).is_err());
        drop(client);

        // Forgetting on the server locks the PC out.
        let mut peers = Peers::load(&s_store).unwrap();
        assert!(peers.forget(&c_dev.id));
        peers.save(&s_store).unwrap();
        assert!(connect_peer(&c_store, "server").is_err());
    }

    fn listen(store: &Store, dev: &Device) -> (Arc<Node>, u16) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let mut dev = dev.clone();
        dev.addrs = vec![format!("127.0.0.1:{port}")];
        let node = Arc::new(Node::new(store.clone(), dev).unwrap());
        let n = node.clone();
        std::thread::spawn(move || serve(n, listener, Arc::new(Echo)));
        (node, port)
    }

    #[test]
    fn joining_one_member_joins_the_mesh_and_relay_reaches_the_rest() {
        let (a_store, a_dev) = node("intro-server", Kind::Server);
        let (b_store, b_dev) = node("intro-pc", Kind::Pc);
        let (c_store, c_dev) = node("intro-phone", Kind::Phone);
        let (a, a_port) = listen(&a_store, &a_dev);
        let (b, b_port) = listen(&b_store, &b_dev);

        // The PC joins the server, then the phone joins the PC: one pairing each, by hand.
        join(&b_store, &b.device, &a.invite_link("127.0.0.1", a_port).unwrap()).unwrap();
        join(&c_store, &c_dev, &b.invite_link("127.0.0.1", b_port).unwrap()).unwrap();

        // The phone was introduced to the server in the pairing answer...
        let c_peers = Peers::load(&c_store).unwrap();
        assert!(c_peers.get("intro-server").is_some(), "the phone was not introduced to the server");
        // ...and the server hears about the phone from the PC.
        let mut found = false;
        for _ in 0..50 {
            if Peers::load(&a_store).unwrap().get("intro-phone").is_some() {
                found = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        assert!(found, "the server was never introduced to the phone");

        // Direct: the phone calls the server with the introduced key.
        let mut none = |_: &str, _: Duration| Vec::new();
        let v = call_peer(&c_store, "intro-server", "echo", json!(1), &mut none).unwrap();
        assert_eq!(v["from"], "intro-phone");

        // Relay: the phone cannot reach the server (a dead address), the PC can.
        set_peer_addr(&c_store, "intro-server", "127.0.0.1:1").unwrap();
        let mut peers = Peers::load(&c_store).unwrap();
        let id = peers.get("intro-server").unwrap().id.clone();
        peers.get_mut(&id).unwrap().alt_addrs.clear();
        peers.save(&c_store).unwrap();
        let v = call_peer(&c_store, "intro-server", "echo", json!(2), &mut none).unwrap();
        assert_eq!(v["from"], "intro-pc", "the call should have come through the PC");

        // Relays cannot be chained or used for mesh operations.
        let (_, mut to_pc) = connect_peer(&c_store, "intro-pc").unwrap();
        assert!(to_pc
            .call("mesh.relay", json!({"to": id, "op": "mesh.sync", "body": null}))
            .is_err());
    }
}
