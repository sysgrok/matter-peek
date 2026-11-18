//! A DTLS 1.2 client on MbedTLS, over any datagram transport.
//!
//! It depends only on `mbedtls-rs-sys` and the timer and select facades, so
//! that it can move into `mbedtls-rs`, which has no DTLS yet. It offers what
//! Thread's commissioner candidate needs: the EC-JPAKE cipher suite, keyed by
//! a password.

use core::ffi::{c_int, c_uchar, c_void};
use core::fmt;
use core::future::Future;
use core::ptr::{addr_of_mut, NonNull};

use alloc::boxed::Box;

use mbedtls_rs_sys::*;
use rand::CryptoRng;

use rs_matter::utils::futures::{select, Either};
use rs_matter::utils::sync::MaybeSend;
use rs_matter::utils::timers::{Duration, Instant, Timer};

/// The largest datagram sent: the IPv6 minimum MTU, less its IPv6 and UDP
/// headers.
pub const MTU: usize = 1232;

/// The largest datagram received.
const MAX_RX: usize = 1500;

/// The cipher suites offered, zero-terminated. MbedTLS keeps the pointer.
static CIPHERSUITES: [c_int; 2] = [MBEDTLS_TLS_ECJPAKE_WITH_AES_128_CCM_8 as c_int, 0];

/// A datagram transport, e.g. a connected UDP socket.
pub trait Datagram {
    /// Send one datagram.
    fn send(&mut self, data: &[u8]) -> impl Future<Output = Result<(), DtlsError>> + MaybeSend;

    /// Receive one datagram into `buf`; its length.
    fn recv(
        &mut self,
        buf: &mut [u8],
    ) -> impl Future<Output = Result<usize, DtlsError>> + MaybeSend;
}

impl<T: Datagram + MaybeSend> Datagram for &mut T {
    fn send(&mut self, data: &[u8]) -> impl Future<Output = Result<(), DtlsError>> + MaybeSend {
        (**self).send(data)
    }

    fn recv(
        &mut self,
        buf: &mut [u8],
    ) -> impl Future<Output = Result<usize, DtlsError>> + MaybeSend {
        (**self).recv(buf)
    }
}

/// What a session is set up with.
#[derive(Debug, Clone)]
pub struct DtlsConfig<'a> {
    /// The EC-JPAKE password: the only key exchange offered.
    pub password: &'a [u8],
    /// How long the handshake waits for an answer before its first
    /// retransmission. It doubles each time, up to `handshake_max`, then gives
    /// up.
    pub handshake_min: Duration,
    pub handshake_max: Duration,
}

impl<'a> DtlsConfig<'a> {
    /// EC-JPAKE with `password`; the handshake gives up after about 30 s.
    pub const fn ecjpake(password: &'a [u8]) -> Self {
        Self {
            password,
            handshake_min: Duration::from_secs(1),
            handshake_max: Duration::from_secs(16),
        }
    }
}

/// Why a session failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DtlsError {
    /// The transport failed.
    Io,
    /// The handshake got no answer in time. With EC-JPAKE, a peer that
    /// was given another password stays silent too.
    Timeout,
    /// The peer ended the handshake with an alert.
    Refused,
    /// The peer closed the session.
    Closed,
    /// Any other MbedTLS error.
    Mbedtls(i32),
}

impl DtlsError {
    fn from_mbedtls(code: c_int) -> Self {
        match code {
            MBEDTLS_ERR_SSL_TIMEOUT => Self::Timeout,
            MBEDTLS_ERR_SSL_FATAL_ALERT_MESSAGE => Self::Refused,
            MBEDTLS_ERR_SSL_PEER_CLOSE_NOTIFY => Self::Closed,
            code => Self::Mbedtls(code),
        }
    }
}

impl fmt::Display for DtlsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io => f.write_str("the network failed"),
            Self::Timeout => f.write_str("no answer in time"),
            Self::Refused => f.write_str("the peer refused the session"),
            Self::Closed => f.write_str("the peer closed the session"),
            Self::Mbedtls(code) => write!(f, "MbedTLS error -0x{:04X}", code.unsigned_abs()),
        }
    }
}

