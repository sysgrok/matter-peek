//! Getting a Thread network from a border router that shares it with a
//! one-time code: Thread 1.4's ephemeral PSKc ("ePSKc").
//!
//! We are a MeshCoP commissioner candidate: DTLS to the border agent, with the
//! code as the EC-JPAKE password, then CoAP over it: petition to become the
//! commissioner, read the active operational dataset, and resign.

use core::future::Future;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use std::io;
use std::net::{SocketAddr, UdpSocket};

use async_io::Async;
use log::{debug, info, warn};
use mbedtls_rs::dtls::{Clock, Datagram, DtlsConfig, Session};
use mbedtls_rs::io::{ErrorKind, ErrorType};
use mbedtls_rs::sys::{MBEDTLS_ERR_SSL_FATAL_ALERT_MESSAGE, MBEDTLS_ERR_SSL_TIMEOUT};
use mbedtls_rs::{
    Authentication, ClientSessionConfig, SessionConfig, SessionError, Tls, TlsReference,
};
use rand::rngs::StdRng;
use rand::Rng as _;

use rs_matter::utils::futures::{select, Either};
use rs_matter::utils::timers::{Duration, Instant, Timer};

use crate::api::PeekError;

/// The name we petition under.
const COMMISSIONER_ID: &str = "matter-peek";

/// The DTLS handshake waits this long for an answer before it sends its
/// flight again, doubling the wait up to `HANDSHAKE_TIMEOUT_MAX`; in
/// milliseconds.
const HANDSHAKE_TIMEOUT_MIN: u32 = 2_000;
const HANDSHAKE_TIMEOUT_MAX: u32 = 8_000;

/// The largest datagram we send: IPv6's minimum MTU, without the IPv6 and UDP
/// headers.
const MTU: u16 = 1232;

/// How long a CoAP request waits for its acknowledgement before it is sent
/// again; doubled each time.
const ACK_TIMEOUT: Duration = Duration::from_secs(2);

/// How many times a CoAP request is sent at most.
const MAX_SENDS: u32 = 4;

/// How long a CoAP request waits for its response in all. A petition goes on
/// to the network's leader, over the mesh.
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(20);

/// MeshCoP TLV types.
const TLV_NETWORK_KEY: u8 = 5;
const TLV_COMMISSIONER_ID: u8 = 10;
const TLV_COMMISSIONER_SESSION_ID: u8 = 11;
const TLV_STATE: u8 = 16;

/// The State TLV's values.
const STATE_ACCEPT: u8 = 1;
const STATE_REJECT: u8 = 0xFF;

/// CoAP message types and codes.
const COAP_CON: u8 = 0;
const COAP_NON: u8 = 1;
const COAP_ACK: u8 = 2;
const COAP_RST: u8 = 3;
const COAP_POST: u8 = 0x02;
const COAP_CHANGED: u8 = 0x44;
const COAP_URI_PATH: u8 = 11;

/// A connected UDP socket, for DTLS.
struct Udp(Async<UdpSocket>);

impl ErrorType for Udp {
    type Error = ErrorKind;
}

impl Datagram for Udp {
    async fn readable(&mut self) -> Result<(), ErrorKind> {
        self.0.readable().await.map_err(io_failed)
    }

    async fn recv(&mut self, buf: &mut [u8]) -> Result<usize, ErrorKind> {
        self.0.recv(buf).await.map_err(io_failed)
    }

    async fn send(&mut self, data: &[u8]) -> Result<(), ErrorKind> {
        self.0.send(data).await.map(|_| ()).map_err(io_failed)
    }
}

/// The DTLS session's clock: rs-matter's.
struct MatterClock;

impl Clock for MatterClock {
    fn now(&self) -> u64 {
        Instant::now().as_millis()
    }

    fn wait_until(&mut self, at: u64) -> impl Future<Output = ()> {
        Timer::at(Instant::from_millis(at))
    }
}

