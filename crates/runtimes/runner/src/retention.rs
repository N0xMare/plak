//! Same-user transport for the independently frozen job carrier. The endpoint
//! never binds a native connection or interprets a foreign build's files.

use std::io;
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::time::Duration;

use plakous_runner_protocol::retention::{self as wire, Answer, Bind, Call, Hello, Origin, Reply};
use serde::{Serialize, de::DeserializeOwned};
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};
use tokio::net::{UnixListener, UnixStream};

use crate::RunnerService;

const IO_BOUND: Duration = Duration::from_secs(30);

mod codec;
mod loss;
pub(crate) mod registry;
use codec::Codec;
pub(crate) use loss::confirmed as confirmed_loss;
pub(crate) use loss::link_native_lock;

pub(crate) fn native_owner(owner: &wire::Owner) -> plakous_runner_protocol::LineOwner {
    plakous_runner_protocol::LineOwner {
        line: owner.line.as_str().into(),
        generation: owner.generation,
        branch: owner.branch.as_str().into(),
        fence: owner.fence,
    }
}

pub(crate) struct Endpoint {
    pub(crate) listener: UnixListener,
    path: PathBuf,
    identity: std::fs::Metadata,
    pub(crate) origin: Origin,
}

impl Endpoint {
    pub(crate) async fn bind(directory: &Path, service: &RunnerService) -> io::Result<Self> {
        private_parent(directory)?;
        let path = directory.join(format!("retention-v{}.sock", wire::VERSION));
        crate::daemon::check_socket_path(&path)?;
        // The native socket lock is held before this function; only its owner
        // may remove a stale endpoint, and only within its own build directory.
        match std::fs::symlink_metadata(&path) {
            Ok(metadata) => {
                check_socket(&metadata)?;
                match tokio::time::timeout(Duration::from_millis(250), UnixStream::connect(&path))
                    .await
                {
                    Ok(Err(error))
                        if matches!(
                            error.kind(),
                            io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound
                        ) =>
                    {
                        std::fs::remove_file(&path)?;
                    }
                    Ok(Ok(_)) => {
                        return Err(io::Error::new(
                            io::ErrorKind::AddrInUse,
                            "retention endpoint is live",
                        ));
                    }
                    Ok(Err(error)) => return Err(error),
                    Err(error) => return Err(timed_out(error)),
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        let listener = UnixListener::bind(&path)?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        let identity = std::fs::symlink_metadata(&path)?;
        let origin = Origin {
            build: wire::Id::try_from(crate::daemon::triple_key(&service.hello().version))
                .map_err(invalid)?,
            version: wire::VERSION,
            instance: wire::Id::try_from(service.hello().instance.clone()).map_err(invalid)?,
        };
        service.retention_directory(directory, origin.clone())?;
        Ok(Self {
            listener,
            path,
            identity,
            origin,
        })
    }
}

impl Drop for Endpoint {
    fn drop(&mut self) {
        if std::fs::symlink_metadata(&self.path)
            .is_ok_and(|now| now.dev() == self.identity.dev() && now.ino() == self.identity.ino())
        {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

pub(crate) fn private_parent(directory: &Path) -> io::Result<()> {
    for parent in [
        directory.parent().and_then(Path::parent),
        directory.parent(),
        Some(directory),
    ]
    .into_iter()
    .flatten()
    {
        let metadata = std::fs::symlink_metadata(parent)?;
        if !metadata.is_dir()
            || metadata.uid() != rustix::process::geteuid().as_raw()
            || metadata.mode() & 0o077 != 0
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "retention parents must be owned private directories, not links",
            ));
        }
    }
    Ok(())
}

fn check_socket(metadata: &std::fs::Metadata) -> io::Result<()> {
    use std::os::unix::fs::FileTypeExt as _;
    if !metadata.file_type().is_socket()
        || metadata.uid() != rustix::process::geteuid().as_raw()
        || metadata.mode() & 0o077 != 0
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "retention endpoint must be an owned private socket",
        ));
    }
    Ok(())
}

pub(crate) async fn serve(
    service: RunnerService,
    stream: UnixStream,
    origin: Origin,
) -> io::Result<()> {
    serve_current(service, stream, origin, wire::VERSION).await
}

async fn serve_current(
    service: RunnerService,
    mut stream: UnixStream,
    origin: Origin,
    current: u32,
) -> io::Result<()> {
    check_peer_uid(
        stream.peer_cred()?.uid(),
        rustix::process::geteuid().as_raw(),
    )?;
    send(
        &mut stream,
        &Hello {
            origin: origin.clone(),
        },
    )
    .await?;
    let scope: Bind = receive(&mut stream).await?;
    let Ok(codec) = Codec::select(scope.version, current) else {
        send(
            &mut stream,
            &Answer::Refused {
                reason: wire::Refusal::IncompatibleVersion,
            },
        )
        .await?;
        return Ok(());
    };
    send(&mut stream, &Answer::Ok {}).await?;
    loop {
        let call = match codec.read_call(&mut stream).await {
            Ok(call) => call,
            Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(error) => return Err(error),
        };
        let id = call.id.clone();
        // Do not race the request against disconnect or cancel a dispatched
        // blocking job control. Its original-worker journal owns the answer.
        let answer = service.retention_call(&scope, call, &origin).await;
        codec
            .write_reply(&mut stream, &Reply { id, answer })
            .await?;
    }
}

