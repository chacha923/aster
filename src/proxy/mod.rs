//! proxy is the mod which contains genneral proxy

mod fnv;
mod handler;
mod ketama;
mod node;
mod ping;

use self::fnv::Fnv1a64;
use self::handler::{Handle, HandleInput};
use self::ketama::HashRing;
use self::node::spawn_node;
use self::ping::Ping;

use com::*;
use ClusterConfig;

use futures::lazy;
use futures::task::Task;
use futures::unsync::mpsc::{channel, Sender};
use futures::unsync::oneshot;
use futures::{AsyncSink, Future, Sink, Stream};
use tokio::net::TcpStream;
use tokio::runtime::current_thread;
use tokio_codec::{Decoder, Encoder};

use std::cell::RefCell;
use std::collections::VecDeque;
use std::collections::{HashMap, HashSet};
use std::fmt::Debug;
use std::net::SocketAddr;
use std::rc::Rc;

pub fn start_proxy<T: Request + 'static>(proxy: Proxy<T>) {
    let addr = proxy
        .cc
        .listen_addr
        .clone()
        .parse::<SocketAddr>()
        .expect("parse socket never fail");

    let fut = lazy(move || -> Result<(SocketAddr, Proxy<T>), ()> { Ok((addr, proxy)) })
        .and_then(|(addr, proxy)| {
            let listen = create_reuse_port_listener(&addr).expect("bind never fail");
            Ok((Rc::new(proxy), listen))
        })
        .and_then(|(proxy, listen)| {
            proxy.init_conns().map_err(|err| {
                error!("fail to create init proxy connections due to {:?}", err);
            })?;
            Ok((proxy, listen))
        })
        .and_then(|(proxy, listen)| {
            let rc_proxy = proxy.clone();
            let amt = listen
                .incoming()
                .for_each(move |sock| {
                    let sock = set_read_write_timeout(
                        sock,
                        rc_proxy.cc.read_timeout,
                        rc_proxy.cc.write_timeout,
                    )
                    .expect("set read/write timeout in proxy frontend must be ok");

                    sock.set_nodelay(true).expect("set nodelay must ok");
                    let codec = T::handle_codec();
                    let (req_tx, req_rx) = codec.framed(sock).split();
                    let (handle_tx, handle_rx) = channel(2048);
                    let (close_tx, close_rx) = oneshot::channel();
                    let input = HandleInput::new(req_rx, handle_tx, close_rx);
                    current_thread::spawn(input);

                    let proxy = rc_proxy.clone();
                    let handle = Handle::new(
                        proxy,
                        handle_rx.map_err(|_err| {
                            error!("fail to recv from upstream handle rx");
                            Error::Critical
                        }),
                        req_tx,
                        close_tx,
                    )
                    .map_err(|err| {
                        error!("get handle error due {:?}", err);
                    });
                    current_thread::spawn(handle);
                    Ok(())
                })
                .map_err(|err| {
                    error!("fail to start_cluster due {:?}", err);
                });
            current_thread::spawn(amt);
            Ok(proxy.clone())
        })
        .and_then(|proxy| {
            if let Some(limit) = proxy.cc.ping_fail_limit {
                info!("setup ping for cluster {}", proxy.cc.name);
                // default ping interval 500
                let interval = proxy.cc.ping_interval.map(|x| x as u64).unwrap_or(500u64);

                let addrs = proxy.spots.keys().cloned();
                for addr in addrs {
                    let ping = Ping::new(proxy.clone(), addr, limit, interval);
                    current_thread::spawn(ping);
                }
            } else {
                debug!("skip proxy ping feature for cluster {}", proxy.cc.name);
            }
            Ok(())
        });

    current_thread::block_on_all(fut).unwrap();
}

#[allow(unused)]
struct ServerLine {
    addr: String,
    weight: usize,
    alias: Option<String>,
}

impl ServerLine {
    fn parse_servers(servers: &[String]) -> AsResult<Vec<ServerLine>> {
        // e.g.: 192.168.1.2:1074:10 redis-20
        let mut sl = Vec::with_capacity(servers.len());
        for server in servers {
            let mut iter = server.split(' ');
            let first_part = iter.next().expect("first partation must exists");
            if first_part.chars().filter(|x| *x == ':').count() == 1 {
                let alias = iter.next().map(|x| x.to_string());
                sl.push(ServerLine {
                    addr: first_part.to_string(),
                    weight: 1,
                    alias,
                });
            }

            let mut fp_sp = first_part.rsplitn(2, ':').filter(|x| !x.is_empty());
            let weight = {
                let weight_str = fp_sp.next().unwrap_or("1");
                weight_str.parse::<usize>()?
            };
            let addr = fp_sp.next().expect("addr never be absent").to_owned();
            drop(fp_sp);
            let alias = iter.next().map(|x| x.to_string());
            sl.push(ServerLine {
                addr,
                weight,
                alias,
            });
        }
        Ok(sl)
    }