/// CoAP requests over a DTLS session.
struct Coap<'a> {
    dtls: Session<'a, Udp, MatterClock>,
    message_id: u16,
    token: u32,
}

impl Coap<'_> {
    /// POST `payload` to `path` (e.g. `c/cp`) and answer the payload of a 2.04
    /// (Changed) response. The response may come with the acknowledgement or
    /// after it, on its own.
    async fn post(&mut self, path: &str, payload: &[u8]) -> Result<Vec<u8>, PeekError> {
        self.message_id = self.message_id.wrapping_add(1);
        self.token = self.token.wrapping_add(1);
        let token = self.token.to_be_bytes();
        let request = coap_post(self.message_id, &token, path, payload);

        let deadline = Instant::now() + RESPONSE_TIMEOUT;
        let mut acked = false;
        let mut sends = 0;
        let mut buf = [0; 1280];

        loop {
            self.dtls.write(&request).await.map_err(dtls_failed)?;
            sends += 1;
            let resend = Instant::now() + ACK_TIMEOUT * (1 << (sends - 1));

            loop {
                let wait = if acked {
                    deadline
                } else {
                    resend.min(deadline)
                };
                let len = match select(self.dtls.read(&mut buf), Timer::at(wait)).await {
                    Either::First(Ok(0)) => {
                        return Err(PeekError::failed("the border router ended the session"))
                    }
                    Either::First(len) => len.map_err(dtls_failed)?,
                    Either::Second(()) if !acked && sends < MAX_SENDS && wait < deadline => break,
                    Either::Second(()) => {
                        return Err(PeekError::failed(format!(
                            "the border router did not answer {path}"
                        )))
                    }
                };

                let Some(message) = CoapMessage::parse(&buf[..len]) else {
                    continue;
                };

                let response = match message.kind {
                    COAP_ACK if message.message_id == self.message_id => {
                        if message.code == 0 {
                            acked = true;
                            continue;
                        }
                        message
                    }
                    COAP_CON | COAP_NON if message.code != 0 && message.token == token => {
                        if message.kind == COAP_CON {
                            self.dtls
                                .write(&coap_ack(message.message_id))
                                .await
                                .map_err(dtls_failed)?;
                        }
                        message
                    }
                    COAP_RST if message.message_id == self.message_id => {
                        return Err(PeekError::failed(format!(
                            "the border router does not take {path}"
                        )))
                    }
                    _ => continue,
                };

                return if response.code == COAP_CHANGED {
                    Ok(response.payload.to_vec())
                } else {
                    Err(PeekError::failed(format!(
                        "the border router answered {path} with {}.{:02}",
                        response.code >> 5,
                        response.code & 0x1F
                    )))
                };
            }
        }
    }
}

/// A CoAP message, as received.
struct CoapMessage<'a> {
    kind: u8,
    code: u8,
    message_id: u16,
    token: &'a [u8],
    payload: &'a [u8],
}

impl<'a> CoapMessage<'a> {
    /// `data` as a CoAP message, its options skipped; `None` if it is not one.
    fn parse(data: &'a [u8]) -> Option<Self> {
        let (&first, rest) = data.split_first()?;
        if first >> 6 != 1 {
            return None;
        }
        let token_len = (first & 0x0F) as usize;
        if token_len > 8 || rest.len() < 3 + token_len {
            return None;
        }

        let code = rest[0];
        let message_id = u16::from_be_bytes([rest[1], rest[2]]);
        let token = &rest[3..3 + token_len];
        let mut rest = &rest[3 + token_len..];

        while let Some((&byte, after)) = rest.split_first() {
            if byte == 0xFF {
                rest = after;
                break;
            }
            let (_, after) = option_nibble(byte >> 4, after)?;
            let (len, after) = option_nibble(byte & 0x0F, after)?;
            rest = after.get(len..)?;
        }

        Some(Self {
            kind: (first >> 4) & 0x03,
            code,
            message_id,
            token,
            payload: rest,
        })
    }
}

