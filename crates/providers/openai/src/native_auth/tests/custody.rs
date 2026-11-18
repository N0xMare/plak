//! Local custody of the stored Codex login: facts equal to the route
//! binding, fields under the Ready lease, and the single writer's refresh.

use plakous_auth::custody::{CustodyError, CustodySlot, CustodyStatus, ExpectedFence};

use super::*;

fn slot() -> TestResult<CustodySlot> {
    Ok(CustodySlot::new("codex")?)
}

#[tokio::test]
async fn custody_facts_carry_the_exact_route_binding() -> TestResult {
    let account = "custody-facts-account";
    let subject = "custody-facts-subject";
    let (fixture, _peers, server) =
        renew_fixture(account, subject, now_unix_secs() + 3_600, 0, Vec::new()).await?;
    let facts = fixture.manager.custody_facts(&slot()?).await?;
    assert_eq!(facts.status, CustodyStatus::Ready);
    assert_eq!(
        facts.account_binding.map(|digest| *digest.as_bytes()),
        Some(*fixture.evidence.account_fingerprint().bytes())
    );
    assert_eq!(
        facts.credential_authority.map(|digest| *digest.as_bytes()),
        Some(*fixture.evidence.credential_authority_binding().bytes())
    );
    assert_eq!(
        facts
            .selection_epoch_digest
            .map(|digest| *digest.as_bytes()),
        Some(*fixture.evidence.credential_selection_epoch().bytes())
    );
    assert_eq!(
        facts.generation.map(CredentialGeneration::get),
        Some(fixture.evidence.selected_credential_generation())
    );
    assert_eq!(
        facts.access_expires_at_unix_ms,
        Some((now_unix_secs() + 3_600) * 1000)
    );
    let (requests, unexpected) = server.await??;
    assert_eq!(request_kinds(&requests), ["catalog"]);
    assert!(!unexpected, "reading custody facts reached the wire");
    Ok(())
}

#[tokio::test]
async fn custody_fields_come_from_the_exact_fence_under_its_lease() -> TestResult {
    let account = "custody-fields-account";
    let (fixture, _peers, _server) = renew_fixture(
        account,
        "custody-fields-subject",
        now_unix_secs() + 3_600,
        0,
        Vec::new(),
    )
    .await?;
    let slot = slot()?;
    let facts = fixture.manager.custody_facts(&slot).await?;
    let expected = facts.expected_fence().ok_or("no fence")?;
    let mut fields = fixture.manager.custody_fields(&slot, &expected).await?;
    assert_eq!(
        fields.names().collect::<Vec<_>>(),
        ["authorization", "chatgpt-account-id"]
    );
    assert!(fields.take_hold().is_some(), "the Ready lease is held");
    assert_eq!(
        fields
            .take("authorization")
            .ok_or("authorization")?
            .into_header_bytes(),
        b"Bearer opaque-seeded-access"
    );
    assert_eq!(
        fields
            .take("chatgpt-account-id")
            .ok_or("account")?
            .into_header_bytes(),
        account.as_bytes()
    );
    let moved = ExpectedFence {
        generation: expected.generation.next()?,
        ..expected
    };
    assert!(matches!(
        fixture.manager.custody_fields(&slot, &moved).await,
        Err(CustodyError::FenceMoved { .. })
    ));
    let foreign = ExpectedFence {
        account_binding: plakous_auth::custody::IdentityDigest::from_bytes([7; 32]),
        ..expected
    };
    assert!(matches!(
        fixture.manager.custody_fields(&slot, &foreign).await,
        Err(CustodyError::AccountChanged)
    ));
    Ok(())
}