    fn unwrap_spot(sls: &[ServerLine]) -> (Vec<String>, Vec<String>, Vec<usize>) {
        let mut nodes = Vec::new();
        let mut alias = Vec::new();
        let mut weights = Vec::new();
        for sl in sls {
            if sl.alias.is_some() {
                alias.push(
                    sl.alias
                        .as_ref()
                        .cloned()
                        .expect("node addr can't be empty"),
                );
            }
            nodes.push(sl.addr.clone());
            weights.push(sl.weight);
        }
        (nodes, alias, weights)
    }
}

pub struct Proxy<T: Request> {
    pub cc: ClusterConfig,

    spots: HashMap<String, usize>,
    alias: RefCell<HashMap<String, String>>,

    ring: RefCell<HashRing<Fnv1a64>>,
    conns: RefCell<HashMap<String, Sender<T>>>,
    is_alias: bool,
    hash_tag: Vec<u8>,
}

impl<T: Request + 'static> Proxy<T> {
    pub fn new(cc: ClusterConfig) -> AsResult<Proxy<T>> {
        let sls = ServerLine::parse_servers(&cc.servers)?;
        let (addrs, alias, weights) = ServerLine::unwrap_spot(&sls);
        let ring = if alias.is_empty() {
            HashRing::<Fnv1a64>::new(addrs.clone(), weights.clone())?
        } else {
            HashRing::<Fnv1a64>::new(alias.clone(), weights.clone())?
        };
        let alias_map: HashMap<_, _> = alias
            .iter()
            .enumerate()
            .map(|(index, alias_name)| (alias_name.clone(), addrs[index].clone()))
            .collect();

        let spots: HashMap<_, _> = if alias.is_empty() {
            addrs.iter()
        } else {
            alias.iter()
        }
        .zip(weights.iter())
        .map(|(x, y)| (x.clone(), *y))
        .collect();

        let hash_tag = cc
            .hash_tag
            .as_ref()
            .cloned()
            .unwrap_or_else(|| "".to_string())
            .as_bytes()
            .to_vec();

        Ok(Proxy {
            cc,
            spots,
            hash_tag,
            ring: RefCell::new(ring),
            conns: RefCell::new(HashMap::new()),
            is_alias: !alias_map.is_empty(),
            alias: RefCell::new(alias_map),
        })
    }

    fn reconnect(&self, node: String) -> AsResult<()> {
        let conn = Self::create_conn(&node, self.cc.read_timeout, self.cc.write_timeout)?;
        if let Some(mut old) = self.conns.borrow_mut().insert(node, conn) {
            old.close().map_err(|err| {
                error!("force done for replaced data");
                let req = err.into_inner();
                req.done_with_error(Error::ClusterDown);
                Error::Critical
            })?;
        }
        Ok(())
    }

    fn trim_hash_tag<'b>(&self, key: &'b [u8]) -> &'b [u8] {
        trim_hash_tag(&self.hash_tag, key)
    }

    fn init_conns(&self) -> AsResult<()> {
        for server in self.alias.borrow().values().map(|x| x.to_string()) {
            let conn = Self::create_conn(&server, self.cc.read_timeout, self.cc.write_timeout)?;
            self.conns.borrow_mut().insert(server.clone(), conn);
        }
        Ok(())
    }

    fn create_conn(node: &str, rt: Option<u64>, wt: Option<u64>) -> AsResult<Sender<T>> {
        let node_addr = node.to_string();
        let (tx, rx) = channel(1024 * 8);
        let ret_tx = tx.clone();
        let amt = lazy(|| -> Result<(), ()> { Ok(()) })
            .and_then(move |_| {
                node_addr
                    .as_str()
                    .parse()
                    .map_err(|err| error!("fail to parse addr {:?}", err))
            })
            .and_then(|addr| {
                TcpStream::connect(&addr).map_err(|err| error!("fail to connect {:?}", err))
            })
            .and_then(move |sock| {
                let sock = set_read_write_timeout(sock, rt, wt).expect("set timeout must be ok");

                sock.set_nodelay(true).expect("set nodelay must ok");
                let codec = <T as Request>::node_codec();
                let (sink, stream) = codec.framed(sock).split();
                let arx = rx.map_err(|err| {
                    info!("fail to send due to {:?}", err);
                    Error::Critical
                });
                let (nd, nr) = spawn_node(arx, sink, stream);
                current_thread::spawn(nd);
                current_thread::spawn(nr);
                Ok(())
            });
        current_thread::spawn(amt);
        Ok(ret_tx)
    }

    fn add_node(&self, node: &str) {
        if let Some(spot) = self.spots.get(node) {
            self.ring.borrow_mut().add_node(node.to_string(), *spot);
        }
    }

    fn del_node(&self, node: &str) {
        self.ring.borrow_mut().del_node(node);
    }

    fn trans_alias(&self, who: &str) -> String {
        if self.is_alias {
            if let Some(addr) = self.alias.borrow().get(who) {
                return addr.to_string();
            }
        }
        who.to_string()
    }

    fn execute(&self, who: &str, req: T) -> Result<AsyncSink<T>, Error> {
        let node = self.trans_alias(who);

        let rslt = {
            let mut conns = self.conns.borrow_mut();
            let conn = conns
                .get_mut(&node)
                .expect("start_send conn must be exists");
            conn.start_send(req)
        };
        match rslt {
            Ok(AsyncSink::Ready) => match self
                .conns
                .borrow_mut()
                .get_mut(&node)
                .expect("complte conn must be exists")
                .poll_complete()
            {
                Ok(_) => Ok(AsyncSink::Ready),
                Err(err) => {
                    self.reconnect(node)?;
                    error!("execute fail to poll_complete due to {:?}", err);
                    let req = err.into_inner();
                    req.done_with_error(Error::ClusterDown);
                    Err(Error::Critical)
                }
            },
            Ok(AsyncSink::NotReady(r)) => Ok(AsyncSink::NotReady(r)),
            Err(err) => {
                self.reconnect(node)?;
                error!("fail to execute to backend due to {:?}", err);
                let req = err.into_inner();
                req.done_with_error(Error::ClusterDown);
                Err(Error::ClusterDown)
            }
        }
    }

    fn dispatch_all(&self, cmds: &mut VecDeque<T>) -> Result<AsyncSink<()>, Error> {
        let mut nodes = HashSet::new();
        loop {
            if let Some(req) = cmds.front().cloned() {
                let name = self.ring.borrow().get_node(self.trim_hash_tag(&req.key()));
                let node = self.trans_alias(&name);
                let result = {
                    let mut conns = self.conns.borrow_mut();
                    let conn = conns.get_mut(&node).expect("must get the conn");
                    conn.start_send(req.clone()).map_err(|err| {
                        error!("fail to dispatch to backend due to {:?}", err);
                        Error::Critical
                    })
                };

                match result {
                    Ok(AsyncSink::Ready) => {
                        let _ = cmds.pop_front().expect("pop_front never be empty");
                        nodes.insert(node.clone());
                    }
                    Ok(AsyncSink::NotReady(_)) => {
                        break;
                    }
                    Err(err) => {
                        self.reconnect(node)?;
                        return Err(err);
                    }
                }
            } else {
                return Ok(AsyncSink::Ready);
            }
        }

        if !nodes.is_empty() {
            for node in nodes.into_iter() {
                let rslt = self
                    .conns
                    .borrow_mut()
                    .get_mut(&node)
                    .expect("node connection is never be absent")
                    .poll_complete();
                match rslt {
                    Ok(_) => {}
                    Err(err) => {
                        self.reconnect(node)?;
                        error!("fail to complete to proxy to backend due to {:?}", err);
                        return Err(Error::ClusterDown);
                    }
                }
            }
        }

        Ok(AsyncSink::Ready)
    }
}

