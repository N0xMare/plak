//! The web UI's static files: exactly the build's files from `--ui`, read
//! into immutable in-memory snapshots, the SPA fallback to
//! `index.html`, the cache rules, the build's CSP as a header on every
//! response, and the service-worker kill switch.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, PoisonError, RwLock};

use bytes::Bytes;
use http_body_util::Full;
use hyper::header::{self, HeaderMap, HeaderValue};
use hyper::{Method, Request, Response, StatusCode};
use plakous_hosted::error::{CliFailure, CliResult};
use serde::Deserialize;

/// The largest file the UI serves.
const MAX_FILE_BYTES: u64 = 32 * 1024 * 1024;
/// The most files, and bytes in all, a build may hold.
const MAX_FILES: usize = 4096;
const MAX_BUILD_BYTES: u64 = 128 * 1024 * 1024;
/// The one path a service worker may be registered from.
const SERVICE_WORKER: &str = "/service-worker.js";
/// What a worker registered from any path the build does not hold is
/// answered with: it unregisters itself and reloads its pages.
const KILL_SWITCH: &str = "self.addEventListener('install', () => self.skipWaiting());\n\
self.addEventListener('activate', (event) => {\n\
  event.waitUntil(self.registration.unregister()\n\
    .then(() => self.clients.matchAll())\n\
    .then((clients) => clients.forEach((client) => client.navigate(client.url))));\n\
});\n";
/// Nothing the app does not use.
const PERMISSIONS_POLICY: &str = "accelerometer=(), camera=(), geolocation=(), gyroscope=(), \
magnetometer=(), microphone=(), payment=(), usb=()";

/// One file of the build, in memory.
struct File {
    bytes: Bytes,
    content_type: HeaderValue,
}

/// A loaded UI build: its files by URL path and its CSP.
pub(crate) struct Ui {
    files: BTreeMap<String, File>,
    csp: HeaderValue,
    csp_report_only: Option<HeaderValue>,
}

/// A validated build. Each request retains one snapshot for both files and CSP.
pub(crate) struct Builds {
    root: PathBuf,
    current: RwLock<Arc<Ui>>,
}

impl Builds {
    pub(crate) fn load(root: &Path) -> CliResult<Self> {
        Ok(Self {
            root: root.to_path_buf(),
            current: RwLock::new(Arc::new(Ui::load(root)?)),
        })
    }

    pub(crate) fn snapshot(&self) -> Arc<Ui> {
        Arc::clone(&self.current.read().unwrap_or_else(PoisonError::into_inner))
    }

    pub(crate) fn reload(&self) -> CliResult<()> {
        let next = Arc::new(Ui::load(&self.root)?);
        *self.current.write().unwrap_or_else(PoisonError::into_inner) = next;
        Ok(())
    }
}

/// `csp.json` in the UI directory, as the web build writes it.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CspFile {
    header: String,
    #[serde(default)]
    report_only_header: Option<String>,
}

impl Ui {
    /// Loads the build at `root`: every regular file under it whose path has
    /// no component starting with `.`, and its `csp.json`.
    ///
    /// # Errors
    ///
    /// A directory that does not exist; a missing or malformed CSP, or one
    /// that allows framing; a build past its file or byte caps.
    pub(crate) fn load(root: &Path) -> CliResult<Self> {
        let root = std::fs::canonicalize(root).map_err(|_| {
            CliFailure::new(format!(
                "plak host --web: no UI directory {}",
                root.display()
            ))
        })?;
        let text = std::fs::read_to_string(root.join("csp.json")).map_err(|_| {
            CliFailure::new(
                "plak host --web: the UI directory has no csp.json; serve a complete build",
            )
        })?;
        let file: CspFile = serde_json::from_str(&text).map_err(|_| {
            CliFailure::new("plak host --web: csp.json is not {\"header\": \"...\"}")
        })?;
        if !forbids_framing(&file.header) {
            return Err(CliFailure::new(
                "plak host --web: csp.json's header must carry frame-ancestors 'none'",
            ));
        }
        let value = |text: &str| {
            HeaderValue::from_str(text).map_err(|_| {
                CliFailure::new("plak host --web: csp.json holds a header that is not valid")
            })
        };
        let mut files = BTreeMap::new();
        let mut total = 0_u64;
        collect(&root, &root, &mut files, &mut total)?;
        Ok(Self {
            csp: value(&file.header)?,
            csp_report_only: file.report_only_header.as_deref().map(value).transpose()?,
            files,
        })
    }

    fn file(&self, path: &str) -> Option<&File> {
        self.files.get(path)
    }
}

/// Whether a CSP header carries `frame-ancestors 'none'`.
fn forbids_framing(csp: &str) -> bool {
    csp.split(';').any(|directive| {
        let mut words = directive.split_ascii_whitespace();
        words
            .next()
            .is_some_and(|name| name.eq_ignore_ascii_case("frame-ancestors"))
            && words.next() == Some("'none'")
            && words.next().is_none()
    })
}