impl core::error::Error for DtlsError {}

/// A DTLS client session over `D`.
pub struct DtlsSession<'a, D> {
    io: D,
    /// On the heap, as MbedTLS keeps pointers into it; freed on drop.
    state: NonNull<State<'a>>,
}

// MbedTLS's contexts hold raw pointers only into `state`, which the session
// owns alone.
unsafe impl<D: Send> Send for DtlsSession<'_, D> {}

impl<'a, D: Datagram> DtlsSession<'a, D> {
    /// A client session over `io`, not connected yet.
    pub fn new(
        io: D,
        config: &DtlsConfig<'_>,
        rng: &'a mut (dyn CryptoRng + Send),
    ) -> Result<Self, DtlsError> {
        Self::with_endpoint(io, config, rng, MBEDTLS_SSL_IS_CLIENT as c_int)
    }

    /// A server session over `io`, for tests: it sends no HelloVerifyRequest.
    #[cfg(test)]
    pub(crate) fn server(
        io: D,
        config: &DtlsConfig<'_>,
        rng: &'a mut (dyn CryptoRng + Send),
    ) -> Result<Self, DtlsError> {
        Self::with_endpoint(io, config, rng, MBEDTLS_SSL_IS_SERVER as c_int)
    }

    fn with_endpoint(
        io: D,
        config: &DtlsConfig<'_>,
        rng: &'a mut (dyn CryptoRng + Send),
        endpoint: c_int,
    ) -> Result<Self, DtlsError> {
        let state = NonNull::from(Box::leak(Box::new(State {
            // SAFETY: plain C structs, for which all zeroes is valid; the
            // `_init` calls below set them up.
            ssl: unsafe { core::mem::zeroed() },
            conf: unsafe { core::mem::zeroed() },
            bio: Bio {
                rx: [0; MAX_RX],
                rx_len: 0,
                tx: [0; MTU],
                tx_len: 0,
            },
            timer: Deadlines::default(),
            rng,
        })));

        let s = state.as_ptr();
        // SAFETY: `s` is valid and stays where it is until the drop frees it.
        // Initialized here, so that the drop may free them whatever fails.
        unsafe {
            mbedtls_ssl_init(addr_of_mut!((*s).ssl));
            mbedtls_ssl_config_init(addr_of_mut!((*s).conf));
        }

        let session = Self { io, state };
        // SAFETY: as above.
        unsafe { session.setup(config, endpoint) }?;

        Ok(session)
    }

    /// Do the handshake.
    pub async fn connect(&mut self) -> Result<(), DtlsError> {
        self.drive(|ssl| unsafe { mbedtls_ssl_handshake(ssl) })
            .await
            .map(|_| ())
    }

    /// Send `data` as one record.
    pub async fn send(&mut self, data: &[u8]) -> Result<(), DtlsError> {
        self.drive(|ssl| unsafe { mbedtls_ssl_write(ssl, data.as_ptr(), data.len()) })
            .await
            .map(|_| ())
    }

    /// Receive one record into `buf`; its length. Waits for as long as it
    /// takes: the caller times it out.
    pub async fn recv(&mut self, buf: &mut [u8]) -> Result<usize, DtlsError> {
        self.drive(|ssl| unsafe { mbedtls_ssl_read(ssl, buf.as_mut_ptr(), buf.len()) })
            .await
    }

    /// Tell the peer that the session ends.
    pub async fn close(&mut self) -> Result<(), DtlsError> {
        self.drive(|ssl| unsafe { mbedtls_ssl_close_notify(ssl) })
            .await
            .map(|_| ())
    }

