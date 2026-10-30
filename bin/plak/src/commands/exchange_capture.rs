//! Automatic private maintainer model-exchange capture; explicit test-hooks capture.

use plakous::exchange_observation::ExchangeObserver;
use std::sync::{Arc, OnceLock};

pub(super) fn from_environment(
    home: &std::path::Path,
) -> Result<Option<Arc<dyn ExchangeObserver>>, String> {
    // CLI services and TUI reattachments construct roots separately. They must
    // share one process sequence rather than collide with earlier file names.
    static CAPTURE: OnceLock<Result<Option<Arc<dyn ExchangeObserver>>, String>> = OnceLock::new();
    let capture = CAPTURE
        .get_or_init(|| {
            let dir = std::env::var_os("PLAK_TEST_EXCHANGE_CAPTURE_DIR");
            #[cfg(not(any(feature = "maintainer", feature = "test-hooks")))]
            {
                let _ = home;
                if dir.is_some() {
                    Err(
                        "private exchange capture requires a maintainer or test-hooks build"
                            .to_owned(),
                    )
                } else {
                    Ok(None)
                }
            }
            #[cfg(any(feature = "maintainer", feature = "test-hooks"))]
            {
                #[cfg(not(feature = "maintainer"))]
                if dir.is_none() {
                    return Ok(None);
                }
                Ok(Some(Arc::new(enabled::LazyCapture::new(
                    home.to_path_buf(),
                    dir.map(std::path::PathBuf::from),
                )) as Arc<dyn ExchangeObserver>))
            }
        })
        .clone()?;
    Ok(Some(Arc::new(LookupObserver(capture))))
}

struct LookupObserver(Option<Arc<dyn ExchangeObserver>>);

impl ExchangeObserver for LookupObserver {
    fn required(&self) -> bool {
        self.0.as_ref().is_some_and(|capture| capture.required())
    }

    fn start(
        &self,
        body: &[u8],
    ) -> Option<Box<dyn plakous::exchange_observation::ExchangeObservation>> {
        self.0.as_ref()?.start(body)
    }

    fn lookup_retry(&self, attempt: u8, cause: &str) {
        tracing::warn!(target: "plak.lookup", attempt, cause, "retrying model lookup");
        crate::lookup_status::retry(attempt, cause);
    }

    fn lookup_finished(&self) {
        crate::lookup_status::finished();
    }
}