#[tokio::test]
async fn concurrent_rejections_of_one_generation_claim_exactly_one_grant() -> TestResult {
    let account = "custody-single-writer-account";
    let subject = "custody-single-writer-subject";
    let (fixture, peers, server) = renew_fixture(
        account,
        subject,
        now_unix_secs() + 3_600,
        1,
        refresh_exchange(subject)?,
    )
    .await?;
    let slot = slot()?;
    let facts = fixture.manager.custody_facts(&slot).await?;
    let rejected = facts.expected_fence().ok_or("no fence")?;
    let selection = facts.selection_epoch_digest;
    // One manager's callers serialize on its lifecycle lock; separate
    // processes serialize on the store's refresh claim.
    let peer = peers.first().ok_or("no peer")?;
    let refreshes: Vec<_> = (0..4)
        .map(|_| {
            let (peer, slot) = (Arc::clone(peer), slot.clone());
            tokio::spawn(async move {
                peer.custody_refresh_after_rejection(&slot, &rejected, selection)
                    .await
            })
        })
        .collect();
    let mut answers = Vec::new();
    for refresh in refreshes {
        answers.push(refresh.await?);
    }
    let current = fixture.manager.custody_facts(&slot).await?;
    assert_eq!(current.status, CustodyStatus::Ready);
    assert_eq!(current.account_binding, facts.account_binding);
    assert_eq!(current.selection_epoch_digest, facts.selection_epoch_digest);
    assert!(current.generation > Some(rejected.generation));
    for answer in answers {
        assert_eq!(answer?.generation, current.generation);
    }
    let (requests, unexpected) = server.await??;
    assert_eq!(request_kinds(&requests), ["catalog", "refresh", "userinfo"]);
    assert!(!unexpected, "a second grant reached the token endpoint");
    Ok(())
}

#[tokio::test]
async fn a_refresh_between_the_reads_moves_the_fence_with_the_current_facts() -> TestResult {
    let account = "custody-race-account";
    let (fixture, _peers, _server) = renew_fixture(
        account,
        "custody-race-subject",
        now_unix_secs() + 3_600,
        0,
        Vec::new(),
    )
    .await?;
    let slot = slot()?;
    let expected = fixture
        .manager
        .custody_facts(&slot)
        .await?
        .expected_fence()
        .ok_or("no fence")?;
    fixture.manager.refresh_between_custody_reads()?;
    let refusal = fixture.manager.custody_fields(&slot, &expected).await;
    let Err(CustodyError::FenceMoved { facts }) = refusal else {
        return Err(format!("the moved record was not named: {refusal:?}").into());
    };
    assert_eq!(facts.generation, Some(expected.generation.next()?));
    assert_eq!(facts.account_binding, Some(expected.account_binding));
    let moved = facts.expected_fence().ok_or("no moved fence")?;
    assert!(
        fixture
            .manager
            .custody_fields(&slot, &moved)
            .await?
            .take_hold()
            .is_some()
    );
    Ok(())
}

mod relayed {
    //! The race end to end: a relay prepared against a generation a refresh
    //! replaces between the custodian's two reads is prepared again exactly
    //! once and dials upstream exactly once.

    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    use super::*;
    use plakous_auth::custody::gateway::gateway_refusal;
    use plakous_auth::custody::{CredentialField, IdentityDigest};
    use plakous_gateway_protocol::envelope::RelayEnvelope;
    use plakous_gateway_protocol::refusal::GatewayRefusal;
    use plakous_gateway_protocol::{Endpoint, UuidText};
    use plakous_http::relay::{GatewayEndpoint, InMemoryGateway, RelayClient};
    use plakous_http::relay_server::{GatewayService, Injection, Peer, RelayServer, ServiceFuture};
    use plakous_http::{
        Bytes, ExchangeControl, HeaderMap, HeaderName, Limits, Method, Origin, Request,
        SensitiveBytes, sensitive_header_value,
    };

    /// A gateway serving one Codex login through its local custodian.
    struct Gateway {
        manager: Arc<CodexSubscriptionCredentialManager>,
        instance: UuidText,
        injections: AtomicUsize,
    }

    impl GatewayService for Gateway {
        type Principal = ();

        fn authenticate(&self, peer: &Peer, _key: Option<&[u8]>) -> Result<(), GatewayRefusal> {
            match peer {
                Peer::InProcess => Ok(()),
                _ => Err(GatewayRefusal::Unauthenticated {}),
            }
        }

