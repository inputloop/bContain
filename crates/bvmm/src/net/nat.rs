// SPDX-FileCopyrightText: 2026 Bryant Frankford <nathanielfrankford@gmail.com> and Contributors
// SPDX-License-Identifier: GPL-3.0-or-later OR LGPL-3.0-or-later
//! A private network for one guest, with the host's internet behind it.
//!
//! The guest sees an Ethernet with three machines on it:
//!
//! ```text
//! 10.0.2.2   the gateway (also the DHCP server)
//! 10.0.2.3   the name server
//! 10.0.2.15  the guest itself, by DHCP
//! ```
//!
//! Nothing here touches the host's network configuration. Every connection
//! the guest opens is ended here and made again with an ordinary host
//! socket, so the host's firewall, VPN and proxy rules apply to the guest
//! exactly as to any program:
//!
//! - **TCP**: a TCP/IP stack (smoltcp) answers the guest as whatever address
//!   it called, and the bytes are carried over a host `TcpStream`.
//! - **UDP**: each guest port gets a host `UdpSocket`; replies are wrapped
//!   and sent back.
//! - **Names**: queries to 10.0.2.3 are answered with the host's resolver.
//! - **Ping**: only the gateway answers.
//! - **Pages**: the gateway can serve a few fixed pages at
//!   `http://10.0.2.2/`, for what a guest should be able to read with no
//!   internet at all; see [`Page`].
//!
//! IPv4 only. The guest cannot be reached from outside, and cannot reach
//! the host's own loopback services.

use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::net::{Ipv4Addr, Shutdown, SocketAddr, SocketAddrV4, TcpStream, ToSocketAddrs, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use smoltcp::iface::{Config, Interface, SocketHandle, SocketSet};
use smoltcp::phy::{Device, DeviceCapabilities, Medium};
use smoltcp::socket::tcp;
use smoltcp::wire::{EthernetAddress, HardwareAddress, IpAddress, IpCidr, IpListenEndpoint};

use super::packet::{self, Lease};

pub const GATEWAY: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 2);
pub const NAME_SERVER: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 3);
pub const GUEST: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 15);
const MASK: Ipv4Addr = Ipv4Addr::new(255, 255, 255, 0);
/// The gateway's hardware address (locally administered).
pub const GATEWAY_MAC: [u8; 6] = [0x52, 0x55, 0x0a, 0x00, 0x02, 0x02];

/// Bytes held for each TCP connection, in each direction, before the side
/// producing them is made to wait.
const HIGH_WATER: usize = 512 << 10;
const TCP_BUFFER: usize = 128 << 10;
const MAX_CONNECTIONS: usize = 512;
const MAX_UDP_FLOWS: usize = 256;
const MAX_LOOKUPS: usize = 32;
/// A UDP port mapping is forgotten after this long without traffic.
const UDP_IDLE: u64 = 120;

#[derive(Debug, Clone, Default)]
pub struct Options {
    /// Let the guest reach the host's own loopback addresses (127.0.0.0/8).
    /// Off by default: services bound to loopback expect only local users.
    pub allow_loopback: bool,
    /// Pages the gateway serves to the guest at `http://10.0.2.2/`.
    pub pages: Vec<Page>,
}

/// One fixed page served by the gateway.
#[derive(Debug, Clone)]
pub struct Page {
    /// As asked for, with its leading slash: `/help.html`.
    pub path: String,
    /// For example `text/html; charset=utf-8`.
    pub content_type: String,
    pub body: Vec<u8>,
}

/// Serve `pages` on a loopback port of the host's, for as long as `keep`
/// is held somewhere. Connections to the gateway's port 80 are carried
/// there like any other; nothing else is told the port.
fn serve_pages(pages: Vec<Page>, keep: &Arc<()>) -> Option<SocketAddr> {
    let listener = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).ok()?;
    let addr = listener.local_addr().ok()?;
    listener.set_nonblocking(true).ok()?;
    let keep = Arc::downgrade(keep);
    let pages = Arc::new(pages);
    std::thread::spawn(move || {
        while keep.strong_count() > 0 {
            match listener.accept() {
                Ok((stream, _)) => {
                    let pages = pages.clone();
                    std::thread::spawn(move || answer_page(stream, &pages));
                }
                Err(_) => std::thread::sleep(Duration::from_millis(50)),
            }
        }
    });
    Some(addr)
}

/// Answer one request for a page and close.
fn answer_page(mut stream: TcpStream, pages: &[Page]) {
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(10)));
    let mut request = Vec::new();
    let mut buf = [0u8; 1024];
    while !request.windows(4).any(|w| w == b"\r\n\r\n") && request.len() < 8192 {
        match stream.read(&mut buf) {
            Ok(n) if n > 0 => request.extend_from_slice(&buf[..n]),
            _ => break,
        }
    }
    let request = String::from_utf8_lossy(&request);
    let mut words = request.lines().next().unwrap_or("").split(' ');
    let method = words.next().unwrap_or("");
    let asked = words.next().unwrap_or("");
    let path = asked.split(['?', '#']).next().unwrap_or("");
    let page = pages.iter().find(|p| p.path == path);
    let (status, kind, body): (&str, &str, &[u8]) = match (method, page) {
        ("GET" | "HEAD", Some(p)) => ("200 OK", &p.content_type, &p.body),
        ("GET" | "HEAD", None) => ("404 Not Found", "text/plain", b"Not found.\n"),
        _ => ("405 Method Not Allowed", "text/plain", b"Not allowed.\n"),
    };
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {kind}\r\nContent-Length: {}\r\n\
         Cache-Control: no-cache\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(head.as_bytes());
    if method != "HEAD" {
        let _ = stream.write_all(body);
    }
    let _ = stream.flush();
    let _ = stream.shutdown(Shutdown::Write);
    // Let the reader finish before the socket goes away.
    let _ = stream.read(&mut buf);
}

