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
        let _lock = LogFileLock::acquire(&state.file)?;
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
            let _ = flush_event_count_index(&self.home, &mut state);
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

fn flush_writer(home: &SessionHome, state: &mut WriterState) -> Result<(), String> {
    let _lock = LogFileLock::acquire(&state.file)?;
    refresh_writer_state(state)?;
    flush_event_count_index(home, state)
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

/// Write the in-memory count against the log's current byte length. The length
/// is read here rather than tracked per append so a partially written record
/// can never leave a plausible-but-wrong sidecar behind.
fn flush_event_count_index(home: &SessionHome, state: &mut WriterState) -> Result<(), String> {
    if !state.index_dirty {
        return Ok(());
    }
    let log_bytes = state
        .file
        .metadata()
        .map(|metadata| metadata.len())
        .map_err(|_| "could not inspect session event log".to_owned())?;
    write_event_count_index(
        home,
        &state.session_id,
        &EventCountIndex {
            format_revision: EVENT_COUNT_FORMAT_REVISION,
            log_bytes,
            event_count: state.event_count,
            last_sequence: state.next_seq,
        },
    )?;
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
    let _lock = LogFileLock::acquire(&file)?;
    let next_seq = recover_last_sequence(&mut file)?;
    let event_count = count_events_locked(home, session_id)?;
    let log_bytes = file
        .metadata()
        .map_err(|_| "could not inspect session event log".to_owned())?
        .len();
    let needs_line_separator = needs_line_separator(&mut file, log_bytes)?;
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
    let _lock = LogFileLock::acquire(&file)?;
    count_events_locked(home, session_id)
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

fn count_events_locked(home: &SessionHome, session_id: &str) -> Result<u64, String> {
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
        return Ok(index.event_count);
    }
    let file = File::open(&path).map_err(|_| "could not read session event log".to_owned())?;
    let mut reader = BufReader::new(file);
    let count = count_valid_records(&mut reader)?;
    let _ = write_event_count_index(
        home,
        session_id,
        &EventCountIndex {
            format_revision: EVENT_COUNT_FORMAT_REVISION,
            log_bytes,
            event_count: count,
            last_sequence: last_recorded_sequence(&path)?,
        },
    );
    Ok(count)
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
    let ends_with_newline = tail.last() == Some(&b'\n');
    let sequence =
        tail.rsplit(|byte| *byte == b'\n')
            .enumerate()
            .find_map(|(reverse_index, line)| {
                let line = trim_ascii(line);
                event_sequence(line).or_else(|| {
                    (ends_with_newline || reverse_index > 0)
                        .then(|| legacy_event_sequence(line))
                        .flatten()
                })
            });
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

/// Old builds escaped quotes and backslashes but could write literal control
/// characters. Such a record is not returned as JSON, but its leading sequence
/// remains useful to prevent reuse when repairing the append boundary.
fn legacy_event_sequence(line: &[u8]) -> Option<u64> {
    let text = std::str::from_utf8(line).ok()?;
    let (_, suffix) = text.split_once("\"seq\"")?;
    let suffix = suffix.trim_start();
    let suffix = suffix.strip_prefix(':')?.trim_start();
    let digits = suffix.bytes().take_while(u8::is_ascii_digit).count();
    (digits > 0)
        .then(|| suffix[..digits].parse().ok())
        .flatten()
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
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests")]
mod tests {
    use super::*;
    use tempfile::tempdir;

    /// The log needs a directory, not a product layout: the fixture used to
    /// build a whole `PlakPaths` for one `sessions_dir()`, which is the
    /// dependency the 3.4 split took out.
    fn paths() -> (tempfile::TempDir, SessionHome) {
        let dir = tempdir().unwrap();
        let home = SessionHome::new(dir.path().join("sessions"));
        home.ensure().unwrap();
        (dir, home)
    }

    #[test]
    fn a_suspended_holder_of_the_log_lock_never_blocks_an_open_or_append() {
        let (_dir, home) = paths();
        let id = "44444444-5555-6666-7777-888888888888";
        let log = SessionLog::open(&home, id).unwrap();
        log.append(SessionEvent::new("before")).unwrap();
        // A separate open file description is a separate lock owner, like a
        // stopped process that held the lock when it was suspended.
        let holder = File::open(home.session_events_file(id)).unwrap();
        holder.lock().unwrap();
        let (sent, received) = std::sync::mpsc::channel();
        let opener = home.clone();
        std::thread::spawn(move || {
            let _ = sent.send(SessionLog::open(&opener, id).err());
        });
        let refused = received
            .recv_timeout(LOG_LOCK_WAIT * 4)
            .expect("an open waited on the held log lock past its bound");
        assert_eq!(refused.as_deref(), Some("session event log is busy"));
        let started = Instant::now();
        assert!(log.append(SessionEvent::new("while held")).is_err());
        assert!(started.elapsed() < LOG_LOCK_WAIT * 4);
        holder.unlock().unwrap();
        log.append(SessionEvent::new("after")).unwrap();
        assert_eq!(count_events(&home, id).unwrap(), 2);
    }

    #[test]
    fn unknown_abandonment_claim_survives_tail_kind_filter_and_count_index() {
        let (_dir, home) = paths();
        let id = "33333333-4444-5555-6666-777777777777";
        let log = SessionLog::open(&home, id).unwrap();
        for claim in ["tool_round_completed", "future_unknown_claim"] {
            log.append(SessionEvent::new("turn.abandoned").str("claim", claim))
                .unwrap();
        }
        log.flush().unwrap();
        let lines = read_tail(&home, id, 10).unwrap();
        assert_eq!(lines.len(), 2);
        assert_eq!(
            read_tail_for_kind(&home, id, 10, Some("turn.abandoned")).unwrap(),
            lines
        );
        assert_eq!(filter_kind(&lines, "turn"), lines);
        assert_eq!(count_events(&home, id).unwrap(), 2);
        assert_eq!(count_events_read_only(&home, id).unwrap(), 2);
        std::fs::remove_file(event_count_index_path(&home, id)).unwrap();
        assert_eq!(count_events(&home, id).unwrap(), 2);
        assert_eq!(
            serde_json::from_str::<Value>(&lines[0]).unwrap()["claim"],
            "tool_round_completed"
        );
    }

    /// Pins the record a schema rejection writes.
    ///
    /// A live run recorded
    /// `{"kind":"tool.rejected","name":"exec_command","reason":"InvalidArguments",
    /// "arguments":null,"subject":null}` — the Debug of the error kind and two
    /// null identifying fields, so the cause had to be recovered from the
    /// runtime checkpoint store instead of read from the log.
    #[test]
    fn a_schema_rejection_names_the_tool_the_arguments_and_why() {
        let (_dir, home) = paths();
        let id = "33333333-4444-5555-6666-777777777777";
        let log = SessionLog::open(&home, id).unwrap();
        let reason = tool_rejected_reason("InvalidArguments", Some("wait does not accept cell_id"));
        log.append(tool_rejected_record(
            "wait",
            &reason,
            "call-1",
            Some("cell 2"),
            Some(r#"{"cell_id": 2}"#),
        ))
        .unwrap();
        let lines = read_tail(&home, id, 10).unwrap();
        let record: Value = serde_json::from_str(&lines[0]).unwrap();
        assert_eq!(record["kind"], "tool.rejected");
        assert_eq!(record["name"], "wait");
        assert_eq!(record["call_id"], "call-1");
        assert_eq!(record["subject"], "cell 2");
        assert_eq!(
            record["arguments"], r#"{"cell_id": 2}"#,
            "the refused arguments must be recoverable from the log alone"
        );
        // The category is kept, so a reader grepping for it still finds it,
        // and the descriptive message says which constraint was missed.
        let reason = record["reason"].as_str().unwrap();
        assert!(reason.starts_with("InvalidArguments"), "{reason}");
        assert!(reason.contains("wait does not accept cell_id"), "{reason}");
        // With no descriptive message the record is what it always was.
        assert_eq!(
            tool_rejected_reason("InvalidArguments", None),
            "InvalidArguments"
        );
    }

    /// Pins the shape of the record a committed tool-result placeholders writes.
    ///
    /// A reader of the log alone has to be able to tell how much was replaced
    /// and what the projection cost before and after, so these four field
    /// names and the kind are part of the product surface.
    #[test]
    fn a_committed_placeholders_writes_its_counts_under_a_stable_kind() {
        let (_dir, home) = paths();
        let id = "22222222-3333-4444-5555-666666666666";
        let log = SessionLog::open(&home, id).unwrap();
        log.append(
            SessionEvent::new("context.placeholders")
                .u64("results", 3)
                .u64("bytes", 122_880)
                .u64("tokens_before", 36_617)
                .u64("tokens_after", 852),
        )
        .unwrap();
        let lines = read_tail(&home, id, 10).unwrap();
        assert_eq!(lines.len(), 1);
        let record: Value = serde_json::from_str(&lines[0]).unwrap();
        assert_eq!(record["kind"], "context.placeholders");
        assert_eq!(record["results"], 3);
        assert_eq!(record["bytes"], 122_880);
        assert_eq!(record["tokens_before"], 36_617);
        assert_eq!(record["tokens_after"], 852);
    }

    #[test]
    fn append_and_tail_round_trip() {
        let (_dir, paths) = paths();
        let home = paths.clone();
        let id = "11111111-2222-3333-4444-555555555555";
        let log = SessionLog::open(&home, id).unwrap();
        log.append(SessionEvent::new("session.created").str("id", id))
            .unwrap();
        log.append(
            SessionEvent::new("host.attached")
                .str("account", "codex")
                .str("model", "m")
                .bool("ok", true),
        )
        .unwrap();
        let lines = read_tail(&home, id, 10).unwrap();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("session.created"));
        assert!(lines[1].contains("host.attached"));
        assert_eq!(event_sequence(lines[1].as_bytes()), Some(2));
        log.flush().unwrap();
        EVENT_COUNT_TEST_SCANNED_BYTES.with(|bytes| bytes.set(0));
        assert_eq!(count_events(&home, id).unwrap(), 2);
        assert_eq!(
            EVENT_COUNT_TEST_SCANNED_BYTES.with(std::cell::Cell::get),
            0,
            "event-count reads after a flush must use the maintained sidecar"
        );
    }

    #[test]
    fn log_paths_require_the_exact_canonical_product_uuid() {
        let (_dir, paths) = paths();
        let home = paths.clone();
        for invalid in [
            "../outside",
            "11111111222233334444555555555555",
            "11111111-2222-3333-4444-55555555555A",
            "11111111-2222-3333-4444-555555555555.events",
        ] {
            assert!(SessionLog::open(&home, invalid).is_err(), "{invalid}");
            assert!(read_tail(&home, invalid, 1).is_err(), "{invalid}");
            assert!(count_events(&home, invalid).is_err(), "{invalid}");
        }
        assert!(!paths.sessions_dir().join("outside.events.jsonl").exists());
    }

    #[test]
    fn parsed_kind_namespace_filter_applies_before_tail_limit() {
        let (_dir, paths) = paths();
        let home = paths.clone();
        let id = "11111111-2222-3333-4444-555555555555";
        let log = SessionLog::open(&home, id).unwrap();
        log.append(SessionEvent::new("turn.user")).unwrap();
        log.append(SessionEvent::new("turn.user.delta")).unwrap();
        log.append(SessionEvent::new("other").str("detail", "turn.user"))
            .unwrap();
        log.append(SessionEvent::new("turn.user")).unwrap();

        let lines = read_tail_for_kind(&home, id, 2, Some("turn.user")).unwrap();
        assert_eq!(lines.len(), 2);
        assert!(
            lines
                .iter()
                .all(|line| event_kind_is(line.as_bytes(), "turn.user"))
        );
        let filtered = filter_kind(&read_tail(&home, id, 4).unwrap(), "turn.user");
        assert_eq!(&filtered[filtered.len() - 2..], lines);
    }

    #[test]
    fn reopen_skips_empty_or_truncated_final_line_and_keeps_unique_sequence() {
        let (_dir, paths) = paths();
        let home = paths.clone();
        let id = "11111111-2222-3333-4444-555555555555";
        {
            let log = SessionLog::open(&home, id).unwrap();
            log.append(SessionEvent::new("one")).unwrap();
            log.append(SessionEvent::new("two")).unwrap();
        }
        let path = home.session_events_file(id);
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(b"\n{\"ts\":0,\"seq\":3,\"kind\":\"cut")
            .unwrap();
        drop(file);

        let log = SessionLog::open(&home, id).unwrap();
        log.append(SessionEvent::new("after-reopen")).unwrap();
        let lines = read_tail(&home, id, 10).unwrap();
        let seqs = lines
            .iter()
            .filter_map(|line| event_sequence(line.as_bytes()))
            .collect::<Vec<_>>();
        assert_eq!(seqs, vec![1, 2, 3]);
        assert_eq!(count_events(&home, id).unwrap(), 3);
    }

    #[test]
    fn reopen_fails_closed_when_bounded_corrupt_tail_hides_prior_sequence() {
        let (_dir, paths) = paths();
        let home = paths.clone();
        let id = "11111111-2222-3333-4444-555555555555";
        let path = home.session_events_file(id);
        let mut file = File::create(path).unwrap();
        writeln!(file, "{{\"ts\":0,\"seq\":41,\"kind\":\"valid\"}}").unwrap();
        file.write_all(&vec![b'x'; MAX_SEQUENCE_RECOVERY_BYTES + 1])
            .unwrap();
        drop(file);
        assert_eq!(
            SessionLog::open(&home, id).err().as_deref(),
            Some("could not recover bounded session event sequence")
        );
    }

    #[test]
    fn steady_append_never_reads_prior_log_and_open_recovery_is_bounded() {
        let (_dir, paths) = paths();
        let home = paths.clone();
        let id = "11111111-2222-3333-4444-555555555555";
        let path = home.session_events_file(id);
        for scale in [1, 2, 4] {
            let mut file = File::create(&path).unwrap();
            for seq in 1..=scale * 4_096 {
                writeln!(file, "{{\"ts\":0,\"seq\":{seq},\"kind\":\"seed\"}}").unwrap();
            }
            drop(file);
            let before = std::fs::metadata(&path).unwrap().len();
            let log = SessionLog::open(&home, id).unwrap();
            for _ in 0..128 {
                log.append(SessionEvent::new("steady")).unwrap();
            }
            let after = std::fs::metadata(&path).unwrap().len();
            assert!(after > before);
            let mut file = File::open(&path).unwrap();
            let (_, recovery_bytes) =
                read_reverse_records(&mut file, usize::MAX, Some(MAX_SEQUENCE_RECOVERY_BYTES))
                    .unwrap();
            assert!(recovery_bytes <= MAX_SEQUENCE_RECOVERY_BYTES);
        }
    }

    #[test]
    fn fixed_tail_work_is_independent_of_total_log_size() {
        let (_dir, paths) = paths();
        let home = paths.clone();
        let id = "11111111-2222-3333-4444-555555555555";
        let path = home.session_events_file(id);
        let mut observed = Vec::new();
        for scale in [1, 2, 4] {
            let mut file = File::create(&path).unwrap();
            for seq in 1..=scale * 8_192 {
                writeln!(file, "{{\"ts\":0,\"seq\":{seq},\"kind\":\"seed\"}}").unwrap();
            }
            drop(file);
            let mut file = File::open(&path).unwrap();
            let (tail, bytes) = read_reverse_records(&mut file, 16, None).unwrap();
            assert_eq!(tail.len(), 16);
            observed.push(bytes);
        }
        assert_eq!(observed[0], observed[1]);
        assert_eq!(observed[1], observed[2]);
    }

    #[test]
    fn exact_count_streams_past_the_old_ten_thousand_limit() {
        let (_dir, paths) = paths();
        let home = paths.clone();
        let id = "11111111-2222-3333-4444-555555555555";
        let path = home.session_events_file(id);
        let mut file = File::create(path).unwrap();
        for seq in 1..=10_017 {
            writeln!(file, "{{\"ts\":0,\"seq\":{seq},\"kind\":\"seed\"}}").unwrap();
        }
        assert_eq!(count_events(&home, id).unwrap(), 10_017);
    }

    #[test]
    fn event_count_sidecar_rebuilds_once_then_scales_independently_of_log_size() {
        let (_dir, paths) = paths();
        let home = paths.clone();
        let id = "11111111-2222-3333-4444-555555555555";
        let path = home.session_events_file(id);
        let mut file = File::create(&path).unwrap();
        for seq in 1..=24_017 {
            writeln!(file, "{{\"ts\":0,\"seq\":{seq},\"kind\":\"seed\"}}").unwrap();
        }
        drop(file);

        EVENT_COUNT_TEST_SCANNED_BYTES.with(|bytes| bytes.set(0));
        assert_eq!(count_events(&home, id).unwrap(), 24_017);
        assert!(EVENT_COUNT_TEST_SCANNED_BYTES.with(std::cell::Cell::get) > 0);

        EVENT_COUNT_TEST_SCANNED_BYTES.with(|bytes| bytes.set(0));
        for _ in 0..128 {
            assert_eq!(count_events(&home, id).unwrap(), 24_017);
        }
        assert_eq!(
            EVENT_COUNT_TEST_SCANNED_BYTES.with(std::cell::Cell::get),
            0,
            "fresh sidecar reads must not scan the event log"
        );

        let mut file = OpenOptions::new().append(true).open(path).unwrap();
        writeln!(file, "{{\"ts\":0,\"seq\":24018,\"kind\":\"new\"}}").unwrap();
        drop(file);
        assert_eq!(count_events(&home, id).unwrap(), 24_018);
        assert!(EVENT_COUNT_TEST_SCANNED_BYTES.with(std::cell::Cell::get) > 0);
    }

    #[test]
    fn burst_appends_stay_ordered_and_well_formed() {
        let (_dir, paths) = paths();
        let home = paths.clone();
        let id = "11111111-2222-3333-4444-555555555555";
        {
            let log = SessionLog::open(&home, id).unwrap();
            for index in 0..10_000_u64 {
                log.append(SessionEvent::new("burst").u64("index", index))
                    .unwrap();
            }
        }
        let encoded = std::fs::read_to_string(home.session_events_file(id)).unwrap();
        let mut records = 0_u64;
        for (offset, line) in encoded.lines().enumerate() {
            let decoded: Value = serde_json::from_str(line)
                .unwrap_or_else(|error| panic!("record {offset} is malformed: {error}"));
            let offset = u64::try_from(offset).unwrap();
            assert_eq!(decoded["seq"].as_u64(), Some(offset + 1));
            assert_eq!(decoded["index"].as_u64(), Some(offset));
            assert_eq!(decoded["kind"].as_str(), Some("burst"));
            records += 1;
        }
        assert_eq!(records, 10_000);
        assert_eq!(count_events(&home, id).unwrap(), 10_000);
    }

    #[test]
    fn appends_never_fsync_per_event_and_close_flushes_the_index() {
        let (_dir, paths) = paths();
        let home = paths.clone();
        let id = "11111111-2222-3333-4444-555555555555";
        let log = SessionLog::open(&home, id).unwrap();
        EVENT_COUNT_TEST_INDEX_WRITES.with(|writes| writes.set(0));
        let started = Instant::now();
        for _ in 0..10_000 {
            log.append(SessionEvent::new("burst")).unwrap();
        }
        let elapsed = started.elapsed();
        let interval_flushes = elapsed.as_nanos() / SESSION_LOG_INDEX_INTERVAL.as_nanos();
        let during_appends = EVENT_COUNT_TEST_INDEX_WRITES.with(std::cell::Cell::get);
        assert!(
            u128::try_from(during_appends).unwrap() <= interval_flushes,
            "10,000 appends over {elapsed:?} performed {during_appends} index writes; at most \
             {interval_flushes} interval flushes are permitted and no per-event fsync"
        );

        drop(log);
        assert_eq!(
            EVENT_COUNT_TEST_INDEX_WRITES.with(std::cell::Cell::get),
            during_appends + 1,
            "close must persist the index exactly once"
        );
        EVENT_COUNT_TEST_SCANNED_BYTES.with(|bytes| bytes.set(0));
        assert_eq!(count_events(&home, id).unwrap(), 10_000);
        assert_eq!(
            EVENT_COUNT_TEST_SCANNED_BYTES.with(std::cell::Cell::get),
            0,
            "the close flush must leave an exact sidecar behind"
        );
    }

    #[test]
    fn a_clean_flush_writes_once_and_a_second_flush_is_a_no_op() {
        let (_dir, paths) = paths();
        let home = paths.clone();
        let id = "11111111-2222-3333-4444-555555555555";
        let log = SessionLog::open(&home, id).unwrap();
        log.append(SessionEvent::new("one")).unwrap();
        EVENT_COUNT_TEST_INDEX_WRITES.with(|writes| writes.set(0));
        log.flush().unwrap();
        log.flush().unwrap();
        assert_eq!(EVENT_COUNT_TEST_INDEX_WRITES.with(std::cell::Cell::get), 1);
        drop(log);
        assert_eq!(
            EVENT_COUNT_TEST_INDEX_WRITES.with(std::cell::Cell::get),
            1,
            "close must not rewrite an already-clean index"
        );
    }

    #[test]
    fn a_session_switch_flushes_the_index_of_the_session_being_left() {
        let (_dir, paths) = paths();
        let home = paths.clone();
        let first = "11111111-2222-3333-4444-555555555555";
        let second = "11111111-2222-3333-4444-555555555556";
        let log = SessionLog::open(&home, first).unwrap();
        log.append(SessionEvent::new("one")).unwrap();
        log.append(SessionEvent::new("two")).unwrap();
        log.append_for(second, SessionEvent::new("elsewhere"))
            .unwrap();

        EVENT_COUNT_TEST_SCANNED_BYTES.with(|bytes| bytes.set(0));
        assert_eq!(count_events(&home, first).unwrap(), 2);
        assert_eq!(
            EVENT_COUNT_TEST_SCANNED_BYTES.with(std::cell::Cell::get),
            0,
            "the switch must have persisted the departed session's count"
        );
        log.flush().unwrap();
        assert_eq!(count_events(&home, second).unwrap(), 1);
    }

    #[test]
    fn an_index_stale_against_a_longer_log_rescans_to_the_right_count() {
        let (_dir, paths) = paths();
        let home = paths.clone();
        let id = "11111111-2222-3333-4444-555555555555";
        let path = home.session_events_file(id);
        {
            let log = SessionLog::open(&home, id).unwrap();
            for _ in 0..8 {
                log.append(SessionEvent::new("before-loss")).unwrap();
            }
        }
        let index_path = event_count_index_path(&home, id);
        let persisted =
            serde_json::from_slice::<EventCountIndex>(&std::fs::read(&index_path).unwrap())
                .unwrap();
        assert_eq!(persisted.event_count, 8);

        // Simulate an abrupt loss after an interval flush: records reached the
        // log, the sidecar did not advance.
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        for seq in 9..=13 {
            writeln!(file, "{{\"ts\":0,\"seq\":{seq},\"kind\":\"after-loss\"}}").unwrap();
        }
        drop(file);

        EVENT_COUNT_TEST_SCANNED_BYTES.with(|bytes| bytes.set(0));
        assert_eq!(count_events(&home, id).unwrap(), 13);
        assert!(
            EVENT_COUNT_TEST_SCANNED_BYTES.with(std::cell::Cell::get) > 0,
            "a byte-length mismatch must force the bounded rescan"
        );

        // The rescan repairs the sidecar, and a deleted sidecar rebuilds too.
        EVENT_COUNT_TEST_SCANNED_BYTES.with(|bytes| bytes.set(0));
        assert_eq!(count_events(&home, id).unwrap(), 13);
        assert_eq!(EVENT_COUNT_TEST_SCANNED_BYTES.with(std::cell::Cell::get), 0);
        std::fs::remove_file(&index_path).unwrap();
        assert_eq!(count_events(&home, id).unwrap(), 13);
    }

    #[test]
    fn independent_handles_resume_the_shared_sequence_and_count() {
        let (_dir, home) = paths();
        let id = "11111111-2222-3333-4444-666666666666";
        let first = SessionLog::open(&home, id).unwrap();
        let second = SessionLog::open(&home, id).unwrap();
        first.append(SessionEvent::new("host")).unwrap();
        second.append(SessionEvent::new("catalog.refresh")).unwrap();
        first.append(SessionEvent::new("host.again")).unwrap();
        second.flush().unwrap();
        first.flush().unwrap();
        let sequences: Vec<_> = read_tail(&home, id, 10)
            .unwrap()
            .iter()
            .map(|line| event_sequence(line.as_bytes()).unwrap())
            .collect();
        assert_eq!(sequences, [1, 2, 3]);
        assert_eq!(count_events(&home, id).unwrap(), 3);
        drop((first, second));
        let reopened = SessionLog::open(&home, id).unwrap();
        reopened.append(SessionEvent::new("reopened")).unwrap();
        reopened.flush().unwrap();
        assert_eq!(
            event_sequence(read_tail(&home, id, 1).unwrap()[0].as_bytes()),
            Some(4)
        );
        assert_eq!(count_events(&home, id).unwrap(), 4);
    }

    #[test]
    fn a_stale_writer_flush_reconciles_before_publishing_the_index() {
        let (_dir, paths) = paths();
        let home = paths.clone();
        let id = "11111111-2222-3333-4444-666666666666";
        let path = home.session_events_file(id);
        let index_path = event_count_index_path(&home, id);

        let log = SessionLog::open(&home, id).unwrap();
        log.append(SessionEvent::new("host")).unwrap();
        // A second writer appends and leaves the log at sequence 2.
        {
            let second = SessionLog::open(&home, id).unwrap();
            second.append(SessionEvent::new("second")).unwrap();
        }
        // The host must catch up before publishing, despite its dirty cache.
        log.flush().unwrap();
        let persisted =
            serde_json::from_slice::<EventCountIndex>(&std::fs::read(&index_path).unwrap())
                .unwrap();
        assert_eq!(
            persisted.log_bytes,
            path.metadata().unwrap().len(),
            "the projection describes the locked current prefix"
        );
        assert_eq!(persisted.event_count, 2);
        assert_eq!(persisted.last_sequence, 2);

        drop(log);
        assert_eq!(count_events(&home, id).unwrap(), 2);
        let repaired =
            serde_json::from_slice::<EventCountIndex>(&std::fs::read(&index_path).unwrap())
                .unwrap();
        assert_eq!(repaired.event_count, 2);
        assert_eq!(repaired.last_sequence, 2);
    }

    #[test]
    fn pre_coordination_index_is_rebuilt_even_when_duplicate_sequence_stamp_matches() {
        let (_dir, home) = paths();
        let id = "11111111-2222-3333-4444-666666666666";
        let path = home.session_events_file(id);
        let mut file = File::create(&path).unwrap();
        for seq in [1, 2, 2] {
            writeln!(file, "{{\"seq\":{seq},\"kind\":\"historical\"}}").unwrap();
        }
        write_event_count_index(
            &home,
            id,
            &EventCountIndex {
                format_revision: 2,
                log_bytes: file.metadata().unwrap().len(),
                event_count: 2,
                last_sequence: 2,
            },
        )
        .unwrap();
        assert_eq!(count_events(&home, id).unwrap(), 3);
        let repaired: EventCountIndex =
            serde_json::from_slice(&std::fs::read(event_count_index_path(&home, id)).unwrap())
                .unwrap();
        assert_eq!(repaired.format_revision, EVENT_COUNT_FORMAT_REVISION);
        assert_eq!(repaired.event_count, 3);
    }

    #[test]
    fn concurrent_independent_handles_do_not_duplicate_or_interleave_records() {
        let (_dir, home) = paths();
        let id = "11111111-2222-3333-4444-666666666666";
        let ready = std::sync::Arc::new(std::sync::Barrier::new(4));
        std::thread::scope(|scope| {
            for writer in 0..4 {
                let ready = &ready;
                let home = &home;
                scope.spawn(move || {
                    let log = SessionLog::open(home, id).unwrap();
                    ready.wait();
                    for event in 0..32 {
                        log.append(
                            SessionEvent::new("concurrent")
                                .u64("writer", writer)
                                .u64("event", event),
                        )
                        .unwrap();
                    }
                });
            }
        });
        let lines = read_tail(&home, id, 128).unwrap();
        assert_eq!(lines.len(), 128);
        for (index, line) in lines.iter().enumerate() {
            assert_eq!(
                event_sequence(line.as_bytes()),
                Some(u64::try_from(index).unwrap() + 1)
            );
        }
        assert_eq!(count_events(&home, id).unwrap(), 128);
    }

    fn wait_for_test_file(path: &std::path::Path) -> bool {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !path.is_file() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        path.is_file()
    }

    struct TestChild(std::process::Child);

    impl Drop for TestChild {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    #[test]
    fn independent_processes_share_sequence() {
        const CHILD_HOME: &str = "PLAK_SESSION_LOG_TEST_CHILD_HOME";
        let id = "11111111-2222-3333-4444-666666666666";
        if let Some(path) = std::env::var_os(CHILD_HOME) {
            let home = SessionHome::new(path.into());
            let log = SessionLog::open(&home, id).unwrap();
            std::fs::write(home.sessions_dir().join("ready"), b"").unwrap();
            assert!(wait_for_test_file(&home.sessions_dir().join("start")));
            for _ in 0..32 {
                log.append(SessionEvent::new("child")).unwrap();
            }
            return;
        }
        let (_dir, home) = paths();
        let log = SessionLog::open(&home, id).unwrap();
        let mut child = TestChild(
            std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "session_log::tests::independent_processes_share_sequence",
                    "--nocapture",
                ])
                .env(CHILD_HOME, home.sessions_dir())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .unwrap(),
        );
        assert!(wait_for_test_file(&home.sessions_dir().join("ready")));
        // Both processes opened at zero; the child must see this later append.
        log.append(SessionEvent::new("before-child")).unwrap();
        std::fs::write(home.sessions_dir().join("start"), b"").unwrap();
        for _ in 0..32 {
            log.append(SessionEvent::new("parent")).unwrap();
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(status) = child.0.try_wait().unwrap() {
                assert!(status.success());
                break;
            }
            assert!(Instant::now() < deadline, "journal child did not finish");
            std::thread::sleep(Duration::from_millis(5));
        }
        log.flush().unwrap();
        let lines = read_tail(&home, id, 65).unwrap();
        assert_eq!(lines.len(), 65);
        for (index, line) in lines.iter().enumerate() {
            assert_eq!(
                event_sequence(line.as_bytes()),
                Some(u64::try_from(index).unwrap() + 1)
            );
        }
        assert_eq!(count_events(&home, id).unwrap(), 65);
    }

    #[test]
    fn truncates_long_unicode_strings_on_a_character_boundary() {
        let ev = SessionEvent::new("turn.user").str("text", "é".repeat(3_000));
        let line = String::from_utf8(ev.to_json_line(1, 0).unwrap()).unwrap();
        assert!(line.contains("chars]"));
        assert!(line.len() < 5_000);
    }

    #[test]
    fn serde_encoding_keeps_every_event_on_one_valid_physical_line() {
        let (_dir, paths) = paths();
        let home = paths.clone();
        let id = "11111111-2222-3333-4444-555555555555";
        let value = "line one\nline two\r\ttab\0control:\u{0001} unicode:界";
        let log = SessionLog::open(&home, id).unwrap();
        log.append(
            SessionEvent::new("tool.\nstarted")
                .str("odd\nkey", value)
                .str("text", value),
        )
        .unwrap();

        let encoded = std::fs::read_to_string(home.session_events_file(id)).unwrap();
        assert_eq!(encoded.lines().count(), 1);
        let decoded: Value = serde_json::from_str(encoded.trim_end()).unwrap();
        assert_eq!(decoded["odd\nkey"], value);
        assert_eq!(decoded["text"], value);
    }

    #[test]
    fn reopen_repairs_after_legacy_multiline_record_without_reusing_sequence() {
        let (_dir, paths) = paths();
        let home = paths.clone();
        let id = "11111111-2222-3333-4444-555555555555";
        let path = home.session_events_file(id);
        std::fs::write(
            &path,
            b"{\"ts\":0, \"seq\":41, \"kind\":\"legacy\", \"text\":\"split\nhere\"}\n",
        )
        .unwrap();

        let log = SessionLog::open(&home, id).unwrap();
        log.append(SessionEvent::new("repaired")).unwrap();
        let lines = read_tail(&home, id, 10).unwrap();
        assert_eq!(lines.len(), 1);
        assert_eq!(event_sequence(lines[0].as_bytes()), Some(42));
        assert_eq!(count_events(&home, id).unwrap(), 1);
    }

    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "one table pins the small preview rule set"
    )]
    fn preview_redaction_rules_table() {
        // Token-shaped inputs are split with concat! so secret scanners do not
        // flag these synthetic values in the source.
        let cases = [
            ("API-key route", "API-key route"),
            ("tokenizer=cl100k", "tokenizer=cl100k"),
            ("input_tokens: 123", "input_tokens: 123"),
            ("max_tokens=4096", "max_tokens=4096"),
            (r#""input_tokens": 123"#, r#""input_tokens": 123"#),
            (r#""max_tokens": 4096"#, r#""max_tokens": 4096"#),
            (
                concat!(r#"{"api_key": ""#, "abcdefgh", "12345", r#""}"#),
                r#"{"api_key": "[redacted]"}"#,
            ),
            ("'token': 'abcdefgh'", "'token': '[redacted]'"),
            ("token_count", "token_count"),
            (
                "a password or token in prose",
                "a password or token in prose",
            ),
            ("git push -u origin main", "git push -u origin main"),
            ("xai-api", "xai-api"),
            ("[redacted]", "[redacted]"),
            (
                "[redacted credential-like content]",
                "[redacted credential-like content]",
            ),
            ("token=short", "token=short"),
            (concat!("sk-", "abcdefghijklmnopqrst"), "[redacted]"),
            (concat!("sk-ant-", "abcdefghijklmnopqrst"), "[redacted]"),
            (concat!("sk-proj-", "abcdefghijklmnopqrst"), "[redacted]"),
            (concat!("AKIA", "ABCDEFGHIJKLMNOP"), "[redacted]"),
            ("Bearer abcdefghijklmnop", "Bearer [redacted]"),
            ("bEaReR\tabcdefghijklmnop==", "bEaReR\t[redacted]"),
            ("Basic abcdefghijklmnop==", "Basic [redacted]"),
            ("token=abcdefgh", "token=[redacted]"),
            ("GITHUB_TOKEN=abcdefgh", "GITHUB_TOKEN=[redacted]"),
            ("OPENAI_API_KEY='abcdefgh'", "OPENAI_API_KEY='[redacted]'"),
            ("access_token: \"abcdefgh\"", "access_token: \"[redacted]\""),
            ("password=abcdefgh", "password=[redacted]"),
            (
                "API_TOKEN=\\\"hunter2hunter2\\\"",
                "API_TOKEN=\\\"[redacted]\\\"",
            ),
            ("passwd=abcdefgh", "passwd=[redacted]"),
            ("secret=abcdefgh", "secret=[redacted]"),
            (concat!("ghp_", "abcdefghijklmnopqrst"), "[redacted]"),
            (concat!("gho_", "abcdefghijklmnopqrst"), "[redacted]"),
            (concat!("ghs_", "abcdefghijklmnopqrst"), "[redacted]"),
            (concat!("ghu_", "abcdefghijklmnopqrst"), "[redacted]"),
            (concat!("ghr_", "abcdefghijklmnopqrst"), "[redacted]"),
            (concat!("github_pat_", "abcdefghijklmnopqrst"), "[redacted]"),
            (concat!("glpat-", "abcdefghijklmnopqrst"), "[redacted]"),
            (concat!("AIza", "abcdefghijklmnopqrst"), "[redacted]"),
            (concat!("xoxb-", "abcdefghijklmnopqrst"), "[redacted]"),
            (
                concat!(
                    "xoxa-",
                    "abcdefghijklmnopqrst ",
                    "xoxp-",
                    "abcdefghijklmnopqrst ",
                    "xoxr-",
                    "abcdefghijklmnopqrst ",
                    "xoxs-",
                    "abcdefghijklmnopqrst",
                ),
                "[redacted] [redacted] [redacted] [redacted]",
            ),
            (
                concat!(
                    "eyJhbGciOiJIUzI1NiJ9",
                    ".",
                    "eyJzdWIiOiJ0ZXN0In0",
                    ".signature"
                ),
                "[redacted]",
            ),
            (
                concat!(
                    "-----BEGIN PRIVATE",
                    " KEY-----\nsynthetic-test-material\n",
                    "-----END PRIVATE",
                    " KEY-----",
                ),
                "[redacted]",
            ),
            (
                "Authorization: Bearer short\nnext",
                "Authorization: [redacted]\nnext",
            ),
            (
                "Proxy-Authorization: Basic short",
                "Proxy-Authorization: [redacted]",
            ),
            (
                "curl -H 'X-Api-Key: short' https://safe.example",
                "curl -H 'X-Api-Key: [redacted]' https://safe.example",
            ),
            ("Cookie: name=short; other=value", "Cookie: [redacted]"),
            (
                "https://user:short@example.test/path",
                "https://user:[redacted]@example.test/path",
            ),
            (
                "https://example.test/?token=a&key=b&api_key=c&access_token=d&password=e&secret=f&sig=g&input_tokens=123",
                "https://example.test/?token=[redacted]&key=[redacted]&api_key=[redacted]&access_token=[redacted]&password=[redacted]&secret=[redacted]&sig=[redacted]&input_tokens=123",
            ),
        ];
        for (input, expected) in cases {
            let actual = redact_text_preview(input);
            assert_eq!(actual, expected, "{input}");
            assert_eq!(redact_text_preview(&actual), actual, "second pass: {input}");
            let json = redact_json_preview(&serde_json::json!({"value": input}));
            assert_eq!(
                serde_json::from_str::<Value>(&json).unwrap(),
                serde_json::json!({"value": expected}),
                "JSON leaf: {input}"
            );
        }
        let input = serde_json::json!({
            "nested": {"access_token": "short", "password": 123, "secret": false},
            "OPENAI_API_KEY": "short", "input_tokens": 123, "max_tokens": 4096,
            "token_count": 2, "tokenizer": "cl100k", "token": ["readable"],
            "password": ["hunter2hunter2"],
            "api_key": {"v": "hunter3hunter3", "n": 1, "ok": false}
        });
        let expected = serde_json::json!({
            "nested": {"access_token": "[redacted]", "password": "[redacted]", "secret": false},
            "OPENAI_API_KEY": "[redacted]", "input_tokens": 123, "max_tokens": 4096,
            "token_count": 2, "tokenizer": "cl100k", "token": ["[redacted]"],
            "password": ["[redacted]"],
            "api_key": {"v": "[redacted]", "n": "[redacted]", "ok": false}
        });
        let actual = redact_json_preview(&input);
        assert_eq!(serde_json::from_str::<Value>(&actual).unwrap(), expected);
        assert_eq!(redact_json_preview(&expected), actual);
        let input = format!("{}token=abcdefgh", "x ".repeat(995));
        let preview = redact_text_preview(&input);
        assert!(preview.contains("token=[red"), "{preview}");
        assert!(!preview.contains("abcdefgh"));
    }

    #[test]
    fn persisted_previews_are_bounded_and_redact_nested_and_inline_secrets() {
        const SECRET: &str = "SECRET_SENTINEL_DO_NOT_RETAIN";
        let preview = redact_json_preview(&serde_json::json!({
            "path": "/tmp/useful",
            "nested": {"api_token": SECRET},
            "command": format!("curl -H Authorization: Bearer {SECRET}"),
            "large": "界".repeat(2_000),
        }));
        assert!(preview.contains("/tmp/useful"));
        assert!(!preview.contains(SECRET));
        assert!(preview.len() <= 2_050);

        for value in [
            format!("token={SECRET}"),
            format!("Bearer {SECRET}"),
            format!("sk-{SECRET}"),
            format!("curl -H 'X-API-Key: {SECRET}' https://safe.example/path"),
            format!("curl --header X_API_KEY={SECRET} https://safe.example/path"),
            format!("Authorization: Basic {SECRET}\n--safe flag"),
        ] {
            let redacted = redact_text_preview(&value);
            assert!(!redacted.contains(SECRET), "{value:?} => {redacted:?}");
            if value.contains("safe") {
                assert!(redacted.contains("safe"));
            }
            assert!(redacted.len() <= 2_050);
        }

        let headers = redact_json_preview(&serde_json::json!({
            "headers": {"X_API_KEY": SECRET, "authorization": format!("Bearer {SECRET}"), "Content-Type": "application/json"},
            "x-api-key": SECRET,
            "safe_path": "/tmp/retained"
        }));
        assert!(!headers.contains(SECRET));
        assert!(headers.contains("application/json"));
        assert!(headers.contains("/tmp/retained"));
    }

    #[test]
    fn redaction_is_idempotent_and_never_accretes_punctuation() {
        // The argument dump reached the operator as
        // `"max_output_tokens": "[redacted]]]]"`. The marker itself ends in
        // `]`, and the punctuation-preserving rewrite treated that `]` as
        // structure to keep, so every extra pass appended one more.
        let arguments = serde_json::json!({
            "command": ["ls", "-la"],
            "max_output_tokens": 4096,
            "timeout_ms": 30_000,
            "api_key": "synthetic-secret",
        });
        let once = redact_json_preview(&arguments);
        assert!(once.contains("\"[redacted]\""), "{once}");

        let mut repeated = once.clone();
        for pass in 1..=4 {
            repeated = redact_text_preview(&repeated);
            assert_eq!(
                repeated, once,
                "pass {pass} changed an already-redacted preview"
            );
        }
        assert!(
            !repeated.contains("[redacted]]"),
            "no stray bracket may accrete:\n{repeated}"
        );
        // The structure the rewrite exists to preserve still survives.
        assert!(repeated.contains("\"-la\""), "{repeated}");
        assert!(
            serde_json::from_str::<Value>(&repeated).is_ok(),
            "the preview stays parseable JSON:\n{repeated}"
        );

        // A real secret is still redacted on the first pass and stable after.
        let secret = "s3cr3t-value-that-must-never-appear";
        let text = format!("curl -H 'X-API-Key: {secret}' https://safe.example/path");
        let first = redact_text_preview(&text);
        assert!(!first.contains(secret));
        assert_eq!(redact_text_preview(&first), first);
    }

    #[test]
    fn malformed_physical_records_have_bounded_forward_and_reverse_working_sets() {
        let (_dir, paths) = paths();
        let home = paths.clone();
        let id = "11111111-2222-3333-4444-555555555555";
        let path = home.session_events_file(id);
        let mut file = File::create(&path).unwrap();
        writeln!(file, "{{\"ts\":0,\"seq\":1,\"kind\":\"valid\"}}").unwrap();
        file.write_all(&vec![
            b'x';
            MAX_REVERSE_SCAN_BYTES + MAX_PHYSICAL_RECORD_BYTES
        ])
        .unwrap();
        writeln!(file).unwrap();
        writeln!(file, "{{\"ts\":0,\"seq\":2,\"kind\":\"tail\"}}").unwrap();
        drop(file);

        assert_eq!(
            count_events(&home, id).unwrap_err(),
            "session event count encountered an oversized record"
        );
        assert_eq!(
            read_tail(&home, id, 2).unwrap_err(),
            "session event tail exceeds the bounded reverse scan"
        );
        let mut file = File::open(&path).unwrap();
        let (tail, scanned) = read_reverse_records(&mut file, 2, None).unwrap();
        assert_eq!(tail.len(), 1);
        assert!(tail[0].contains("\"seq\":2"));
        assert!(scanned <= MAX_REVERSE_SCAN_BYTES);

        let mut reader = BufReader::new(File::open(path).unwrap());
        let mut record = Vec::new();
        assert_eq!(
            read_bounded_record(&mut reader, &mut record).unwrap(),
            Some(true)
        );
        assert_eq!(
            read_bounded_record(&mut reader, &mut record).unwrap(),
            Some(false)
        );
        assert!(record.len() <= MAX_PHYSICAL_RECORD_BYTES);
    }
}