/// Fetch the active operational dataset of the Thread network of the border
/// agent at `agent`, with the one-time `code` it shows; the dataset's TLVs,
/// with the network key.
///
/// On a thread of its own: MbedTLS's sessions are not `Send`, and the
/// service's futures may have to be.
pub async fn fetch_dataset(agent: SocketAddr, code: &str) -> Result<Vec<u8>, PeekError> {
    let code = String::from(code);
    let (sender, receiver) = async_channel::bounded(1);

    std::thread::Builder::new()
        .name("meshcop".into())
        .spawn(move || {
            let mut rng = rand::make_rng::<StdRng>();

            // SAFETY: `tls` is dropped at the end of its arm, before `rng`, and
            // is not leaked.
            let dataset = match unsafe { Tls::new_local_borrows(&mut rng) } {
                Ok(tls) => async_io::block_on(fetch(tls.reference(), agent, &code)),
                Err(_) => Err(PeekError::failed("another sharing code is in use")),
            };

            let _ = sender.send_blocking(dataset);
        })
        .map_err(|e| PeekError::failed(format!("cannot start the fetch: {e}")))?;

    receiver
        .recv()
        .await
        .map_err(|_| PeekError::failed("the fetch stopped"))?
}

/// [`fetch_dataset`], with the active `Tls` instance.
async fn fetch(tls: TlsReference<'_>, agent: SocketAddr, code: &str) -> Result<Vec<u8>, PeekError> {
    let local: SocketAddr = match agent {
        SocketAddr::V4(_) => (std::net::Ipv4Addr::UNSPECIFIED, 0).into(),
        SocketAddr::V6(_) => (std::net::Ipv6Addr::UNSPECIFIED, 0).into(),
    };
    let socket = Async::<UdpSocket>::bind(local)
        .and_then(|socket| {
            socket.get_ref().connect(agent)?;
            Ok(socket)
        })
        .map_err(|e| PeekError::failed(format!("cannot reach {agent}: {e}")))?;

    let mut rng = rand::make_rng::<StdRng>();
    let message_id = rng.next_u32() as u16;
    let token = rng.next_u32();

    let config = SessionConfig::Client(ClientSessionConfig {
        authentication: Some(Authentication::EcJpake(code.as_bytes())),
        ..ClientSessionConfig::new()
    });
    let dtls_config = DtlsConfig {
        handshake_timeout_min: HANDSHAKE_TIMEOUT_MIN,
        handshake_timeout_max: HANDSHAKE_TIMEOUT_MAX,
        mtu: Some(MTU),
        ..DtlsConfig::new()
    };
    let mut dtls =
        Session::new(tls, Udp(socket), MatterClock, &config, &dtls_config).map_err(dtls_failed)?;

    info!("meshcop: connecting to the border agent at {agent}");
    dtls.connect().await.map_err(|e| match e {
        // With EC-JPAKE, a peer given another password stays silent.
        SessionError::MbedTls(e) if e.code() == MBEDTLS_ERR_SSL_TIMEOUT => PeekError::failed(
            "the border router did not answer: the code may be mistyped, expired or used already",
        ),
        SessionError::MbedTls(e) if e.code() == MBEDTLS_ERR_SSL_FATAL_ALERT_MESSAGE => {
            PeekError::failed("the border router refused the code")
        }
        SessionError::Io(ErrorKind::ConnectionRefused) => {
            PeekError::failed("the border router is not listening: its code may have expired")
        }
        e => dtls_failed(e),
    })?;

    let mut coap = Coap {
        dtls,
        message_id,
        token,
    };

    let mut petition = Vec::new();
    put_tlv(
        &mut petition,
        TLV_COMMISSIONER_ID,
        COMMISSIONER_ID.as_bytes(),
    );
    // A border agent that refuses the petition (another commissioner is
    // active) may still answer the read.
    let session_id = match coap.post("c/cp", &petition).await {
        Ok(answer) if tlv(&answer, TLV_STATE) == Some(&[STATE_ACCEPT]) => {
            tlv(&answer, TLV_COMMISSIONER_SESSION_ID).map(<[u8]>::to_vec)
        }
        Ok(_) => {
            warn!("meshcop: the petition was rejected");
            None
        }
        Err(e) => {
            warn!("meshcop: the petition failed: {e}");
            None
        }
    };

    let dataset = coap.post("c/ag", &[]).await;

    if let Some(session_id) = session_id {
        let mut resign = Vec::new();
        put_tlv(&mut resign, TLV_STATE, &[STATE_REJECT]);
        put_tlv(&mut resign, TLV_COMMISSIONER_SESSION_ID, &session_id);
        if let Err(e) = coap.post("c/ca", &resign).await {
            debug!("meshcop: resigning failed: {e}");
        }
    }
    let _ = coap.dtls.close().await;

    let dataset = dataset?;
    if tlv(&dataset, TLV_NETWORK_KEY).is_none() {
        return Err(PeekError::failed(
            "the border router gave its network without the network key",
        ));
    }

    Ok(dataset)
}