pub(crate) struct Client {
    stream: UnixStream,
    pub(crate) origin: Origin,
    codec: Codec,
}

impl Client {
    pub(crate) async fn inspect(
        builds: &Path,
        build: &wire::Id,
        scope: &Bind,
        version: u32,
        current: u32,
    ) -> io::Result<Self> {
        Self::open(builds, build, scope, version, None, current).await
    }

    pub(crate) async fn connect(builds: &Path, origin: &Origin, scope: &Bind) -> io::Result<Self> {
        if !wire::admits(origin.version) {
            return Err(invalid(
                "finish/consume/disown jobs through a retained compatible build, then retry",
            ));
        }
        Self::open(
            builds,
            &origin.build,
            scope,
            origin.version,
            Some(origin),
            wire::VERSION,
        )
        .await
    }

    async fn open(
        builds: &Path,
        build: &wire::Id,
        scope: &Bind,
        version: u32,
        expected: Option<&Origin>,
        current: u32,
    ) -> io::Result<Self> {
        let codec = Codec::select(version, current)?;
        if !build_key(build.as_str()) {
            return Err(invalid("invalid build directory"));
        }
        let directory = builds.join(build.as_str());
        private_parent(&directory)?;
        let path = directory.join(format!("retention-v{version}.sock"));
        check_socket(&std::fs::symlink_metadata(&path)?)?;
        let mut stream = tokio::time::timeout(IO_BOUND, UnixStream::connect(path))
            .await
            .map_err(timed_out)??;
        check_peer_uid(
            stream.peer_cred()?.uid(),
            rustix::process::geteuid().as_raw(),
        )?;
        let hello: Hello = receive(&mut stream).await?;
        if hello.origin.build != *build || hello.origin.version != version {
            return Err(invalid("retention version or build mismatch"));
        }
        if expected.is_some_and(|origin| hello.origin != *origin) {
            return Err(loss::error());
        }
        let mut scope = scope.clone();
        scope.version = version;
        send(&mut stream, &scope).await?;
        if receive::<_, Answer>(&mut stream).await? != (Answer::Ok {}) {
            return Err(invalid("retention bind refused"));
        }
        Ok(Self {
            stream,
            origin: hello.origin,
            codec,
        })
    }

    pub(crate) async fn call(&mut self, call: &Call) -> io::Result<Answer> {
        if call.instance != self.origin.instance {
            return Err(invalid("original drainer instance was lost"));
        }
        self.codec.write_call(&mut self.stream, call).await?;
        let reply = self.codec.read_reply(&mut self.stream).await?;
        if reply.id != call.id {
            return Err(invalid("retention reply identity mismatch"));
        }
        Ok(reply.answer)
    }
}

fn check_peer_uid(peer: u32, owner: u32) -> io::Result<()> {
    if peer != owner {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "retention peer uid mismatch",
        ));
    }
    Ok(())
}

pub(crate) fn build_key(text: &str) -> bool {
    text.starts_with('v')
        && text.len() <= 64
        && text.contains('-')
        && text
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

pub(crate) async fn send<W: AsyncWrite + Unpin>(
    writer: &mut W,
    value: &impl Serialize,
) -> io::Result<()> {
    let body = serde_json::to_vec(value).map_err(invalid)?;
    if body.len() > wire::MAX_FRAME_BYTES {
        return Err(invalid("retention frame exceeds its bound"));
    }
    let length = u32::try_from(body.len()).map_err(invalid)?.to_be_bytes();
    tokio::time::timeout(IO_BOUND, async {
        writer.write_all(&length).await?;
        writer.write_all(&body).await?;
        writer.flush().await
    })
    .await
    .map_err(timed_out)?
}

pub(crate) async fn receive<R: AsyncRead + Unpin, T: DeserializeOwned>(
    reader: &mut R,
) -> io::Result<T> {
    tokio::time::timeout(IO_BOUND, async {
        let length = reader.read_u32().await?;
        let length = usize::try_from(length).map_err(invalid)?;
        if length > wire::MAX_FRAME_BYTES {
            return Err(invalid("retention frame exceeds its bound"));
        }
        let mut bytes = vec![0; length];
        reader.read_exact(&mut bytes).await?;
        serde_json::from_slice(&bytes).map_err(invalid)
    })
    .await
    .map_err(timed_out)?
}

fn invalid(error: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error.to_string())
}
fn timed_out(error: tokio::time::error::Elapsed) -> io::Error {
    io::Error::new(io::ErrorKind::TimedOut, error)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "private endpoint fixtures")]
mod tests;
