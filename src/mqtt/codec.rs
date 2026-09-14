//! MQTT 3.1.1 packet encoding and decoding.
//!
//! No sockets, no runtime, no allocation beyond `alloc` — the same split
//! [`discovery`](crate::discovery) makes, and for the same reason (D-100): the part that decides
//! *what goes on the wire* can be tested exhaustively and against the specification's own byte
//! layouts, while the part that moves bytes cannot be tested without a peer.
//!
//! Only the packets this crate sends and receives are modelled. There is no `PUBREC`/`PUBREL`/
//! `PUBCOMP`, because [`QoS`] has no `ExactlyOnce` — see its documentation for why that is a
//! feature rather than a gap.
//!
//! References are to the OASIS MQTT 3.1.1 specification, e.g. `[MQTT 3.1.1 §3.1.2.3]`.

use crate::std_shim::{String, ToString, Vec, format, vec};

/// Quality of service for a publication.
///
/// **There is no `ExactlyOnce`.** MQTT's QoS 2 needs a four-packet handshake and a store of
/// in-flight state on both ends, and OpenADR needs neither: `[Notifiers §16.2]` tells a client to
/// re-`GET` the object rather than trust the message, so a duplicate notification costs a
/// conditional read that returns `304` and nothing else.
///
/// Absent rather than rejected at runtime: a variant nothing can honour is a configuration that
/// compiles and then times out waiting for an acknowledgement the protocol never sends.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum QoS {
    /// Fire and forget. The publish completes when the bytes reach the socket.
    AtMostOnce,
    /// Acknowledged. The publish completes when the broker sends `PUBACK` (D-057).
    #[default]
    AtLeastOnce,
}

impl QoS {
    fn bits(self) -> u8 {
        match self {
            QoS::AtMostOnce => 0,
            QoS::AtLeastOnce => 1,
        }
    }

    fn from_bits(bits: u8) -> Result<Self, CodecError> {
        match bits {
            0 => Ok(QoS::AtMostOnce),
            1 => Ok(QoS::AtLeastOnce),
            // A broker may legitimately deliver QoS 2 to a subscriber that asked for less, but
            // nothing here ever asks for more than 1, so a 2 on the wire is a broker doing
            // something neither end agreed to. 3 is malformed `[MQTT 3.1.1 §3.3.1.2]`.
            other => Err(CodecError::Malformed(format!(
                "unsupported QoS {other} in a PUBLISH"
            ))),
        }
    }
}

/// Why a packet could not be read or written.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CodecError {
    /// The bytes are not a well-formed packet.
    #[error("malformed MQTT packet: {0}")]
    Malformed(String),
    /// A packet larger than this client will handle.
    #[error("MQTT packet of {size} bytes exceeds the {limit}-byte limit")]
    TooLarge {
        /// The declared size.
        size: usize,
        /// The ceiling.
        limit: usize,
    },
    /// A string field was not UTF-8, which MQTT requires `[MQTT 3.1.1 §1.5.3]`.
    #[error("MQTT string field is not UTF-8")]
    NotUtf8,
}

/// Largest packet this client will encode or accept.
///
/// A notification is one object's JSON; the VTN's own body limit is 8 MB and a broker will refuse
/// far less. The bound exists because the remaining-length field can declare 256 MB and a decoder
/// that believes it is a decoder that allocates 256 MB on one hostile frame — the shape of D-047,
/// where the check belongs before the allocation rather than after it.
pub const MAX_PACKET: usize = 8 * 1024 * 1024;

/// What the broker told us when it accepted or refused the connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConnAck {
    /// Whether the broker restored a previous session for this client id `[MQTT 3.1.1 §3.2.2.2]`.
    ///
    /// **The subscriber must re-`SUBSCRIBE` when this is false.** A client that connects with
    /// `cleanSession = 0` and assumes its subscriptions survived is a client that stops receiving
    /// anything the first time the broker is restarted, with no error on either end — which is
    /// exactly the silent failure per-VEN topics exist to avoid.
    pub session_present: bool,
    /// The return code `[MQTT 3.1.1 §3.2.2.3]`. Zero is accepted.
    pub code: u8,
}

impl ConnAck {
    /// Whether the broker accepted the connection.
    pub fn is_accepted(&self) -> bool {
        self.code == 0
    }

