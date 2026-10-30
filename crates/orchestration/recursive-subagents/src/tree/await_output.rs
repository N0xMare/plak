//! The neutral await: what every await presentation asks for, the one
//! receipt it returns, and that receipt's aggregate output budget.
//!
//! `wait_agent` is a presentation of this capability: it maps its arguments
//! onto an [`AwaitRequest`] and returns the [`AwaitOutput`] JSON unchanged.

use serde::Serialize;

use super::{AttemptId, InboxMail, MessagePurpose, WaitAgentRequest, WaitAgentRow, WaitWakeReason};
use crate::protocol::{MAX_WAIT_TARGETS, MAX_WAIT_TIMEOUT_MS, TopologyError, TopologyErrorCode};

/// Serialized bytes one await return may carry beyond its first budgeted
/// item; the admission order is spec 03 §4.4 "Output budget".
pub const WAIT_OUTPUT_BUDGET_BYTES: usize = 256 * 1024;

/// Host presentation subject bound: the same 240 bytes as a tool-observation subject.
const AWAIT_SUBJECT_BYTES: usize = 240;

const OMITTED_NOTE: &str = "over the 262144-byte wait budget: withheld results stay pending and withheld mail stays unread; wait again";

/// Appended when a re-read was withheld: an identical wait would withhold it
/// again, so the note names the one call shape that returns it.
const REREADS_NOTE: &str = "; rereads lists already delivered outputs that were withheld: name each of those agent_ids alone to read it";

/// Which children an await watches.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AwaitAgents {
    /// Every managed child that is not closed and has something unread.
    Pending,
    /// Exactly these children.
    Named(Vec<String>),
    /// No children and no mail: the await names processes only.
    Unwatched,
}

/// One await over the caller's children, their mail and its own processes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AwaitRequest {
    /// The children watched.
    pub agents: AwaitAgents,
    /// The caller's own promoted processes, as `p<N>`; a terminal one wakes it.
    pub processes: Vec<String>,
    /// Clamped ceiling in milliseconds.
    pub timeout_ms: u64,
}

impl AwaitRequest {
    /// The `wait_agent` presentation: agents only, never process targets.
    ///
    /// # Errors
    ///
    /// The request's own shape refusal.
    pub fn wait_agent(request: &WaitAgentRequest) -> Result<Self, TopologyErrorCode> {
        let timeout_ms = request.validate()?;
        Ok(Self {
            agents: request
                .agent_ids
                .clone()
                .map_or(AwaitAgents::Pending, AwaitAgents::Named),
            processes: Vec::new(),
            timeout_ms,
        })
    }

    /// Grok's output presentation over named children and the caller's own
    /// processes (`p<N>`). An omitted or zero `timeout_ms` is a snapshot: an
    /// await whose ceiling has already expired.
    ///
    /// # Errors
    ///
    /// Refuses more than [`MAX_WAIT_TARGETS`] children or a repeated id.
    pub fn task_output(
        agents: Vec<String>,
        processes: Vec<String>,
        timeout_ms: u64,
    ) -> Result<Self, TopologyError> {
        let mut seen = std::collections::BTreeSet::new();
        if agents.len() > MAX_WAIT_TARGETS || agents.iter().any(|id| !seen.insert(id.as_str())) {
            return Err(TopologyError::new(
                TopologyErrorCode::SchemaMismatch,
                format!(
                    "task_ids must name at most {MAX_WAIT_TARGETS} distinct agents; an agent id repeats or there are too many"
                ),
            ));
        }
        Ok(Self {
            agents: AwaitAgents::Named(agents),
            processes,
            timeout_ms: timeout_ms.min(MAX_WAIT_TIMEOUT_MS),
        })
    }
}

/// A named process as the await saw it. A snapshot: reading it consumes no
/// notice and grants no process authority.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct AwaitJobRow {
    /// `p<N>`.
    pub id: String,
    /// `running`, `exited`, `stopped`, `unknown` or `already_reported`.
    pub status: &'static str,
    /// Natural exit status, when there is one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// Retained output tail.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
}

