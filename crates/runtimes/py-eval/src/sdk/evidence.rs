//! The agent incarnation's id tables: evidence references and process jobs.
//!
//! An id is an opaque random string and a capability only together with the
//! table that issued it. A forged id, an id another agent's table issued, and
//! an id whose evidence is no longer retained all resolve to nothing.

use std::{
    collections::{BTreeMap, VecDeque},
    fmt::Write as _,
    sync::{Arc, Mutex},
};

use plakous_tools::ProcessValue;

/// Cell outputs kept for `ctx.read(ref)` at once.
const RETAINED_CELL_OUTPUTS: usize = 64;
/// Bytes of cell output kept for `ctx.read(ref)` at once.
const RETAINED_CELL_OUTPUT_BYTES: usize = 16 * 1024 * 1024;

/// A fresh opaque id: 128 random bits in lowercase hex.
pub(super) fn new_id() -> Result<String, getrandom::Error> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes)?;
    let mut id = String::with_capacity(32);
    for byte in bytes {
        let _ = write!(id, "{byte:02x}");
    }
    Ok(id)
}

/// What one reference names.
#[derive(Clone, Debug)]
pub(super) enum Evidence {
    /// A process's full output, an issued job-output file read through `read_file`.
    ProcessOutput(String),
    /// A cell's own full output, held in bounded process memory.
    CellOutput(Arc<str>),
}

/// One issued job.
#[derive(Clone, Debug)]
pub(super) enum Job {
    /// A running process session.
    Session(u64),
    /// A process that ended before it was promoted; its last observation.
    Ended(ProcessValue),
}

#[derive(Default)]
struct Tables {
    references: BTreeMap<String, Evidence>,
    process_outputs: BTreeMap<String, String>,
    cell_outputs: VecDeque<(String, usize)>,
    cell_output_bytes: usize,
    jobs: BTreeMap<String, Job>,
}

/// The id tables of one agent incarnation, shared by its generations.
#[derive(Default)]
pub(super) struct EvidenceTable {
    tables: Mutex<Tables>,
}

impl EvidenceTable {
    fn tables(&self) -> std::sync::MutexGuard<'_, Tables> {
        self.tables
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The reference to an issued job-output file, one per path.
    pub(super) fn process_output(&self, path: &str) -> Result<String, getrandom::Error> {
        let mut tables = self.tables();
        if let Some(id) = tables.process_outputs.get(path) {
            return Ok(id.clone());
        }
        let id = new_id()?;
        tables
            .references
            .insert(id.clone(), Evidence::ProcessOutput(path.to_owned()));
        tables.process_outputs.insert(path.to_owned(), id.clone());
        Ok(id)
    }

    /// A reference to one cell's full output, evicting the oldest kept
    /// outputs past the retention bound.
    pub(super) fn cell_output(&self, text: &str) -> Result<String, getrandom::Error> {
        let id = new_id()?;
        let mut tables = self.tables();
        tables
            .references
            .insert(id.clone(), Evidence::CellOutput(text.into()));
        tables.cell_outputs.push_back((id.clone(), text.len()));
        tables.cell_output_bytes += text.len();
        while tables.cell_outputs.len() > RETAINED_CELL_OUTPUTS
            || tables.cell_output_bytes > RETAINED_CELL_OUTPUT_BYTES
        {
            let Some((evicted, bytes)) = tables.cell_outputs.pop_front() else {
                break;
            };
            tables.cell_output_bytes -= bytes;
            tables.references.remove(&evicted);
        }
        Ok(id)
    }

    /// What `id` names, if this table issued it and still retains it.
    pub(super) fn resolve(&self, id: &str) -> Option<Evidence> {
        self.tables().references.get(id).cloned()
    }

    /// Issues a job id.
    pub(super) fn issue_job(&self, job: Job) -> Result<String, getrandom::Error> {
        let id = new_id()?;
        self.tables().jobs.insert(id.clone(), job);
        Ok(id)
    }

    /// The job `id` names, if this table issued it.
    pub(super) fn job(&self, id: &str) -> Option<Job> {
        self.tables().jobs.get(id).cloned()
    }
}
