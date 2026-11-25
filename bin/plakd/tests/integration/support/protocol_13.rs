//! Adapter for durability assertions that inspect the neutral test vocabulary.
//! Only the exact served decoder is used before projecting that vocabulary.

use plakd_wire_types::{PlakExtension, v13};
use plakous_host_protocol::{SessionObservation, views::transcript::TranscriptStreamDelta, wire};
use serde_json::{Value, json};

pub(crate) fn decode(
    line: &str,
) -> Result<Option<wire::HostFrame<PlakExtension>>, wire::WireError> {
    use v13::HostFrameV13 as F;
    let frame = wire::decode_line::<v13::HostFrame>(line)?;
    let to_value = |frame: &F| serde_json::to_value(frame).map_err(|error| invalid(&error));
    Ok(Some(match frame {
        v13::HostFrame::Shared(frame) => frame.into_inner(),
        v13::HostFrame::Draft(frame) => {
            match *frame {
                F::Snapshot(page) => wire::HostFrame::Snapshot(wire::SnapshotPage {
                    attachment: page.attachment,
                    schema: plakous_host_protocol::views::presentation::PRESENTATION_SCHEMA_V3,
                    scope: page.scope,
                    revision: page.revision.revision,
                    page: page.part,
                    items: page
                        .items
                        .into_iter()
                        .map(
                            |item| plakous_host_protocol::views::transcript::TranscriptItem {
                                key: item.key,
                                row: item.row.0,
                            },
                        )
                        .collect(),
                    last: page.last,
                }),
                F::Event {
                    attachment,
                    seq,
                    class,
                    observation,
                    client_id,
                    blobs,
                } => {
                    let observation = match *observation {
                    v13::Observation::Shared(observation) => observation.into_inner().0,
                    v13::Observation::Draft(observation) => match observation {
                        v13::ObservationV13::Extension(v13::SurfaceObservation::Shared(event)) =>
                            SessionObservation::Extension(event.into_inner()),
                        v13::ObservationV13::Extension(v13::SurfaceObservation::Draft(event)) => {
                            use plakd_wire_types::SurfaceObservation as S;
                            SessionObservation::Extension(match event {
                                v13::SurfaceObservationV13::Profiles(view) => S::Profiles(
                                    plakous_host_protocol::views::ProfilesView {
                                        rows: view.rows.into_iter().map(profile_row).collect::<Result<_, _>>()?,
                                        current: view.current,
                                        diagnostics: view.diagnostics,
                                    }
                                ),
                                v13::SurfaceObservationV13::SessionProfile(row) =>
                                    S::SessionProfile(profile_row(*row)?),
                            })
                        }
                        v13::ObservationV13::TranscriptToolStream { scope, tool } =>
                            SessionObservation::TranscriptStream { scope, delta: TranscriptStreamDelta::Tool(Box::new((*tool).into())) },
                        v13::ObservationV13::TranscriptStream(stream) =>
                            SessionObservation::TranscriptStream {
                                scope: plakous_host_protocol::views::transcript::TranscriptScope::Root,
                                delta: TranscriptStreamDelta::Keyed {
                                    row: stream.row, offset: stream.offset, end: stream.end, text: stream.text,
                                },
                            },
                        _ => return Ok(None),
                    },
                };
                    wire::HostFrame::Event {
                        attachment,
                        seq,
                        class,
                        observation,
                        client_id,
                        blobs,
                    }
                }
                F::HostLifecycle {
                    phase: v13::HostLifecyclePhase::Starting,
                    ..
                } => return Ok(None),
                frame @ F::HostLifecycle { .. } => {
                    let mut value = to_value(&frame)?;
                    if let Some(fields) = value.as_object_mut() {
                        fields.remove("seconds_remaining");
                        fields.remove("deadline_at");
                    }
                    serde_json::from_value(value).map_err(|error| invalid(&error))?
                }
                frame @ F::Directory { .. } => {
                    let mut value = to_value(&frame)?;
                    rows(&mut value["rows"]);
                    serde_json::from_value(value).map_err(|error| invalid(&error))?
                }
                frame @ F::Response { .. } => {
                    let mut value = to_value(&frame)?;
                    let answer = &mut value["outcome"]["ok"];
                    match answer["answer"].as_str() {
                        Some("attached") => {
                            if let Some(fields) = answer.as_object_mut() {
                                fields.remove("transcript");
                            }
                        }
                        Some("sessions") => rows(&mut answer["rows"]),
                        Some("options") => {
                            let options = &mut answer["options"];
                            if let Some(runners) = options["runners"].as_array_mut() {
                                for runner in runners {
                                    placement(&mut runner["status"]);
                                }
                            }
                            choices(options);
                            choices(&mut options["workspace"]);
                            if options["workspace"].get("refusal").is_some() {
                                phase(&mut options["workspace"]["refusal"]);
                            }
                        }
                        Some("host") => {
                            let response = &mut answer["response"];
                            match response["response"].as_str() {
                                Some("templates_get") => {
                                    template(&mut response["template"]["body"], "extra");
                                }
                                Some("templates_list") => {
                                    if let Some(entries) = response["rows"].as_array_mut() {
                                        for row in entries {
                                            template(&mut row["summary"], "tier");
                                        }
                                    }
                                }
                                Some("profiles_list") => {
                                    if let Some(entries) = response["rows"].as_array_mut() {
                                        for row in entries {
                                            profile(&mut row["summary"]);
                                        }
                                    }
                                }
                                Some("profiles_get") => {
                                    profile(&mut response["profile"]["summary"]);
                                }
                                _ => {}
                            }
                        }
                        Some(
                            "history_started" | "transcript_row" | "attachment_status_requested",
                        ) => return Ok(None),
                        Some(_) => {}
                        None => {
                            if let Some(refusal) = value["outcome"].get_mut("refused")
                                && refusal["refusal"] == "placement_refused"
                            {
                                phase(&mut refusal["reason"]);
                            }
                        }
                    }
                    if let Some(outcome) = value["outcome"].as_object_mut()
                        && outcome.get("ok").is_some_and(Value::is_null)
                    {
                        outcome.remove("ok");
                    }
                    serde_json::from_value(value).map_err(|error| invalid(&error))?
                }
                frame @ F::OptionsProgress { .. } => {
                    let mut value = to_value(&frame)?;
                    phase(&mut value);
                    if !value["row"].is_null() {
                        placement(&mut value["row"]["status"]);
                    }
                    serde_json::from_value(value).map_err(|error| invalid(&error))?
                }
                _ => return Ok(None),
            }
        }
    }))
}

