// SPDX-FileCopyrightText: 2026 Bryant Frankford <nathanielfrankford@gmail.com> and Contributors
// SPDX-License-Identifier: GPL-3.0-or-later OR LGPL-3.0-or-later
//! Just enough of Ethernet, IPv4, UDP, DHCP and DNS to serve one guest.
//! Written from RFC 791, 768, 2131, 2132 and 1035.

use std::net::Ipv4Addr;

pub const ETHERTYPE_IPV4: u16 = 0x0800;
pub const ETHERTYPE_ARP: u16 = 0x0806;
pub const PROTO_ICMP: u8 = 1;
pub const PROTO_TCP: u8 = 6;
pub const PROTO_UDP: u8 = 17;
pub const BROADCAST: [u8; 6] = [0xff; 6];

fn be16(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_be_bytes([*b.get(at)?, *b.get(at + 1)?]))
}

fn ip_at(b: &[u8], at: usize) -> Option<Ipv4Addr> {
    let s = b.get(at..at + 4)?;
    Some(Ipv4Addr::new(s[0], s[1], s[2], s[3]))
}

/// An Ethernet frame's addresses and what it carries.
pub struct Ethernet<'a> {
    pub src: [u8; 6],
    pub ethertype: u16,
    pub payload: &'a [u8],
}

pub fn ethernet(frame: &[u8]) -> Option<Ethernet<'_>> {
    Some(Ethernet {
        src: frame.get(6..12)?.try_into().ok()?,
        ethertype: be16(frame, 12)?,
        payload: frame.get(14..)?,
    })
}

/// An IPv4 packet's header fields and payload. Fragments are refused.
pub struct Ipv4<'a> {
    pub src: Ipv4Addr,
    pub dst: Ipv4Addr,
    pub protocol: u8,
    pub payload: &'a [u8],
}

pub fn ipv4(packet: &[u8]) -> Option<Ipv4<'_>> {
    let first = *packet.first()?;
    if first >> 4 != 4 {
        return None;
    }
    let header = (first & 0xf) as usize * 4;
    let total = be16(packet, 2)? as usize;
    let fragment = be16(packet, 6)?;
    if header < 20 || total < header || packet.len() < total || fragment & 0x3fff != 0 {
        return None;
    }
    Some(Ipv4 {
        src: ip_at(packet, 12)?,
        dst: ip_at(packet, 16)?,
        protocol: packet[9],
        payload: &packet[header..total],
    })
}

/// A UDP datagram's ports and data.
pub struct Udp<'a> {
    pub src_port: u16,
    pub dst_port: u16,
    pub data: &'a [u8],
}

pub fn udp(segment: &[u8]) -> Option<Udp<'_>> {
    let len = be16(segment, 4)? as usize;
    if len < 8 || segment.len() < len {
        return None;
    }
    Some(Udp {
        src_port: be16(segment, 0)?,
        dst_port: be16(segment, 2)?,
        data: &segment[8..len],
    })
}

/// A TCP segment's ports, and whether it opens a connection (SYN without ACK).
pub fn tcp_ports(segment: &[u8]) -> Option<(u16, u16, bool)> {
    let flags = *segment.get(13)?;
    Some((be16(segment, 0)?, be16(segment, 2)?, flags & 0x12 == 0x02))
}

/// The Internet checksum (RFC 1071).
pub fn checksum(data: &[u8]) -> u16 {
    let mut sum = 0u32;
    for pair in data.chunks(2) {
        sum += u16::from_be_bytes([pair[0], *pair.get(1).unwrap_or(&0)]) as u32;
    }
    while sum >> 16 != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !(sum as u16)
}

