//! Credential-free loopback endpoint and framing for native OAuth fixtures.

use std::io;
use std::sync::Arc;

use plakous_http::{Client, Limits, TlsConfigIdentity};
use rustls::{ClientConfig, RootCertStore, ServerConfig};
use tokio::io::AsyncReadExt as _;
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::TlsAcceptor;

/// Subscription fixture clock anchored just after the compiled review, advancing
/// monotonically so access-token refresh deadlines remain meaningful offline.
pub struct ReviewClock<const REVIEWED: u64>;

impl<const REVIEWED: u64> plakous_model::Clock for ReviewClock<REVIEWED> {
    #[allow(
        clippy::expect_used,
        reason = "A fixture review epoch plus elapsed test time must fit a Timestamp."
    )]
    fn now(&self) -> plakous_model::Timestamp {
        static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
        plakous_model::Timestamp::new(
            REVIEWED
                + 60
                + START
                    .get_or_init(std::time::Instant::now)
                    .elapsed()
                    .as_secs(),
            0,
        )
        .expect("review time")
    }
}

/// Fixture failures retain their underlying transport/configuration source.
pub type TestError = Box<dyn std::error::Error + Send + Sync>;
/// Result of an isolated fixture operation.
pub type TestResult<T = ()> = Result<T, TestError>;

/// Loopback-only TLS endpoint; the caller owns its serving task.
pub struct TestEndpoint {
    /// Bound IPv4 loopback listener.
    pub listener: TcpListener,
    /// TLS acceptor for the generated certificate.
    pub acceptor: TlsAcceptor,
    /// Client trusting only this fixture's certificate.
    pub client: Client,
    /// Exact bound loopback origin.
    pub origin: String,
}

/// Creates a loopback TLS endpoint with a fresh certificate and transport identity.
///
/// # Errors
///
/// Returns certificate, TLS configuration, bind, or transport configuration errors.
pub async fn test_endpoint() -> TestResult<TestEndpoint> {
    let certified =
        rcgen::generate_simple_self_signed(vec!["localhost".to_owned(), "127.0.0.1".to_owned()])?;
    let certificate = certified.cert.der().clone();
    let private_key =
        rustls::pki_types::PrivatePkcs8KeyDer::from(certified.signing_key.serialize_der());
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut server = ServerConfig::builder_with_provider(Arc::clone(&provider))
        .with_safe_default_protocol_versions()?
        .with_no_client_auth()
        .with_single_cert(vec![certificate.clone()], private_key.into())?;
    server.alpn_protocols = vec![b"http/1.1".to_vec()];
    let identity =
        TlsConfigIdentity::from_parts(1, &[certificate.as_ref(), b"native-auth-fixture"])?;
    let mut roots = RootCertStore::empty();
    roots.add(certificate)?;
    let client_config = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()?
        .with_root_certificates(roots)
        .with_no_client_auth();
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let origin = format!("https://{}/", listener.local_addr()?);
    let client = Client::with_tls_config_and_identity(Limits::default(), client_config, identity)?;
    Ok(TestEndpoint {
        listener,
        acceptor: TlsAcceptor::from(Arc::new(server)),
        client,
        origin,
    })
}

/// Frames one controlled HTTP/1.1 response.
#[must_use]
pub fn response(status: &str, content_type: &str, body: &[u8], extra: &str) -> Vec<u8> {
    let mut response = format!(
            "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n{extra}\r\n",
            body.len()
        )
        .into_bytes();
    response.extend_from_slice(body);
    response
}

/// Reads one complete request, refusing truncated headers or bodies.
///
/// # Errors
///
/// Returns I/O, UTF-8, or premature-EOF errors from the controlled peer.
pub async fn read_request(
    stream: &mut tokio_rustls::server::TlsStream<TcpStream>,
) -> TestResult<Vec<u8>> {
    let mut request = Vec::new();
    let mut buffer = [0_u8; 1024];
    let head_end = loop {
        let read = stream.read(&mut buffer).await?;
        if read == 0 {
            return Err(io::Error::other("request ended before head").into());
        }
        request.extend_from_slice(&buffer[..read]);
        if let Some(index) = request.windows(4).position(|window| window == b"\r\n\r\n") {
            break index + 4;
        }
    };
    let head = std::str::from_utf8(&request[..head_end])?;
    let length = head
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())
                .flatten()
        })
        .unwrap_or(0);
    while request.len() < head_end + length {
        let read = stream.read(&mut buffer).await?;
        if read == 0 {
            return Err(io::Error::other("request ended before body").into());
        }
        request.extend_from_slice(&buffer[..read]);
    }
    Ok(request)
}
