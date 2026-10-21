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
mod tests {
    use super::*;
    use crate::protocol::{AttemptStatus, MessagePriority, NodeStatus};

    fn row(agent_id: &str, output: Option<usize>) -> WaitAgentRow {
        WaitAgentRow {
            agent_id: agent_id.to_owned(),
            status: NodeStatus::Completed,
            attempt_id: Some(AttemptId::new(agent_id, 1)),
            attempt: AttemptStatus::Completed,
            candidate: None,
            output_json: output.map(|bytes| "o".repeat(bytes)),
            blocked: None,
            failure: None,
            usage: None,
            waiting_on: None,
        }
    }

    fn mail(id: &str, from: &str, purpose: MessagePurpose, bytes: usize) -> InboxMail {
        InboxMail {
            body_bytes: bytes,
            message_id: id.to_owned(),
            from: from.to_owned(),
            purpose,
            priority: MessagePriority::Deferred,
            in_reply_to: None,
            body: "m".repeat(bytes),
            attempt_id: (purpose == MessagePurpose::Result)
                .then(|| AttemptId::new(from, 1).as_str().to_owned()),
            status: None,
            candidate: None,
        }
    }

    fn output(agents: Vec<WaitAgentRow>, mail: Vec<InboxMail>, newly: &[&str]) -> AwaitOutput {
        AwaitOutput {
            mail,
            woke_by: WaitWakeReason::Settled,
            agents,
            timed_out: false,
            processes: Vec::new(),
            jobs: Vec::new(),
            omitted: None,
            newly_settled: newly.iter().map(|id| (*id).to_owned()).collect(),
        }
    }

    fn len(output: &AwaitOutput) -> usize {
        serde_json::to_string(output).unwrap().len()
    }

    const KIB: usize = 1024;

    #[test]
    fn budget_admits_new_results_then_mail_then_rereads_then_tails() {
        let mut fitted = output(
            vec![
                row("c1", Some(120 * KIB)),
                row("c2", Some(100 * KIB)),
                row("c3", Some(40 * KIB)),
            ],
            vec![
                mail("m1", "c3", MessagePurpose::Question, 2 * KIB),
                mail("m2", "c2", MessagePurpose::Result, 0),
                mail("m3", "c1", MessagePurpose::Coordinate, 2 * KIB),
            ],
            &["c2", "c3"],
        );
        fitted.jobs = vec![AwaitJobRow {
            id: "p1".to_owned(),
            status: "exited",
            exit_code: Some(0),
            output: Some("t".repeat(4 * KIB)),
        }];
        fitted.fit_budget();
        // c2 and c3 are new (target order), then both plain mails; c1's
        // re-read no longer fits and is skipped, the small tail still fits.
        assert!(fitted.agents[0].output_json.is_none());
        assert!(fitted.agents[1].output_json.is_some());
        assert!(fitted.agents[2].output_json.is_some());
        let mail: Vec<_> = fitted
            .mail
            .iter()
            .map(|mail| mail.message_id.as_str())
            .collect();
        assert_eq!(mail, ["m1", "m2", "m3"]);
        assert!(fitted.jobs[0].output.is_some());
        assert_eq!(
            fitted.omitted,
            Some(AwaitOmitted {
                outputs: Vec::new(),
                rereads: vec!["c1".to_owned()],
                mail: 0,
                job_output: Vec::new(),
            })
        );
        // A withheld re-read leaves the row delivered: an identical wait
        // would withhold it again, so the note says to name it alone.
        assert!(!fitted.output_withheld("c1"));
        assert!(!fitted.output_withheld("c2"));
        assert!(len(&fitted) <= WAIT_OUTPUT_BUDGET_BYTES);
        let rendered = serde_json::to_value(&fitted).unwrap();
        assert_eq!(rendered["omitted"]["outputs"], serde_json::json!([]));
        assert_eq!(
            rendered["omitted"]["rereads"],
            serde_json::Value::from(vec!["c1"])
        );
        assert_eq!(
            rendered["omitted"]["note"],
            format!("{OMITTED_NOTE}{REREADS_NOTE}")
        );
        assert!(REREADS_NOTE.contains("alone"));
        assert!(OMITTED_NOTE.contains(&WAIT_OUTPUT_BUDGET_BYTES.to_string()));
    }