enum Event {
    /// A frame from the guest.
    Guest(Vec<u8>),
    /// The host side of TCP connection `.0` is open.
    Connected(u64, TcpStream),
    /// It could not be opened, or broke.
    Failed(u64),
    HostData(u64, Vec<u8>),
    HostEof(u64),
    /// A datagram for the guest: to its port, from a remote address.
    Udp {
        to: (Ipv4Addr, u16),
        from: (Ipv4Addr, u16),
        data: Vec<u8>,
    },
    /// Nothing new; look at the connections again.
    Wake,
}

/// Where the guest's frames are handed in.
pub struct Nat {
    events: Mutex<Sender<Event>>,
}

impl Nat {
    /// A network, not yet running, and what starts it.
    pub fn new(options: Options) -> (Nat, Runner) {
        let (tx, rx) = std::sync::mpsc::channel();
        let nat = Nat {
            events: Mutex::new(tx.clone()),
        };
        (nat, Runner { tx, rx, options })
    }

    /// Hand in one Ethernet frame from the guest. Never blocks.
    pub fn frame(&self, frame: Vec<u8>) {
        let _ = self.events.lock().unwrap().send(Event::Guest(frame));
    }
}

pub struct Runner {
    tx: Sender<Event>,
    rx: Receiver<Event>,
    options: Options,
}

impl Runner {
    /// Run the network on its own thread. `to_guest` delivers a frame to
    /// the guest; the network stops soon after `alive` turns false.
    pub fn start(
        self,
        to_guest: impl Fn(Vec<u8>) -> bool + Send + 'static,
        alive: impl Fn() -> bool + Send + 'static,
    ) {
        std::thread::spawn(move || {
            let mut state = State::new(self.tx, self.options);
            state.run(self.rx, &to_guest, &alive);
        });
    }
}

// --- the TCP/IP stack's view of the guest's Ethernet ----------------------

#[derive(Default)]
struct Phy {
    rx: VecDeque<Vec<u8>>,
    tx: Vec<Vec<u8>>,
}

struct RxToken(Vec<u8>);
struct TxToken<'a>(&'a mut Vec<Vec<u8>>);

impl smoltcp::phy::RxToken for RxToken {
    fn consume<R, F: FnOnce(&[u8]) -> R>(self, f: F) -> R {
        f(&self.0)
    }
}

impl smoltcp::phy::TxToken for TxToken<'_> {
    fn consume<R, F: FnOnce(&mut [u8]) -> R>(self, len: usize, f: F) -> R {
        let mut frame = vec![0u8; len];
        let r = f(&mut frame);
        self.0.push(frame);
        r
    }
}

impl Device for Phy {
    type RxToken<'a> = RxToken;
    type TxToken<'a> = TxToken<'a>;

    fn receive(&mut self, _: smoltcp::time::Instant) -> Option<(RxToken, TxToken<'_>)> {
        let frame = self.rx.pop_front()?;
        Some((RxToken(frame), TxToken(&mut self.tx)))
    }

    fn transmit(&mut self, _: smoltcp::time::Instant) -> Option<TxToken<'_>> {
        Some(TxToken(&mut self.tx))
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.medium = Medium::Ethernet;
        caps.max_transmission_unit = 1514;
        caps
    }
}

// --- connections ----------------------------------------------------------

type Tuple = (Ipv4Addr, u16, Ipv4Addr, u16);

struct Connection {
    handle: SocketHandle,
    tuple: Tuple,
    /// Kept to shut the host side down, which also ends its two threads.
    host: Option<TcpStream>,
    /// To the thread that writes to the host. An empty buffer means "the
    /// guest has finished sending".
    to_host: Option<Sender<Vec<u8>>>,
    /// Bytes handed to that thread and not yet written.
    to_host_backlog: Arc<AtomicUsize>,
    /// Bytes read from the host and not yet taken by the guest's side.
    from_host: VecDeque<Vec<u8>>,
    from_host_backlog: Arc<AtomicUsize>,
    host_finished: bool,
    told_guest_finished: bool,
    told_host_finished: bool,
    /// The host's half could not be made, or broke, or is not allowed.
    failed: bool,
    /// Seen closed once already: the stack has had its turn to tell the
    /// guest, so the socket can go.
    closed: bool,
}

struct UdpFlow {
    socket: Arc<UdpSocket>,
    last_used: Arc<AtomicU64>,
    alive: Arc<AtomicBool>,
}

struct State {
    events: Sender<Event>,
    options: Options,
    phy: Phy,
    iface: Interface,
    sockets: SocketSet<'static>,
    connections: HashMap<u64, Connection>,
    by_tuple: HashMap<Tuple, u64>,
    next_id: u64,
    udp: HashMap<(Ipv4Addr, u16), UdpFlow>,
    lookups: Arc<AtomicUsize>,
    guest_mac: Option<[u8; 6]>,
    epoch: Instant,
    /// Where the gateway's pages are really served, if it has any.
    pages_at: Option<SocketAddr>,
    /// Held so that the page server stops when the network does.
    _pages_keep: Arc<()>,
}

/// Whether to report name lookups and connections on standard error
/// (`BVMM_NET_TRACE=1`), for finding out why a guest is not getting through.
fn tracing() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("BVMM_NET_TRACE").is_some())
}

