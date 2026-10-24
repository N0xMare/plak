//! Append-only session event log (agent-readable JSONL).

use std::cell::RefCell;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::session_home::SessionHome;
use crate::sessions::canonical_product_session_id;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

const REVERSE_CHUNK_BYTES: usize = 8 * 1024;
const MAX_SEQUENCE_RECOVERY_BYTES: usize = 64 * 1024;
const MAX_PHYSICAL_RECORD_BYTES: usize = 64 * 1024;
const MAX_REVERSE_SCAN_BYTES: usize = 8 * 1024 * 1024;
const EVENT_COUNT_FORMAT_REVISION: u64 = 3;

/// Longest span of appends that may run without persisting the event-count
/// projection. The log is an observational journal, never recovery authority,
/// so a stale sidecar is a performance event: `count_events`
/// detects the byte-length mismatch and rebuilds by a bounded rescan.
pub const SESSION_LOG_INDEX_INTERVAL: Duration = Duration::from_secs(2);

/// Longest wait for another handle's log lock. A holder keeps it only for one
/// append or sidecar write, but a holder suspended inside that window (a
/// stopped process past its lease) keeps it until it resumes. An unbounded
/// wait would hang the next owner's open on an observational journal.
const LOG_LOCK_WAIT: Duration = Duration::from_secs(2);

/// One structured session event (serialized as a single JSON object line).
#[derive(Clone, Debug)]
pub struct SessionEvent {
    pub kind: String,
    pub fields: Vec<(String, JsonVal)>,
}

#[derive(Clone, Debug)]
pub enum JsonVal {
    Null,
    Bool(bool),
    U64(u64),
    Str(String),
}

impl SessionEvent {
    pub fn new(kind: impl Into<String>) -> Self {
        Self {
            kind: kind.into(),
            fields: Vec::new(),
        }
    }

    pub fn str(mut self, key: &str, value: impl Into<String>) -> Self {
        self.fields
            .push((key.to_owned(), JsonVal::Str(value.into())));
        self
    }

    #[must_use]
    pub fn opt_str(self, key: &str, value: Option<&str>) -> Self {
        match value {
            Some(v) => self.str(key, v),
            None => self.null(key),
        }
    }

    #[must_use]
    pub fn bool(mut self, key: &str, value: bool) -> Self {
        self.fields.push((key.to_owned(), JsonVal::Bool(value)));
        self
    }

    #[must_use]
    pub fn u64(mut self, key: &str, value: u64) -> Self {
        self.fields.push((key.to_owned(), JsonVal::U64(value)));
        self
    }

    #[must_use]
    pub fn null(mut self, key: &str) -> Self {
        self.fields.push((key.to_owned(), JsonVal::Null));
        self
    }

    fn to_json_line(&self, seq: u64, ts: u64) -> Result<Vec<u8>, String> {
        let mut object = Map::new();
        object.insert("ts".to_owned(), Value::from(ts));
        object.insert("seq".to_owned(), Value::from(seq));
        object.insert("kind".to_owned(), Value::String(bounded_string(&self.kind)));
        for (k, v) in &self.fields {
            if matches!(k.as_str(), "ts" | "seq" | "kind") {
                continue;
            }
            let value = match v {
                JsonVal::Null => Value::Null,
                JsonVal::Bool(value) => Value::Bool(*value),
                JsonVal::U64(value) => Value::from(*value),
                JsonVal::Str(value) => Value::String(bounded_string(value)),
            };
            object.insert(k.clone(), value);
        }
        let encoded =
            serde_json::to_vec(&object).map_err(|_| "could not encode session event".to_owned())?;
        if encoded.len().saturating_add(1) > MAX_PHYSICAL_RECORD_BYTES {
            return Err("session event exceeds physical record limit".to_owned());
        }
        Ok(encoded)
    }
}

struct WriterState {
    session_id: String,
    file: File,
    next_seq: u64,
    event_count: u64,
    log_bytes: u64,
    needs_line_separator: bool,
    /// Set when `event_count` has advanced past the persisted sidecar.
    index_dirty: bool,
    last_index_write: Instant,
}

/// The event-count sidecar.
///
/// Length and last sequence bind the projection to its observed log prefix.
/// Revision 3 invalidates counts written before independent append handles
/// coordinated sequence allocation; those handles could publish duplicate
/// sequences and an apparently matching, incorrect count.
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct EventCountIndex {
    format_revision: u64,
    log_bytes: u64,
    event_count: u64,
    /// Sequence of the last record the writer counted.
    last_sequence: u64,
}