/// `code` as a border router shows it, without its spaces and dashes; an
/// error if it cannot be one. A code of 9 digits ends in a Verhoeff check
/// digit, as OpenThread makes them: a mistyped one is refused before it
/// costs one of the border router's few attempts.
pub fn sharing_code(code: &str) -> Result<String, PeekError> {
    let code: String = code
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '-')
        .collect();

    if !(6..=32).contains(&code.len()) || !code.chars().all(|c| c.is_ascii_graphic()) {
        return Err(PeekError::failed(
            "a sharing code has 6 to 32 characters, usually 9 digits",
        ));
    }
    if code.len() == 9 && code.chars().all(|c| c.is_ascii_digit()) && !verhoeff_valid(&code) {
        return Err(PeekError::failed(
            "the sharing code is mistyped: its last digit does not match",
        ));
    }

    Ok(code)
}

fn dtls_failed(e: SessionError) -> PeekError {
    PeekError::failed(format!("DTLS with the border router failed: {e}"))
}

fn io_failed(e: io::Error) -> ErrorKind {
    debug!("meshcop: the socket failed: {e}");

    match e.kind() {
        io::ErrorKind::ConnectionRefused => ErrorKind::ConnectionRefused,
        _ => ErrorKind::Other,
    }
}

/// A CoAP confirmable POST.
fn coap_post(message_id: u16, token: &[u8], path: &str, payload: &[u8]) -> Vec<u8> {
    let mut message = Vec::with_capacity(16 + path.len() + payload.len());
    message.push(0x40 | token.len() as u8);
    message.push(COAP_POST);
    message.extend_from_slice(&message_id.to_be_bytes());
    message.extend_from_slice(token);

    let mut delta = COAP_URI_PATH;
    for segment in path.split('/') {
        // MeshCoP's segments are short: their length fits the nibble.
        message.push((delta << 4) | segment.len() as u8);
        message.extend_from_slice(segment.as_bytes());
        delta = 0;
    }

    if !payload.is_empty() {
        message.push(0xFF);
        message.extend_from_slice(payload);
    }

    message
}

/// An empty acknowledgement of message `message_id`.
fn coap_ack(message_id: u16) -> [u8; 4] {
    let [high, low] = message_id.to_be_bytes();
    [0x60, 0x00, high, low]
}

/// An option's delta or length nibble, with its extended bytes in `rest`.
fn option_nibble(nibble: u8, rest: &[u8]) -> Option<(usize, &[u8])> {
    match nibble {
        13 => {
            let (&byte, rest) = rest.split_first()?;
            Some((byte as usize + 13, rest))
        }
        14 => {
            let bytes = rest.get(..2)?;
            Some((
                u16::from_be_bytes([bytes[0], bytes[1]]) as usize + 269,
                &rest[2..],
            ))
        }
        15 => None,
        nibble => Some((nibble as usize, rest)),
    }
}