    /// # Safety
    /// `self.state` is valid and initialized, and nothing else uses it.
    unsafe fn setup(&self, config: &DtlsConfig<'_>, endpoint: c_int) -> Result<(), DtlsError> {
        let s = self.state.as_ptr();
        let conf = addr_of_mut!((*s).conf);
        let ssl = addr_of_mut!((*s).ssl);

        // Once per process; PSA takes its randomness from
        // `mbedtls_psa_external_get_random`.
        check(psa_crypto_init())?;

        check(mbedtls_ssl_config_defaults(
            conf,
            endpoint,
            MBEDTLS_SSL_TRANSPORT_DATAGRAM as c_int,
            MBEDTLS_SSL_PRESET_DEFAULT as c_int,
        ))?;
        (*conf).private_min_tls_version = mbedtls_ssl_protocol_version_MBEDTLS_SSL_VERSION_TLS1_2;
        (*conf).private_max_tls_version = mbedtls_ssl_protocol_version_MBEDTLS_SSL_VERSION_TLS1_2;
        mbedtls_ssl_conf_ciphersuites(conf, CIPHERSUITES.as_ptr());
        mbedtls_ssl_conf_authmode(conf, MBEDTLS_SSL_VERIFY_NONE as c_int);
        mbedtls_ssl_conf_rng(
            conf,
            Some(fill_random),
            addr_of_mut!((*s).rng) as *mut c_void,
        );
        mbedtls_ssl_conf_handshake_timeout(
            conf,
            millis(config.handshake_min),
            millis(config.handshake_max),
        );
        #[cfg(test)]
        if endpoint == MBEDTLS_SSL_IS_SERVER as c_int {
            // No HelloVerifyRequest cookies.
            mbedtls_ssl_conf_dtls_cookies(conf, None, None, core::ptr::null_mut());
        }

        check(mbedtls_ssl_setup(ssl, conf))?;
        mbedtls_ssl_set_bio(
            ssl,
            addr_of_mut!((*s).bio) as *mut c_void,
            Some(bio_send),
            Some(bio_recv),
            None,
        );
        mbedtls_ssl_set_timer_cb(
            ssl,
            addr_of_mut!((*s).timer) as *mut c_void,
            Some(timer_set),
            Some(timer_get),
        );
        mbedtls_ssl_set_mtu(ssl, MTU as u16);
        check(mbedtls_ssl_set_hs_ecjpake_password(
            ssl,
            config.password.as_ptr(),
            config.password.len(),
        ))
    }

    /// Run `op` until it is done, sending what it queued and waiting for the
    /// datagrams it wants, or for its retransmission timer.
    async fn drive(
        &mut self,
        mut op: impl FnMut(*mut mbedtls_ssl_context) -> c_int,
    ) -> Result<usize, DtlsError> {
        loop {
            // SAFETY: valid until the drop; MbedTLS reaches the rest of
            // `state` only from within `op`.
            let ret = op(unsafe { addr_of_mut!((*self.state.as_ptr()).ssl) });

            self.flush().await?;

            match ret {
                MBEDTLS_ERR_SSL_WANT_READ => self.fill().await?,
                MBEDTLS_ERR_SSL_WANT_WRITE => (),
                ret if ret < 0 => return Err(DtlsError::from_mbedtls(ret)),
                ret => return Ok(ret as usize),
            }
        }
    }

    /// Send the datagram MbedTLS queued, if any.
    async fn flush(&mut self) -> Result<(), DtlsError> {
        // SAFETY: MbedTLS does not run meanwhile.
        let bio = unsafe { &mut (*self.state.as_ptr()).bio };

        let len = core::mem::take(&mut bio.tx_len);
        if len > 0 {
            self.io.send(&bio.tx[..len]).await?;
        }

        Ok(())
    }

    /// Wait for a datagram for MbedTLS, or else for its timer to expire.
    async fn fill(&mut self) -> Result<(), DtlsError> {
        // SAFETY: MbedTLS does not run meanwhile.
        let state = unsafe { &mut *self.state.as_ptr() };
        let deadline = state.timer.fin;

        let recv = self.io.recv(&mut state.bio.rx);
        let len = match deadline {
            Some(deadline) => match select(recv, Timer::at(deadline)).await {
                Either::First(len) => len?,
                // MbedTLS finds its timer expired and retransmits.
                Either::Second(()) => return Ok(()),
            },
            None => recv.await?,
        };
        state.bio.rx_len = len;

        Ok(())
    }
}