/// What the budget withheld from one return.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AwaitOmitted {
    /// Agents whose new `output_json` was withheld; their rows stay
    /// undelivered.
    pub outputs: Vec<String>,
    /// Agents whose already delivered `output_json` (a re-read) was withheld.
    pub rereads: Vec<String>,
    /// Unread envelopes left for the next await.
    pub mail: usize,
    /// Named processes whose output tail was withheld.
    pub job_output: Vec<String>,
}

impl Serialize for AwaitOmitted {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        struct View<'a> {
            outputs: &'a [String],
            rereads: &'a [String],
            mail: usize,
            job_output: &'a [String],
            note: String,
        }
        let mut note = OMITTED_NOTE.to_owned();
        if !self.rereads.is_empty() {
            note.push_str(REREADS_NOTE);
        }
        View {
            outputs: &self.outputs,
            rereads: &self.rereads,
            mail: self.mail,
            job_output: &self.job_output,
            note,
        }
        .serialize(serializer)
    }
}

/// The one receipt every await presentation returns.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AwaitOutput {
    /// Exact unread envelopes returned and acknowledged by this await.
    pub mail: Vec<InboxMail>,
    /// Why this await returned.
    pub woke_by: WaitWakeReason,
    /// One row per watched agent, whether or not it settled.
    pub agents: Vec<WaitAgentRow>,
    /// True when the ceiling expired before anything woke the await; the rows
    /// are still the tree's truth at that moment, not an error.
    pub timed_out: bool,
    /// The caller's processes that became terminal after admission.
    pub processes: Vec<String>,
    /// One row per named process.
    pub jobs: Vec<AwaitJobRow>,
    /// What the output budget withheld, when it withheld anything.
    pub omitted: Option<AwaitOmitted>,
    /// Agents whose settled row the caller had not received at poll time.
    pub(crate) newly_settled: Vec<String>,
}

impl AwaitOutput {
    /// Comma-joined agent ids, bounded to 240 bytes.
    ///
    /// Complete ids only, until one more would pass the bound; a single id
    /// longer than the bound is clipped with one trailing `…`. Empty when
    /// `agents` names none. The host-only presentation subject of the
    /// outcome; it is not a field of the model-visible receipt.
    pub(crate) fn presentation_subject(&self) -> Option<String> {
        bounded_agent_subject(self.agents.iter().map(|row| row.agent_id.as_str()))
    }

    /// Whether the result envelope belongs to a row of this output.
    fn has_row_for(&self, mail: &InboxMail) -> bool {
        mail.purpose == MessagePurpose::Result
            && self.agents.iter().any(|row| {
                row.agent_id == mail.from
                    && row.attempt_id.as_ref().map(AttemptId::as_str) == mail.attempt_id.as_deref()
            })
    }

    /// A result envelope whose row is present is rendered by reference only,
    /// so the result body is not returned twice.
    fn mail_json(&self, mail: &InboxMail) -> serde_json::Value {
        if self.has_row_for(mail) {
            let mut compact = serde_json::Map::new();
            compact.insert("message_id".to_owned(), mail.message_id.clone().into());
            compact.insert("purpose".to_owned(), "result".into());
            compact.insert("attempt_id".to_owned(), mail.attempt_id.clone().into());
            compact.into()
        } else {
            serde_json::to_value(mail).unwrap_or_default()
        }
    }

    /// Whether the budget withheld this agent's output.
    pub(crate) fn output_withheld(&self, agent_id: &str) -> bool {
        self.omitted
            .as_ref()
            .is_some_and(|omitted| omitted.outputs.iter().any(|id| id == agent_id))
    }