/// Host-owned appender. It retains one append handle for the attached session.
/// Switching sessions replaces that handle; it never creates ambient authority.
/// Independent handles and processes coordinate through the log file's lock.
pub struct SessionLog {
    home: SessionHome,
    state: RefCell<WriterState>,
}

impl SessionLog {
    pub fn open(home: &SessionHome, session_id: &str) -> Result<Self, String> {
        canonical_product_session_id(session_id)?;
        home.ensure()?;
        Ok(Self {
            home: home.clone(),
            state: RefCell::new(open_state(home, session_id)?),
        })
    }

    pub fn append(&self, event: SessionEvent) -> Result<(), String> {
        let session_id = self.state.borrow().session_id.clone();
        self.append_for(&session_id, event)
    }

    /// Persist the event-count projection now. Called on close and on session
    /// switch so an idle log always leaves an exact sidecar behind.
    pub fn flush(&self) -> Result<(), String> {
        flush_writer(&self.home, &mut self.state.borrow_mut())
    }

    /// Append for the host's current attached session, replacing the retained
    /// handle when an explicit product session switch has occurred.
    ///
    /// The record is written and the call returns: no `sync_all` and no
    /// sidecar rename happen on this path. The count index is advanced in
    /// memory and persisted at most once per `SESSION_LOG_INDEX_INTERVAL`.
    ///
    pub fn append_for(&self, session_id: &str, event: SessionEvent) -> Result<(), String> {
        canonical_product_session_id(session_id)?;
        let mut state = self.state.borrow_mut();
        if state.session_id != session_id {
            let _ = flush_writer(&self.home, &mut state);
            *state = open_state(&self.home, session_id)?;
        }
        let lock = LogFileLock::acquire(&state.file)?;
        refresh_writer_state(&mut state)?;
        let seq = state
            .next_seq
            .checked_add(1)
            .ok_or_else(|| "session event sequence exhausted".to_owned())?;
        let event_count = state
            .event_count
            .checked_add(1)
            .ok_or_else(|| "session event count exhausted".to_owned())?;
        let line = event.to_json_line(seq, unix_ms())?;
        let appended_bytes = u64::try_from(line.len())
            .map_err(|_| "session event size overflow".to_owned())?
            + 1
            + u64::from(state.needs_line_separator);
        if state.needs_line_separator {
            state
                .file
                .write_all(b"\n")
                .map_err(|_| "could not write session event".to_owned())?;
            state.needs_line_separator = false;
        }
        state
            .file
            .write_all(&line)
            .and_then(|()| state.file.write_all(b"\n"))
            .map_err(|_| "could not write session event".to_owned())?;
        state.next_seq = seq;
        state.event_count = event_count;
        state.log_bytes = state.log_bytes.saturating_add(appended_bytes);
        state.index_dirty = true;
        if state.last_index_write.elapsed() >= SESSION_LOG_INDEX_INTERVAL {
            let pending = pending_event_count_index(&state);
            drop(lock);
            if let Ok(Some(index)) = pending {
                let _ = publish_event_count_index(&self.home, &mut state, &index);
            }
        }
        Ok(())
    }
}

impl Drop for SessionLog {
    fn drop(&mut self) {
        let _ = flush_writer(&self.home, &mut self.state.borrow_mut());
    }
}

/// The cloned descriptor shares the retained file's lock identity. No lock is
/// retained across calls, session switches, awaits, or external work.
struct LogFileLock(File);

impl LogFileLock {
    fn acquire(file: &File) -> Result<Self, String> {
        let file = file
            .try_clone()
            .map_err(|_| "could not clone session event log".to_owned())?;
        let deadline = Instant::now() + LOG_LOCK_WAIT;
        let mut pause = Duration::from_millis(1);
        loop {
            match file.try_lock() {
                Ok(()) => return Ok(Self(file)),
                Err(std::fs::TryLockError::WouldBlock) if Instant::now() < deadline => {
                    std::thread::sleep(pause);
                    pause = (pause * 2).min(Duration::from_millis(50));
                }
                Err(std::fs::TryLockError::WouldBlock) => {
                    return Err("session event log is busy".to_owned());
                }
                Err(std::fs::TryLockError::Error(_)) => {
                    return Err("could not lock session event log".to_owned());
                }
            }
        }
    }