fn now() -> smoltcp::time::Instant {
    smoltcp::time::Instant::from(Instant::now())
}

/// Whether the guest may be connected to `dst` on the host's network.
fn forwardable(dst: Ipv4Addr, options: &Options) -> bool {
    let o = dst.octets();
    let private_net = o[0] == 10 && o[1] == 0 && o[2] == 2;
    !(private_net
        || dst.is_unspecified()
        || dst.is_broadcast()
        || dst.is_multicast()
        || dst.is_link_local()
        || (dst.is_loopback() && !options.allow_loopback))
}

impl State {
    fn new(events: Sender<Event>, options: Options) -> State {
        let mut phy = Phy::default();
        let mut config = Config::new(HardwareAddress::Ethernet(EthernetAddress(GATEWAY_MAC)));
        use std::hash::BuildHasher;
        config.random_seed =
            std::collections::hash_map::RandomState::new().hash_one(std::process::id());
        let mut iface = Interface::new(config, &mut phy, now());
        iface.update_ip_addrs(|addrs| {
            let _ = addrs.push(IpCidr::new(IpAddress::Ipv4(GATEWAY), 24));
        });
        // "Any address": the stack answers as whatever host the guest
        // called, provided a route says that address is reached through us.
        let _ = iface.routes_mut().add_default_ipv4_route(GATEWAY);
        iface.set_any_ip(true);
        let pages_keep = Arc::new(());
        let pages_at = if options.pages.is_empty() {
            None
        } else {
            serve_pages(options.pages.clone(), &pages_keep)
        };
        State {
            pages_at,
            _pages_keep: pages_keep,
            events,
            options,
            phy,
            iface,
            sockets: SocketSet::new(Vec::new()),
            connections: HashMap::new(),
            by_tuple: HashMap::new(),
            next_id: 1,
            udp: HashMap::new(),
            lookups: Arc::default(),
            guest_mac: None,
            epoch: Instant::now(),
        }
    }