    #[test]
    fn a_withheld_result_keeps_its_envelope_and_mail_is_admitted_in_arrival_order() {
        let mut fitted = output(
            vec![row("c1", Some(200 * KIB)), row("c2", Some(200 * KIB))],
            (0..40)
                .map(|index| {
                    mail(
                        &format!("m{index}"),
                        "c1",
                        MessagePurpose::Coordinate,
                        2 * KIB,
                    )
                })
                .chain([mail("r2", "c2", MessagePurpose::Result, 0)])
                .collect(),
            &["c1", "c2"],
        );
        fitted.fit_budget();
        assert!(fitted.agents[0].output_json.is_some());
        assert!(fitted.output_withheld("c2"));
        assert!(fitted.mail.iter().all(|mail| mail.message_id != "r2"));
        let admitted: Vec<_> = fitted
            .mail
            .iter()
            .map(|mail| mail.message_id.clone())
            .collect();
        let expected: Vec<_> = (0..admitted.len())
            .map(|index| format!("m{index}"))
            .collect();
        assert_eq!(admitted, expected);
        assert!(!admitted.is_empty() && admitted.len() < 40);
        let omitted = fitted.omitted.as_ref().unwrap();
        assert_eq!(omitted.mail, 41 - admitted.len());
        assert_eq!(omitted.rereads, [] as [String; 0]);
        assert!(len(&fitted) <= WAIT_OUTPUT_BUDGET_BYTES);
        let rendered = serde_json::to_value(&fitted).unwrap();
        assert_eq!(rendered["omitted"]["note"], OMITTED_NOTE);
    }

    /// Plain mail is acknowledged as an arrival-order prefix of plain mail; a
    /// Result envelope travels with its row's admitted output whatever its
    /// position, even when earlier plain mail was withheld.
    #[test]
    fn plain_mail_is_an_arrival_prefix_and_a_result_envelope_travels_with_its_output() {
        let mut fitted = output(
            vec![row("c1", Some(200 * KIB)), row("c2", None)],
            (0..40)
                .map(|index| {
                    mail(
                        &format!("m{index}"),
                        "c2",
                        MessagePurpose::Coordinate,
                        2 * KIB,
                    )
                })
                .chain([mail("r1", "c1", MessagePurpose::Result, 0)])
                .collect(),
            &["c1"],
        );
        fitted.fit_budget();
        assert!(fitted.omitted.as_ref().unwrap().mail > 0);
        assert!(fitted.agents[0].output_json.is_some());
        let admitted: Vec<String> = fitted
            .mail
            .iter()
            .map(|mail| mail.message_id.clone())
            .collect();
        let (result, plain) = admitted.split_last().unwrap();
        assert_eq!(result, "r1");
        let arrival: Vec<String> = (0..plain.len()).map(|index| format!("m{index}")).collect();
        assert_eq!(plain, arrival.as_slice());
        assert!(plain.len() < 40);
    }

    #[test]
    fn the_first_budgeted_item_is_returned_whole_past_the_budget() {
        let escaped = "\"".repeat(200 * KIB);
        let mut fitted = output(vec![row("c1", None)], Vec::new(), &["c1"]);
        fitted.agents[0].output_json = Some(escaped.clone());
        fitted.fit_budget();
        assert_eq!(
            fitted.agents[0].output_json.as_deref(),
            Some(escaped.as_str())
        );
        assert_eq!(fitted.omitted, None);
        assert!(len(&fitted) > WAIT_OUTPUT_BUDGET_BYTES);
    }

    #[test]
    fn an_output_within_the_budget_is_unchanged() {
        let mut fitted = output(
            vec![row("c1", Some(KIB)), row("c2", None)],
            vec![mail("r1", "c1", MessagePurpose::Result, 0)],
            &["c1"],
        );
        let before = fitted.clone();
        fitted.fit_budget();
        assert_eq!(fitted, before);
        assert!(
            serde_json::to_value(&fitted)
                .unwrap()
                .get("omitted")
                .is_none()
        );
    }

    #[test]
    fn the_receipt_keeps_its_shape_and_the_host_subject_is_separate() {
        let fitted = output(
            vec![row("c1", None), row(" c2 ", None), row("", None)],
            Vec::new(),
            &[],
        );
        let value = serde_json::to_value(&fitted).unwrap();
        assert!(value.get("subject").is_none());
        assert_eq!(fitted.presentation_subject().as_deref(), Some("c1,c2"));
        assert!(
            output(vec![row("", None)], Vec::new(), &[])
                .presentation_subject()
                .is_none()
        );
        let long = "a".repeat(AWAIT_SUBJECT_BYTES + 8);
        let clipped = output(vec![row(&long, None)], Vec::new(), &[])
            .presentation_subject()
            .unwrap();
        assert!(clipped.ends_with('…'));
        assert!(clipped.len() <= AWAIT_SUBJECT_BYTES);
        let many = (0..32)
            .map(|index| row(&format!("agent-{index:02}-{}", "n".repeat(20)), None))
            .collect();
        let bounded = output(many, Vec::new(), &[])
            .presentation_subject()
            .unwrap();
        assert!(bounded.len() <= AWAIT_SUBJECT_BYTES);
        assert!(!bounded.ends_with(','), "{bounded}");
        assert!(bounded.contains("agent-00"));
        // Trim and drop blanks. A repeated id stays: the subject is the
        // joined ids, not a set.
        assert_eq!(
            bounded_agent_subject([" c1 ", "c1", "", "c2"]),
            Some("c1,c1,c2".into())
        );
    }
}