    fn try_acquire(file: &File) -> Result<Self, String> {
        let file = file
            .try_clone()
            .map_err(|_| "could not clone session event log".to_owned())?;
        file.try_lock()
            .map_err(|_| "session event log is busy".to_owned())?;
        Ok(Self(file))
    }
}

impl Drop for LogFileLock {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

/// The sidecar is computed under the log lock and written after it is
/// released: its `sync_all` and rename never hold up another appender. A
/// sidecar published late is still exact for the prefix it names, and a
/// reader that finds the log longer rebuilds it.
fn flush_writer(home: &SessionHome, state: &mut WriterState) -> Result<(), String> {
    let pending = {
        let _lock = LogFileLock::acquire(&state.file)?;
        refresh_writer_state(state)?;
        pending_event_count_index(state)?
    };
    match pending {
        Some(index) => publish_event_count_index(home, state, &index),
        None => Ok(()),
    }
}

fn refresh_writer_state(state: &mut WriterState) -> Result<(), String> {
    let log_bytes = state
        .file
        .metadata()
        .map_err(|_| "could not inspect session event log".to_owned())?
        .len();
    if state.log_bytes == log_bytes {
        return Ok(());
    }
    if log_bytes < state.log_bytes {
        return Err("session event log was truncated while attached".to_owned());
    }
    // Catch up only the unseen append suffix. In particular this does not
    // rebuild/fsync a sidecar on every alternating writer's append.
    state
        .file
        .seek(SeekFrom::Start(state.log_bytes))
        .map_err(|_| "could not inspect session event log".to_owned())?;
    let added = count_valid_records(&mut BufReader::new(&mut state.file))?;
    let next_seq = recover_last_sequence(&mut state.file)?;
    let event_count = state
        .event_count
        .checked_add(added)
        .ok_or_else(|| "session event count exhausted".to_owned())?;
    state.needs_line_separator = needs_line_separator(&mut state.file, log_bytes)?;
    state.next_seq = next_seq;
    state.event_count = event_count;
    state.log_bytes = log_bytes;
    state.index_dirty = true;
    Ok(())
}

/// The in-memory count against the log's current byte length, taken under
/// the log lock; `None` when nothing changed since the last write. The length
/// is read here rather than tracked per append so a partially written record
/// can never leave a plausible-but-wrong sidecar behind.
fn pending_event_count_index(state: &WriterState) -> Result<Option<EventCountIndex>, String> {
    if !state.index_dirty {
        return Ok(None);
    }
    let log_bytes = state
        .file
        .metadata()
        .map(|metadata| metadata.len())
        .map_err(|_| "could not inspect session event log".to_owned())?;
    Ok(Some(EventCountIndex {
        format_revision: EVENT_COUNT_FORMAT_REVISION,
        log_bytes,
        event_count: state.event_count,
        last_sequence: state.next_seq,
    }))
}

/// Writes a [`pending_event_count_index`] outside the log lock. The writer's
/// state is borrowed exclusively from the snapshot to here, so no append of
/// this writer falls between them.
fn publish_event_count_index(
    home: &SessionHome,
    state: &mut WriterState,
    index: &EventCountIndex,
) -> Result<(), String> {
    write_event_count_index(home, &state.session_id, index)?;
    state.index_dirty = false;
    state.last_index_write = Instant::now();
    Ok(())
}

fn open_state(home: &SessionHome, session_id: &str) -> Result<WriterState, String> {
    canonical_product_session_id(session_id)?;
    let path = home.session_events_file(session_id);
    let mut file = OpenOptions::new()
        .create(true)
        .read(true)
        .append(true)
        .open(&path)
        .map_err(|_| "could not open session event log".to_owned())?;
    let lock = LogFileLock::acquire(&file)?;
    let next_seq = recover_last_sequence(&mut file)?;
    let (event_count, rebuilt) = count_events_locked(home, session_id)?;
    let log_bytes = file
        .metadata()
        .map_err(|_| "could not inspect session event log".to_owned())?
        .len();
    let needs_line_separator = needs_line_separator(&mut file, log_bytes)?;
    drop(lock);
    if let Some(index) = rebuilt {
        let _ = write_event_count_index(home, session_id, &index);
    }
    Ok(WriterState {
        session_id: session_id.to_owned(),
        file,
        next_seq,
        event_count,
        log_bytes,
        needs_line_separator,
        index_dirty: false,
        last_index_write: Instant::now(),
    })
}

fn needs_line_separator(file: &mut File, log_bytes: u64) -> Result<bool, String> {
    Ok(log_bytes
        .checked_sub(1)
        .map(|last| {
            let mut byte = [0_u8; 1];
            file.seek(SeekFrom::Start(last))
                .and_then(|_| file.read_exact(&mut byte))
                .map(|()| byte[0] != b'\n')
                .map_err(|_| "could not inspect session event log".to_owned())
        })
        .transpose()?
        .unwrap_or(false))
}

/// Convenience for an observation about a session other than the host's
/// currently attached session. The bounded reverse scan keeps this cold path
/// independent of total log size.
pub fn emit(home: &SessionHome, session_id: &str, event: SessionEvent) {
    if let Ok(log) = SessionLog::open(home, session_id) {
        let _ = log.append(event);
    }
}

/// Read last `limit` valid raw records (oldest first among the window).
pub fn read_tail(
    home: &SessionHome,
    session_id: &str,
    limit: usize,
) -> Result<Vec<String>, String> {
    read_tail_for_kind(home, session_id, limit, None)
}

/// Read the last `limit` valid records whose parsed `kind` equals `kind` or is
/// in its dot-delimited namespace (for example, `tool` matches `tool.started`).
/// Filtering happens during the reverse scan, so `limit` applies to matching
/// records rather than to an arbitrarily selected unfiltered suffix.
pub fn read_tail_for_kind(
    home: &SessionHome,
    session_id: &str,
    limit: usize,
    kind: Option<&str>,
) -> Result<Vec<String>, String> {
    canonical_product_session_id(session_id)?;
    let path = home.session_events_file(session_id);
    if !path.is_file() || limit == 0 {
        return Ok(Vec::new());
    }
    let mut file = File::open(path).map_err(|_| "could not read session event log".to_owned())?;
    let result = read_reverse_records_filtered(&mut file, limit, None, kind)?;
    if !result.complete && result.lines.len() < limit {
        return Err("session event tail exceeds the bounded reverse scan".to_owned());
    }
    Ok(result.lines)
}

/// Read the rebuildable event-count projection. A missing, corrupt, or stale
/// sidecar is reconstructed by one bounded-memory log pass; ordinary reads are
/// independent of total event-log size.
pub fn count_events(home: &SessionHome, session_id: &str) -> Result<u64, String> {
    canonical_product_session_id(session_id)?;
    let path = home.session_events_file(session_id);
    if !path.is_file() {
        return Ok(0);
    }
    let file = File::open(&path).map_err(|_| "could not read session event log".to_owned())?;
    let (count, rebuilt) = {
        let _lock = LogFileLock::acquire(&file)?;
        count_events_locked(home, session_id)?
    };
    if let Some(index) = rebuilt {
        let _ = write_event_count_index(home, session_id, &index);
    }
    Ok(count)
}

/// Counts valid event records without creating or refreshing the rebuildable
/// index. Diagnostic callers can inspect a session without changing its home.
pub fn count_events_read_only(home: &SessionHome, session_id: &str) -> Result<u64, String> {
    canonical_product_session_id(session_id)?;
    let path = home.session_events_file(session_id);
    if !path.is_file() {
        return Ok(0);
    }
    let file = File::open(&path).map_err(|_| "could not read session event log".to_owned())?;
    let _lock = LogFileLock::try_acquire(&file)?;
    count_valid_records(&mut BufReader::new(file))
}

/// The event count under the log lock, and the rebuilt sidecar the caller
/// writes after releasing it when the stored one was missing or stale.
fn count_events_locked(
    home: &SessionHome,
    session_id: &str,
) -> Result<(u64, Option<EventCountIndex>), String> {
    let path = home.session_events_file(session_id);
    let log_bytes = path
        .metadata()
        .map_err(|_| "could not inspect session event log".to_owned())?
        .len();
    if let Ok(encoded) = std::fs::read(event_count_index_path(home, session_id))
        && let Ok(index) = serde_json::from_slice::<EventCountIndex>(&encoded)
        && index.format_revision == EVENT_COUNT_FORMAT_REVISION
        && index.log_bytes == log_bytes
        // The writer stamp: a sidecar written by a writer that had not seen the
        // log's last record is stale even when the byte length matches.
        && last_recorded_sequence(&path)? == index.last_sequence
    {
        return Ok((index.event_count, None));
    }
    let file = File::open(&path).map_err(|_| "could not read session event log".to_owned())?;
    let mut reader = BufReader::new(file);
    let count = count_valid_records(&mut reader)?;
    let index = EventCountIndex {
        format_revision: EVENT_COUNT_FORMAT_REVISION,
        log_bytes,
        event_count: count,
        last_sequence: last_recorded_sequence(&path)?,
    };
    Ok((count, Some(index)))
}

fn count_valid_records(reader: &mut impl BufRead) -> Result<u64, String> {
    let mut line = Vec::with_capacity(REVERSE_CHUNK_BYTES);
    let mut count = 0_u64;
    while let Some(within_limit) = read_bounded_record(reader, &mut line)
        .map_err(|_| "could not read session event log".to_owned())?
    {
        #[cfg(test)]
        EVENT_COUNT_TEST_SCANNED_BYTES.with(|bytes| {
            bytes.set(bytes.get().saturating_add(line.len()));
        });
        if !within_limit {
            return Err("session event count encountered an oversized record".to_owned());
        }
        if event_sequence(trim_ascii(&line)).is_some() {
            count = count
                .checked_add(1)
                .ok_or_else(|| "session event count exhausted".to_owned())?;
        }
    }
    Ok(count)
}

/// The sequence of the log's last valid record, or `0` for an empty log.
/// Bounded reverse read, so it is independent of total log size.
fn last_recorded_sequence(path: &std::path::Path) -> Result<u64, String> {
    let mut file = File::open(path).map_err(|_| "could not read session event log".to_owned())?;
    recover_last_sequence(&mut file)
}

/// Read one newline-delimited physical record without ever retaining more than
/// the accepted record limit. Oversized records are consumed and reported as
/// malformed so a hostile observation log cannot force proportional memory.
fn read_bounded_record<R: BufRead>(
    reader: &mut R,
    out: &mut Vec<u8>,
) -> std::io::Result<Option<bool>> {
    out.clear();
    let mut saw_bytes = false;
    let mut within_limit = true;
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return Ok(saw_bytes.then_some(within_limit));
        }
        saw_bytes = true;
        let consumed = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(available.len(), |index| index + 1);
        if within_limit {
            if out.len().saturating_add(consumed) <= MAX_PHYSICAL_RECORD_BYTES {
                out.extend_from_slice(&available[..consumed]);
            } else {
                within_limit = false;
                out.clear();
            }
        }
        let ended = available[consumed - 1] == b'\n';
        reader.consume(consumed);
        if ended {
            return Ok(Some(within_limit));
        }
    }
}