/// One UDP datagram in an IPv4 packet in an Ethernet frame. The UDP
/// checksum is left at zero, which IPv4 allows ("not computed").
pub fn udp_frame(
    eth_dst: [u8; 6],
    eth_src: [u8; 6],
    src: (Ipv4Addr, u16),
    dst: (Ipv4Addr, u16),
    data: &[u8],
) -> Vec<u8> {
    let udp_len = 8 + data.len();
    let total = 20 + udp_len;
    let mut f = Vec::with_capacity(14 + total);
    f.extend_from_slice(&eth_dst);
    f.extend_from_slice(&eth_src);
    f.extend_from_slice(&ETHERTYPE_IPV4.to_be_bytes());
    let ip = f.len();
    f.extend_from_slice(&[0x45, 0]);
    f.extend_from_slice(&(total as u16).to_be_bytes());
    f.extend_from_slice(&[0, 0, 0x40, 0, 64, PROTO_UDP, 0, 0]); // don't fragment, TTL 64
    f.extend_from_slice(&src.0.octets());
    f.extend_from_slice(&dst.0.octets());
    let sum = checksum(&f[ip..ip + 20]);
    f[ip + 10..ip + 12].copy_from_slice(&sum.to_be_bytes());
    f.extend_from_slice(&src.1.to_be_bytes());
    f.extend_from_slice(&dst.1.to_be_bytes());
    f.extend_from_slice(&(udp_len as u16).to_be_bytes());
    f.extend_from_slice(&[0, 0]);
    f.extend_from_slice(data);
    f
}

/// The most data one UDP datagram can carry to the guest unfragmented.
pub const MAX_UDP_DATA: usize = 1500 - 20 - 8;

// --- DHCP -----------------------------------------------------------------

pub const DHCP_DISCOVER: u8 = 1;
pub const DHCP_OFFER: u8 = 2;
pub const DHCP_REQUEST: u8 = 3;
pub const DHCP_ACK: u8 = 5;
const DHCP_MAGIC: [u8; 4] = [99, 130, 83, 99];

/// What a DHCP client asked: its transaction, its hardware address, and the
/// kind of message.
pub struct DhcpRequest {
    pub xid: [u8; 4],
    pub flags: [u8; 2],
    pub chaddr: [u8; 16],
    pub kind: u8,
}

pub fn dhcp_request(data: &[u8]) -> Option<DhcpRequest> {
    if data.len() < 240 || data[0] != 1 || data[236..240] != DHCP_MAGIC {
        return None;
    }
    let mut kind = None;
    let mut at = 240;
    while at < data.len() {
        match data[at] {
            0 => at += 1,
            255 => break,
            code => {
                let len = *data.get(at + 1)? as usize;
                let value = data.get(at + 2..at + 2 + len)?;
                if code == 53 && len == 1 {
                    kind = Some(value[0]);
                }
                at += 2 + len;
            }
        }
    }
    Some(DhcpRequest {
        xid: data[4..8].try_into().ok()?,
        flags: data[10..12].try_into().ok()?,
        chaddr: data[28..44].try_into().ok()?,
        kind: kind?,
    })
}

/// The addresses a DHCP reply hands out.
pub struct Lease {
    pub client: Ipv4Addr,
    pub server: Ipv4Addr,
    pub mask: Ipv4Addr,
    pub router: Ipv4Addr,
    pub dns: Ipv4Addr,
}

pub fn dhcp_reply(request: &DhcpRequest, kind: u8, lease: &Lease) -> Vec<u8> {
    let mut r = vec![0u8; 240];
    r[0] = 2; // reply
    r[1] = 1; // Ethernet
    r[2] = 6;
    r[4..8].copy_from_slice(&request.xid);
    r[10..12].copy_from_slice(&request.flags);
    r[16..20].copy_from_slice(&lease.client.octets());
    r[20..24].copy_from_slice(&lease.server.octets());
    r[28..44].copy_from_slice(&request.chaddr);
    r[236..240].copy_from_slice(&DHCP_MAGIC);
    let mut option = |code: u8, value: &[u8]| {
        r.push(code);
        r.push(value.len() as u8);
        r.extend_from_slice(value);
    };
    option(53, &[kind]);
    option(54, &lease.server.octets());
    option(51, &(7 * 86400u32).to_be_bytes());
    option(1, &lease.mask.octets());
    option(3, &lease.router.octets());
    option(6, &lease.dns.octets());
    r.push(255);
    r
}