    /// Applies [`WAIT_OUTPUT_BUDGET_BYTES`] deterministically.
    ///
    /// Every row (without `output_json`), every process name and every job
    /// row (without its tail) is always returned. The budgeted items are then
    /// considered in this order, each in target or arrival order: outputs of
    /// newly settled rows, mail, outputs of rows already delivered, job
    /// tails. The first budgeted item is always admitted whole, so every
    /// await makes progress; after it, an item is admitted only if the whole
    /// serialized return stays within the budget, and an item that does not
    /// fit is skipped. Plain mail stops at the first envelope that does not
    /// fit, so acknowledged plain mail stays a prefix of its arrival order. A
    /// result envelope whose row carries an output is not plain mail: it
    /// travels with that output, admitted or withheld with it regardless of
    /// its position. Withheld outputs leave their rows
    /// undelivered and withheld mail stays unread, so the next await returns
    /// them at once.
    pub(crate) fn fit_budget(&mut self) {
        enum Item {
            Output(usize),
            Mail(usize),
            Tail(usize),
        }
        let outputs: Vec<Option<String>> = self
            .agents
            .iter_mut()
            .map(|row| row.output_json.take())
            .collect();
        let tails: Vec<Option<String>> =
            self.jobs.iter_mut().map(|job| job.output.take()).collect();
        let mail = std::mem::take(&mut self.mail);
        let coupled: Vec<Option<usize>> = mail
            .iter()
            .map(|envelope| {
                self.has_row_for(envelope)
                    .then(|| {
                        self.agents
                            .iter()
                            .position(|row| row.agent_id == envelope.from)
                            .filter(|index| outputs[*index].is_some())
                    })
                    .flatten()
            })
            .collect();
        let worst = AwaitOmitted {
            outputs: self
                .agents
                .iter()
                .zip(&outputs)
                .filter(|(_, output)| output.is_some())
                .map(|(row, _)| row.agent_id.clone())
                .collect(),
            // Each id appears in one list, so moving re-reads there costs
            // nothing; only the reread note is added below.
            rereads: Vec::new(),
            mail: mail.len(),
            job_output: self
                .jobs
                .iter()
                .zip(&tails)
                .filter(|(_, tail)| tail.is_some())
                .map(|(job, _)| job.id.clone())
                .collect(),
        };
        let newly = |index: usize| self.newly_settled.contains(&self.agents[index].agent_id);
        let reread_note =
            if (0..outputs.len()).any(|index| outputs[index].is_some() && !newly(index)) {
                REREADS_NOTE.len()
            } else {
                0
            };
        let mut used = json_len(&*self) + ",\"omitted\":".len() + json_len(&worst) + reread_note;
        let order = (0..outputs.len())
            .filter(|index| outputs[*index].is_some() && newly(*index))
            .map(Item::Output)
            .chain(
                (0..mail.len())
                    .filter(|index| coupled[*index].is_none())
                    .map(Item::Mail),
            )
            .chain(
                (0..outputs.len())
                    .filter(|index| outputs[*index].is_some() && !newly(*index))
                    .map(Item::Output),
            )
            .chain(
                (0..tails.len())
                    .filter(|index| tails[*index].is_some())
                    .map(Item::Tail),
            );
        let mut admitted_outputs = vec![false; outputs.len()];
        let mut admitted_mail = vec![false; mail.len()];
        let mut admitted_tails = vec![false; tails.len()];
        let mut first = true;
        let mut mail_closed = false;
        for item in order {
            let cost = match item {
                Item::Output(index) => {
                    ",\"output_json\":".len()
                        + json_len(&outputs[index])
                        + mail
                            .iter()
                            .zip(&coupled)
                            .filter(|(_, row)| **row == Some(index))
                            .map(|(envelope, _)| json_len(&self.mail_json(envelope)) + 1)
                            .sum::<usize>()
                }
                Item::Mail(index) => json_len(&self.mail_json(&mail[index])) + 1,
                Item::Tail(index) => ",\"output\":".len() + json_len(&tails[index]),
            };
            if mail_closed && matches!(item, Item::Mail(_)) {
                continue;
            }
            if !first && used + cost > WAIT_OUTPUT_BUDGET_BYTES {
                // Acknowledged plain mail stays a prefix of arrival order.
                mail_closed |= matches!(item, Item::Mail(_));
                continue;
            }
            first = false;
            used += cost;
            match item {
                Item::Output(index) => admitted_outputs[index] = true,
                Item::Mail(index) => admitted_mail[index] = true,
                Item::Tail(index) => admitted_tails[index] = true,
            }
        }
        let mut omitted = AwaitOmitted::default();
        for ((row, output), admitted) in self.agents.iter_mut().zip(outputs).zip(&admitted_outputs)
        {
            if *admitted {
                row.output_json = output;
            } else if output.is_some() {
                if self.newly_settled.contains(&row.agent_id) {
                    omitted.outputs.push(row.agent_id.clone());
                } else {
                    omitted.rereads.push(row.agent_id.clone());
                }
            }
        }
        for ((job, tail), admitted) in self.jobs.iter_mut().zip(tails).zip(&admitted_tails) {
            if *admitted {
                job.output = tail;
            } else if tail.is_some() {
                omitted.job_output.push(job.id.clone());
            }
        }
        for ((envelope, row), admitted) in mail.into_iter().zip(coupled).zip(admitted_mail) {
            if row.map_or(admitted, |index| admitted_outputs[index]) {
                self.mail.push(envelope);
            } else {
                omitted.mail += 1;
            }
        }
        self.omitted = (omitted != AwaitOmitted::default()).then_some(omitted);
    }
}