    fn run(
        &mut self,
        rx: Receiver<Event>,
        to_guest: &dyn Fn(Vec<u8>) -> bool,
        alive: &dyn Fn() -> bool,
    ) {
        let mut out = Vec::new();
        loop {
            // Twice: serving the connections can give the stack more to send.
            for _ in 0..2 {
                self.iface.poll(now(), &mut self.phy, &mut self.sockets);
                self.serve_connections();
            }
            out.append(&mut self.phy.tx);
            for frame in out.drain(..) {
                if !to_guest(frame) {
                    return;
                }
            }
            let wait = self
                .iface
                .poll_delay(now(), &self.sockets)
                .map(|d| Duration::from_micros(d.total_micros()))
                .unwrap_or(Duration::from_secs(1))
                .min(Duration::from_secs(1));
            match rx.recv_timeout(wait) {
                Ok(event) => {
                    self.handle(event, &mut out);
                    // Take what else is waiting, so one poll serves it all.
                    for event in rx.try_iter().take(256) {
                        self.handle(event, &mut out);
                    }
                }
                Err(RecvTimeoutError::Timeout) => {
                    if !alive() {
                        return;
                    }
                }
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
    }

    fn handle(&mut self, event: Event, out: &mut Vec<Vec<u8>>) {
        match event {
            Event::Guest(frame) => self.guest_frame(frame, out),
            Event::Connected(id, stream) => self.connected(id, stream),
            Event::Failed(id) => {
                if let Some(c) = self.connections.get_mut(&id) {
                    c.failed = true;
                }
            }
            Event::HostData(id, data) => {
                if let Some(c) = self.connections.get_mut(&id) {
                    c.from_host.push_back(data);
                }
            }
            Event::HostEof(id) => {
                if let Some(c) = self.connections.get_mut(&id) {
                    c.host_finished = true;
                }
            }
            Event::Udp { to, from, data } => {
                if let Some(mac) = self.guest_mac {
                    out.push(packet::udp_frame(mac, GATEWAY_MAC, from, to, &data));
                }
            }
            Event::Wake => {}
        }
    }

    fn guest_frame(&mut self, frame: Vec<u8>, out: &mut Vec<Vec<u8>>) {
        let Some(eth) = packet::ethernet(&frame) else {
            return;
        };
        self.guest_mac = Some(eth.src);
        match eth.ethertype {
            packet::ETHERTYPE_ARP => self.phy.rx.push_back(frame),
            packet::ETHERTYPE_IPV4 => {
                let Some(ip) = packet::ipv4(eth.payload) else {
                    return;
                };
                match ip.protocol {
                    packet::PROTO_UDP => {
                        if let Some(udp) = packet::udp(ip.payload) {
                            self.udp_from_guest(eth.src, ip.src, ip.dst, udp, out);
                        }
                    }
                    packet::PROTO_TCP => {
                        if let Some((sport, dport, opens)) = packet::tcp_ports(ip.payload) {
                            if opens {
                                self.open((ip.src, sport, ip.dst, dport));
                            }
                        }
                        // With no socket waiting for it, the stack refuses
                        // the connection, as a closed port would.
                        self.phy.rx.push_back(frame);
                    }
                    packet::PROTO_ICMP if ip.dst == GATEWAY => self.phy.rx.push_back(frame),
                    _ => {}
                }
            }
            _ => {} // IPv6 and the rest: not carried
        }
    }

    // --- TCP ---------------------------------------------------------------

    /// The guest is opening a connection: have a socket waiting for it, and
    /// start making the host's half.
    fn open(&mut self, tuple: Tuple) {
        let (_, _, dst, dport) = tuple;
        if self.by_tuple.contains_key(&tuple) {
            return; // the guest repeating itself
        }
        // What may not be connected is still answered, and then reset: the
        // stack on its own says nothing for an address that is not its own,
        // and the guest would wait out a long timeout.
        let pages = self.pages_at.filter(|_| dst == GATEWAY && dport == 80);
        let refused = self.connections.len() >= MAX_CONNECTIONS
            || (pages.is_none() && !forwardable(dst, &self.options));
        if tracing() {
            let how = if refused { "refused" } else { "opening" };
            eprintln!("bvmm net: tcp {dst}:{dport} {how}");
        }
        let mut socket = tcp::Socket::new(
            tcp::SocketBuffer::new(vec![0u8; TCP_BUFFER]),
            tcp::SocketBuffer::new(vec![0u8; TCP_BUFFER]),
        );
        socket.set_timeout(Some(smoltcp::time::Duration::from_secs(60)));
        let endpoint = IpListenEndpoint {
            addr: Some(IpAddress::Ipv4(dst)),
            port: dport,
        };
        if socket.listen(endpoint).is_err() {
            return;
        }
        let handle = self.sockets.add(socket);
        let id = self.next_id;
        self.next_id += 1;
        self.by_tuple.insert(tuple, id);
        self.connections.insert(
            id,
            Connection {
                handle,
                tuple,
                host: None,
                to_host: None,
                to_host_backlog: Arc::default(),
                from_host: VecDeque::new(),
                from_host_backlog: Arc::default(),
                host_finished: false,
                told_guest_finished: false,
                told_host_finished: false,
                failed: refused,
                closed: false,
            },
        );
        if refused {
            return;
        }
        let events = self.events.clone();
        std::thread::spawn(move || {
            let addr = pages.unwrap_or(SocketAddr::V4(SocketAddrV4::new(dst, dport)));
            let event = match TcpStream::connect_timeout(&addr, Duration::from_secs(15)) {
                Ok(stream) => {
                    let _ = stream.set_nodelay(true);
                    Event::Connected(id, stream)
                }
                Err(_) => Event::Failed(id),
            };
            let _ = events.send(event);
        });
    }

    /// The host's half is open: start the two threads that carry its bytes.
    fn connected(&mut self, id: u64, stream: TcpStream) {
        let Some(c) = self.connections.get_mut(&id) else {
            let _ = stream.shutdown(Shutdown::Both);
            return;
        };
        let (Ok(mut reader), Ok(mut writer)) = (stream.try_clone(), stream.try_clone()) else {
            c.failed = true;
            return;
        };
        c.host = Some(stream);

        let events = self.events.clone();
        let backlog = c.from_host_backlog.clone();
        std::thread::spawn(move || {
            let mut buf = vec![0u8; 16 << 10];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => {
                        let _ = events.send(Event::HostEof(id));
                        return;
                    }
                    Ok(n) => {
                        backlog.fetch_add(n, Ordering::SeqCst);
                        if events.send(Event::HostData(id, buf[..n].to_vec())).is_err() {
                            return;
                        }
                        // Wait while the guest is slower than the host.
                        while backlog.load(Ordering::SeqCst) > HIGH_WATER {
                            std::thread::sleep(Duration::from_millis(2));
                        }
                    }
                }
            }
        });

        let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
        c.to_host = Some(tx);
        let events = self.events.clone();
        let backlog = c.to_host_backlog.clone();
        std::thread::spawn(move || {
            for buf in rx {
                if buf.is_empty() {
                    let _ = writer.shutdown(Shutdown::Write);
                    continue;
                }
                if writer.write_all(&buf).is_err() {
                    let _ = events.send(Event::Failed(id));
                    return;
                }
                let before = backlog.fetch_sub(buf.len(), Ordering::SeqCst);
                if before >= HIGH_WATER && events.send(Event::Wake).is_err() {
                    return;
                }
            }
        });
    }

    fn serve_connections(&mut self) {
        let mut finished = Vec::new();
        for (id, c) in self.connections.iter_mut() {
            let socket = self.sockets.get_mut::<tcp::Socket>(c.handle);
            // Reset the guest's half, once it exists to be reset.
            if c.failed
                && socket.state() != tcp::State::Listen
                && socket.state() != tcp::State::Closed
            {
                socket.abort();
            }
            // Host to guest.
            while socket.can_send() {
                let Some(front) = c.from_host.front_mut() else {
                    break;
                };
                let n = socket.send_slice(front).unwrap_or(0);
                c.from_host_backlog.fetch_sub(n, Ordering::SeqCst);
                if n == front.len() {
                    c.from_host.pop_front();
                } else {
                    front.drain(..n);
                    break;
                }
            }
            if c.host_finished && c.from_host.is_empty() && !c.told_guest_finished {
                socket.close();
                c.told_guest_finished = true;
            }
            // Guest to host.
            if let Some(to_host) = &c.to_host {
                while socket.can_recv() && c.to_host_backlog.load(Ordering::SeqCst) < HIGH_WATER {
                    let mut buf = vec![0u8; 16 << 10];
                    let n = socket.recv_slice(&mut buf).unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    buf.truncate(n);
                    c.to_host_backlog.fetch_add(n, Ordering::SeqCst);
                    let _ = to_host.send(buf);
                }
                let guest_finished = matches!(
                    socket.state(),
                    tcp::State::CloseWait
                        | tcp::State::LastAck
                        | tcp::State::Closing
                        | tcp::State::TimeWait
                        | tcp::State::Closed
                );
                if guest_finished && !socket.can_recv() && !c.told_host_finished {
                    let _ = to_host.send(Vec::new());
                    c.told_host_finished = true;
                }
            }
            if socket.state() == tcp::State::Closed {
                if c.closed {
                    finished.push(*id);
                }
                c.closed = true;
            }
        }
        for id in finished {
            if let Some(c) = self.connections.remove(&id) {
                self.sockets.remove(c.handle);
                self.by_tuple.remove(&c.tuple);
                // The writer first sends what it still holds, then both
                // threads end when the stream is shut.
                if let Some(host) = c.host {
                    if c.failed || c.to_host_backlog.load(Ordering::SeqCst) == 0 {
                        let _ = host.shutdown(Shutdown::Both);
                    } else {
                        let _ = host.shutdown(Shutdown::Read);
                    }
                }
            }
        }
    }