// --- DNS ------------------------------------------------------------------

pub const DNS_A: u16 = 1;

/// The one question of a DNS query: the name, its type, and how many bytes
/// of the message the header and question take.
pub struct DnsQuestion {
    pub name: String,
    pub qtype: u16,
    pub end: usize,
}

pub fn dns_question(message: &[u8]) -> Option<DnsQuestion> {
    // A query (QR clear) with exactly one question.
    if message.len() < 12 || message[2] & 0x80 != 0 || be16(message, 4)? != 1 {
        return None;
    }
    let mut at = 12;
    let mut labels = Vec::new();
    loop {
        let len = *message.get(at)? as usize;
        at += 1;
        if len == 0 {
            break;
        }
        if len > 63 {
            return None; // compression has no place in a question
        }
        labels.push(String::from_utf8_lossy(message.get(at..at + len)?).into_owned());
        at += len;
    }
    let qtype = be16(message, at)?;
    let class = be16(message, at + 2)?;
    if class != 1 || labels.is_empty() {
        return None;
    }
    Some(DnsQuestion {
        name: labels.join("."),
        qtype,
        end: at + 4,
    })
}

/// Answer `query` (whose question ends at `question_end`) with `addresses`
/// as A records, or with `rcode` and no records.
pub fn dns_reply(query: &[u8], question_end: usize, addresses: &[Ipv4Addr], rcode: u8) -> Vec<u8> {
    let mut r = query[..question_end].to_vec();
    r[2] = 0x80 | (query[2] & 0x01); // a response; echo "recursion desired"
    r[3] = 0x80 | (rcode & 0x0f); // recursion available
    r[6..8].copy_from_slice(&(addresses.len() as u16).to_be_bytes());
    r[8..12].fill(0);
    for a in addresses {
        r.extend_from_slice(&[0xc0, 12]); // the name: a pointer to the question
        r.extend_from_slice(&DNS_A.to_be_bytes());
        r.extend_from_slice(&1u16.to_be_bytes());
        r.extend_from_slice(&60u32.to_be_bytes());
        r.extend_from_slice(&4u16.to_be_bytes());
        r.extend_from_slice(&a.octets());
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_udp_frame_parses_back_and_its_header_checksum_holds() {
        let a = (Ipv4Addr::new(10, 0, 2, 3), 53);
        let b = (Ipv4Addr::new(10, 0, 2, 15), 40000);
        let f = udp_frame([1; 6], [2; 6], a, b, b"hello");
        let eth = ethernet(&f).unwrap();
        assert_eq!((eth.src, eth.ethertype), ([2; 6], ETHERTYPE_IPV4));
        assert_eq!(
            checksum(&eth.payload[..20]),
            0,
            "a correct header sums to zero"
        );
        let ip = ipv4(eth.payload).unwrap();
        assert_eq!((ip.src, ip.dst, ip.protocol), (a.0, b.0, PROTO_UDP));
        let u = udp(ip.payload).unwrap();
        assert_eq!((u.src_port, u.dst_port, u.data), (53, 40000, &b"hello"[..]));
        // Truncated and fragmented packets are refused, not misread.
        assert!(ipv4(&eth.payload[..25]).is_none());
        let mut frag = eth.payload.to_vec();
        frag[6] = 0x20;
        assert!(ipv4(&frag).is_none());
    }

    #[test]
    fn the_checksum_matches_the_rfc_example() {
        // RFC 1071 section 3: the sum of these words is ddf2, its complement 220d.
        assert_eq!(
            checksum(&[0x00, 0x01, 0xf2, 0x03, 0xf4, 0xf5, 0xf6, 0xf7]),
            0x220d
        );
        assert_eq!(checksum(&[0xff]), 0x00ff, "an odd byte is padded with zero");
    }

    fn discover(kind: u8) -> Vec<u8> {
        let mut d = vec![0u8; 240];
        d[0] = 1;
        d[4..8].copy_from_slice(&[0xde, 0xad, 0xbe, 0xef]);
        d[28..34].copy_from_slice(&[2, 0, 0, 0, 0, 1]);
        d[236..240].copy_from_slice(&DHCP_MAGIC);
        d.extend_from_slice(&[0, 55, 2, 1, 3, 53, 1, kind, 255]);
        d
    }

    #[test]
    fn dhcp_requests_are_read_and_answered() {
        let req = dhcp_request(&discover(DHCP_DISCOVER)).unwrap();
        assert_eq!(
            (req.kind, req.xid),
            (DHCP_DISCOVER, [0xde, 0xad, 0xbe, 0xef])
        );
        let lease = Lease {
            client: Ipv4Addr::new(10, 0, 2, 15),
            server: Ipv4Addr::new(10, 0, 2, 2),
            mask: Ipv4Addr::new(255, 255, 255, 0),
            router: Ipv4Addr::new(10, 0, 2, 2),
            dns: Ipv4Addr::new(10, 0, 2, 3),
        };
        let reply = dhcp_reply(&req, DHCP_OFFER, &lease);
        assert_eq!((reply[0], &reply[4..8]), (2, &[0xde, 0xad, 0xbe, 0xef][..]));
        assert_eq!(&reply[16..20], &[10, 0, 2, 15], "the address offered");
        assert_eq!(&reply[28..34], &[2, 0, 0, 0, 0, 1]);
        let options = &reply[240..];
        assert_eq!(&options[..3], &[53, 1, DHCP_OFFER]);
        assert!(
            options.windows(6).any(|w| w == [6, 4, 10, 0, 2, 3]),
            "the DNS server"
        );
        assert_eq!(*options.last().unwrap(), 255);
        // Not DHCP at all, and a reply instead of a request.
        assert!(dhcp_request(&[0u8; 100]).is_none());
        let mut wrong = discover(DHCP_REQUEST);
        wrong[0] = 2;
        assert!(dhcp_request(&wrong).is_none());
    }

    fn query(name: &str, qtype: u16) -> Vec<u8> {
        let mut q = vec![0x12, 0x34, 0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0];
        for label in name.split('.') {
            q.push(label.len() as u8);
            q.extend_from_slice(label.as_bytes());
        }
        q.push(0);
        q.extend_from_slice(&qtype.to_be_bytes());
        q.extend_from_slice(&1u16.to_be_bytes());
        q
    }

    #[test]
    fn dns_questions_are_read_and_answered() {
        let q = query("example.org", DNS_A);
        let question = dns_question(&q).unwrap();
        assert_eq!(
            (question.name.as_str(), question.qtype, question.end),
            ("example.org", 1, q.len())
        );
        let r = dns_reply(&q, question.end, &[Ipv4Addr::new(93, 184, 216, 34)], 0);
        assert_eq!(&r[..2], &[0x12, 0x34], "the same transaction");
        assert_eq!(
            (r[2] & 0x80, r[3] & 0x0f),
            (0x80, 0),
            "a response, no error"
        );
        assert_eq!(&r[6..8], &[0, 1], "one answer");
        assert_eq!(&r[r.len() - 4..], &[93, 184, 216, 34]);
        // No such name: the code, and no records.
        let r = dns_reply(&q, question.end, &[], 3);
        assert_eq!((r[3] & 0x0f, &r[6..8], r.len()), (3, &[0, 0][..], q.len()));
        // A response is not a question; neither is a truncated message.
        assert!(dns_question(&r).is_none());
        assert!(dns_question(&q[..q.len() - 3]).is_none());
    }
}