impl<D> Drop for DtlsSession<'_, D> {
    fn drop(&mut self) {
        let s = self.state.as_ptr();
        // SAFETY: from `Box::leak` in `with_endpoint`, initialized there.
        unsafe {
            mbedtls_ssl_free(addr_of_mut!((*s).ssl));
            mbedtls_ssl_config_free(addr_of_mut!((*s).conf));
            drop(Box::from_raw(s));
        }
    }
}

/// What MbedTLS reaches through the pointers it is given.
struct State<'a> {
    ssl: mbedtls_ssl_context,
    conf: mbedtls_ssl_config,
    bio: Bio,
    timer: Deadlines,
    rng: &'a mut (dyn CryptoRng + Send),
}

/// One datagram each way, between MbedTLS and the transport.
struct Bio {
    rx: [u8; MAX_RX],
    /// 0: none received.
    rx_len: usize,
    tx: [u8; MTU],
    /// 0: none queued.
    tx_len: usize,
}

/// MbedTLS's retransmission timer.
#[derive(Default)]
struct Deadlines {
    int: Option<Instant>,
    /// `None`: stopped.
    fin: Option<Instant>,
}

fn check(code: c_int) -> Result<(), DtlsError> {
    if code < 0 {
        Err(DtlsError::from_mbedtls(code))
    } else {
        Ok(())
    }
}

fn millis(duration: Duration) -> u32 {
    duration.as_millis().try_into().unwrap_or(u32::MAX)
}

unsafe extern "C" fn bio_send(ctx: *mut c_void, buf: *const c_uchar, len: usize) -> c_int {
    let bio = &mut *(ctx as *mut Bio);

    if bio.tx_len > 0 {
        return MBEDTLS_ERR_SSL_WANT_WRITE;
    }
    if len > bio.tx.len() {
        return MBEDTLS_ERR_SSL_BAD_INPUT_DATA;
    }

    bio.tx[..len].copy_from_slice(core::slice::from_raw_parts(buf, len));
    bio.tx_len = len;

    len as c_int
}

unsafe extern "C" fn bio_recv(ctx: *mut c_void, buf: *mut c_uchar, len: usize) -> c_int {
    let bio = &mut *(ctx as *mut Bio);

    if bio.rx_len == 0 {
        return MBEDTLS_ERR_SSL_WANT_READ;
    }

    let len = core::mem::take(&mut bio.rx_len).min(len);
    core::slice::from_raw_parts_mut(buf, len).copy_from_slice(&bio.rx[..len]);

    len as c_int
}

unsafe extern "C" fn timer_set(ctx: *mut c_void, int_ms: u32, fin_ms: u32) {
    let timer = &mut *(ctx as *mut Deadlines);

    if fin_ms == 0 {
        *timer = Deadlines::default();
    } else {
        let now = Instant::now();
        timer.int = Some(now + Duration::from_millis(int_ms as u64));
        timer.fin = Some(now + Duration::from_millis(fin_ms as u64));
    }
}

/// -1 stopped, 0 running, 1 past the intermediate delay, 2 expired.
unsafe extern "C" fn timer_get(ctx: *mut c_void) -> c_int {
    let timer = &*(ctx as *const Deadlines);

    let Some(fin) = timer.fin else {
        return -1;
    };

    let now = Instant::now();
    if now >= fin {
        2
    } else if timer.int.is_some_and(|int| now >= int) {
        1
    } else {
        0
    }
}

unsafe extern "C" fn fill_random(ctx: *mut c_void, out: *mut c_uchar, len: usize) -> c_int {
    let rng = &mut *(ctx as *mut &mut (dyn CryptoRng + Send));
    rng.fill_bytes(core::slice::from_raw_parts_mut(out, len));

    0
}