fn event_count_index_path(home: &SessionHome, session_id: &str) -> std::path::PathBuf {
    home.session_events_file(session_id)
        .with_extension("count.json")
}

fn write_event_count_index(
    home: &SessionHome,
    session_id: &str,
    index: &EventCountIndex,
) -> Result<(), String> {
    let encoded = serde_json::to_vec(index).map_err(|_| "could not encode session event count")?;
    let path = event_count_index_path(home, session_id);
    let temp = home.sessions_dir().join(format!(
        ".{session_id}.event-count.{}.tmp",
        uuid::Uuid::new_v4()
    ));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    #[cfg(test)]
    EVENT_COUNT_TEST_INDEX_WRITES.with(|writes| {
        writes.set(writes.get().saturating_add(1));
    });
    #[cfg(test)]
    if File::open(home.session_events_file(session_id)).is_ok_and(|probe| probe.try_lock().is_err())
    {
        EVENT_COUNT_TEST_LOCKED_WRITES.with(|writes| writes.set(writes.get().saturating_add(1)));
    }
    let result = (|| {
        let mut file = options.open(&temp)?;
        file.write_all(&encoded)?;
        file.sync_all()?;
        std::fs::rename(&temp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(temp);
    }
    result.map_err(|_| "could not write session event count".to_owned())
}

/// Filter lines whose parsed `kind` matches the expected namespace.
#[must_use]
pub fn filter_kind(lines: &[String], expected: &str) -> Vec<String> {
    lines
        .iter()
        .filter(|line| event_kind_is(line.as_bytes(), expected))
        .cloned()
        .collect()
}

fn recover_last_sequence(file: &mut File) -> Result<u64, String> {
    let file_len = file
        .metadata()
        .map_err(|_| "could not inspect session event log".to_owned())?
        .len();
    let bytes_read = usize::try_from(file_len.min(MAX_SEQUENCE_RECOVERY_BYTES as u64))
        .unwrap_or(MAX_SEQUENCE_RECOVERY_BYTES);
    let tail_offset =
        i64::try_from(bytes_read).map_err(|_| "could not seek session event log".to_owned())?;
    file.seek(SeekFrom::End(-tail_offset))
        .map_err(|_| "could not seek session event log".to_owned())?;
    let mut tail = vec![0_u8; bytes_read];
    file.read_exact(&mut tail)
        .map_err(|_| "could not read session event log".to_owned())?;
    let sequence = tail
        .rsplit(|byte| *byte == b'\n')
        .find_map(|line| event_sequence(trim_ascii(line)));
    match sequence {
        Some(sequence) => Ok(sequence),
        None if file_len <= bytes_read as u64 => Ok(0),
        None => Err("could not recover bounded session event sequence".to_owned()),
    }
}

/// Reverse-chunk reader. At most `byte_limit` bytes are examined when supplied;
/// otherwise reading stops as soon as `limit` records have been collected.
#[cfg(test)]
fn read_reverse_records(
    file: &mut File,
    limit: usize,
    byte_limit: Option<usize>,
) -> Result<(Vec<String>, usize), String> {
    let result = read_reverse_records_filtered(file, limit, byte_limit, None)?;
    Ok((result.lines, result.bytes_read))
}

struct ReverseRead {
    lines: Vec<String>,
    #[allow(
        dead_code,
        reason = "bounded-scan accounting is asserted in regression tests"
    )]
    bytes_read: usize,
    complete: bool,
}