    // --- UDP ---------------------------------------------------------------

    fn udp_from_guest(
        &mut self,
        mac: [u8; 6],
        src: Ipv4Addr,
        dst: Ipv4Addr,
        udp: packet::Udp<'_>,
        out: &mut Vec<Vec<u8>>,
    ) {
        if udp.dst_port == 67 {
            self.dhcp(udp.data, out);
        } else if dst == NAME_SERVER && udp.dst_port == 53 {
            self.lookup(mac, (src, udp.src_port), udp.data, out);
        } else if forwardable(dst, &self.options) {
            self.udp_forward((src, udp.src_port), (dst, udp.dst_port), udp.data);
        }
    }

    fn dhcp(&mut self, data: &[u8], out: &mut Vec<Vec<u8>>) {
        let Some(request) = packet::dhcp_request(data) else {
            return;
        };
        let kind = match request.kind {
            packet::DHCP_DISCOVER => packet::DHCP_OFFER,
            packet::DHCP_REQUEST => packet::DHCP_ACK,
            _ => return,
        };
        let lease = Lease {
            client: GUEST,
            server: GATEWAY,
            mask: MASK,
            router: GATEWAY,
            dns: NAME_SERVER,
        };
        let reply = packet::dhcp_reply(&request, kind, &lease);
        // To everyone: the client has no address to be reached at yet.
        out.push(packet::udp_frame(
            packet::BROADCAST,
            GATEWAY_MAC,
            (GATEWAY, 67),
            (Ipv4Addr::BROADCAST, 68),
            &reply,
        ));
    }

    /// Answer a name query with the host's own resolver.
    fn lookup(
        &mut self,
        mac: [u8; 6],
        client: (Ipv4Addr, u16),
        query: &[u8],
        out: &mut Vec<Vec<u8>>,
    ) {
        let Some(question) = packet::dns_question(query) else {
            return;
        };
        let reply = |addresses: &[Ipv4Addr], rcode: u8| {
            let message = packet::dns_reply(query, question.end, addresses, rcode);
            packet::udp_frame(mac, GATEWAY_MAC, (NAME_SERVER, 53), client, &message)
        };
        if tracing() {
            eprintln!("bvmm net: lookup {} type {}", question.name, question.qtype);
        }
        if question.qtype != packet::DNS_A {
            // No such record, but the name may exist: an empty answer, so
            // the guest goes on to ask for the IPv4 address.
            out.push(reply(&[], 0));
            return;
        }
        if self.lookups.load(Ordering::SeqCst) >= MAX_LOOKUPS {
            return; // the guest will ask again
        }
        // The resolver can take seconds: not on this thread.
        self.lookups.fetch_add(1, Ordering::SeqCst);
        let (lookups, events) = (self.lookups.clone(), self.events.clone());
        let (query, end, name) = (query.to_vec(), question.end, question.name);
        std::thread::spawn(move || {
            let found: Vec<Ipv4Addr> = (name.as_str(), 0)
                .to_socket_addrs()
                .map(|addrs| {
                    addrs
                        .filter_map(|a| match a {
                            SocketAddr::V4(v4) => Some(*v4.ip()),
                            SocketAddr::V6(_) => None,
                        })
                        .take(8)
                        .collect()
                })
                .unwrap_or_default();
            let rcode = if found.is_empty() { 3 } else { 0 }; // 3: no such name
            if tracing() {
                eprintln!("bvmm net: {name} is {found:?}");
            }
            let data = packet::dns_reply(&query, end, &found, rcode);
            lookups.fetch_sub(1, Ordering::SeqCst);
            let _ = events.send(Event::Udp {
                to: client,
                from: (NAME_SERVER, 53),
                data,
            });
        });
    }