    /// What a non-zero return code means, in words `[MQTT 3.1.1 §3.2.2.3]`.
    pub fn reason(&self) -> &'static str {
        match self.code {
            0 => "accepted",
            1 => "unacceptable protocol version",
            2 => "client identifier rejected",
            3 => "server unavailable",
            4 => "bad username or password",
            5 => "not authorized",
            _ => "unknown return code",
        }
    }
}

/// A publication travelling in either direction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Publish {
    /// The topic it was published to.
    pub topic: String,
    /// Quality of service.
    pub qos: QoS,
    /// The retain flag.
    pub retain: bool,
    /// Whether the broker marked this a redelivery.
    pub dup: bool,
    /// The identifier, present when `qos` is not [`QoS::AtMostOnce`].
    pub packet_id: Option<u16>,
    /// The message body.
    pub payload: Vec<u8>,
}

/// A packet this client understands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Packet {
    /// Broker's answer to `CONNECT`.
    ConnAck(ConnAck),
    /// A publication.
    Publish(Publish),
    /// Broker's acknowledgement of a QoS 1 publication.
    PubAck(u16),
    /// Broker's answer to `SUBSCRIBE`, one return code per requested filter.
    SubAck {
        /// The identifier of the `SUBSCRIBE` being answered.
        packet_id: u16,
        /// Granted QoS per filter, or `0x80` for a refusal `[MQTT 3.1.1 §3.9.3]`.
        codes: Vec<u8>,
    },
    /// Broker's answer to `PINGREQ`.
    PingResp,
}

/// What a `CONNECT` says.
#[derive(Debug, Clone)]
pub struct Connect {
    /// Client identifier. Must be unique per connection to a broker.
    pub client_id: String,
    /// Keep-alive, in seconds.
    pub keep_alive_secs: u16,
    /// Whether the broker should discard any previous session for this client id.
    pub clean_session: bool,
    /// Username, when the broker authenticates.
    pub username: Option<String>,
    /// Password — where OpenADR puts the access token `[Notifiers §12.2]`.
    pub password: Option<String>,
}

// --- encoding ---------------------------------------------------------------

/// `CONNECT` `[MQTT 3.1.1 §3.1]`.
pub fn encode_connect(connect: &Connect) -> Vec<u8> {
    let mut body = Vec::new();
    put_string(&mut body, "MQTT");
    body.push(4); // protocol level 4 == MQTT 3.1.1 `[MQTT 3.1.1 §3.1.2.2]`
    let mut flags = 0u8;
    if connect.clean_session {
        flags |= 0x02;
    }
    if connect.username.is_some() {
        flags |= 0x80;
    }
    if connect.password.is_some() {
        flags |= 0x40;
    }
    body.push(flags);
    body.extend_from_slice(&connect.keep_alive_secs.to_be_bytes());
    put_string(&mut body, &connect.client_id);
    if let Some(u) = &connect.username {
        put_string(&mut body, u);
    }
    if let Some(p) = &connect.password {
        put_string(&mut body, p);
    }
    frame(0x10, body)
}

/// `PUBLISH` `[MQTT 3.1.1 §3.3]`.
pub fn encode_publish(publish: &Publish) -> Vec<u8> {
    let mut header = 0x30 | (publish.qos.bits() << 1);
    if publish.retain {
        header |= 0x01;
    }
    if publish.dup {
        header |= 0x08;
    }
    let mut body = Vec::new();
    put_string(&mut body, &publish.topic);
    if publish.qos != QoS::AtMostOnce {
        // A QoS 1 publish without an identifier is unacknowledgeable, so this is a programming
        // error rather than a wire condition; zero is the value a broker rejects.
        body.extend_from_slice(&publish.packet_id.unwrap_or(0).to_be_bytes());
    }
    body.extend_from_slice(&publish.payload);
    frame(header, body)
}

/// `PUBACK` `[MQTT 3.1.1 §3.4]` — sent by a subscriber for each QoS 1 message it receives.
pub fn encode_puback(packet_id: u16) -> Vec<u8> {
    frame(0x40, packet_id.to_be_bytes().to_vec())
}

/// `SUBSCRIBE` `[MQTT 3.1.1 §3.8]`. The fixed header's low nibble is reserved as `0b0010`.
pub fn encode_subscribe(packet_id: u16, filters: &[(String, QoS)]) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&packet_id.to_be_bytes());
    for (filter, qos) in filters {
        put_string(&mut body, filter);
        body.push(qos.bits());
    }
    frame(0x82, body)
}

