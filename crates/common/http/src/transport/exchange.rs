//! The private request path of `Client`: execution, dispatch, connect and exchange.

use std::sync::Arc;
use std::time::Instant;

use http::Request as WireRequest;
use rustls::pki_types::ServerName;
use tokio_rustls::TlsConnector;

use crate::{
    ConnectionUse, ErrorKind, ExchangeControl, Failure, Origin, Phase, Request, TransmissionFact,
    WireProtocol,
};

use super::{
    Client, ConnectionSettings, Exchanged, Live, Pool, PoolKey, ProgressSlot, RequestBody,
    RequestParts, Response, Reuse, Sender, TaskGuard, WireStream, establish, request_error_kind,
    resolve, response_from_wire, wait_response, wait_task,
};

impl Client {
    pub(super) async fn execute_inner(
        &self,
        request: Request,
        control: ExchangeControl,
    ) -> Result<Response, Failure> {
        request.validate(&self.inner.limits).map_err(|error| {
            Failure::new(
                Phase::RequestWrite,
                TransmissionFact::NotStarted,
                request_error_kind(&error),
            )
        })?;
        if control.cancellation().is_cancelled() {
            return Err(Failure::new(
                Phase::Resolution,
                TransmissionFact::NotStarted,
                ErrorKind::Cancelled,
            ));
        }
        if Instant::now() >= control.deadline() {
            return Err(Failure::new(
                Phase::Resolution,
                TransmissionFact::NotStarted,
                ErrorKind::DeadlineExceeded,
            ));
        }

        let (origin, method, path_and_query, headers, body) = request.into_parts();
        #[cfg(feature = "fault-injection")]
        let stream_fault = match self
            .faults
            .as_ref()
            .and_then(|plan| plan.select(origin.host(), path_and_query.as_str()))
        {
            None => None,
            Some(super::fault::Selected::Invalid) => {
                return Err(Failure::new(
                    Phase::Resolution,
                    TransmissionFact::NotStarted,
                    ErrorKind::Internal,
                ));
            }
            Some(super::fault::Selected::Connect) => {
                return Err(Failure::new(
                    Phase::Connect,
                    TransmissionFact::NotStarted,
                    ErrorKind::Io,
                )
                .with_io_cause(&std::io::Error::from(std::io::ErrorKind::ConnectionRefused)));
            }
            Some(super::fault::Selected::WriteReset) => {
                return Err(Failure::new(
                    Phase::RequestWriteOrResponseHead,
                    TransmissionFact::RequestIncomplete,
                    ErrorKind::Protocol,
                ));
            }
            Some(super::fault::Selected::RecordRejected) => {
                return Err(Failure::new(
                    Phase::RequestWriteOrResponseHead,
                    TransmissionFact::RequestIncomplete,
                    ErrorKind::Protocol,
                )
                .with_peer_rejected_record()
                .with_wire_protocol(WireProtocol::Http1));
            }
            Some(super::fault::Selected::Status {
                status,
                retry_after,
            }) => {
                return super::fault::injected_status(
                    status,
                    retry_after,
                    control,
                    &self.inner.limits,
                )
                .await;
            }
            Some(super::fault::Selected::Stream(stream_fault)) => Some(stream_fault),
        };
        let parts = RequestParts {
            origin,
            method,
            path_and_query,
            headers: headers.into_inner(),
            body,
        };
        #[cfg_attr(
            not(feature = "fault-injection"),
            allow(unused_mut, reason = "set only by faults")
        )]
        let mut response = self.dispatch(&parts, &control).await?;
        #[cfg(feature = "fault-injection")]
        {
            response.body.fault = stream_fault;
        }
        Ok(response)
    }

    /// Sends the request on an idle pooled connection when there is one,
    /// replacing that once with a fresh connection when it proves stale.
    async fn dispatch(
        &self,
        parts: &RequestParts,
        control: &ExchangeControl,
    ) -> Result<Response, Failure> {
        let pool = self
            .inner
            .pool
            .as_ref()
            .map(|pool| (pool, self.pool_key(&parts.origin)));
        let mut connection_use = ConnectionUse::Fresh;
        let mut idle_age = None;
        if let Some((pool, key)) = &pool
            && !control.fresh_connection()
            && let Some((idle, age)) = pool.checkout(key)
        {
            idle_age = Some(age);
            let progress = parts.progress(idle.protocol);
            idle.slot.replace(Arc::clone(&progress));
            let live = Live {
                connection: idle.connection,
                slot: idle.slot,
                progress,
                protocol: idle.protocol,
            };
            match self
                .exchange(
                    live,
                    parts,
                    control,
                    ConnectionUse::Reused,
                    Some((pool, key)),
                )
                .await
            {
                Ok(mut response) => {
                    response.body.idle_age = idle_age;
                    return Ok(response);
                }
                Err(Exchanged::Stale(_)) => connection_use = ConnectionUse::ReplacedStale,
                Err(Exchanged::Failed(failure)) => return Err(failure.with_idle_age(idle_age)),
            }
        }
        let live = self.connect(parts, control).await.map_err(|failure| {
            failure
                .with_connection_use(connection_use)
                .with_idle_age(idle_age)
        })?;
        let pool = pool.as_ref().map(|(pool, key)| (*pool, key));
        self.exchange(live, parts, control, connection_use, pool)
            .await
            .map(|mut response| {
                response.body.idle_age = idle_age;
                response
            })
            .map_err(|(Exchanged::Stale(failure) | Exchanged::Failed(failure))| {
                failure.with_idle_age(idle_age)
            })
    }

    fn pool_key(&self, origin: &Origin) -> PoolKey {
        PoolKey {
            host: origin.host().to_owned(),
            port: origin.port(),
            plain_loopback: origin.is_loopback_http(),
            address_policy: self.inner.address_policy,
            tls_config: Arc::as_ptr(&self.inner.tls_config) as usize,
            policy: self.inner.policy,
            settings: ConnectionSettings::new(&self.inner.limits),
        }
    }

    /// Resolves, connects and, for HTTPS, verifies TLS/ALPN for a fresh
    /// connection.
    pub(super) async fn connect_stream(
        &self,
        origin: &Origin,
        control: &ExchangeControl,
    ) -> Result<(WireStream, WireProtocol), Failure> {
        let resolver = self.inner.resolver.clone();
        let policy = self.inner.address_policy;
        let target = origin.clone();
        let addresses = wait_task(
            TaskGuard::new(tokio::spawn(async move {
                resolve(resolver, &target, policy).await
            })),
            control,
            self.inner.limits.resolution_timeout,
            Phase::Resolution,
            TransmissionFact::NotStarted,
        )
        .await?;

        let stream = wait_task(
            TaskGuard::new(tokio::spawn(async move {
                super::connect::connect(addresses).await
            })),
            control,
            self.inner.limits.connect_timeout,
            Phase::Connect,
            TransmissionFact::NotStarted,
        )
        .await?;
        if origin.is_loopback_http() {
            return Ok((WireStream::Plain(stream), WireProtocol::Http1));
        }

        let server_name = ServerName::try_from(origin.host().to_owned())
            .map_err(|_| Failure::new(Phase::Tls, TransmissionFact::NotStarted, ErrorKind::Tls))?;
        let tls_config = Arc::clone(&self.inner.tls_config);
        let tls_stream = wait_task(
            TaskGuard::new(tokio::spawn(async move {
                TlsConnector::from(tls_config)
                    .connect(server_name, stream)
                    .await
                    .map_err(|error| {
                        Failure::new(Phase::Tls, TransmissionFact::NotStarted, ErrorKind::Tls)
                            .with_io_cause(&error)
                    })
            })),
            control,
            self.inner.limits.tls_timeout,
            Phase::Tls,
            TransmissionFact::NotStarted,
        )
        .await?;

        let protocol = match tls_stream.get_ref().1.alpn_protocol() {
            Some(b"h2") => WireProtocol::Http2,
            Some(b"http/1.1") | None => WireProtocol::Http1,
            Some(_) => {
                return Err(Failure::new(
                    Phase::Tls,
                    TransmissionFact::NotStarted,
                    ErrorKind::Tls,
                ));
            }
        };
        Ok((WireStream::Tls(Box::new(tls_stream)), protocol))
    }

    async fn connect(
        &self,
        parts: &RequestParts,
        control: &ExchangeControl,
    ) -> Result<Live, Failure> {
        let (stream, protocol) = self.connect_stream(&parts.origin, control).await?;
        // A fresh exchange's progress also counts the HTTP/2 preface and
        // settings the handshake writes.
        let progress = parts.progress(protocol);
        let slot = ProgressSlot::new(Arc::clone(&progress));
        let wire_slot = Arc::clone(&slot);
        let limits = self.inner.limits.clone();
        let connection = wait_task(
            TaskGuard::new(tokio::spawn(async move {
                establish(stream, protocol, &limits, wire_slot).await
            })),
            control,
            self.inner.limits.tls_timeout,
            Phase::Tls,
            TransmissionFact::NotStarted,
        )
        .await
        .map_err(|failure| failure.with_wire_protocol(protocol))?;
        Ok(Live {
            connection,
            slot,
            progress,
            protocol,
        })
    }

    /// Sends the request on one negotiated connection and validates the head.
    ///
    /// Only a reused connection fails [`Exchanged::Stale`]: one whose stream
    /// failed before accepting a single byte of this request, so the peer
    /// cannot have seen any of it.
    async fn exchange(
        &self,
        live: Live,
        parts: &RequestParts,
        control: &ExchangeControl,
        connection_use: ConnectionUse,
        pool: Option<(&Arc<Pool>, &PoolKey)>,
    ) -> Result<Response, Exchanged> {
        let Live {
            mut connection,
            slot,
            progress,
            protocol,
        } = live;
        let failed = |failure: Failure| {
            Exchanged::Failed(
                failure
                    .with_wire_protocol(protocol)
                    .with_connection_use(connection_use),
            )
        };
        let limits = &self.inner.limits;
        if protocol == WireProtocol::Http2 && parts.body.is_empty() {
            progress.mark_complete();
        }
        let mut wire_request =
            WireRequest::new(RequestBody::new(parts.body.clone(), Arc::clone(&progress)));
        *wire_request.method_mut() = parts.method.clone();
        *wire_request.headers_mut() = parts.headers.clone();
        let sent = match connection.sender.take() {
            Some(Sender::Http1(mut sender)) => {
                *wire_request.uri_mut() = parts.http1_target().map_err(failed)?;
                let host = parts.host_header().map_err(failed)?;
                wire_request.headers_mut().insert(http::header::HOST, host);
                wait_response(
                    sender.send_request(wire_request),
                    connection,
                    Arc::clone(&progress),
                    control,
                    limits,
                )
                .await
                .map(|(response, mut connection)| {
                    connection.sender = Some(Sender::Http1(sender));
                    (response, connection)
                })
            }
            Some(Sender::Http2(mut sender)) => {
                *wire_request.uri_mut() = parts.http2_target().map_err(failed)?;
                wait_response(
                    sender.send_request(wire_request),
                    connection,
                    Arc::clone(&progress),
                    control,
                    limits,
                )
                .await
                .map(|(response, mut connection)| {
                    // Hyper's H2 dispatch future ends when its sender drops,
                    // even while executor-owned tasks still carry the
                    // response stream.
                    connection.sender = Some(Sender::Http2(sender));
                    (response, connection)
                })
            }
            None => Err(Failure::new(
                Phase::RequestWrite,
                TransmissionFact::NotStarted,
                ErrorKind::Internal,
            )),
        };
        let (wire_response, connection) = match sent {
            Ok(sent) => sent,
            Err(failure)
                if connection_use == ConnectionUse::Reused
                    && failure.kind() == ErrorKind::Protocol
                    && progress.accepted() == 0 =>
            {
                return Err(Exchanged::Stale(failure));
            }
            Err(failure) => return Err(failed(failure)),
        };
        let reuse = pool.map(|(pool, key)| Reuse {
            pool: Arc::downgrade(pool),
            key: key.clone(),
            slot,
        });
        response_from_wire(
            wire_response,
            connection,
            control.clone(),
            protocol,
            Some(connection_use),
            reuse,
            limits,
        )
        .await
        .map_err(failed)
    }
}