fn put_tlv(tlvs: &mut Vec<u8>, tlv_type: u8, value: &[u8]) {
    tlvs.push(tlv_type);
    tlvs.push(value.len() as u8);
    tlvs.extend_from_slice(value);
}

/// The value of the first MeshCoP TLV of `tlv_type` in `tlvs`.
fn tlv(mut tlvs: &[u8], tlv_type: u8) -> Option<&[u8]> {
    while tlvs.len() >= 2 {
        let (len, header) = if tlvs[1] == 0xFF {
            let bytes = tlvs.get(2..4)?;
            (u16::from_be_bytes([bytes[0], bytes[1]]) as usize, 4)
        } else {
            (tlvs[1] as usize, 2)
        };
        let value = tlvs.get(header..header + len)?;
        if tlvs[0] == tlv_type {
            return Some(value);
        }
        tlvs = &tlvs[header + len..];
    }

    None
}

/// Whether the last digit of `digits` is their Verhoeff check digit.
fn verhoeff_valid(digits: &str) -> bool {
    const D: [[u8; 10]; 10] = [
        [0, 1, 2, 3, 4, 5, 6, 7, 8, 9],
        [1, 2, 3, 4, 0, 6, 7, 8, 9, 5],
        [2, 3, 4, 0, 1, 7, 8, 9, 5, 6],
        [3, 4, 0, 1, 2, 8, 9, 5, 6, 7],
        [4, 0, 1, 2, 3, 9, 5, 6, 7, 8],
        [5, 9, 8, 7, 6, 0, 4, 3, 2, 1],
        [6, 5, 9, 8, 7, 1, 0, 4, 3, 2],
        [7, 6, 5, 9, 8, 2, 1, 0, 4, 3],
        [8, 7, 6, 5, 9, 3, 2, 1, 0, 4],
        [9, 8, 7, 6, 5, 4, 3, 2, 1, 0],
    ];
    const P: [[u8; 10]; 8] = [
        [0, 1, 2, 3, 4, 5, 6, 7, 8, 9],
        [1, 5, 7, 6, 2, 8, 3, 0, 9, 4],
        [5, 8, 0, 3, 7, 9, 6, 1, 4, 2],
        [8, 9, 1, 6, 0, 4, 3, 5, 2, 7],
        [9, 4, 5, 3, 1, 2, 6, 8, 7, 0],
        [4, 2, 8, 6, 5, 7, 3, 9, 0, 1],
        [2, 7, 9, 3, 8, 0, 6, 4, 1, 5],
        [7, 0, 4, 6, 9, 1, 3, 2, 5, 8],
    ];

    digits
        .bytes()
        .rev()
        .enumerate()
        .fold(0, |check, (i, digit)| {
            D[check as usize][P[i % 8][(digit - b'0') as usize] as usize]
        })
        == 0
}

#[cfg(test)]
mod tests {
    use mbedtls_rs::dtls::{CookieCheck, Cookies};
    use mbedtls_rs::ServerSessionConfig;
    use rs_matter::utils::futures::join;

    use super::*;

    /// mbedtls-rs has one `Tls` instance at a time.
    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// The border agent's end: answers whoever sent the last datagram.
    struct AgentUdp {
        socket: Async<UdpSocket>,
        peer: Option<SocketAddr>,
    }

    impl ErrorType for AgentUdp {
        type Error = ErrorKind;
    }

    impl Datagram for AgentUdp {
        async fn readable(&mut self) -> Result<(), ErrorKind> {
            self.socket.readable().await.map_err(io_failed)
        }

        async fn recv(&mut self, buf: &mut [u8]) -> Result<usize, ErrorKind> {
            let (len, peer) = self.socket.recv_from(buf).await.map_err(io_failed)?;
            self.peer = Some(peer);
            Ok(len)
        }