/// `PINGREQ` `[MQTT 3.1.1 §3.12]`.
pub fn encode_pingreq() -> Vec<u8> {
    vec![0xc0, 0x00]
}

/// `DISCONNECT` `[MQTT 3.1.1 §3.14]`.
pub fn encode_disconnect() -> Vec<u8> {
    vec![0xe0, 0x00]
}

fn frame(header: u8, body: Vec<u8>) -> Vec<u8> {
    let mut out = Vec::with_capacity(body.len() + 5);
    out.push(header);
    put_remaining_length(&mut out, body.len());
    out.extend_from_slice(&body);
    out
}

fn put_string(out: &mut Vec<u8>, s: &str) {
    let bytes = s.as_bytes();
    out.extend_from_slice(&(bytes.len() as u16).to_be_bytes());
    out.extend_from_slice(bytes);
}

fn put_remaining_length(out: &mut Vec<u8>, mut value: usize) {
    loop {
        let mut byte = (value % 128) as u8;
        value /= 128;
        if value > 0 {
            byte |= 0x80;
        }
        out.push(byte);
        if value == 0 {
            break;
        }
    }
}

// --- decoding ---------------------------------------------------------------

/// Try to read one packet from the front of `buf`.
///
/// `Ok(None)` means the buffer does not yet hold a whole packet — the caller reads more and tries
/// again. `Ok(Some((packet, used)))` means `used` bytes should be consumed.
///
/// Packets this client never receives (`CONNECT`, `SUBSCRIBE`, `PINGREQ`, …) and QoS 2's handshake
/// are consumed and reported as `Ok(None)` with their length — see [`decode`]'s `used` — rather
/// than treated as errors, because a broker sending one is not a reason to drop a connection that
/// is otherwise delivering dispatch instructions.
pub fn decode(buf: &[u8]) -> Result<Option<(Option<Packet>, usize)>, CodecError> {
    if buf.len() < 2 {
        return Ok(None);
    }
    let header = buf[0];
    let Some((remaining, length_bytes)) = take_remaining_length(&buf[1..])? else {
        return Ok(None);
    };
    if remaining > MAX_PACKET {
        return Err(CodecError::TooLarge {
            size: remaining,
            limit: MAX_PACKET,
        });
    }
    let start = 1 + length_bytes;
    let total = start + remaining;
    if buf.len() < total {
        return Ok(None);
    }
    let body = &buf[start..total];
    let packet = decode_body(header, body)?;
    Ok(Some((packet, total)))
}

fn decode_body(header: u8, body: &[u8]) -> Result<Option<Packet>, CodecError> {
    match header >> 4 {
        // CONNACK
        2 => {
            if body.len() < 2 {
                return Err(CodecError::Malformed("short CONNACK".to_string()));
            }
            Ok(Some(Packet::ConnAck(ConnAck {
                // Only bit 0 is the session-present flag; the rest are reserved and must be zero.
                session_present: body[0] & 0x01 == 1,
                code: body[1],
            })))
        }
        // PUBLISH
        3 => {
            let qos = QoS::from_bits((header >> 1) & 0x03)?;
            let mut cursor = 0usize;
            let topic = take_string(body, &mut cursor)?;
            let packet_id = if qos == QoS::AtMostOnce {
                None
            } else {
                Some(take_u16(body, &mut cursor)?)
            };
            Ok(Some(Packet::Publish(Publish {
                topic,
                qos,
                retain: header & 0x01 == 1,
                dup: header & 0x08 == 0x08,
                packet_id,
                payload: body[cursor..].to_vec(),
            })))
        }
        // PUBACK
        4 => {
            let mut cursor = 0usize;
            Ok(Some(Packet::PubAck(take_u16(body, &mut cursor)?)))
        }
        // SUBACK
        9 => {
            let mut cursor = 0usize;
            let packet_id = take_u16(body, &mut cursor)?;
            Ok(Some(Packet::SubAck {
                packet_id,
                codes: body[cursor..].to_vec(),
            }))
        }
        // PINGRESP
        13 => Ok(Some(Packet::PingResp)),
        // Anything else is well-framed and not ours. Consumed, not fatal.
        _ => Ok(None),
    }
}