/// The host presentation subject for these agent ids. Same bound as
/// [`AwaitOutput::presentation_subject`].
pub(crate) fn bounded_agent_subject<'a>(ids: impl IntoIterator<Item = &'a str>) -> Option<String> {
    let mut subject = String::new();
    for id in ids {
        let id = id.trim();
        if id.is_empty() {
            continue;
        }
        let separator = usize::from(!subject.is_empty());
        if subject.len() + separator + id.len() > AWAIT_SUBJECT_BYTES {
            if subject.is_empty() {
                subject = clip_subject(id);
            }
            break;
        }
        if separator == 1 {
            subject.push(',');
        }
        subject.push_str(id);
    }
    (!subject.is_empty()).then_some(subject)
}

fn clip_subject(id: &str) -> String {
    let ellipsis = "…";
    let mut end = AWAIT_SUBJECT_BYTES.saturating_sub(ellipsis.len());
    while end > 0 && !id.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{ellipsis}", &id[..end])
}

fn to_json<T: Serialize + ?Sized, E: serde::ser::Error>(value: &T) -> Result<serde_json::Value, E> {
    serde_json::to_value(value).map_err(E::custom)
}

fn json_len<T: Serialize + ?Sized>(value: &T) -> usize {
    serde_json::to_string(value).map_or(0, |text| text.len())
}

impl Serialize for AwaitOutput {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut output = serde_json::Map::new();
        output.insert("agents".to_owned(), to_json::<_, S::Error>(&self.agents)?);
        let mail: Vec<_> = self.mail.iter().map(|mail| self.mail_json(mail)).collect();
        output.insert("mail".to_owned(), mail.into());
        output.insert("timed_out".to_owned(), self.timed_out.into());
        output.insert("woke_by".to_owned(), to_json::<_, S::Error>(&self.woke_by)?);
        if !self.processes.is_empty() {
            output.insert(
                "processes".to_owned(),
                to_json::<_, S::Error>(&self.processes)?,
            );
        }
        if !self.jobs.is_empty() {
            output.insert("jobs".to_owned(), to_json::<_, S::Error>(&self.jobs)?);
        }
        if let Some(omitted) = &self.omitted {
            output.insert("omitted".to_owned(), to_json::<_, S::Error>(omitted)?);
        }
        output.serialize(serializer)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests may assert with unwrap")]
mod tests;