        async fn send(&mut self, data: &[u8]) -> Result<(), ErrorKind> {
            let peer = self.peer.ok_or(ErrorKind::NotConnected)?;
            self.socket
                .send_to(data, peer)
                .await
                .map(|_| ())
                .map_err(io_failed)
        }
    }

    const DATASET: &[u8] = &[
        2, 8, 0x1A, 0x2B, 0x3C, 0x4D, 0x5E, 0x6F, 0x70, 0x81, // extended PAN ID
        3, 4, b'T', b'e', b's', b't', // network name
        5, 16, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, // network key
    ];

    fn coap_response(kind: u8, message_id: u16, token: &[u8], payload: &[u8]) -> Vec<u8> {
        let mut message = alloc::vec![(1 << 6) | (kind << 4) | token.len() as u8, COAP_CHANGED];
        message.extend_from_slice(&message_id.to_be_bytes());
        message.extend_from_slice(token);
        message.push(0xFF);
        message.extend_from_slice(payload);
        message
    }

    /// A border agent as OpenThread's answers: the petition with an empty
    /// acknowledgement, then a separate response; the read at once. Answers
    /// the requests it got.
    async fn agent(tls: TlsReference<'_>, socket: Async<UdpSocket>, code: &[u8]) -> Vec<Vec<u8>> {
        // As OpenThread's, it checks the client's address with a cookie.
        let (_, client) = socket.peek_from(&mut [0]).await.unwrap();
        let client = format!("{client}");
        let cookies = Cookies::new(tls).unwrap();

        let config = SessionConfig::Server(ServerSessionConfig::new(Authentication::EcJpake(code)));
        let dtls_config = DtlsConfig {
            cookie_check: Some(CookieCheck::Verify {
                cookies: &cookies,
                client: client.as_bytes(),
            }),
            ..DtlsConfig::new()
        };
        let mut dtls = Session::new(
            tls,
            AgentUdp { socket, peer: None },
            MatterClock,
            &config,
            &dtls_config,
        )
        .unwrap();
        dtls.connect().await.unwrap();

        let mut requests = Vec::new();
        let mut buf = [0; 1280];
        // Until the close notify.
        while let Ok(len @ 1..) = dtls.read(&mut buf).await {
            let request = CoapMessage::parse(&buf[..len]).unwrap();
            let (message_id, token) = (request.message_id, request.token.to_vec());
            requests.push(request.payload.to_vec());

            if tlv(request.payload, TLV_COMMISSIONER_ID).is_some() {
                dtls.write(&coap_ack(message_id)).await.unwrap();
                let mut answer = Vec::new();
                put_tlv(&mut answer, TLV_STATE, &[STATE_ACCEPT]);
                put_tlv(&mut answer, TLV_COMMISSIONER_SESSION_ID, &[0, 1]);
                let response = coap_response(COAP_NON, 0x7000, &token, &answer);
                dtls.write(&response).await.unwrap();
            } else if request.payload.is_empty() {
                let response = coap_response(COAP_ACK, message_id, &token, DATASET);
                dtls.write(&response).await.unwrap();
            } else {
                let response = coap_response(COAP_ACK, message_id, &token, &[TLV_STATE, 1, 1]);
                dtls.write(&response).await.unwrap();
            }
        }

        requests
    }

    #[test]
    fn fetches_the_dataset_from_a_border_agent() {
        let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

        let socket = Async::<UdpSocket>::bind(([127, 0, 0, 1], 0)).unwrap();
        let addr = socket.get_ref().local_addr().unwrap();

        let mut rng = rand::make_rng::<StdRng>();
        // SAFETY: `tls` is dropped at the end of the test, before `rng`, and is
        // not leaked.
        let tls = unsafe { Tls::new_local_borrows(&mut rng) }.unwrap();

        let (dataset, requests) = async_io::block_on(join(
            fetch(tls.reference(), addr, "123456789"),
            agent(tls.reference(), socket, b"123456789"),
        ));

        assert_eq!(dataset.unwrap(), DATASET);
        // Petition, read, resign.
        assert_eq!(requests.len(), 3);
        assert_eq!(
            tlv(&requests[0], TLV_COMMISSIONER_ID),
            Some(COMMISSIONER_ID.as_bytes())
        );
        assert!(requests[1].is_empty());
        assert_eq!(tlv(&requests[2], TLV_STATE), Some(&[STATE_REJECT][..]));
        assert_eq!(
            tlv(&requests[2], TLV_COMMISSIONER_SESSION_ID),
            Some(&[0, 1][..])
        );
    }