        fn inject<'a>(
            &'a self,
            (): &'a (),
            envelope: &'a RelayEnvelope,
        ) -> ServiceFuture<'a, Injection> {
            Box::pin(async move {
                self.injections.fetch_add(1, Ordering::SeqCst);
                let slot = CustodySlot::new(envelope.account.as_str())
                    .map_err(|_| GatewayRefusal::AccountUnknown {})?;
                let expected = ExpectedFence {
                    account_binding: IdentityDigest::from_hex(
                        envelope.expected.account_binding.as_str(),
                    )
                    .ok_or(GatewayRefusal::EnvelopeInvalid {})?,
                    generation: CredentialGeneration::new(envelope.expected.generation)
                        .map_err(|_| GatewayRefusal::EnvelopeInvalid {})?,
                };
                let mut fields = self
                    .manager
                    .custody_fields(&slot, &expected)
                    .await
                    .map_err(|error| gateway_refusal(&error, &self.instance))?;
                let values = envelope
                    .custodied_names()
                    .map(|name| {
                        fields
                            .take(name)
                            .map(|field| SensitiveBytes::new(field.into_header_bytes()))
                            .ok_or(GatewayRefusal::CustodiedHeadersMismatch {})
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                let injection = Injection::new(values);
                Ok(match fields.take_hold() {
                    Some(hold) => injection.holding(hold.into_inner()),
                    None => injection,
                })
            })
        }

        fn call<'a>(
            &'a self,
            (): &'a (),
            _endpoint: Endpoint,
            _body: &'a [u8],
        ) -> ServiceFuture<'a, Vec<u8>> {
            Box::pin(async { Err(GatewayRefusal::ScopeDenied {}) })
        }
    }

    /// A loopback upstream answering `200`, counting its connections.
    async fn upstream() -> TestResult<(Origin, Arc<AtomicUsize>)> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let port = listener.local_addr()?.port();
        let accepted = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&accepted);
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                counter.fetch_add(1, Ordering::SeqCst);
                tokio::spawn(async move {
                    let mut request = Vec::new();
                    let mut buffer = [0_u8; 4096];
                    while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                        match socket.read(&mut buffer).await {
                            Ok(0) | Err(_) => return,
                            Ok(read) => request.extend_from_slice(&buffer[..read]),
                        }
                    }
                    let _ = socket
                        .write_all(b"HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok")
                        .await;
                    let _ = socket.shutdown().await;
                });
            }
        });
        Ok((
            Origin::loopback_http(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), port)?,
            accepted,
        ))
    }

    #[tokio::test]
    async fn a_refresh_between_the_reads_is_prepared_again_once_and_dialed_once() -> TestResult {
        let account = "custody-relay-race-account";
        let (fixture, _peers, _server) = renew_fixture(
            account,
            "custody-relay-race-subject",
            now_unix_secs() + 3_600,
            0,
            Vec::new(),
        )
        .await?;
        let slot = slot()?;
        let expected = fixture
            .manager
            .custody_facts(&slot)
            .await?
            .expected_fence()
            .ok_or("no fence")?;
        let (dial, accepted) = upstream().await?;
        let instance = plakous_http::relay_server::new_instance().ok_or("instance")?;
        let gateway = Arc::new(Gateway {
            manager: Arc::clone(&fixture.manager),
            instance: instance.clone(),
            injections: AtomicUsize::new(0),
        });
        let server = RelayServer::new(
            Arc::clone(&gateway),
            Client::new_http1(Limits::default())?,
            instance,
        )
        .with_dial_override("https://chatgpt.com", dial);
        let relay = RelayClient::new(
            GatewayEndpoint::InMemory(InMemoryGateway::new(move |io| {
                let server = server.clone();
                tokio::spawn(async move {
                    let _ = server.serve(io, Peer::InProcess).await;
                });
            })),
            None,
        )?;
        let client = Client::new_http1(Limits::default())?.with_relay(Arc::new(relay));
        let mut headers = HeaderMap::new();
        for name in ["authorization", "chatgpt-account-id"] {
            headers.insert(
                HeaderName::from_static(name),
                sensitive_header_value(
                    CredentialField::custodied(&slot, &expected).into_header_bytes(),
                )?,
            );
        }
        fixture.manager.refresh_between_custody_reads()?;
        let response = client
            .execute(
                Request::new(
                    Origin::parse("https://chatgpt.com/")?,
                    Method::POST,
                    "/backend-api/codex/responses",
                    headers,
                    Bytes::from_static(b"{}"),
                    &Limits::default(),
                )?,
                ExchangeControl::new(Instant::now() + Duration::from_secs(10)),
            )
            .await?;
        assert_eq!(response.status().as_u16(), 200);
        assert_eq!(
            gateway.injections.load(Ordering::SeqCst),
            2,
            "exactly one re-preparation"
        );
        assert_eq!(
            accepted.load(Ordering::SeqCst),
            1,
            "exactly one upstream dial"
        );
        Ok(())
    }
}