fn read_reverse_records_filtered(
    file: &mut File,
    limit: usize,
    byte_limit: Option<usize>,
    kind: Option<&str>,
) -> Result<ReverseRead, String> {
    if limit == 0 {
        return Ok(ReverseRead {
            lines: Vec::new(),
            bytes_read: 0,
            complete: true,
        });
    }
    let mut pos = file
        .seek(SeekFrom::End(0))
        .map_err(|_| "could not seek session event log".to_owned())?;
    let mut reversed_record = Vec::with_capacity(REVERSE_CHUNK_BYTES);
    let mut record_oversized = false;
    let mut newest_first = Vec::new();
    let mut bytes_read = 0_usize;
    let scan_limit = byte_limit.unwrap_or(MAX_REVERSE_SCAN_BYTES);

    while pos > 0 && newest_first.len() < limit {
        let remaining_budget = scan_limit.saturating_sub(bytes_read);
        if remaining_budget == 0 {
            break;
        }
        let chunk_len = usize::try_from(pos.min(REVERSE_CHUNK_BYTES as u64))
            .unwrap_or(REVERSE_CHUNK_BYTES)
            .min(remaining_budget);
        pos -= chunk_len as u64;
        let mut chunk = vec![0; chunk_len];
        file.seek(SeekFrom::Start(pos))
            .and_then(|_| file.read_exact(&mut chunk))
            .map_err(|_| "could not read session event log".to_owned())?;
        bytes_read += chunk_len;
        for byte in chunk.into_iter().rev() {
            if byte == b'\n' {
                push_reversed_record(
                    &mut reversed_record,
                    &mut record_oversized,
                    &mut newest_first,
                    kind,
                );
                if newest_first.len() == limit {
                    break;
                }
            } else if !record_oversized {
                if reversed_record.len() < MAX_PHYSICAL_RECORD_BYTES.saturating_sub(1) {
                    reversed_record.push(byte);
                } else {
                    reversed_record.clear();
                    record_oversized = true;
                }
            }
        }
    }

    if pos == 0 && newest_first.len() < limit {
        push_reversed_record(
            &mut reversed_record,
            &mut record_oversized,
            &mut newest_first,
            kind,
        );
    }
    newest_first.reverse();
    Ok(ReverseRead {
        lines: newest_first,
        bytes_read,
        complete: pos == 0,
    })
}