fn take_remaining_length(buf: &[u8]) -> Result<Option<(usize, usize)>, CodecError> {
    let mut value = 0usize;
    let mut multiplier = 1usize;
    for (index, byte) in buf.iter().take(4).enumerate() {
        value += usize::from(byte & 0x7f) * multiplier;
        if byte & 0x80 == 0 {
            return Ok(Some((value, index + 1)));
        }
        multiplier *= 128;
    }
    // Four continuation bytes is the maximum `[MQTT 3.1.1 §2.2.3]`; a fifth is malformed rather
    // than "read more", and treating it as the latter is an infinite read on one bad byte.
    if buf.len() >= 4 {
        return Err(CodecError::Malformed(
            "remaining length is longer than four bytes".to_string(),
        ));
    }
    Ok(None)
}

fn take_u16(buf: &[u8], cursor: &mut usize) -> Result<u16, CodecError> {
    if buf.len() < *cursor + 2 {
        return Err(CodecError::Malformed("truncated 16-bit field".to_string()));
    }
    let value = u16::from_be_bytes([buf[*cursor], buf[*cursor + 1]]);
    *cursor += 2;
    Ok(value)
}

fn take_string(buf: &[u8], cursor: &mut usize) -> Result<String, CodecError> {
    let len = usize::from(take_u16(buf, cursor)?);
    if buf.len() < *cursor + len {
        return Err(CodecError::Malformed("truncated string field".to_string()));
    }
    let s = core::str::from_utf8(&buf[*cursor..*cursor + len])
        .map_err(|_| CodecError::NotUtf8)?
        .to_string();
    *cursor += len;
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_connect_carries_the_protocol_name_level_and_credentials() {
        let bytes = encode_connect(&Connect {
            client_id: "ven-1".into(),
            keep_alive_secs: 30,
            clean_session: false,
            username: Some("ven-1".into()),
            password: Some("token".into()),
        });
        assert_eq!(bytes[0], 0x10);
        // `MQTT` as a length-prefixed string, then protocol level 4.
        assert_eq!(&bytes[2..8], b"\x00\x04MQTT");
        assert_eq!(bytes[8], 4);
        // username + password + no clean session.
        assert_eq!(bytes[9], 0xc0);
        assert_eq!(u16::from_be_bytes([bytes[10], bytes[11]]), 30);
    }

    #[test]
    fn clean_session_is_a_single_bit_and_off_by_default_here() {
        let dirty = encode_connect(&Connect {
            client_id: "a".into(),
            keep_alive_secs: 1,
            clean_session: false,
            username: None,
            password: None,
        });
        let clean = encode_connect(&Connect {
            client_id: "a".into(),
            keep_alive_secs: 1,
            clean_session: true,
            username: None,
            password: None,
        });
        assert_eq!(dirty[9], 0x00);
        assert_eq!(clean[9], 0x02);
    }

    #[test]
    fn a_qos1_publish_round_trips_through_the_decoder() {
        let original = Publish {
            topic: "openadr3/events/vens/ven-1/update".into(),
            qos: QoS::AtLeastOnce,
            retain: false,
            dup: false,
            packet_id: Some(0x1234),
            payload: b"{\"objectType\":\"EVENT\"}".to_vec(),
        };
        let bytes = encode_publish(&original);
        let (packet, used) = decode(&bytes).unwrap().unwrap();
        assert_eq!(used, bytes.len());
        assert_eq!(packet, Some(Packet::Publish(original)));
    }

    #[test]
    fn a_qos0_publish_carries_no_identifier() {
        let original = Publish {
            topic: "t".into(),
            qos: QoS::AtMostOnce,
            retain: true,
            dup: false,
            packet_id: None,
            payload: b"x".to_vec(),
        };
        let bytes = encode_publish(&original);
        assert_eq!(bytes[0] & 0x06, 0); // QoS bits clear
        assert_eq!(bytes[0] & 0x01, 1); // retain set
        let (packet, _) = decode(&bytes).unwrap().unwrap();
        assert_eq!(packet, Some(Packet::Publish(original)));
    }

    #[test]
    fn a_connack_reports_the_session_flag_separately_from_the_code() {
        // The flag is bit 0 of the first byte; the code is the second.
        let (packet, _) = decode(&[0x20, 0x02, 0x01, 0x00]).unwrap().unwrap();
        let Some(Packet::ConnAck(ack)) = packet else {
            panic!("expected CONNACK")
        };
        assert!(ack.session_present);
        assert!(ack.is_accepted());

        let (packet, _) = decode(&[0x20, 0x02, 0x00, 0x05]).unwrap().unwrap();
        let Some(Packet::ConnAck(ack)) = packet else {
            panic!("expected CONNACK")
        };
        assert!(!ack.session_present);
        assert!(!ack.is_accepted());
        assert_eq!(ack.reason(), "not authorized");
    }

    #[test]
    fn a_suback_carries_one_code_per_filter_and_0x80_is_a_refusal() {
        let (packet, _) = decode(&[0x90, 0x04, 0x00, 0x07, 0x01, 0x80])
            .unwrap()
            .unwrap();
        assert_eq!(
            packet,
            Some(Packet::SubAck {
                packet_id: 7,
                codes: vec![0x01, 0x80],
            })
        );
    }

    #[test]
    fn a_subscribe_sets_the_reserved_nibble_brokers_check() {
        // `0x82`, not `0x80`. A broker is required to treat the other value as a protocol
        // violation and close the connection `[MQTT 3.1.1 §3.8.1]` — which looks like a broker
        // that dislikes the topic rather than a client that framed the packet wrongly.
        let bytes = encode_subscribe(9, &[("a/b".to_string(), QoS::AtLeastOnce)]);
        assert_eq!(bytes[0], 0x82);
    }

    #[test]
    fn a_partial_buffer_asks_for_more_rather_than_erroring() {
        let bytes = encode_publish(&Publish {
            topic: "some/topic".into(),
            qos: QoS::AtLeastOnce,
            retain: false,
            dup: false,
            packet_id: Some(1),
            payload: vec![0u8; 64],
        });
        for cut in 0..bytes.len() {
            assert_eq!(decode(&bytes[..cut]).unwrap(), None, "cut at {cut}");
        }
        assert!(decode(&bytes).unwrap().is_some());
    }

    #[test]
    fn two_packets_in_one_read_are_taken_one_at_a_time() {
        let mut buf = encode_puback(1);
        buf.extend(encode_pingreq());
        buf.extend(encode_puback(2));
        let (first, used) = decode(&buf).unwrap().unwrap();
        assert_eq!(first, Some(Packet::PubAck(1)));
        // PINGREQ is not a packet a client receives: consumed, reported as nothing.
        let (second, used2) = decode(&buf[used..]).unwrap().unwrap();
        assert_eq!(second, None);
        let (third, _) = decode(&buf[used + used2..]).unwrap().unwrap();
        assert_eq!(third, Some(Packet::PubAck(2)));
    }

    #[test]
    fn a_remaining_length_uses_every_byte_it_needs() {
        for len in [0usize, 1, 127, 128, 16_383, 16_384, 2_097_151] {
            let mut out = Vec::new();
            put_remaining_length(&mut out, len);
            let (decoded, used) = take_remaining_length(&out).unwrap().unwrap();
            assert_eq!(decoded, len);
            assert_eq!(used, out.len());
        }
    }

    #[test]
    fn a_fifth_continuation_byte_is_malformed_rather_than_a_request_for_more() {
        // Believing it is an unbounded read driven by one bad byte.
        let err = take_remaining_length(&[0xff, 0xff, 0xff, 0xff, 0x7f]).unwrap_err();
        assert!(matches!(err, CodecError::Malformed(_)));
    }

    #[test]
    fn a_declared_length_beyond_the_ceiling_is_refused_before_anything_is_allocated() {
        // 0xFFFFFF7F is ~256 MB. The refusal happens on the header alone, with no body present.
        let err = decode(&[0x30, 0xff, 0xff, 0xff, 0x7f]).unwrap_err();
        assert!(matches!(err, CodecError::TooLarge { .. }));
    }

    #[test]
    fn a_qos2_publish_is_refused_rather_than_misread() {
        // Neither end ever asks for QoS 2, so a broker sending one is doing something unagreed.
        let err = decode(&[0x34, 0x05, 0x00, 0x01, b'a', 0x00, 0x01]).unwrap_err();
        assert!(matches!(err, CodecError::Malformed(_)));
    }

    #[test]
    fn a_topic_that_is_not_utf8_is_named_as_such() {
        let err = decode(&[0x30, 0x04, 0x00, 0x02, 0xff, 0xfe]).unwrap_err();
        assert_eq!(err, CodecError::NotUtf8);
    }

    #[test]
    fn a_truncated_string_field_does_not_index_past_the_body() {
        // Declares a two-byte topic and supplies one.
        let err = decode(&[0x30, 0x03, 0x00, 0x02, b'a']).unwrap_err();
        assert!(matches!(err, CodecError::Malformed(_)));
    }
}