pub trait Request: Sized + Clone + Debug {
    type Reply: Clone + Debug;
    type HandleCodec: Decoder<Item = Self, Error = Error> + Encoder<Item = Self, Error = Error>;
    type NodeCodec: Decoder<Item = Self::Reply, Error = Error> + Encoder<Item = Self, Error = Error>;

    fn ping_request() -> Self;
    fn handle_codec() -> Self::HandleCodec;
    fn node_codec() -> Self::NodeCodec;
    fn reregister(&self, task: Task);

    fn key(&self) -> Vec<u8>;
    fn subs(&self) -> Option<Vec<Self>>;
    fn is_done(&self) -> bool;

    fn valid(&self) -> bool;
    fn done(&self, data: Self::Reply);
    fn done_with_error(&self, err: Error);
}

#[inline]
fn trim_hash_tag<'a, 'b>(hash_tag: &'a [u8], key: &'b [u8]) -> &'b [u8] {
    if hash_tag.len() != 2 {
        return key;
    }
    if let Some(begin) = key.iter().position(|x| *x == hash_tag[0]) {
        if let Some(end_offset) = key[begin..].iter().position(|x| *x == hash_tag[1]) {
            return &key[begin + 1..begin + end_offset];
        }
    }
    key
}

#[cfg(test)]
#[test]
fn test_trim_hash_tag() {
    assert_eq!(trim_hash_tag(b"{}", b"abc{a}b"), b"a");
    assert_eq!(trim_hash_tag(b"{}", b"abc{ab"), b"abc{ab");
    assert_eq!(trim_hash_tag(b"{}", b"abc{ab}"), b"ab");
    assert_eq!(trim_hash_tag(b"{}", b"abc{ab}asd{abc}d"), b"ab");
    assert_eq!(
        trim_hash_tag(b"{", b"abc{ab}asd{abc}d"),
        b"abc{ab}asd{abc}d"
    );
    assert_eq!(trim_hash_tag(b"", b"abc{ab}asd{abc}d"), b"abc{ab}asd{abc}d");
    assert_eq!(
        trim_hash_tag(b"abc", b"abc{ab}asd{abc}d"),
        b"abc{ab}asd{abc}d"
    );
    assert_eq!(trim_hash_tag(b"ab", b"abc{ab}asd{abc}d"), b"");
}
