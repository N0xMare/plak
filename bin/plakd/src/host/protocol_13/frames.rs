use plakd_wire_types::{PlakExtension, SurfaceObservation, console, v13};
use plakous_host_protocol::{SessionObservation, wire};
use plakous_hosted::host_service::wire::{ClientInput, TranscriptRead, ViewportRevision};
use wire::{ClientFrame, HostFrame, Op, WireError};

pub(in crate::host) fn decode(line: &str) -> Result<ClientInput<PlakExtension>, WireError> {
    let (id, op) = match wire::decode_line::<v13::ClientFrame>(line)? {
        v13::ClientFrame::Shared(frame) => return Ok(ClientInput::Frame(frame.into_inner(), None)),
        v13::ClientFrame::Draft(v13::ClientFrameV13::Request { id, op }) => (id, *op),
    };
    let op = match op {
        v13::Op::Shared(op) => op.into_inner(),
        v13::Op::Draft(v13::OpV13::Attach { node, retained }) => {
            let retained = retained.and_then(|r| {
                let token = uuid::Uuid::parse_str(&r.token).ok()?;
                (token.to_string() == r.token).then_some(ViewportRevision {
                    host_generation: r.host_generation,
                    revision: r.revision,
                    token,
                })
            });
            return Ok(ClientInput::Frame(
                ClientFrame::Request {
                    id,
                    op: Op::Attach { node },
                },
                retained,
            ));
        }
        v13::Op::Draft(v13::OpV13::Host { request }) => Op::Host {
            request: match *request {
                v13::HostRequest::Shared(request) => request.into_inner(),
                v13::HostRequest::Draft(v13::HostRequestV13::TemplatePut {
                    id,
                    changes,
                    if_match,
                    confirmation,
                }) => console::HostRequest::TemplatePut {
                    id,
                    changes: changes
                        .into_iter()
                        .map(|change| match change {
                            v13::TemplateChange::Shared(change) => change.into_inner(),
                            v13::TemplateChange::Draft(v13::TemplateChangeV13::ServiceTier {
                                value,
                            }) => console::TemplateChange::ServiceTier { value },
                        })
                        .collect(),
                    if_match,
                    confirmation,
                },
            },
        },
        v13::Op::Draft(v13::OpV13::OlderHistory { attachment, cursor }) => {
            return Ok(ClientInput::Transcript(
                id,
                attachment,
                TranscriptRead::Older(cursor.0),
            ));
        }
        v13::Op::Draft(v13::OpV13::ReadTranscriptRow {
            attachment,
            row,
            offset,
        }) => {
            return Ok(ClientInput::Transcript(
                id,
                attachment,
                TranscriptRead::Row(row, offset),
            ));
        }
        v13::Op::Draft(v13::OpV13::AttachmentStatus { attachment }) => {
            return Ok(ClientInput::Transcript(
                id,
                attachment,
                TranscriptRead::Status,
            ));
        }
    };
    Ok(ClientInput::Frame(ClientFrame::Request { id, op }, None))
}

pub(in crate::host) fn encode(
    frame: HostFrame<PlakExtension>,
    directory: impl FnOnce(Vec<wire::SessionRow>) -> Vec<v13::SessionRow>,
) -> Result<Option<String>, WireError> {
    use v13::HostFrameV13 as F;
    let frame = match frame {
        HostFrame::Response { id, outcome } => F::Response {
            id,
            outcome: match outcome {
                wire::Outcome::Refused(reason) => {
                    v13::Outcome::Refused(Box::new(super::options::refusal(reason)))
                }
                wire::Outcome::Ok(wire::Answer::Host { response }) => {
                    return wire::encode_line(&host_response(id, response)?).map(Some);
                }
                wire::Outcome::Ok(answer) => v13::Outcome::Ok(Box::new(match answer {
                    wire::Answer::Attached { attachment } => {
                        v13::Answer::Draft(v13::AnswerV13::Attached {
                            attachment,
                            transcript: v13::AttachTranscript::FirstPage,
                        })
                    }
                    wire::Answer::Sessions { rows } => {
                        v13::Answer::Draft(v13::AnswerV13::Sessions {
                            rows: directory(rows),
                        })
                    }
                    wire::Answer::Options { options } => {
                        v13::Answer::Draft(v13::AnswerV13::Options {
                            options: Box::new(super::options::options(*options)),
                        })
                    }
                    shared => v13::Answer::unchanged(shared).ok_or_else(invalid)?,
                })),
            },
        },
        HostFrame::Directory {
            attachment,
            seq,
            snapshot,
            rows,
        } => F::Directory {
            attachment,
            seq,
            snapshot,
            rows: directory(rows),
        },
        HostFrame::OptionsProgress {
            request_id,
            runner,
            phase,
            row,
        } => F::OptionsProgress {
            request_id,
            runner,
            phase: phase.and_then(super::options::phase),
            row: row.map(super::options::runner).map(Box::new),
        },
        HostFrame::Event {
            attachment,
            seq,
            class,
            observation,
            client_id,
            blobs,
        } => {
            let observation = event(observation).ok_or_else(invalid)?;
            F::Event {
                attachment,
                seq,
                class,
                observation: Box::new(observation),
                client_id,
                blobs,
            }
        }
        shared => {
            return wire::encode_line(&v13::HostFrame::unchanged(shared).ok_or_else(invalid)?)
                .map(Some);
        }
    };
    wire::encode_line(&frame).map(Some)
}

