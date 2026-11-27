use super::*;
use plakd_wire_types::console::{HostRequest, HostResponse, Page};
use plakous_host_protocol::wire::{Answer, HostFrame, NEUTRAL_WIRE_VERSION, Outcome};
use plakous_hosted::error::{CliFailure, CliResult};
use plakous_hosted::host_service::wire::{
    HostPrincipal, HostReadLimits, HostReadTask, HostRequestContext, HostSessions, SessionHost,
    StoredSessionView,
};
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _};

async fn websocket_client(
    address: std::net::SocketAddr,
) -> Result<WebSocketStream<tokio::net::TcpStream>, Box<dyn std::error::Error + Send + Sync>> {
    let mut stream = tokio::net::TcpStream::connect(address).await?;
    stream.write_all(format!("GET /wire HTTP/1.1\r\nHost: {address}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n").as_bytes()).await?;
    let mut reader = tokio::io::BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).await?;
    assert!(line.starts_with("HTTP/1.1 101"));
    loop {
        line.clear();
        if reader.read_line(&mut line).await? == 0 {
            return Err("upgrade ended early".into());
        }
        if line == "\r\n" {
            break;
        }
    }
    Ok(WebSocketStream::from_raw_socket(reader.into_inner(), Role::Client, None).await)
}

#[derive(Default)]
struct FixtureSessions {
    principals: Arc<Mutex<Vec<HostPrincipal>>>,
    opens: Arc<AtomicUsize>,
    archived: bool,
    history: Option<fn() -> Vec<plakous_host_protocol::SessionObservation<PlakExtension>>>,
}

pub(super) fn empty_host() -> Arc<SessionHost<PlakExtension>> {
    SessionHost::new(FixtureSessions::default(), "fixture", 1)
}

pub(super) fn archived_host() -> Arc<SessionHost<PlakExtension>> {
    SessionHost::new(
        FixtureSessions {
            archived: true,
            ..FixtureSessions::default()
        },
        "fixture",
        1,
    )
}

pub(super) fn archived_history(
    history: fn() -> Vec<plakous_host_protocol::SessionObservation<PlakExtension>>,
) -> Arc<SessionHost<PlakExtension>> {
    SessionHost::new(
        FixtureSessions {
            archived: true,
            history: Some(history),
            ..FixtureSessions::default()
        },
        "fixture",
        1,
    )
}