fn invalid(error: &serde_json::Error) -> wire::WireError {
    wire::WireError::Malformed(error.to_string())
}

fn rows(value: &mut Value) {
    let Some(rows) = value.as_array_mut() else {
        return;
    };
    for row in rows {
        let Some(fields) = row.as_object_mut() else {
            continue;
        };
        let attention = fields.remove("attention").unwrap_or_default();
        fields.insert(
            "needs_operator".into(),
            json!(attention.as_array().is_some_and(|reasons| {
                reasons
                    .iter()
                    .any(|reason| reason["reason"] != "interrupted_by_upgrade")
            })),
        );
        for field in ["preview", "cost_root", "cost_tree"] {
            fields.remove(field);
        }
        placement(&mut row["runner_status"]);
        differences(row);
    }
}

fn placement(value: &mut Value) {
    if value.get("reason").is_some() {
        phase(&mut value["reason"]);
    }
}

fn phase(value: &mut Value) {
    if let Some(p) = value.get_mut("phase")
        && let Some(name) = p["phase"].as_str()
    {
        *p = if name == "reconcile"
            && let Some(subphase) = p["subphase"].as_str()
        {
            json!(format!("reconcile_{subphase}"))
        } else {
            json!(name)
        };
    }
}

fn choices(value: &mut Value) {
    if let Some(templates) = value.get_mut("templates").and_then(Value::as_array_mut) {
        for row in templates {
            template(row, "tier");
        }
    }
    if let Some(rows) = value
        .get_mut("profiles")
        .and_then(|p| p.get_mut("rows"))
        .and_then(Value::as_array_mut)
    {
        for row in rows {
            profile(row);
        }
    }
}

fn template(row: &mut Value, field: &str) {
    if let Some(fields) = row.as_object_mut() {
        let tier = fields.remove("service_tier").unwrap_or_default();
        let extra = tier
            .as_str()
            .map(|tier| if tier == "fast" { "priority" } else { tier });
        fields.insert(
            field.into(),
            if field == "tier" {
                json!(extra.unwrap_or("standard"))
            } else {
                json!(extra)
            },
        );
    }
}

fn profile(row: &mut Value) {
    fn role(row: &mut Value) {
        if let Some(fields) = row.as_object_mut() {
            let named = fields
                .remove("service_tier")
                .is_some_and(|value| !value.is_null());
            fields.insert("fast".into(), json!(named));
        }
    }
    differences(row);
    role(&mut row["root"]);
    if let Some(cards) = row["cards"].as_array_mut() {
        for card in cards {
            role(card);
        }
    }
}

fn profile_row(
    row: v13::ProfileRow,
) -> Result<plakous_host_protocol::views::ProfileRow, wire::WireError> {
    let mut value = serde_json::to_value(row).map_err(|error| invalid(&error))?;
    profile(&mut value);
    serde_json::from_value(value).map_err(|error| invalid(&error))
}

fn differences(row: &mut Value) {
    if let Some(differences) = row.get_mut("differences").and_then(Value::as_array_mut) {
        for difference in differences {
            if let Some(named) = difference.get("root_service_tier") {
                *difference = json!({"root_fast": {"session": !named["session"].is_null()}});
            }
        }
    }
}
