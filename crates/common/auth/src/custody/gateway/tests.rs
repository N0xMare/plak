use super::*;
use plakous_http::relay::{GatewayEndpoint, InMemoryGateway};
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::io::{
    AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _, BufReader, DuplexStream,
};

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

struct Peer {
    facts: AtomicUsize,
    refreshes: AtomicUsize,
}

impl Peer {
    fn custodian(self: Arc<Self>) -> TestResult<GatewayCustodian> {
        let endpoint = GatewayEndpoint::InMemory(InMemoryGateway::new(move |io| {
            let peer = Arc::clone(&self);
            tokio::spawn(async move { peer.serve(io).await });
        }));
        Ok(GatewayCustodian::new(
            Arc::new(RelayClient::new(endpoint, None)?),
            |_| None,
        ))
    }

    async fn serve(&self, mut io: DuplexStream) -> TestResult {
        let mut reader = BufReader::new(&mut io);
        let mut request = String::new();
        reader.read_line(&mut request).await?;
        let path = request
            .split_whitespace()
            .nth(1)
            .ok_or("missing request path")?;
        let mut body_len = 0;
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).await? == 0 {
                return Err("incomplete request head".into());
            }
            if line == "\r\n" {
                break;
            }
            if let Some((name, length)) = line.split_once(':')
                && name.eq_ignore_ascii_case("content-length")
            {
                body_len = length.trim().parse()?;
            }
        }
        let mut body = vec![0; body_len];
        reader.read_exact(&mut body).await?;
        if path.ends_with("/refresh") {
            let _: protocol::facts::RefreshRequest = protocol::call::decode(&body)?;
            self.refreshes.fetch_add(1, Ordering::Relaxed);
            // The entire refresh was read; close before returning any answer.
            return Ok(());
        }
        let instance = protocol::UuidText::new("00000000-0000-0000-0000-000000000001")?;
        let (status, headers, reply) = if path == protocol::VERSIONS_PATH {
            (
                "200 OK",
                "",
                protocol::call::encode(&protocol::VersionsReply {
                    versions: protocol::SUPPORTED_VERSIONS.to_vec(),
                    instance,
                })?,
            )
        } else {
            assert!(path.ends_with("/facts"));
            let _: protocol::facts::FactsRequest = protocol::call::decode(&body)?;
            if self.facts.fetch_add(1, Ordering::Relaxed) == 0 {
                (
                    "503 Service Unavailable",
                    "plak-gateway-error: busy\r\n",
                    protocol::call::encode(&protocol::refusal::RefusalBody {
                        gateway_error: GatewayRefusal::Busy {
                            retry_after_ms: 250,
                        },
                    })?,
                )
            } else {
                let facts = AccountFacts::signed_out(CustodySlot::new("fixture")?);
                (
                    "200 OK",
                    "",
                    protocol::call::encode(&protocol::facts::FactsReply {
                        facts: vec![wire_facts(&facts, &instance, 0)?],
                    })?,
                )
            }
        };
        io.write_all(
            format!(
                "HTTP/1.1 {status}\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n",
                reply.len()
            )
            .as_bytes(),
        )
        .await?;
        io.write_all(&reply).await?;
        io.shutdown().await?;
        Ok(())
    }
}

#[tokio::test(start_paused = true)]
async fn busy_facts_waits_but_eof_after_refresh_write_is_not_retried() -> TestResult {
    let peer = Arc::new(Peer {
        facts: AtomicUsize::new(0),
        refreshes: AtomicUsize::new(0),
    });
    let custodian = Arc::clone(&peer).custodian()?;
    let slot = CustodySlot::new("fixture")?;
    let expected = ExpectedFence {
        account_binding: IdentityDigest::from_bytes([1; 32]),
        generation: CredentialGeneration::new(1)?,
    };
    let (observed, replies) = tokio::sync::oneshot::channel();
    let operation = Box::pin(async move {
        let facts = custodian.facts(&slot).await;
        let refresh = custodian
            .refresh(&slot, RefreshRequest::IfDue { expected })
            .await;
        let _ = observed.send((facts, refresh));
        Err(RuntimeModelAuthorityError::Unavailable)
    });
    let mut check = authority_acquisition::Check::new(operation, Duration::from_secs(60));
    let authority_acquisition::Step::Continue(backoff) = check.step().await else {
        return Err("Busy did not park the facts call".into());
    };
    assert_eq!(backoff.delay, Duration::from_millis(250));
    assert_eq!(peer.facts.load(Ordering::Relaxed), 1);
    tokio::time::sleep(backoff.delay).await;
    assert!(matches!(
        check.step().await,
        authority_acquisition::Step::Break(..)
    ));
    let (facts, refresh) = replies.await?;
    assert_eq!(facts?.status, CustodyStatus::SignedOut);
    assert!(matches!(refresh, Err(CustodyError::Unavailable)));
    assert_eq!(peer.facts.load(Ordering::Relaxed), 2);
    assert_eq!(peer.refreshes.load(Ordering::Relaxed), 1);
    Ok(())
}
