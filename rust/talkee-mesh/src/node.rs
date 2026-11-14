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
use crate::peers::{now, KeyMatch, Peer, Peers};
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

/// This device, its peers, and where they are kept.
pub struct Node {
    pub store: Store,
    pub device: Device,
    pub peers: Mutex<Peers>,
}

impl Node {
    pub fn new(store: Store, device: Device) -> Result<Self> {
        let peers = Peers::load(&store)?;
        Ok(Self {
            store,
            device,
            peers: Mutex::new(peers),
        })
    }

    /// Re-reads `mesh.dat`, so an invite made by another process (`talkeed pair`) is seen.
    fn reload(&self) {
        if let Ok(fresh) = Peers::load(&self.store) {
            *self.peers.lock().unwrap() = fresh;
        }
    }

    fn save(&self) -> Result<()> {
        self.peers.lock().unwrap().save(&self.store)
    }

    /// Makes an invite and the link that carries it.
    pub fn invite_link(&self, host: &str, port: u16) -> Result<String> {
        self.reload();
        let psk = self.peers.lock().unwrap().invite()?;
        self.save()?;
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
        if let Some(p) = node.peers.lock().unwrap().get_mut(id) {
            p.last_seen = now();
        }
        let _ = node.save();
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
                    Some(peer) => builtin(node, &req).or_else(|| {
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
    let peer = Peer::new(them.id.clone(), name, them.kind, them.caps, &key, None);
    node.peers.lock().unwrap().accept(invite, peer);
    node.save()?;
    Ok((
        them.id,
        json!({"device": node.device, "key": key.to_base64url()}),
    ))
}

fn builtin(node: &Node, req: &Request) -> Option<Result<Value>> {
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
        _ => return None,
    })
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
    let mut peers = Peers::load(store)?;
    peers.upsert(peer.clone());
    peers.save(store)?;
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
}