fn push_reversed_record(
    reversed: &mut Vec<u8>,
    oversized: &mut bool,
    newest_first: &mut Vec<String>,
    kind: Option<&str>,
) {
    if !*oversized {
        reversed.reverse();
        let candidate = trim_ascii(reversed);
        if !candidate.is_empty()
            && event_sequence(candidate).is_some()
            && kind.is_none_or(|expected| event_kind_is(candidate, expected))
            && let Ok(line) = std::str::from_utf8(candidate)
        {
            newest_first.push(line.to_owned());
        }
    }
    reversed.clear();
    *oversized = false;
}

fn event_sequence(line: &[u8]) -> Option<u64> {
    serde_json::from_slice::<Value>(line)
        .ok()?
        .as_object()?
        .get("seq")?
        .as_u64()
}

fn event_kind_is(line: &[u8], expected: &str) -> bool {
    let Ok(Value::Object(object)) = serde_json::from_slice::<Value>(line) else {
        return false;
    };
    object
        .get("kind")
        .and_then(Value::as_str)
        .is_some_and(|kind| {
            kind == expected
                || kind
                    .strip_prefix(expected)
                    .is_some_and(|suffix| suffix.starts_with('.'))
        })
}

fn trim_ascii(mut bytes: &[u8]) -> &[u8] {
    while bytes.first().is_some_and(u8::is_ascii_whitespace) {
        bytes = &bytes[1..];
    }
    while bytes.last().is_some_and(u8::is_ascii_whitespace) {
        bytes = &bytes[..bytes.len() - 1];
    }
    bytes
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

fn bounded_string(value: &str) -> String {
    if value.len() > 2_000 {
        let boundary = value
            .char_indices()
            .map(|(index, _)| index)
            .take_while(|index| *index <= 2_000)
            .last()
            .unwrap_or(0);
        format!("{}…[{} chars]", &value[..boundary], value.chars().count())
    } else {
        value.to_owned()
    }
}

#[must_use]
pub fn redact_json_preview(value: &Value) -> String {
    let mut value = value.clone();
    plakous::tools::preview_redaction::json(&mut value);
    bound_json_atoms(&mut value);
    let rendered = serde_json::to_string_pretty(&value)
        .unwrap_or_else(|_| String::from("[arguments unavailable]"));
    bounded_string(&rendered)
}

#[must_use]
pub fn redact_text_preview(value: &str) -> String {
    bounded_string(&plakous::tools::preview_redaction::text(value))
}

fn bound_json_atoms(value: &mut Value) {
    match value {
        Value::Object(object) => object.values_mut().for_each(bound_json_atoms),
        Value::Array(values) => values.iter_mut().for_each(bound_json_atoms),
        Value::String(text) if text.len() > 512 => {
            let mut boundary = 512;
            while !text.is_char_boundary(boundary) {
                boundary -= 1;
            }
            *text = format!("{}…", &text[..boundary]);
        }
        _ => {}
    }
}

/// The `reason` a rejection record carries.
///
/// The category alone does not diagnose one: every schema refusal is
/// `InvalidArguments`, while the descriptive message says which constraint the
/// call missed. Both are kept, category first, so a reader grepping for the
/// category still finds it.
pub(crate) fn tool_rejected_reason(kind: &str, detail: Option<&str>) -> String {
    detail.map_or_else(
        || redact_text_preview(kind),
        |detail| redact_text_preview(&format!("{kind}: {detail}")),
    )
}

/// The session-log record for one rejected tool call.
///
/// `subject` and `arguments` come from the rejection's own observation: a call
/// that never starts leaves nothing in the subject stash, which is why both
/// used to be null and a rejection was undiagnosable after the fact.
pub(crate) fn tool_rejected_record(
    name: &str,
    reason: &str,
    call_id: &str,
    subject: Option<&str>,
    arguments: Option<&str>,
) -> SessionEvent {
    SessionEvent::new("tool.rejected")
        .str("name", name)
        .str("reason", reason)
        .opt_str("call_id", Some(call_id))
        .opt_str("subject", subject)
        .opt_str("arguments", arguments)
}

#[cfg(test)]
thread_local! {
    /// Per-test-thread instrumentation: parallel log tests must not contaminate
    /// one another's structural I/O measurements.
    static EVENT_COUNT_TEST_SCANNED_BYTES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };

    /// Every `sync_all` in this module happens inside `write_event_count_index`,
    /// so counting index writes counts session-log fsyncs exactly.
    static EVENT_COUNT_TEST_INDEX_WRITES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };

    /// Index writes that ran while the session log's lock was held.
    static EVENT_COUNT_TEST_LOCKED_WRITES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
mod tests;