impl HostSessions<PlakExtension> for FixtureSessions {
    fn host_read_limits(&self) -> HostReadLimits {
        HostReadLimits {
            request_bytes: 64 * 1024,
            per_connection: 1,
            per_principal: 1,
            host: 1,
            deadline: Duration::from_secs(2),
            messages_per_second: 12,
            burst: 12,
        }
    }
    fn welcome_extension(
        &self,
        _: &HostPrincipal,
        _: &HostReadLimits,
    ) -> <PlakExtension as plakous_host_protocol::ProtocolExtension>::WelcomeExtension {
        None
    }
    fn host_request(
        &self,
        context: HostRequestContext,
        _: HostRequest,
    ) -> HostReadTask<PlakExtension> {
        self.principals
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(context.principal);
        Box::pin(async {
            Ok(HostResponse::AccountsStatus {
                as_of: "fixture".into(),
                rows: Vec::new(),
                diagnostics: Vec::new(),
                page: Page {
                    next: None,
                    truncated: false,
                },
            })
        })
    }
    fn contains(&self, session: &str) -> Result<bool, String> {
        Ok(self.archived && session == "archived")
    }
    fn rows(
        &self,
        _: Option<&str>,
        _: Option<&str>,
        _: bool,
    ) -> Result<Vec<plakous_host_protocol::wire::SessionRow>, String> {
        Ok(Vec::new())
    }
    fn session_of(
        &self,
        node: &plakous_host_protocol::wire::NodeAddress,
    ) -> Result<Option<String>, String> {
        Ok(self.archived.then(|| node.tree_id.clone()))
    }
    fn stored_view(
        &self,
        session: &str,
        _: bool,
        _: bool,
    ) -> Result<Option<StoredSessionView<PlakExtension>>, plakous_host_protocol::wire::RequestRefusal>
    {
        use plakous_host_protocol::{
            SessionObservation, views::presentation::PresentationSnapshot,
        };
        Ok(
            (self.archived && session == "archived").then(|| StoredSessionView {
                archived: true,
                session: session.into(),
                root: plakous_host_protocol::wire::NodeAddress {
                    tree_id: session.into(),
                    agent_id: session.into(),
                },
                observations: self.history.map_or_else(
                    || {
                        vec![SessionObservation::PresentationSnapshot(
                            PresentationSnapshot::empty(1),
                        )]
                    },
                    |history| history(),
                ),
            }),
        )
    }
    fn open(
        &self,
        session: &str,
    ) -> CliResult<plakous_hosted::host_service::wire::OpenedSession<PlakExtension>> {
        self.opens.fetch_add(1, Ordering::SeqCst);
        if session != "resident" {
            return Err(CliFailure::new("no session"));
        }
        let (actions, intents) = plakous_host_protocol::intent_channel();
        let (observations, events) = plakous_host_protocol::observation_channel();
        let worker = plakous_hosted::runtime::HostWorkerHandle::spawn(move || {
            let _observations = observations;
            while !intents.abandon_and_release_requested() {
                if let Err(std::sync::mpsc::RecvTimeoutError::Disconnected) =
                    intents.recv_timeout(Duration::from_millis(10))
                {
                    break;
                }
            }
            Ok(())
        });
        Ok(plakous_hosted::host_service::wire::OpenedSession {
            runner_pending: None,
            root: plakous_host_protocol::wire::NodeAddress {
                tree_id: session.into(),
                agent_id: session.into(),
            },
            session_id: session.into(),
            actions,
            events,
            worker,
            attachments: plakous_hosted::host_service::AttachmentCount::default(),
            roots: Box::new(|_| None),
        })
    }
    fn refusal(&self, _: &str, _: &CliFailure) -> plakous_host_protocol::wire::RequestRefusal {
        plakous_host_protocol::wire::RequestRefusal::UnknownSession
    }
    fn create(
        &self,
        _: &plakous_hosted::host_service::wire::Creation<'_>,
    ) -> Result<
        plakous_hosted::host_service::wire::Created,
        plakous_host_protocol::wire::RequestRefusal,
    > {
        self.opens.fetch_add(1, Ordering::SeqCst);
        Err(plakous_host_protocol::wire::RequestRefusal::UnknownSession)
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn web_drive_binds_the_verified_login_before_welcome_and_host_dispatch()
-> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let home = tempfile::tempdir()?;
    let paths = crate::home::paths::PlakPaths::explicit(home.path().to_path_buf());
    paths.ensure_layout()?;
    let principals = Arc::new(Mutex::new(Vec::new()));
    let opens = Arc::new(AtomicUsize::new(0));
    let host = SessionHost::new(
        FixtureSessions {
            principals: Arc::clone(&principals),
            opens: Arc::clone(&opens),
            archived: false,
            history: None,
        },
        "fixture",
        1,
    );
    let served = Arc::new(Served {
        host,
        stop: super::super::StopProbe::armed(),
        ui: None,
        gate: super::super::auth::Gate::new(
            super::super::auth::WebAccess::Dev {
                login: "verified-operator".into(),
                origin: super::super::auth::WebOrigin::parse("http://127.0.0.1:9191")?,
            },
            super::super::auth::CookieKey::load_or_create(&paths)?,
        ),
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let (done, finished) = tokio::sync::oneshot::channel();
    let done = Arc::new(Mutex::new(Some(done)));
    let connection_task = tokio::spawn(async move {
        let (stream, _) = listener.accept().await?;
        hyper::server::conn::http1::Builder::new()
            .serve_connection(
                TokioIo::new(stream),
                hyper::service::service_fn(
                    move |mut request: hyper::Request<hyper::body::Incoming>| {
                        let served = Arc::clone(&served);
                        let done = Arc::clone(&done);
                        async move {
                            let accept = accept_key(request.headers())
                                .ok_or_else(|| std::io::Error::other("valid websocket upgrade"))?;
                            let upgrade = hyper::upgrade::on(&mut request);
                            tokio::spawn(async move {
                                if let Ok(upgraded) = upgrade.await {
                                    let socket = WebSocketStream::from_raw_socket(
                                        ProgressIo::new(TokioIo::new(upgraded)),
                                        Role::Server,
                                        None,
                                    )
                                    .await;
                                    let _ = drive(socket, &served, "verified-operator").await;
                                }
                                if let Some(done) =
                                    done.lock().unwrap_or_else(PoisonError::into_inner).take()
                                {
                                    let _ = done.send(());
                                }
                            });
                            Ok::<_, std::io::Error>(switching_protocols(&accept))
                        }
                    },
                ),
            )
            .with_upgrades()
            .await
            .map_err(std::io::Error::other)?;
        Ok::<_, std::io::Error>(())
    });
    let mut client = websocket_client(address).await?;
    client.send(Message::text(serde_json::json!({"frame":"hello","proto":NEUTRAL_WIRE_VERSION,"binary_id":"fixture","caps":[]}).to_string())).await?;
    let welcome = client.next().await.ok_or("welcome")??;
    assert!(matches!(
        serde_json::from_str::<HostFrame<PlakExtension>>(welcome.to_text()?)?,
        HostFrame::Welcome(_)
    ));
    client.send(Message::text(serde_json::json!({"frame":"request","id":1,"op":{"op":"host","request":{"request":"accounts_status"}}}).to_string())).await?;
    let response = client.next().await.ok_or("host answer")??;
    assert!(matches!(
        serde_json::from_str::<HostFrame<PlakExtension>>(response.to_text()?)?,
        HostFrame::Response {
            id: 1,
            outcome: Outcome::Ok(Answer::Host { .. })
        }
    ));
    assert_eq!(
        *principals.lock().unwrap_or_else(PoisonError::into_inner),
        vec![HostPrincipal::Web("verified-operator".into())]
    );
    assert_eq!(opens.load(Ordering::SeqCst), 0);
    client.close(None).await?;
    tokio::time::timeout(Duration::from_secs(5), finished).await??;
    connection_task.await??;
    Ok(())
}