#[cfg(any(feature = "maintainer", feature = "test-hooks"))]
mod enabled {
    use plakous::exchange_observation::{
        ConnectionUse, ExchangeObservation, ExchangeObserver, ExchangeTerminal,
    };
    use std::collections::{BTreeMap, BTreeSet};
    use std::fs::{self, File, OpenOptions};
    use std::io::{Seek as _, SeekFrom, Write as _};
    use std::os::unix::fs::DirBuilderExt as _;
    use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _};
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex, OnceLock};

    const SEGMENT_EXCHANGES: usize = 128;
    const MAX_RUN_BYTES: usize = 1024 * 1024 * 1024;
    #[cfg(any(feature = "maintainer", test))]
    const RETAIN_COMPLETED_RUNS: usize = 8;
    const MAX_REQUEST_BYTES: usize = 8 * 1024 * 1024;
    const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;

    pub(super) struct LazyCapture {
        home: PathBuf,
        explicit: Option<PathBuf>,
        capture: OnceLock<Option<Capture>>,
    }

    impl LazyCapture {
        pub(super) fn new(home: PathBuf, explicit: Option<PathBuf>) -> Self {
            Self {
                home,
                explicit,
                capture: OnceLock::new(),
            }
        }
    }

    impl ExchangeObserver for LazyCapture {
        fn required(&self) -> bool {
            self.explicit.is_some()
        }

        fn start(&self, request_body: &[u8]) -> Option<Box<dyn ExchangeObservation>> {
            self.capture.get_or_init(|| {
                let capture = match &self.explicit {
                    Some(dir) => Capture::open(dir, &Path::new(env!("CARGO_MANIFEST_DIR"))
                        .join("../../tests/provider-authority")),
                    None => {
                        #[cfg(feature = "maintainer")]
                        { Capture::automatic(&self.home) }
                        #[cfg(not(feature = "maintainer"))]
                        { let _ = &self.home; return None; }
                    }
                };
                match capture {
                    Ok(capture) => {
                        tracing::warn!(target: "plak.capture", directory = %capture.directory().display(),
                            process_id = std::process::id(), required = capture.required(),
                            "private inference capture selected");
                        Some(capture)
                    }
                    Err(error) => {
                        tracing::warn!(target: "plak.capture", %error, required = self.required(),
                            "private capture unavailable");
                        None
                    }
                }
            }).as_ref()?.start(request_body)
        }
    }

    #[cfg(all(test, feature = "maintainer"))]
    #[test]
    fn selecting_observer_is_lazy_and_first_exchange_selects_one_run() {
        let home = tempfile::tempdir().unwrap();
        let observer = super::from_environment(home.path()).unwrap().unwrap();
        assert!(!observer.required());
        let root = home.path().join("wire-captures");
        assert!(!root.exists(), "selecting an observer must create nothing");
        let mut first = observer.start(b"synthetic request").unwrap();
        first.terminal(&ExchangeTerminal::Complete);
        let again = super::from_environment(home.path()).unwrap().unwrap();
        let mut second = again.start(b"another synthetic request").unwrap();
        second.terminal(&ExchangeTerminal::Complete);
        let runs = fs::read_dir(root)
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(
            runs.len(),
            1,
            "roots and reattachments share the capture run"
        );
        let process = runs[0].path().join("process/segment-0000");
        assert!(process.join("exchange-0001.request").exists());
        assert!(process.join("exchange-0002.request").exists());
    }

    #[derive(Default)]
    struct Budget {
        bytes: usize,
        current: usize,
        segments: BTreeMap<usize, Segment>,
        removed: Vec<(usize, usize)>,
    }

    #[derive(Default)]
    struct Segment {
        bytes: usize,
        active: usize,
        protected: BTreeSet<usize>,
        exchanges: BTreeMap<usize, usize>,
        first: usize,
        last: usize,
    }

    pub(super) struct Capture {
        dir: PathBuf,
        next: Mutex<usize>,
        required: bool,
        // Locks protect live runs (including exchanges outliving their observer).
        run: Option<Arc<File>>,
        reserved: Arc<Mutex<Budget>>,
        max_run_bytes: usize,
        segment_exchanges: usize,
        diagnostics: Mutex<Diagnostics>,
    }

    #[derive(Default)]
    struct Diagnostics {
        reasons: BTreeMap<&'static str, ReasonDiagnostics>,
    }

    impl Diagnostics {
        fn report(&mut self, reason: &'static str, now: std::time::Instant) -> Option<u64> {
            self.reasons.entry(reason).or_default().report(now)
        }

        fn pending(&mut self) -> Vec<(&'static str, u64)> {
            self.reasons
                .iter_mut()
                .filter_map(|(&reason, state)| {
                    let count = std::mem::take(&mut state.suppressed);
                    (count > 0).then_some((reason, count))
                })
                .collect()
        }
    }

    #[derive(Default)]
    struct ReasonDiagnostics {
        last: Option<std::time::Instant>,
        suppressed: u64,
    }

    impl ReasonDiagnostics {
        fn report(&mut self, now: std::time::Instant) -> Option<u64> {
            if self
                .last
                .is_some_and(|last| now.duration_since(last).as_secs() < 60)
            {
                self.suppressed = self.suppressed.saturating_add(1);
                return None;
            }
            self.last = Some(now);
            Some(std::mem::take(&mut self.suppressed))
        }
    }

    impl Drop for Capture {
        fn drop(&mut self) {
            if let Ok(diagnostics) = self.diagnostics.get_mut() {
                for (reason, suppressed) in diagnostics.pending() {
                    tracing::warn!(target: "plak.capture", reason, required = self.required,
                        suppressed, "private wire evidence incomplete; final suppressed summary");
                }
            }
        }
    }

    impl Capture {
        #[cfg(any(feature = "maintainer", test))]
        pub(super) fn automatic(home: &Path) -> Result<Self, String> {
            let home = fs::canonicalize(home).map_err(|_| "capture home unavailable")?;
            Self::create(&home.join("wire-captures"))
        }

        #[cfg(any(feature = "maintainer", test))]
        pub(super) fn create(root: &Path) -> Result<Self, String> {
            match fs::DirBuilder::new().mode(0o700).create(root) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(_) => return Err("could not create private capture root".to_owned()),
            }
            let metadata = fs::symlink_metadata(root).map_err(|_| "capture root unavailable")?;
            if !metadata.file_type().is_dir()
                || metadata.uid() != rustix::process::geteuid().as_raw()
                || metadata.permissions().mode() & 0o777 != 0o700
            {
                return Err("capture root must be owned, non-symlink and mode 0700".to_owned());
            }
            let run = root.join(format!("plak-{}", uuid::Uuid::new_v4()));
            let dir = run.join("process");
            fs::DirBuilder::new()
                .mode(0o700)
                .create(&run)
                .map_err(|_| "could not create private capture run")?;
            fs::DirBuilder::new()
                .mode(0o700)
                .create(&dir)
                .map_err(|_| "could not create private capture process")?;
            let lock = OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(run.join("automatic-v1.lock"))
                .map_err(|_| "could not create private capture run lock")?;
            lock.lock()
                .map_err(|_| "could not lock private capture run")?;
            lock.sync_all()
                .map_err(|_| "could not persist private capture run lock")?;
            let mut capture = Self::open(&dir, root)?;
            capture.required = false;
            capture.run = Some(Arc::new(lock));
            if Self::retain(root).is_err() {
                capture.diagnose("retention failed; old evidence preserved");
            }
            Ok(capture)
        }

        #[cfg(any(feature = "maintainer", test))]
        fn retain(root: &Path) -> std::io::Result<()> {
            let mut completed = Vec::new();
            for entry in fs::read_dir(root)? {
                let entry = entry?;
                if !entry.file_type()?.is_dir()
                    || !entry.file_name().to_string_lossy().starts_with("plak-")
                {
                    continue;
                }
                let marker = entry.path().join("automatic-v1.lock");
                let Ok(meta) = fs::symlink_metadata(&marker) else {
                    continue;
                };
                if !meta.is_file()
                    || meta.uid() != rustix::process::geteuid().as_raw()
                    || meta.permissions().mode() & 0o777 != 0o600
                {
                    continue;
                }
                let file = OpenOptions::new().read(true).write(true).open(&marker)?;
                if file.try_lock().is_ok() && Self::successful_run(&entry.path())? {
                    completed.push((meta.modified()?, entry.path(), file));
                }
            }
            completed.sort_by_key(|(modified, _, _)| *modified);
            let remove = completed.len().saturating_sub(RETAIN_COMPLETED_RUNS);
            for (_, path, _lock) in completed.into_iter().take(remove) {
                fs::remove_dir_all(path)?;
            }
            File::open(root)?.sync_all()
        }

        // Inspect only bounded private metadata, never request/response bodies.
        // Unknown layouts and unsafe permissions fail closed for deletion.
        #[cfg(any(feature = "maintainer", test))]
        fn successful_run(run: &Path) -> std::io::Result<bool> {
            fn inspect(dir: &Path, depth: usize) -> std::io::Result<bool> {
                let meta = fs::symlink_metadata(dir)?;
                if !meta.is_dir()
                    || meta.uid() != rustix::process::geteuid().as_raw()
                    || meta.permissions().mode() & 0o777 != 0o700
                {
                    return Ok(false);
                }
                for entry in fs::read_dir(dir)? {
                    let entry = entry?;
                    let meta = fs::symlink_metadata(entry.path())?;
                    let name = entry.file_name();
                    let name = name.to_string_lossy();
                    if meta.is_dir() {
                        if !((depth == 0 && name == "process")
                            || (depth == 1 && name.starts_with("segment-")))
                            || !inspect(&entry.path(), depth + 1)?
                        {
                            return Ok(false);
                        }
                        continue;
                    }
                    if !meta.is_file()
                        || meta.uid() != rustix::process::geteuid().as_raw()
                        || meta.permissions().mode() & 0o777 != 0o600
                        || name == "incomplete.log"
                    {
                        return Ok(false);
                    }
                    if name.starts_with("exchange-") && name.ends_with(".info") {
                        use std::io::Read as _;
                        let mut info = String::new();
                        File::open(entry.path())?
                            .take(4097)
                            .read_to_string(&mut info)?;
                        if info.len() > 4096
                            || !entry.path().with_extension("request").try_exists()?
                            || !entry.path().with_extension("body").try_exists()?
                            || !info.lines().any(|line| {
                                matches!(line, "terminal=Complete" | "terminal=ProtocolComplete")
                            })
                            || !info.lines().any(|line| line == "incomplete=false")
                            || !info.lines().any(|line| {
                                line == "status=None"
                                    || line
                                        .strip_prefix("status=Some(")
                                        .and_then(|value| value.strip_suffix(')'))
                                        .and_then(|value| value.parse::<u16>().ok())
                                        .is_some_and(|status| status < 400)
                            })
                        {
                            return Ok(false);
                        }
                    } else if name.starts_with("exchange-")
                        && (name.ends_with(".request") || name.ends_with(".body"))
                    {
                        if !entry.path().with_extension("info").try_exists()? {
                            return Ok(false);
                        }
                    } else if !((depth == 0 && name == "automatic-v1.lock")
                        || (depth == 1 && name == "retention.info"))
                    {
                        return Ok(false);
                    }
                }
                Ok(true)
            }
            if !run.join("process").try_exists()? {
                return Ok(false);
            }
            inspect(run, 0)
        }

        fn diagnose(&self, reason: &'static str) {
            if let Ok(mut diagnostics) = self.diagnostics.lock()
                && let Some(suppressed) = diagnostics.report(reason, std::time::Instant::now())
            {
                tracing::warn!(target: "plak.capture", reason, required = self.required,
                    suppressed, "private wire evidence incomplete");
            }
            // A sticky run-level gap record complements per-exchange crash markers.
            // Never include prompts, bodies or filesystem error strings here.
            if let Ok(mut file) = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(self.dir.join("incomplete.log"))
            {
                let _ = writeln!(file, "{reason}");
                let _ = file.sync_all();
                let _ = File::open(&self.dir).and_then(|dir| dir.sync_all());
            }
        }

        pub(super) fn directory(&self) -> &Path {
            &self.dir
        }

        pub(super) fn open(dir: &Path, root: &Path) -> Result<Self, String> {
            let root_meta =
                fs::symlink_metadata(root).map_err(|_| "fixture root does not exist")?;
            let parent = dir.parent().ok_or("capture path has no parent")?;
            let run_parent = parent.parent().ok_or("capture run has no parent")?;
            let parent_meta =
                fs::symlink_metadata(parent).map_err(|_| "capture run does not exist")?;
            if !dir.is_absolute()
                || !root_meta.file_type().is_dir()
                || root_meta.uid() != rustix::process::geteuid().as_raw()
                || root_meta.permissions().mode() & 0o777 != 0o700
                || fs::canonicalize(run_parent).ok() != fs::canonicalize(root).ok()
                || !parent_meta.file_type().is_dir()
                || parent_meta.uid() != rustix::process::geteuid().as_raw()
                || parent_meta.permissions().mode() & 0o777 != 0o700
            {
                return Err(
                    "capture directory must be under a private run in tests/provider-authority"
                        .to_owned(),
                );
            }
            let meta = fs::symlink_metadata(dir).map_err(|_| "capture directory does not exist")?;
            if !meta.file_type().is_dir()
                || meta.uid() != rustix::process::geteuid().as_raw()
                || meta.permissions().mode() & 0o777 != 0o700
            {
                return Err("capture directory must be owned, non-symlink and mode 0700".to_owned());
            }
            // Sync directory links as well as the evidence files. A fresh run
            // must remain reachable after a crash once dispatch is admitted.
            for directory in [
                parent,
                root,
                root.parent().ok_or("capture root has no parent")?,
            ] {
                File::open(directory)
                    .and_then(|file| file.sync_all())
                    .map_err(|_| "could not persist private capture directories")?;
            }
            Ok(Self {
                dir: dir.to_owned(),
                next: Mutex::new(0),
                required: true,
                run: None,
                reserved: Arc::new(Mutex::new(Budget::default())),
                max_run_bytes: MAX_RUN_BYTES,
                segment_exchanges: SEGMENT_EXCHANGES,
                diagnostics: Mutex::new(Diagnostics::default()),
            })
        }

        fn reclaim(
            &self,
            budget: &mut Budget,
            current: usize,
            needed: usize,
        ) -> std::io::Result<()> {
            let candidates: Vec<_> = budget
                .segments
                .range(..current)
                .filter(|(_, segment)| {
                    segment.active == 0
                        && segment
                            .exchanges
                            .keys()
                            .any(|id| !segment.protected.contains(id))
                })
                .map(|(&id, _)| id)
                .collect();
            for id in candidates {
                if needed <= self.max_run_bytes.saturating_sub(budget.bytes) {
                    break;
                }
                let selected = &budget.segments[&id];
                let exchanges: Vec<_> = selected
                    .exchanges
                    .keys()
                    .copied()
                    .filter(|index| !selected.protected.contains(index))
                    .collect();
                budget
                    .removed
                    .extend(exchanges.iter().map(|index| (*index, *index)));
                budget.removed.sort_unstable();
                // Disjoint ranges are separated by retained (charged) segments;
                // merging prevents summary growth with the lifetime start count.
                let mut merged: Vec<(usize, usize)> = Vec::new();
                for &(first, last) in &budget.removed {
                    if let Some(range) = merged.last_mut()
                        && first <= range.1 + 1
                    {
                        range.1 = range.1.max(last);
                    } else {
                        merged.push((first, last));
                    }
                }
                budget.removed = merged;
                // Persist the omission notice before unlinking evidence. If IO
                // fails, ranges conservatively include a selected but retained segment.
                let mut summary = OpenOptions::new()
                    .write(true)
                    .create(true)
                    .truncate(true)
                    .mode(0o600)
                    .open(self.dir.join("retention.tmp"))?;
                writeln!(
                    summary,
                    "full_run=false\nselected_successful_exchange_ranges={:?}",
                    budget.removed
                )?;
                summary.sync_all()?;
                fs::rename(
                    self.dir.join("retention.tmp"),
                    self.dir.join("retention.info"),
                )?;
                File::open(&self.dir)?.sync_all()?;
                let directory = self.dir.join(format!("segment-{id:04}"));
                for index in exchanges {
                    for extension in ["request", "body", "info"] {
                        let path = directory.join(format!("exchange-{index:04}.{extension}"));
                        match fs::remove_file(path) {
                            Ok(()) => {}
                            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                            Err(error) => return Err(error),
                        }
                    }
                    if let Some(bytes) = budget
                        .segments
                        .get_mut(&id)
                        .and_then(|segment| segment.exchanges.remove(&index))
                    {
                        budget.bytes = budget.bytes.saturating_sub(bytes);
                        if let Some(segment) = budget.segments.get_mut(&id) {
                            segment.bytes = segment.bytes.saturating_sub(bytes);
                        }
                    }
                }
                File::open(&directory)?.sync_all()?;
                if budget.segments[&id].exchanges.is_empty() {
                    fs::remove_dir(&directory)?;
                    budget.segments.remove(&id);
                }
                File::open(&self.dir)?.sync_all()?;
            }
            Ok(())
        }
    }

    impl ExchangeObserver for Capture {
        fn required(&self) -> bool {
            self.required
        }

        #[allow(
            clippy::too_many_lines,
            reason = "Exchange capture initializes request metadata and both private journal streams before returning an observation handle."
        )]
        fn start(&self, request_body: &[u8]) -> Option<Box<dyn ExchangeObservation>> {
            let Ok(mut next) = self.next.lock() else {
                self.diagnose("sequence unavailable");
                return None;
            };
            if request_body.len() > MAX_REQUEST_BYTES {
                self.diagnose("request capacity exceeded");
                return None;
            }
            let mut segment = *next / self.segment_exchanges;
            // Only closed successful segments from this observer's fresh run
            // can be reclaimed. Never scan or delete pre-existing human evidence.
            if !self.required {
                let Ok(mut reserved) = self.reserved.lock() else {
                    self.diagnose("budget unavailable");
                    return None;
                };
                let bytes = request_body.len() + MAX_RESPONSE_BYTES + 4096;
                segment = reserved.current;
                if reserved
                    .segments
                    .get(&segment)
                    .is_some_and(|entry| entry.last - entry.first + 1 >= self.segment_exchanges)
                {
                    segment += 1;
                    reserved.current = segment;
                }
                if self.reclaim(&mut reserved, segment, bytes).is_err() {
                    self.diagnose("segment retention failed; exchange unrecorded");
                    return None;
                }
                // Byte pressure seals even a partially filled segment. Sequence
                // numbers stay contiguous; omission ranges use actual starts.
                if bytes > self.max_run_bytes.saturating_sub(reserved.bytes)
                    && reserved.segments.contains_key(&segment)
                {
                    segment += 1;
                    reserved.current = segment;
                    if self.reclaim(&mut reserved, segment, bytes).is_err() {
                        self.diagnose("segment retention failed; exchange unrecorded");
                        return None;
                    }
                }
                if bytes > self.max_run_bytes.saturating_sub(reserved.bytes) {
                    self.diagnose("run capacity exceeded by protected or current evidence; exchange unrecorded; inference continues");
                    return None;
                }
                reserved.bytes += bytes;
                let entry = reserved.segments.entry(segment).or_default();
                entry.bytes += bytes;
                entry.exchanges.insert(*next + 1, bytes);
                entry.active += 1;
                if entry.first == 0 {
                    entry.first = *next + 1;
                }
                entry.last = *next + 1;
            }
            *next += 1;
            let name = format!("exchange-{:04}", *next);
            let start = || -> std::io::Result<ExchangeCapture> {
                let dir = if self.required {
                    self.dir.clone()
                } else {
                    let dir = self.dir.join(format!("segment-{segment:04}"));
                    match fs::DirBuilder::new().mode(0o700).create(&dir) {
                        Ok(()) => File::open(&self.dir)?.sync_all()?,
                        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                        Err(error) => return Err(error),
                    }
                    let meta = fs::symlink_metadata(&dir)?;
                    if !meta.file_type().is_dir()
                        || meta.uid() != rustix::process::geteuid().as_raw()
                        || meta.permissions().mode() & 0o777 != 0o700
                    {
                        return Err(std::io::Error::other("capture segment is not private"));
                    }
                    dir
                };
                let create = |extension| {
                    OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .mode(0o600)
                        .open(dir.join(format!("{name}.{extension}")))
                };
                // Persist the crash marker before any request can be dispatched.
                let mut info = create("info")?;
                write!(
                    info,
                    "process_id={}\nexchange={}\nstatus=None\nterminal=pending\nrecorded_bytes=0\nincomplete=true\n",
                    std::process::id(),
                    *next
                )?;
                info.sync_all()?;
                let mut request = create("request")?;
                request.write_all(request_body)?;
                request.sync_all()?;
                let file = create("body")?;
                file.sync_all()?;
                File::open(&dir)?.sync_all()?;
                Ok(ExchangeCapture {
                    file,
                    info,
                    bytes: 0,
                    status: None,
                    connection: None,
                    failed: false,
                    _run: self.run.clone(),
                    budget: (!self.required).then(|| self.reserved.clone()),
                    exchange: *next,
                    segment,
                    complete: false,
                })
            };
            if let Ok(exchange) = start() {
                Some(Box::new(exchange))
            } else {
                if !self.required
                    && let Ok(mut budget) = self.reserved.lock()
                {
                    let entry = budget.segments.entry(segment).or_default();
                    entry.active -= 1;
                    entry.protected.insert(*next);
                }
                self.diagnose("could not persist exchange files");
                None
            }
        }
    }

    struct ExchangeCapture {
        file: File,
        info: File,
        bytes: usize,
        status: Option<u16>,
        connection: Option<ConnectionUse>,
        failed: bool,
        _run: Option<Arc<File>>,
        budget: Option<Arc<Mutex<Budget>>>,
        exchange: usize,
        segment: usize,
        complete: bool,
    }

    impl Drop for ExchangeCapture {
        fn drop(&mut self) {
            // Keep actual bytes charged, reclaim unused response reservation.
            // Failed starts conservatively retain their entire reservation.
            if let Some(budget) = &self.budget
                && let Ok(mut reserved) = budget.lock()
            {
                let unused = MAX_RESPONSE_BYTES.saturating_sub(self.bytes);
                reserved.bytes = reserved.bytes.saturating_sub(unused);
                let segment = reserved.segments.entry(self.segment).or_default();
                segment.bytes = segment.bytes.saturating_sub(unused);
                segment.active -= 1;
                if let Some(bytes) = segment.exchanges.get_mut(&self.exchange) {
                    *bytes = bytes.saturating_sub(unused);
                }
                if !self.complete || self.failed || self.status.is_some_and(|status| status >= 400)
                {
                    segment.protected.insert(self.exchange);
                }
            }
        }
    }

    impl ExchangeCapture {
        fn fail(&mut self) {
            if !self.failed {
                tracing::warn!(target: "plak.capture", "private exchange capture IO or capacity failure; evidence incomplete");
            }
            self.failed = true;
        }

        fn write_info(&mut self, terminal: ExchangeTerminal) -> std::io::Result<()> {
            let info = format!(
                "process_id={}\nexchange={}\nstatus={:?}\nconnection={:?}\nterminal={terminal:?}\nrecorded_bytes={}\nincomplete={}\n",
                std::process::id(),
                self.exchange,
                self.status,
                self.connection,
                self.file.metadata()?.len(),
                self.failed
            );
            self.info.seek(SeekFrom::Start(0))?;
            self.info.write_all(info.as_bytes())?;
            self.info.set_len(info.len() as u64)?;
            self.info.sync_all()
        }
    }

    impl ExchangeObservation for ExchangeCapture {
        fn failed(&self) -> bool {
            self.failed
        }

        fn connection(&mut self, connection: ConnectionUse) {
            self.connection = Some(connection);
        }

        fn response_head(&mut self, status: u16) {
            self.status = Some(status);
        }

        fn response_chunk(&mut self, bytes: &[u8]) {
            if self.failed {
                return;
            }
            let remaining = MAX_RESPONSE_BYTES.saturating_sub(self.bytes);
            let kept = bytes.len().min(remaining);
            if self.file.write_all(&bytes[..kept]).is_err() {
                self.fail();
            }
            self.bytes += kept;
            if kept != bytes.len() {
                self.fail();
            }
        }

        fn terminal(&mut self, terminal: &ExchangeTerminal) {
            self.complete = matches!(
                terminal,
                ExchangeTerminal::Complete | ExchangeTerminal::ProtocolComplete
            );
            if !self.complete {
                tracing::warn!(target: "plak.capture", process_id = std::process::id(),
                    exchange = self.exchange, segment = self.segment, connection = ?self.connection, ?terminal,
                    "physical inference exchange did not complete");
            }
            if self.file.sync_all().is_err() {
                self.fail();
            }
            if self.write_info(*terminal).is_err() {
                self.fail();
                // Best effort: retain the incomplete marker even if terminal sync failed.
                let _ = self.write_info(*terminal);
            }
        }
    }

    #[cfg(test)]
    mod tests;
}