/// Reads every servable file under `directory` into `files`, keyed by its
/// URL path. Dot entries are skipped; a symlink is followed only to a file
/// inside `root`.
fn collect(
    root: &Path,
    directory: &Path,
    files: &mut BTreeMap<String, File>,
    total: &mut u64,
) -> CliResult<()> {
    let unreadable = || {
        CliFailure::new(format!(
            "plak host --web: cannot read {}",
            directory.display()
        ))
    };
    let mut entries: Vec<PathBuf> = std::fs::read_dir(directory)
        .map_err(|_| unreadable())?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<Result<_, _>>()
        .map_err(|_| unreadable())?;
    entries.sort();
    for path in entries {
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if name.starts_with('.') {
            continue;
        }
        let Ok(resolved) = std::fs::canonicalize(&path) else {
            continue;
        };
        if !resolved.starts_with(root) {
            continue;
        }
        if resolved.is_dir() {
            if path.is_dir() && !path.is_symlink() {
                collect(root, &path, files, total)?;
            }
            continue;
        }
        let Ok(relative) = path.strip_prefix(root) else {
            continue;
        };
        let Some(relative) = relative.to_str() else {
            continue;
        };
        let size = std::fs::metadata(&resolved)
            .map_err(|_| unreadable())?
            .len();
        *total = total.saturating_add(size);
        if size > MAX_FILE_BYTES || *total > MAX_BUILD_BYTES || files.len() >= MAX_FILES {
            return Err(CliFailure::new(format!(
                "plak host --web: the UI build is past its caps ({MAX_FILES} files, \
                 {MAX_FILE_BYTES} bytes a file, {MAX_BUILD_BYTES} in all)"
            )));
        }
        let bytes = std::fs::read(&resolved).map_err(|_| unreadable())?;
        files.insert(
            format!("/{relative}"),
            File {
                bytes: Bytes::from(bytes),
                content_type: content_type(&path),
            },
        );
    }
    Ok(())
}

/// The headers every response carries, the CSP when a UI is loaded.
pub(crate) fn security_headers(headers: &mut HeaderMap, ui: Option<&Ui>) {
    if let Some(ui) = ui {
        headers.insert(header::CONTENT_SECURITY_POLICY, ui.csp.clone());
        if let Some(report_only) = &ui.csp_report_only {
            headers.insert(
                header::CONTENT_SECURITY_POLICY_REPORT_ONLY,
                report_only.clone(),
            );
        }
    }
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    headers.insert(
        "cross-origin-opener-policy",
        HeaderValue::from_static("same-origin"),
    );
    headers.insert(
        "cross-origin-resource-policy",
        HeaderValue::from_static("same-origin"),
    );
    headers.insert(
        "permissions-policy",
        HeaderValue::from_static(PERMISSIONS_POLICY),
    );
    headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
}

/// Answers a `GET` or `HEAD` for a static path.
pub(crate) fn respond<B>(ui: Option<&Ui>, request: &Request<B>) -> Response<Full<Bytes>> {
    let head = request.method() == Method::HEAD;
    let path = request.uri().path();
    let Some(ui) = ui else {
        return plain(StatusCode::NOT_FOUND, "no UI is served\n", None, head);
    };
    let worker = request
        .headers()
        .get("service-worker")
        .is_some_and(|value| value.as_bytes() == b"script");
    // A worker may be registered only from the build's own worker file;
    // any other script a worker asks for unregisters it.
    if worker && (path != SERVICE_WORKER || ui.file(path).is_none()) {
        return kill_switch(ui, head);
    }
    let (file, fallback) = match ui.file(path) {
        Some(file) => (file, false),
        // A missing build asset is missing; only app routes fall back.
        None if path.starts_with("/_app/") => {
            return plain(StatusCode::NOT_FOUND, "not found\n", Some(ui), head);
        }
        None => match ui.file("/index.html") {
            Some(index) => (index, true),
            None => {
                return plain(
                    StatusCode::NOT_FOUND,
                    "the UI has no index.html\n",
                    Some(ui),
                    head,
                );
            }
        },
    };
    let body = if head {
        Bytes::new()
    } else {
        file.bytes.clone()
    };
    let mut response = Response::new(Full::new(body));
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, file.content_type.clone());
    let immutable = !fallback && path.starts_with("/_app/immutable/");
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(if immutable {
            "public, max-age=31536000, immutable"
        } else {
            "no-cache"
        }),
    );
    security_headers(headers, Some(ui));
    response
}

fn kill_switch(ui: &Ui, head: bool) -> Response<Full<Bytes>> {
    let body = if head {
        Bytes::new()
    } else {
        Bytes::from_static(KILL_SWITCH.as_bytes())
    };
    let mut response = Response::new(Full::new(body));
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/javascript; charset=utf-8"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(
        "clear-site-data",
        HeaderValue::from_static("\"cache\", \"storage\""),
    );
    security_headers(headers, Some(ui));
    response
}

fn plain(
    status: StatusCode,
    text: &'static str,
    ui: Option<&Ui>,
    head: bool,
) -> Response<Full<Bytes>> {
    let body = if head {
        Bytes::new()
    } else {
        Bytes::from_static(text.as_bytes())
    };
    let mut response = Response::new(Full::new(body));
    *response.status_mut() = status;
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    security_headers(headers, ui);
    response
}

fn content_type(file: &Path) -> HeaderValue {
    let extension = file
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or("");
    HeaderValue::from_static(match extension {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" => "application/json",
        "webmanifest" => "application/manifest+json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "ico" => "image/x-icon",
        "woff2" => "font/woff2",
        "woff" => "font/woff",
        "txt" => "text/plain; charset=utf-8",
        "wasm" => "application/wasm",
        _ => "application/octet-stream",
    })
}