    fn udp_forward(&mut self, guest: (Ipv4Addr, u16), dst: (Ipv4Addr, u16), data: &[u8]) {
        let seconds = self.epoch.elapsed().as_secs();
        if self
            .udp
            .get(&guest)
            .is_some_and(|f| !f.alive.load(Ordering::SeqCst))
        {
            self.udp.remove(&guest);
        }
        if !self.udp.contains_key(&guest) {
            self.udp.retain(|_, f| f.alive.load(Ordering::SeqCst));
            if self.udp.len() >= MAX_UDP_FLOWS {
                return;
            }
            let Ok(socket) = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)) else {
                return;
            };
            let _ = socket.set_read_timeout(Some(Duration::from_secs(1)));
            let flow = UdpFlow {
                socket: Arc::new(socket),
                last_used: Arc::new(AtomicU64::new(seconds)),
                alive: Arc::new(AtomicBool::new(true)),
            };
            let (socket, last_used, alive) = (
                flow.socket.clone(),
                flow.last_used.clone(),
                flow.alive.clone(),
            );
            let (events, epoch) = (self.events.clone(), self.epoch);
            std::thread::spawn(move || {
                let mut buf = vec![0u8; 65536];
                loop {
                    match socket.recv_from(&mut buf) {
                        Ok((n, SocketAddr::V4(from))) => {
                            last_used.store(epoch.elapsed().as_secs(), Ordering::SeqCst);
                            // More than fits one frame cannot be delivered.
                            if n > packet::MAX_UDP_DATA {
                                continue;
                            }
                            let event = Event::Udp {
                                to: guest,
                                from: (*from.ip(), from.port()),
                                data: buf[..n].to_vec(),
                            };
                            if events.send(event).is_err() {
                                break;
                            }
                        }
                        Ok(_) => {}
                        Err(_) => {
                            let idle = epoch.elapsed().as_secs() - last_used.load(Ordering::SeqCst);
                            if idle > UDP_IDLE {
                                break;
                            }
                            // A send to an address that refused it shows up
                            // here on Windows; without this the loop spins.
                            std::thread::sleep(Duration::from_millis(1));
                        }
                    }
                }
                alive.store(false, Ordering::SeqCst);
            });
            self.udp.insert(guest, flow);
        }
        if let Some(flow) = self.udp.get(&guest) {
            flow.last_used.store(seconds, Ordering::SeqCst);
            let _ = flow.socket.send_to(data, SocketAddrV4::new(dst.0, dst.1));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    const GUEST_MAC: [u8; 6] = [2, 0, 0, 0, 0, 0x15];

    /// A NAT with the frames it sends the guest arriving on a channel.
    fn nat(options: Options) -> (Nat, Receiver<Vec<u8>>) {
        let (nat, runner) = Nat::new(options);
        let (tx, rx) = std::sync::mpsc::channel();
        runner.start(move |frame| tx.send(frame).is_ok(), || true);
        (nat, rx)
    }

    /// A guest: a second TCP/IP stack at 10.0.2.15, wired to a NAT.
    struct Guest {
        nat: Nat,
        from_nat: Receiver<Vec<u8>>,
        phy: Phy,
        iface: Interface,
        sockets: SocketSet<'static>,
    }

    impl Guest {
        fn new(allow_loopback: bool) -> Guest {
            Guest::with(Options {
                allow_loopback,
                ..Default::default()
            })
        }

        fn with(options: Options) -> Guest {
            let (nat, from_nat) = nat(options);
            let mut phy = Phy::default();
            let config = Config::new(HardwareAddress::Ethernet(EthernetAddress(GUEST_MAC)));
            let mut iface = Interface::new(config, &mut phy, now());
            iface.update_ip_addrs(|a| {
                let _ = a.push(IpCidr::new(IpAddress::Ipv4(GUEST), 24));
            });
            let _ = iface.routes_mut().add_default_ipv4_route(GATEWAY);
            Guest {
                nat,
                from_nat,
                phy,
                iface,
                sockets: SocketSet::new(Vec::new()),
            }
        }

        fn connect(&mut self, dst: Ipv4Addr, port: u16, local: u16) -> SocketHandle {
            let mut socket = tcp::Socket::new(
                tcp::SocketBuffer::new(vec![0u8; 65536]),
                tcp::SocketBuffer::new(vec![0u8; 65536]),
            );
            socket
                .connect(self.iface.context(), (IpAddress::Ipv4(dst), port), local)
                .unwrap();
            self.sockets.add(socket)
        }

        /// Move frames both ways until `done` says so; panic after 20 s.
        fn pump(&mut self, what: &str, mut done: impl FnMut(&mut SocketSet<'static>) -> bool) {
            let deadline = Instant::now() + Duration::from_secs(20);
            loop {
                self.iface.poll(now(), &mut self.phy, &mut self.sockets);
                for frame in self.phy.tx.drain(..) {
                    self.nat.frame(frame);
                }
                if done(&mut self.sockets) {
                    return;
                }
                assert!(Instant::now() < deadline, "timed out waiting for: {what}");
                if let Ok(frame) = self.from_nat.recv_timeout(Duration::from_millis(5)) {
                    self.phy.rx.push_back(frame);
                    self.phy.rx.extend(self.from_nat.try_iter());
                }
            }
        }
    }

    #[test]
    fn tcp_carries_bytes_both_ways_and_closes_cleanly() {
        // A host server that sends back what it gets, then says how much.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let mut total = 0usize;
            let mut buf = [0u8; 8192];
            loop {
                let n = s.read(&mut buf).unwrap();
                if n == 0 {
                    break; // the guest finished sending
                }
                total += n;
                s.write_all(&buf[..n]).unwrap();
            }
            s.write_all(format!("END{total}").as_bytes()).unwrap();
            total
        });

        // More than every buffer on the way, so waiting is exercised.
        let message: Vec<u8> = (0..700_000u32).map(|i| (i % 251) as u8).collect();
        let mut guest = Guest::new(true);
        let h = guest.connect(Ipv4Addr::LOCALHOST, port, 40001);
        let (mut sent, mut got, mut closed) = (0usize, Vec::new(), false);
        guest.pump("the echo and the server's last word", |sockets| {
            let s = sockets.get_mut::<tcp::Socket>(h);
            if s.can_send() && sent < message.len() {
                sent += s.send_slice(&message[sent..]).unwrap();
            }
            if sent == message.len() && !closed {
                s.close();
                closed = true;
            }
            while s.can_recv() {
                let mut buf = [0u8; 4096];
                let n = s.recv_slice(&mut buf).unwrap();
                got.extend_from_slice(&buf[..n]);
            }
            got.ends_with(b"END700000")
        });
        assert_eq!(&got[..message.len()], &message[..], "every byte, in order");
        assert_eq!(server.join().unwrap(), message.len());
        // And the connection winds down on the guest's side too.
        guest.pump("the guest's socket to close", |sockets| {
            !sockets.get::<tcp::Socket>(h).is_open()
        });
    }

    #[test]
    fn a_refused_connection_is_refused_for_the_guest_too() {
        // A port nothing listens on.
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let mut guest = Guest::new(true);
        let h = guest.connect(Ipv4Addr::LOCALHOST, port, 40002);
        guest.pump("the reset", |sockets| {
            sockets.get::<tcp::Socket>(h).state() == tcp::State::Closed
        });
    }

    /// Ask the gateway for `path` and give back the whole answer.
    fn ask_gateway(guest: &mut Guest, path: &str, local: u16) -> String {
        let h = guest.connect(GATEWAY, 80, local);
        let request = format!("GET {path} HTTP/1.1\r\nHost: 10.0.2.2\r\n\r\n");
        let (mut sent, mut got) = (false, Vec::new());
        guest.pump("the gateway's answer", |sockets| {
            let s = sockets.get_mut::<tcp::Socket>(h);
            if s.can_send() && !sent {
                s.send_slice(request.as_bytes()).unwrap();
                sent = true;
            }
            while s.can_recv() {
                let mut buf = [0u8; 4096];
                let n = s.recv_slice(&mut buf).unwrap();
                got.extend_from_slice(&buf[..n]);
            }
            sent && !s.may_recv()
        });
        String::from_utf8(got).unwrap()
    }

    #[test]
    fn the_gateway_serves_its_pages_and_nothing_else() {
        let body = "<h1>How to use this</h1>".repeat(3000); // more than one buffer
        let mut guest = Guest::with(Options {
            pages: vec![Page {
                path: "/help.html".into(),
                content_type: "text/html; charset=utf-8".into(),
                body: body.clone().into_bytes(),
            }],
            ..Default::default()
        });
        let answer = ask_gateway(&mut guest, "/help.html?x=1", 40010);
        assert!(
            answer.starts_with("HTTP/1.1 200 OK\r\n"),
            "{}",
            &answer[..60]
        );
        assert!(answer.contains("Content-Type: text/html; charset=utf-8\r\n"));
        assert!(answer.ends_with(&body), "the whole page arrived");
        let answer = ask_gateway(&mut guest, "/../etc/passwd", 40011);
        assert!(answer.starts_with("HTTP/1.1 404 "), "{answer}");

        // The gateway's other ports stay shut, and with no pages so does 80.
        let h = guest.connect(GATEWAY, 8080, 40012);
        guest.pump("another port to be refused", |sockets| {
            sockets.get::<tcp::Socket>(h).state() == tcp::State::Closed
        });
        let mut bare = Guest::new(true);
        let h = bare.connect(GATEWAY, 80, 40013);
        bare.pump("port 80 to be refused without pages", |sockets| {
            sockets.get::<tcp::Socket>(h).state() == tcp::State::Closed
        });
    }

    #[test]
    fn loopback_and_the_private_network_are_not_forwarded() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        // One guest each: the test's stand-in guest resolves only one
        // neighbour at a time.
        for (dst, dport) in [(Ipv4Addr::LOCALHOST, port), (NAME_SERVER, 853)] {
            let mut guest = Guest::new(false);
            let h = guest.connect(dst, dport, 40003);
            guest.pump("the connection to be refused", |sockets| {
                sockets.get::<tcp::Socket>(h).state() == tcp::State::Closed
            });
        }
        assert!(
            listener.accept().is_err(),
            "the host's loopback service saw nothing"
        );
        let options = Options::default();
        assert!(forwardable(Ipv4Addr::new(93, 184, 216, 34), &options));
        assert!(
            forwardable(Ipv4Addr::new(192, 168, 1, 10), &options),
            "the LAN is allowed"
        );
        for no in [
            GATEWAY,
            Ipv4Addr::BROADCAST,
            Ipv4Addr::new(224, 0, 0, 251),
            Ipv4Addr::new(169, 254, 1, 1),
        ] {
            assert!(!forwardable(no, &options), "{no}");
        }
    }

    /// Wait for a UDP frame from the NAT and return (source, destination port, data).
    fn udp_reply(rx: &Receiver<Vec<u8>>) -> ((Ipv4Addr, u16), u16, Vec<u8>) {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let frame = rx
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .expect("a reply from the NAT");
            let Some(eth) = packet::ethernet(&frame) else {
                continue;
            };
            let Some(ip) = packet::ipv4(eth.payload).filter(|ip| ip.protocol == packet::PROTO_UDP)
            else {
                continue;
            };
            let udp = packet::udp(ip.payload).unwrap();
            return ((ip.src, udp.src_port), udp.dst_port, udp.data.to_vec());
        }
    }

    fn from_guest(src_port: u16, dst: (Ipv4Addr, u16), data: &[u8]) -> Vec<u8> {
        packet::udp_frame(GATEWAY_MAC, GUEST_MAC, (GUEST, src_port), dst, data)
    }

    #[test]
    fn udp_goes_out_and_the_answer_comes_back() {
        let server = UdpSocket::bind("127.0.0.1:0").unwrap();
        let port = server.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let mut buf = [0u8; 100];
            let (n, from) = server.recv_from(&mut buf).unwrap();
            let mut answer = b"re:".to_vec();
            answer.extend_from_slice(&buf[..n]);
            server.send_to(&answer, from).unwrap();
        });
        let (nat, rx) = nat(Options {
            allow_loopback: true,
            ..Default::default()
        });
        nat.frame(from_guest(5555, (Ipv4Addr::LOCALHOST, port), b"ping"));
        let (from, to_port, data) = udp_reply(&rx);
        assert_eq!(
            from,
            (Ipv4Addr::LOCALHOST, port),
            "from the address the guest wrote to"
        );
        assert_eq!((to_port, data.as_slice()), (5555, &b"re:ping"[..]));
    }

    #[test]
    fn the_guest_is_given_its_address() {
        let (nat, rx) = nat(Options {
            allow_loopback: false,
            ..Default::default()
        });
        for (kind, answer) in [
            (packet::DHCP_DISCOVER, packet::DHCP_OFFER),
            (packet::DHCP_REQUEST, packet::DHCP_ACK),
        ] {
            let mut d = vec![0u8; 240];
            d[0] = 1;
            d[4..8].copy_from_slice(&[1, 2, 3, 4]);
            d[28..34].copy_from_slice(&GUEST_MAC);
            d[236..240].copy_from_slice(&[99, 130, 83, 99]);
            d.extend_from_slice(&[53, 1, kind, 255]);
            let frame = packet::udp_frame(
                packet::BROADCAST,
                GUEST_MAC,
                (Ipv4Addr::UNSPECIFIED, 68),
                (Ipv4Addr::BROADCAST, 67),
                &d,
            );
            nat.frame(frame);
            let (from, to_port, reply) = udp_reply(&rx);
            assert_eq!((from, to_port), ((GATEWAY, 67), 68));
            assert_eq!(&reply[16..20], &GUEST.octets());
            assert_eq!(&reply[240..243], &[53, 1, answer]);
        }
    }

    #[test]
    fn names_are_looked_up_with_the_hosts_resolver() {
        let (nat, rx) = nat(Options {
            allow_loopback: false,
            ..Default::default()
        });
        let query = |name: &str, qtype: u16| {
            let mut q = vec![0xab, 0xcd, 0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0];
            for label in name.split('.') {
                q.push(label.len() as u8);
                q.extend_from_slice(label.as_bytes());
            }
            q.push(0);
            q.extend_from_slice(&qtype.to_be_bytes());
            q.extend_from_slice(&[0, 1]);
            q
        };
        // A name every host knows.
        nat.frame(from_guest(4000, (NAME_SERVER, 53), &query("localhost", 1)));
        let (from, to_port, r) = udp_reply(&rx);
        assert_eq!((from, to_port), ((NAME_SERVER, 53), 4000));
        assert_eq!((&r[..2], r[3] & 0xf), (&[0xab, 0xcd][..], 0));
        assert_eq!(&r[r.len() - 4..], &[127, 0, 0, 1]);
        // An IPv6 question gets an empty answer at once, not an error.
        nat.frame(from_guest(4001, (NAME_SERVER, 53), &query("localhost", 28)));
        let (_, to_port, r) = udp_reply(&rx);
        assert_eq!((to_port, r[3] & 0xf, &r[6..8]), (4001, 0, &[0, 0][..]));
        // A name that cannot exist (the .invalid domain is reserved for that).
        nat.frame(from_guest(
            4002,
            (NAME_SERVER, 53),
            &query("nothing.invalid", 1),
        ));
        let (_, to_port, r) = udp_reply(&rx);
        assert_eq!((to_port, r[3] & 0xf), (4002, 3));
    }

    #[test]
    fn the_gateway_answers_a_ping_and_arp() {
        use smoltcp::socket::icmp;
        use smoltcp::wire::{Icmpv4Packet, Icmpv4Repr};
        let mut guest = Guest::new(false);
        let mut socket = icmp::Socket::new(
            icmp::PacketBuffer::new(vec![icmp::PacketMetadata::EMPTY; 4], vec![0u8; 512]),
            icmp::PacketBuffer::new(vec![icmp::PacketMetadata::EMPTY; 4], vec![0u8; 512]),
        );
        socket.bind(icmp::Endpoint::Ident(0x22)).unwrap();
        let h = guest.sockets.add(socket);
        let (mut sent, mut answered) = (false, false);
        // The guest must first find the gateway's hardware address: that is
        // the ARP exchange, done by the two stacks.
        guest.pump("the echo reply", |sockets| {
            let s = sockets.get_mut::<icmp::Socket>(h);
            if !sent && s.can_send() {
                let repr = Icmpv4Repr::EchoRequest {
                    ident: 0x22,
                    seq_no: 1,
                    data: b"hi",
                };
                let buf = s.send(repr.buffer_len(), IpAddress::Ipv4(GATEWAY)).unwrap();
                repr.emit(&mut Icmpv4Packet::new_unchecked(buf), &Default::default());
                sent = true;
            }
            if s.can_recv() {
                let (data, from) = s.recv().unwrap();
                let packet = Icmpv4Packet::new_checked(data).unwrap();
                answered = from == IpAddress::Ipv4(GATEWAY)
                    && matches!(
                        Icmpv4Repr::parse(&packet, &Default::default()),
                        Ok(Icmpv4Repr::EchoReply {
                            ident: 0x22,
                            seq_no: 1,
                            ..
                        })
                    );
            }
            answered
        });
    }
}