fn invalid() -> WireError {
    WireError::Malformed("frame has no protocol 13 projection".into())
}

fn host_response(
    id: wire::RequestId,
    response: console::HostResponse,
) -> Result<v13::HostFrame, WireError> {
    use console::HostResponse as R;
    use v13::HostResponseV13 as D;
    let response = match response {
        r @ R::AccountsUsage { .. } => {
            return crate::draft_wire::accounts_usage(id, r).ok_or_else(invalid);
        }
        r @ R::Refused {
            reason:
                console::ReadRefusal::WorkspaceReadFailed {
                    version: Some(_), ..
                },
        } => return crate::draft_wire::changes_refusal(id, r).ok_or_else(invalid),
        R::Refused { reason } => D::Refused {
            reason: v13::ReadRefusal::Shared(reason),
        },
        R::ProfilesList {
            etag,
            current,
            rows,
            diagnostics,
            page,
        } => D::ProfilesList {
            etag,
            current,
            diagnostics,
            page,
            rows: rows
                .into_iter()
                .map(|row| v13::ProfileSummary {
                    id: row.id,
                    kind: row.kind,
                    summary: super::options::profile(row.summary),
                })
                .collect(),
        },
        R::ProfilesGet { profile } => D::ProfilesGet {
            profile: Box::new(v13::ProfileDefinition {
                id: profile.id,
                kind: profile.kind,
                etag: profile.etag,
                body: profile.body,
                summary: super::options::profile(profile.summary),
                diagnostics: profile.diagnostics,
            }),
        },
        R::TemplatesList {
            etag,
            rows,
            diagnostics,
            page,
        } => D::TemplatesList {
            etag,
            diagnostics,
            page,
            rows: rows
                .into_iter()
                .map(|row| v13::TemplateSummary {
                    id: row.id,
                    source: row.source,
                    summary: super::options::definition_template(row.summary),
                })
                .collect(),
        },
        R::TemplatesGet { template } => {
            let body = template.body;
            D::TemplatesGet {
                template: Box::new(v13::TemplateDefinition {
                    id: template.id,
                    source: template.source,
                    etag: template.etag,
                    diagnostics: template.diagnostics,
                    body: v13::TemplateBody {
                        account: body.account,
                        model: body.model,
                        effort: body.effort,
                        service_tier: super::options::tier(body.extra.as_deref()),
                        composition: body.composition,
                        protocol: body.protocol,
                        description: body.description,
                        max_tier: body.max_tier,
                        tools: body.tools,
                        spawns: body.spawns,
                        may_adopt: body.may_adopt,
                        delegation: body.delegation,
                        cost_tier: body.cost_tier,
                        max_turns: body.max_turns,
                        show_model: body.show_model,
                        on_mint: body.on_mint,
                        env: body.env,
                    },
                }),
            }
        }
        shared => {
            return Ok(response_frame(
                id,
                v13::HostResponse::unchanged(shared).ok_or_else(invalid)?,
            ));
        }
    };
    Ok(response_frame(id, v13::HostResponse::Draft(response)))
}

fn response_frame(id: wire::RequestId, response: v13::HostResponse) -> v13::HostFrame {
    v13::HostFrame::Draft(Box::new(v13::HostFrameV13::Response {
        id,
        outcome: v13::Outcome::Ok(Box::new(v13::Answer::Draft(v13::AnswerV13::Host {
            response: Box::new(response),
        }))),
    }))
}

pub(in crate::host) fn observes(observation: &SessionObservation<PlakExtension>) -> bool {
    !observation.is_unkeyed_text_preview()
}

fn event(observation: SessionObservation<PlakExtension>) -> Option<v13::Observation> {
    use SessionObservation as O;
    use plakous_host_protocol::views::transcript::TranscriptStreamDelta as D;
    use v13::ObservationV13 as V;
    Some(v13::Observation::Draft(match observation {
        O::TranscriptStream {
            scope,
            delta: D::Tool(tool),
        } => V::TranscriptToolStream {
            scope,
            tool: Box::new((*tool).into()),
        },
        O::TranscriptStream { delta, .. } => return super::append(delta),
        O::UsageLimits(view) => crate::draft_wire::usage_limits(view),
        O::TemplateReadiness(rows) => {
            V::TemplateReadiness(rows.into_iter().map(super::options::template).collect())
        }
        O::Extension(ext) => V::Extension(match ext {
            SurfaceObservation::Profiles(view) => v13::SurfaceObservation::Draft(
                v13::SurfaceObservationV13::Profiles(super::options::profiles(view)),
            ),
            SurfaceObservation::SessionProfile(row) => v13::SurfaceObservation::Draft(
                v13::SurfaceObservationV13::SessionProfile(Box::new(super::options::profile(row))),
            ),
            shared => v13::SurfaceObservation::unchanged(shared)?,
        }),
        shared => return v13::Observation::unchanged(shared),
    }))
}