/// PSA's source of randomness (`MBEDTLS_PSA_CRYPTO_EXTERNAL_RNG`, which
/// `mbedtls-rs-sys` always sets). `mbedtls-rs` defines its own.
#[no_mangle]
unsafe extern "C" fn mbedtls_psa_external_get_random(
    _ctx: *mut c_void,
    out: *mut c_uchar,
    size: usize,
    out_len: *mut usize,
) -> c_int {
    if size > 0 {
        rand::fill(core::slice::from_raw_parts_mut(out, size));
    }
    *out_len = size;

    0
}

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;

    use rand::rngs::StdRng;

    use rs_matter::utils::futures::{block_on, join};

    use super::*;

    /// One end of an in-memory datagram link.
    struct Pipe {
        tx: async_channel::Sender<Vec<u8>>,
        rx: async_channel::Receiver<Vec<u8>>,
        /// Datagrams to drop before sending the rest.
        drop: usize,
    }

    impl Datagram for Pipe {
        async fn send(&mut self, data: &[u8]) -> Result<(), DtlsError> {
            if self.drop > 0 {
                self.drop -= 1;
                return Ok(());
            }
            self.tx.send(data.to_vec()).await.map_err(|_| DtlsError::Io)
        }

        async fn recv(&mut self, buf: &mut [u8]) -> Result<usize, DtlsError> {
            let data = self.rx.recv().await.map_err(|_| DtlsError::Io)?;
            buf[..data.len()].copy_from_slice(&data);
            Ok(data.len())
        }
    }

    fn pipes(client_drops: usize) -> (Pipe, Pipe) {
        let (a_tx, a_rx) = async_channel::unbounded();
        let (b_tx, b_rx) = async_channel::unbounded();
        (
            Pipe {
                tx: a_tx,
                rx: b_rx,
                drop: client_drops,
            },
            Pipe {
                tx: b_tx,
                rx: a_rx,
                drop: 0,
            },
        )
    }

    fn config(password: &[u8]) -> DtlsConfig<'_> {
        DtlsConfig {
            password,
            handshake_min: Duration::from_millis(200),
            handshake_max: Duration::from_millis(800),
        }
    }

    /// Hand-shake, then the client sends `ping`, the server answers `pong`.
    fn exchange(client_password: &[u8], client_drops: usize) -> Result<Vec<u8>, DtlsError> {
        let (client_io, server_io) = pipes(client_drops);
        let mut client_rng = rand::make_rng::<StdRng>();
        let mut server_rng = rand::make_rng::<StdRng>();

        let mut client = DtlsSession::new(client_io, &config(client_password), &mut client_rng)?;
        let mut server = DtlsSession::server(server_io, &config(b"123456789"), &mut server_rng)?;

        let client_side = async {
            client.connect().await?;
            client.send(b"ping").await?;
            let mut buf = [0; 64];
            let len = client.recv(&mut buf).await?;
            client.close().await?;
            Ok::<_, DtlsError>(buf[..len].to_vec())
        };
        let server_side = async {
            server.connect().await?;
            let mut buf = [0; 64];
            let len = server.recv(&mut buf).await?;
            assert_eq!(&buf[..len], b"ping");
            server.send(b"pong").await?;
            Ok::<_, DtlsError>(())
        };

        let (answer, _) = block_on(join(
            select(client_side, Timer::after(Duration::from_secs(3))),
            select(server_side, Timer::after(Duration::from_secs(3))),
        ));

        match answer {
            Either::First(result) => result,
            Either::Second(()) => Err(DtlsError::Timeout),
        }
    }

    #[test]
    fn handshakes_and_exchanges_records() {
        assert_eq!(exchange(b"123456789", 0).unwrap(), b"pong");
    }

    #[test]
    fn retransmits_a_lost_flight() {
        assert_eq!(exchange(b"123456789", 1).unwrap(), b"pong");
    }

    #[test]
    fn fails_with_another_password() {
        assert!(exchange(b"987654321", 0).is_err());
    }
}
