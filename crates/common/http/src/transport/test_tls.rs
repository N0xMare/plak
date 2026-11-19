use std::error::Error;
use std::sync::Arc;

use rcgen::{CertifiedKey, generate_simple_self_signed};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::{ClientConfig, RootCertStore, ServerConfig};
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;

use crate::Origin;

pub(crate) struct Endpoint {
    pub(crate) listener: TcpListener,
    pub(crate) acceptor: TlsAcceptor,
    pub(crate) client: ClientConfig,
    pub(crate) origin: Origin,
}

pub(crate) async fn endpoint_with_versions(
    alpn: &[u8],
    versions: &[&'static rustls::SupportedProtocolVersion],
) -> Result<Endpoint, Box<dyn Error + Send + Sync>> {
    let CertifiedKey { cert, signing_key } =
        generate_simple_self_signed(vec!["127.0.0.1".to_owned()])?;
    let certificate = cert.der().clone();
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(signing_key.serialize_der()));
    let mut server = ServerConfig::builder_with_provider(Arc::clone(&provider))
        .with_protocol_versions(versions)?
        .with_no_client_auth()
        .with_single_cert(vec![certificate.clone()], key)?;
    server.alpn_protocols = vec![alpn.to_vec()];
    server.send_tls13_tickets = 0;
    let mut roots = RootCertStore::empty();
    roots.add(CertificateDer::from(certificate.to_vec()))?;
    let client = ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(versions)?
        .with_root_certificates(roots)
        .with_no_client_auth();
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let origin = Origin::parse(&format!("https://{}", listener.local_addr()?))?;
    Ok(Endpoint {
        listener,
        acceptor: TlsAcceptor::from(Arc::new(server)),
        client,
        origin,
    })
}