    #[test]
    fn says_when_no_border_agent_listens() {
        let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

        // A port that was free a moment ago.
        let addr = UdpSocket::bind(("127.0.0.1", 0))
            .unwrap()
            .local_addr()
            .unwrap();

        let error = async_io::block_on(fetch_dataset(addr, "123456789")).unwrap_err();
        assert!(format!("{error}").contains("not listening"), "{error}");
    }

    #[test]
    fn verhoeff() {
        // The classic example: 236 has check digit 3.
        assert!(verhoeff_valid("2363"));
        assert!(!verhoeff_valid("2364"));
        // A transposition is caught.
        assert!(!verhoeff_valid("3263"));
    }

    #[test]
    fn sharing_codes() {
        // Too short.
        assert!(sharing_code("236 3").is_err());

        let code = (0..10)
            .map(|digit| format!("12345678{digit}"))
            .find(|code| verhoeff_valid(code))
            .unwrap();
        let typed = format!("{}-{} {}", &code[..3], &code[3..6], &code[6..]);
        assert_eq!(sharing_code(&typed).unwrap(), code);

        let wrong = format!("12345678{}", (code.as_bytes()[8] - b'0' + 1) % 10);
        assert!(sharing_code(&wrong).is_err());
        assert!(sharing_code("abc").is_err());
    }

    #[test]
    fn coap_round_trip() {
        let message = coap_post(0x1234, &[1, 2], "c/cp", &[TLV_COMMISSIONER_ID, 1, b'x']);
        let parsed = CoapMessage::parse(&message).unwrap();
        assert_eq!(parsed.kind, COAP_CON);
        assert_eq!(parsed.code, COAP_POST);
        assert_eq!(parsed.message_id, 0x1234);
        assert_eq!(parsed.token, &[1, 2]);
        assert_eq!(parsed.payload, &[TLV_COMMISSIONER_ID, 1, b'x']);
        // Uri-Path "c", then "cp".
        assert_eq!(&message[6..11], &[0xB1, b'c', 0x02, b'c', b'p']);

        let ack = coap_ack(0x1234);
        let parsed = CoapMessage::parse(&ack).unwrap();
        assert_eq!(
            (parsed.kind, parsed.code, parsed.message_id),
            (COAP_ACK, 0, 0x1234)
        );
    }

    #[test]
    fn tlvs() {
        let mut tlvs = Vec::new();
        put_tlv(&mut tlvs, TLV_STATE, &[STATE_ACCEPT]);
        put_tlv(&mut tlvs, TLV_COMMISSIONER_SESSION_ID, &[0x12, 0x34]);
        assert_eq!(tlv(&tlvs, TLV_STATE), Some(&[STATE_ACCEPT][..]));
        assert_eq!(
            tlv(&tlvs, TLV_COMMISSIONER_SESSION_ID),
            Some(&[0x12, 0x34][..])
        );
        assert_eq!(tlv(&tlvs, TLV_NETWORK_KEY), None);
        // An extended TLV: 0xFF, then two length bytes.
        let extended = [TLV_NETWORK_KEY, 0xFF, 0x00, 0x02, 0xAA, 0xBB];
        assert_eq!(tlv(&extended, TLV_NETWORK_KEY), Some(&[0xAA, 0xBB][..]));
    }
}
