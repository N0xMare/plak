#![allow(clippy::unwrap_used, reason = "isolated host read fixtures")]

use super::*;
use plakous_host_protocol::{Never, ProtocolExtension};
use std::sync::atomic::{AtomicBool, AtomicUsize};
use std::task::{Context, Poll, Waker};

#[derive(Clone, Debug, Eq, PartialEq)]
struct Reads;
impl ProtocolExtension for Reads {
    type Intent = Never;
    type Observation = Never;
    type HostRequest = u8;
    type HostResponse = u8;
    type WelcomeExtension = Option<bool>;
}

#[derive(Default)]
struct State {
    mutation_timeout: AtomicBool,
    disconnected: AtomicUsize,
    ready: AtomicBool,
    started: AtomicUsize,
    polls: AtomicUsize,
    dropped: AtomicUsize,
    contexts: Mutex<Vec<HostRequestContext>>,
    wakers: Mutex<Vec<Waker>>,
}
impl State {
    fn complete(&self) {
        self.ready.store(true, Ordering::SeqCst);
        for waker in std::mem::take(&mut *self.wakers.lock().unwrap()) {
            waker.wake();
        }
    }
}
struct Read {
    state: Arc<State>,
    request: u8,
}
impl Future for Read {
    type Output = Result<u8, RequestRefusal>;
    fn poll(self: std::pin::Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.state.polls.fetch_add(1, Ordering::SeqCst);
        if self.request != 0 || self.state.ready.load(Ordering::SeqCst) {
            Poll::Ready(Ok(self.request))
        } else {
            self.state.wakers.lock().unwrap().push(cx.waker().clone());
            Poll::Pending
        }
    }
}
impl Drop for Read {
    fn drop(&mut self) {
        self.state.dropped.fetch_add(1, Ordering::SeqCst);
    }
}
struct Stores {
    state: Arc<State>,
    limits: HostReadLimits,
}
fn limits() -> HostReadLimits {
    HostReadLimits {
        request_bytes: 64 * 1024,
        per_connection: 2,
        per_principal: 4,
        host: 32,
        deadline: Duration::from_secs(10),
        messages_per_second: 12,
        burst: 12,
    }
}
fn fixture(policy: HostReadLimits) -> (Arc<SessionHost<Reads>>, Arc<State>) {
    let state = Arc::new(State::default());
    (
        SessionHost::new(
            Stores {
                state: state.clone(),
                limits: policy,
            },
            "test",
            1,
        ),
        state,
    )
}
impl HostSessions<Reads> for Stores {
    fn host_disconnect(&self, _: &HostPrincipal, _: u64, _: u64) {
        self.state.disconnected.fetch_add(1, Ordering::SeqCst);
    }
    fn host_request_timeout(&self, _: &u8) -> Result<u8, RequestRefusal> {
        if self.state.mutation_timeout.load(Ordering::SeqCst) {
            Ok(255)
        } else {
            Err(RequestRefusal::DeadlineExceeded)
        }
    }
    fn host_read_limits(&self) -> HostReadLimits {
        self.limits
    }
    fn welcome_extension(
        &self,
        principal: &HostPrincipal,
        limits: &HostReadLimits,
    ) -> Option<bool> {
        assert_eq!(*limits, self.limits);
        Some(matches!(principal, HostPrincipal::Socket(_)))
    }
    fn host_request(&self, context: HostRequestContext, request: u8) -> HostReadTask<Reads> {
        assert_eq!(context.host_generation, 1);
        self.state.contexts.lock().unwrap().push(context);
        self.state.started.fetch_add(1, Ordering::SeqCst);
        Box::pin(Read {
            state: self.state.clone(),
            request,
        })
    }
    fn contains(&self, _: &str) -> Result<bool, String> {
        panic!("read touched session store")
    }
    fn rows(&self, _: Option<&str>, _: Option<&str>, _: bool) -> Result<Vec<SessionRow>, String> {
        panic!("read touched directory")
    }
    fn session_of(&self, _: &NodeAddress) -> Result<Option<String>, String> {
        panic!("read resolved a node")
    }
    fn open(&self, _: &str) -> CliResult<OpenedSession<Reads>> {
        panic!("read opened a runtime")
    }
    fn refusal(&self, _: &str, _: &CliFailure) -> RequestRefusal {
        panic!("read attempted session admission")
    }
    fn create(&self, _: &Creation<'_>) -> Result<Created, RequestRefusal> {
        panic!("read created a session")
    }
}
#[derive(Clone, Default)]
struct Output(Arc<Mutex<Vec<u8>>>);
impl Write for Output {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
impl Output {
    fn take(&self) -> Vec<HostFrame<Reads>> {
        String::from_utf8(std::mem::take(&mut *self.0.lock().unwrap()))
            .unwrap()
            .lines()
            .map(|line| decode_line(line).unwrap())
            .collect()
    }
}
type Connection = HostConnection<Reads, Output>;

#[test]
fn dropping_a_connection_retires_operator_authority_even_without_explicit_leave() {
    let (host, state) = fixture(limits());
    let mut connection = host.connect(
        Output::default(),
        Face::Remote,
        HostPrincipal::Socket(501),
        || false,
    );
    request(&mut connection, 10, Op::Host { request: 0 });
    connection.flush().unwrap();
    assert_eq!(state.disconnected.load(Ordering::SeqCst), 0);
    drop(connection);
    assert_eq!(state.disconnected.load(Ordering::SeqCst), 1);
    assert_eq!(state.dropped.load(Ordering::SeqCst), 1);
}
fn request(connection: &mut Connection, id: u64, op: Op<Reads>) {
    let line = plakous_host_protocol::wire::encode_line(&ClientFrame::<Reads>::Request { id, op })
        .unwrap();
    assert!(connection.frame(Ok(line)).unwrap());
}
fn refused(frames: &[HostFrame<Reads>], id: u64) -> &RequestRefusal {
    frames
        .iter()
        .find_map(|frame| match frame {
            HostFrame::Response {
                id: found,
                outcome: Outcome::Refused(reason),
            } if *found == id => Some(reason),
            _ => None,
        })
        .unwrap()
}

#[test]
fn welcome_and_context_use_the_required_transport_principal() {
    let (host, state) = fixture(limits());
    for principal in [
        HostPrincipal::Socket(501),
        HostPrincipal::Web("fixture-login".into()),
    ] {
        assert_eq!(
            host.welcome_for(&principal).extension,
            Some(matches!(principal, HostPrincipal::Socket(_)))
        );
        let out = Output::default();
        let mut connection = host.connect(out.clone(), Face::Remote, principal.clone(), || false);
        assert_eq!(connection.face, Face::Remote);
        request(&mut connection, 1, Op::Host { request: 1 });
        connection.flush().unwrap();
        assert_eq!(
            state.contexts.lock().unwrap().last().unwrap().principal,
            principal
        );
        assert!(matches!(
            out.take().as_slice(),
            [HostFrame::Response {
                outcome: Outcome::Ok(Answer::Host { response: 1 }),
                ..
            }]
        ));
    }
}

#[test]
fn host_reads_are_private_bounded_attachment_free_and_do_not_stall_ping() {
    let (host, state) = fixture(limits());
    let out = Output::default();
    let mut connection = host.connect(
        out.clone(),
        Face::Remote,
        HostPrincipal::Socket(501),
        || false,
    );
    for (id, op) in [
        (10, Op::Host { request: 0 }),
        (11, Op::Host { request: 1 }),
        (12, Op::Host { request: 2 }),
        (13, Op::Ping),
    ] {
        request(&mut connection, id, op);
    }
    connection.flush().unwrap();
    let frames = out.take();
    assert!(matches!(
        refused(&frames, 12),
        RequestRefusal::RateLimited { .. }
    ));
    assert!(frames.iter().any(|f| matches!(
        f,
        HostFrame::Response {
            id: 11,
            outcome: Outcome::Ok(Answer::Host { response: 1 })
        }
    )));
    assert!(frames.iter().any(|f| matches!(
        f,
        HostFrame::Response {
            id: 13,
            outcome: Outcome::Ok(Answer::Pong)
        }
    )));
    assert_eq!(state.started.load(Ordering::SeqCst), 2);
    state.complete();
    connection.flush().unwrap();
    assert!(matches!(
        out.take().as_slice(),
        [HostFrame::Response {
            id: 10,
            outcome: Outcome::Ok(Answer::Host { response: 0 })
        }]
    ));
    connection.flush().unwrap();
    assert_eq!(out.take(), []);
}

#[test]
fn principal_and_host_caps_are_shared_and_leave_releases_both_permits() {
    let (host, state) = fixture(limits());
    let mut connections = Vec::new();
    for _ in 0..2 {
        let mut connection = host.connect(
            Output::default(),
            Face::Remote,
            HostPrincipal::Socket(501),
            || false,
        );
        request(&mut connection, 1, Op::Host { request: 0 });
        request(&mut connection, 2, Op::Host { request: 0 });
        connections.push(connection);
    }
    let out = Output::default();
    let mut third = host.connect(
        out.clone(),
        Face::Remote,
        HostPrincipal::Socket(501),
        || false,
    );
    request(&mut third, 3, Op::Host { request: 0 });
    assert!(matches!(
        refused(&out.take(), 3),
        RequestRefusal::RateLimited { .. }
    ));
    assert_eq!(state.started.load(Ordering::SeqCst), 4);
    connections[0].leave();
    request(&mut third, 4, Op::Host { request: 0 });
    assert_eq!(state.started.load(Ordering::SeqCst), 5);
    third.leave();
    connections.clear();
    for uid in 0..32 {
        let mut connection = host.connect(
            Output::default(),
            Face::Remote,
            HostPrincipal::Socket(uid),
            || false,
        );
        request(&mut connection, 1, Op::Host { request: 0 });
        connections.push(connection);
    }
    let mut extra = host.connect(
        out.clone(),
        Face::Remote,
        HostPrincipal::Web("other".into()),
        || false,
    );
    request(&mut extra, 5, Op::Host { request: 0 });
    assert!(matches!(
        refused(&out.take(), 5),
        RequestRefusal::RateLimited { .. }
    ));
    connections[0].leave();
    request(&mut extra, 6, Op::Host { request: 0 });
    assert!(extra.host_reads.contains_key(&6));
}

#[test]
fn duplicate_pending_id_never_starts_a_second_read() {
    let (host, state) = fixture(limits());
    let out = Output::default();
    let mut connection = host.connect(
        out.clone(),
        Face::Remote,
        HostPrincipal::Socket(501),
        || false,
    );
    request(&mut connection, 10, Op::Host { request: 0 });
    request(&mut connection, 10, Op::Host { request: 1 });
    assert!(matches!(
        refused(&out.take(), 10),
        RequestRefusal::Malformed { .. }
    ));
    assert_eq!(state.started.load(Ordering::SeqCst), 1);
    assert!(connection.host_reads.contains_key(&10));
}

#[test]
fn expired_read_releases_permits_and_discards_late_completion() {
    let (host, state) = fixture(limits());
    let out = Output::default();
    let mut connection = host.connect(
        out.clone(),
        Face::Remote,
        HostPrincipal::Socket(501),
        || false,
    );
    request(&mut connection, 10, Op::Host { request: 0 });
    connection.flush().unwrap();
    connection.host_reads.get_mut(&10).unwrap().deadline = Instant::now();
    connection.flush().unwrap();
    assert_eq!(refused(&out.take(), 10), &RequestRefusal::DeadlineExceeded);
    assert_eq!(state.dropped.load(Ordering::SeqCst), 1);
    assert!(connection.host_reads.is_empty());
    state.complete();
    connection.flush().unwrap();
    assert!(
        out.take().is_empty(),
        "late wake must not answer after deadline refusal"
    );
    request(&mut connection, 11, Op::Host { request: 1 });
    connection.flush().unwrap();
    assert!(matches!(
        out.take().as_slice(),
        [HostFrame::Response {
            id: 11,
            outcome: Outcome::Ok(Answer::Host { response: 1 })
        }]
    ));
}

#[tokio::test]
async fn a_mutation_deadline_answers_the_product_unknown_outcome_not_a_no_effect_refusal() {
    let (host, state) = fixture(limits());
    state.mutation_timeout.store(true, Ordering::SeqCst);
    let out = Output::default();
    let mut connection = host.connect(
        out.clone(),
        Face::Remote,
        HostPrincipal::Socket(501),
        || false,
    );
    request(&mut connection, 10, Op::Host { request: 0 });
    connection.flush().unwrap();
    connection.host_reads.get_mut(&10).unwrap().deadline = Instant::now();
    connection.flush().unwrap();
    assert!(matches!(
        out.take().as_slice(),
        [HostFrame::Response {
            id: 10,
            outcome: Outcome::Ok(Answer::Host { response: 255 })
        }]
    ));
    assert_eq!(state.dropped.load(Ordering::SeqCst), 1);
    state.complete();
    connection.flush().unwrap();
    assert!(
        out.take().is_empty(),
        "late work cannot produce another answer"
    );
}

#[tokio::test]
async fn only_woken_reads_are_polled_and_completion_wakes_the_acceptor() {
    let (host, state) = fixture(limits());
    let out = Output::default();
    let mut connection = host.connect(
        out.clone(),
        Face::Remote,
        HostPrincipal::Socket(501),
        || false,
    );
    request(&mut connection, 10, Op::Host { request: 0 });
    connection.flush().unwrap();
    connection.flush().unwrap();
    assert_eq!(state.polls.load(Ordering::SeqCst), 1);
    let initially_notified = std::future::poll_fn(|cx| {
        let mut changed = std::pin::pin!(connection.changed());
        Poll::Ready(changed.as_mut().poll(cx).is_ready())
    })
    .await;
    assert!(initially_notified, "enqueue must wake the acceptor");
    {
        let mut changed = std::pin::pin!(connection.changed());
        let mut cx = Context::from_waker(Waker::noop());
        assert!(changed.as_mut().poll(&mut cx).is_pending());
        state.complete();
        assert!(changed.as_mut().poll(&mut cx).is_ready());
    }
    connection.flush().unwrap();
    assert_eq!(state.polls.load(Ordering::SeqCst), 2);
    assert_eq!(out.take().len(), 1);
}

#[tokio::test]
async fn an_unwoken_read_deadline_wakes_the_acceptor() {
    let (host, _) = fixture(limits());
    let out = Output::default();
    let mut connection = host.connect(
        out.clone(),
        Face::Remote,
        HostPrincipal::Socket(501),
        || false,
    );
    request(&mut connection, 10, Op::Host { request: 0 });
    connection.flush().unwrap();
    let initially_notified = std::future::poll_fn(|cx| {
        let mut changed = std::pin::pin!(connection.changed());
        Poll::Ready(changed.as_mut().poll(cx).is_ready())
    })
    .await;
    assert!(initially_notified);
    connection.host_reads.get_mut(&10).unwrap().deadline =
        Instant::now().checked_sub(Duration::from_secs(1)).unwrap();
    let deadline_ready = std::future::poll_fn(|cx| {
        let mut changed = std::pin::pin!(connection.changed());
        Poll::Ready(changed.as_mut().poll(cx).is_ready())
    })
    .await;
    assert!(
        deadline_ready,
        "deadline must wake without a new frame or task wake"
    );
    connection.flush().unwrap();
    assert_eq!(refused(&out.take(), 10), &RequestRefusal::DeadlineExceeded);
}

#[test]
fn console_bucket_and_predecode_byte_bound_preserve_the_connection() {
    let mut policy = limits();
    policy.messages_per_second = 0; // No clock-dependent refill in this fixture.
    policy.burst = 2;
    policy.request_bytes = 256;
    let (host, state) = fixture(policy);
    let out = Output::default();
    let mut one = host.connect(
        out.clone(),
        Face::Remote,
        HostPrincipal::Socket(501),
        || false,
    );
    let mut two = host.connect(
        out.clone(),
        Face::Remote,
        HostPrincipal::Socket(501),
        || false,
    );
    let mut oversized = host.connect(
        out.clone(),
        Face::Remote,
        HostPrincipal::Socket(501),
        || false,
    );
    let large =
        serde_json::json!({"frame":"request","id":9,"op":{"op":"host","request":"x".repeat(1024)}})
            .to_string();
    assert!(Connection::is_host_request(&large));
    assert!(!oversized.frame(Ok(large)).unwrap());
    assert!(matches!(
        refused(&out.take(), 9),
        RequestRefusal::Malformed { .. }
    ));
    drop(oversized);
    request(&mut one, 1, Op::Host { request: 1 });
    one.flush().unwrap();
    out.take();
    request(&mut two, 2, Op::Host { request: 1 });
    two.flush().unwrap();
    out.take();
    request(&mut one, 3, Op::Host { request: 1 });
    assert!(matches!(
        refused(&out.take(), 3),
        RequestRefusal::RateLimited { .. }
    ));
    assert_eq!(state.started.load(Ordering::SeqCst), 2);
    request(&mut one, 4, Op::Ping);
    assert!(matches!(
        out.take().as_slice(),
        [HostFrame::Response {
            id: 4,
            outcome: Outcome::Ok(Answer::Pong)
        }]
    ));
    drop(one);
    drop(two);
    let mut reconnected = host.connect(
        out.clone(),
        Face::Remote,
        HostPrincipal::Socket(501),
        || false,
    );
    request(&mut reconnected, 5, Op::Host { request: 1 });
    assert!(matches!(
        refused(&out.take(), 5),
        RequestRefusal::RateLimited { .. }
    ));
    assert_eq!(
        state.started.load(Ordering::SeqCst),
        2,
        "reconnect cannot replenish a consumed principal bucket"
    );
}
