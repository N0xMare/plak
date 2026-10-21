//! Canonical bundle encodings of manifest metadata, provider configuration,
//! components, and ledger records.

#![allow(
    clippy::wildcard_imports,
    reason = "a split of the manifest module that shares its vocabulary"
)]

use super::*;

pub(crate) struct RuntimeManifestBundleMetadata {
    provenance: ProducerProvenance,
    pub(crate) topology: RuntimeTopology,
    pub(crate) lifecycle: RuntimeLifecycle,
    interpreter_observation: Option<InterpreterObservation>,
    jobs: JobLedger,
    pub(crate) durability: RuntimeDurability,
}

pub(crate) fn encode_bundle_metadata(
    manifest: &RuntimeManifest,
) -> Result<Vec<u8>, RuntimeManifestError> {
    encode_bundle_metadata_value(&RuntimeManifestBundleMetadata {
        provenance: manifest.provenance.clone(),
        topology: manifest.topology,
        lifecycle: manifest.lifecycle.clone(),
        interpreter_observation: manifest.interpreter_observation.clone(),
        jobs: manifest.jobs.clone(),
        durability: manifest.durability,
    })
}

pub(crate) fn encode_bundle_metadata_value(
    metadata: &RuntimeManifestBundleMetadata,
) -> Result<Vec<u8>, RuntimeManifestError> {
    let mut writer = Writer::default();
    writer.string(&metadata.provenance.producer, MAX_PROVENANCE_BYTES)?;
    writer.string(&metadata.provenance.build, MAX_PROVENANCE_BYTES)?;
    writer.string(&metadata.provenance.features, MAX_PROVENANCE_BYTES)?;
    writer.topology(metadata.topology)?;
    writer.lifecycle(&metadata.lifecycle)?;
    writer.u32(metadata.jobs.next_id());
    writer.usize(metadata.jobs.rows().len())?;
    for row in metadata.jobs.rows() {
        writer.u32(row.number);
        writer.string(&row.command, 1024)?;
        writer.u64(row.started_at.seconds());
        writer.u32(row.started_at.nanoseconds());
        writer.string(&row.owner, 128)?;
        writer.bytes.extend_from_slice(&row.session_id.as_bytes());
        writer.u64(row.owner_generation.get());
        writer.optional(row.terminal.is_some());
        if let Some(terminal) = &row.terminal {
            writer.u8(match terminal.status {
                JobTerminalStatus::Exited => 1,
                JobTerminalStatus::Stopped => 2,
            });
            writer.optional(terminal.exit_code.is_some());
            if let Some(code) = terminal.exit_code {
                writer.u32(u32::from_be_bytes(code.to_be_bytes()));
            }
            writer.optional(terminal.stopper.is_some());
            if let Some(stopper) = terminal.stopper {
                writer.u8(match stopper {
                    plakous_tools::JobStopReason::Model => 1,
                    plakous_tools::JobStopReason::User => 2,
                    plakous_tools::JobStopReason::SessionExit => 3,
                    plakous_tools::JobStopReason::AttemptEnded => 4,
                    plakous_tools::JobStopReason::HardCap => 5,
                });
            }
            writer.u64(terminal.elapsed.as_secs());
            writer.u32(terminal.elapsed.subsec_nanos());
            writer.string(&terminal.tail, 4096)?;
        }
    }
    writer.optional(metadata.interpreter_observation.is_some());
    if let Some(observation) = &metadata.interpreter_observation {
        writer.u64(observation.generation);
        writer.u8(match observation.availability {
            InterpreterAvailability::Available => 1,
            InterpreterAvailability::Reset => 2,
        });
        writer.optional(observation.reset_reason.is_some());
        if let Some(reason) = observation.reset_reason {
            writer.u8(match reason {
                InterpreterResetReason::ProcessReopen => 1,
                InterpreterResetReason::HostLost => 2,
                InterpreterResetReason::ProtocolViolation => 3,
                InterpreterResetReason::Cancellation => 4,
                InterpreterResetReason::Drift => 5,
                InterpreterResetReason::AgentShutdown => 6,
            });
        }
        writer.optional(observation.affected_cell.is_some());
        if let Some(cell) = &observation.affected_cell {
            writer.string(cell, MAX_ID_BYTES)?;
        }
        writer.optional(observation.last_acknowledged.is_some());
        if let Some(boundary) = observation.last_acknowledged {
            writer.u8(match boundary {
                InterpreterAcknowledgedBoundary::Launch => 1,
                InterpreterAcknowledgedBoundary::SourceAccepted => 2,
                InterpreterAcknowledgedBoundary::ChildResultAccepted => 3,
                InterpreterAcknowledgedBoundary::Terminal => 4,
                InterpreterAcknowledgedBoundary::Join => 5,
            });
        }
    }
    writer.u8(match metadata.durability {
        RuntimeDurability::Required => 1,
        RuntimeDurability::BestEffort => 2,
        RuntimeDurability::Disabled => 3,
    });
    Ok(writer.bytes)
}

pub(crate) fn decode_bundle_metadata(
    bytes: &[u8],
) -> Result<RuntimeManifestBundleMetadata, RuntimeManifestError> {
    let mut reader = Reader::new(bytes);
    let provenance = ProducerProvenance::new(
        reader.string(MAX_PROVENANCE_BYTES)?,
        reader.string(MAX_PROVENANCE_BYTES)?,
        reader.string(MAX_PROVENANCE_BYTES)?,
    )?;
    let topology = reader.topology()?;
    let lifecycle = reader.lifecycle()?;
    let next_id = reader.u32()?;
    let count = reader.count()?;
    if count > MAX_JOB_ROWS {
        return Err(RuntimeManifestError::RecordLimit);
    }
    let mut rows = Vec::with_capacity(count);
    for _ in 0..count {
        rows.push(JobRow {
            number: reader.u32()?,
            command: reader.string(1024)?,
            started_at: Timestamp::new(reader.u64()?, reader.u32()?)
                .map_err(|_| RuntimeManifestError::Malformed)?,
            owner: reader.string(128)?,
            session_id: reader.process_session_id()?,
            owner_generation: reader.process_session_owner_generation()?,
            terminal: reader
                .optional()?
                .then(|| {
                    let status = match reader.u8()? {
                        1 => JobTerminalStatus::Exited,
                        2 => JobTerminalStatus::Stopped,
                        tag => {
                            return Err(RuntimeManifestError::UnknownTag {
                                field: "job terminal status",
                                tag,
                            });
                        }
                    };
                    let exit_code = reader
                        .optional()?
                        .then(|| {
                            reader
                                .u32()
                                .map(|code| i32::from_be_bytes(code.to_be_bytes()))
                        })
                        .transpose()?;
                    let stopper = reader
                        .optional()?
                        .then(|| {
                            Ok(match reader.u8()? {
                                1 => plakous_tools::JobStopReason::Model,
                                2 => plakous_tools::JobStopReason::User,
                                3 => plakous_tools::JobStopReason::SessionExit,
                                4 => plakous_tools::JobStopReason::AttemptEnded,
                                5 => plakous_tools::JobStopReason::HardCap,
                                tag => {
                                    return Err(RuntimeManifestError::UnknownTag {
                                        field: "job stopper",
                                        tag,
                                    });
                                }
                            })
                        })
                        .transpose()?;
                    let seconds = reader.u64()?;
                    let nanos = reader.u32()?;
                    if nanos >= 1_000_000_000 {
                        return Err(RuntimeManifestError::Malformed);
                    }
                    Ok(JobTerminal {
                        status,
                        exit_code,
                        stopper,
                        elapsed: std::time::Duration::new(seconds, nanos),
                        tail: reader.string(4096)?,
                    })
                })
                .transpose()?,
        });
    }
    let jobs = JobLedger::new(next_id, rows)?;
    let interpreter_observation = reader
        .optional()?
        .then(|| {
            let generation = reader.u64()?;
            let availability = match reader.u8()? {
                1 => InterpreterAvailability::Available,
                2 => InterpreterAvailability::Reset,
                tag => {
                    return Err(RuntimeManifestError::UnknownTag {
                        field: "interpreter availability",
                        tag,
                    });
                }
            };
            let reset_reason = reader
                .optional()?
                .then(|| match reader.u8()? {
                    1 => Ok(InterpreterResetReason::ProcessReopen),
                    2 => Ok(InterpreterResetReason::HostLost),
                    3 => Ok(InterpreterResetReason::ProtocolViolation),
                    4 => Ok(InterpreterResetReason::Cancellation),
                    5 => Ok(InterpreterResetReason::Drift),
                    6 => Ok(InterpreterResetReason::AgentShutdown),
                    tag => Err(RuntimeManifestError::UnknownTag {
                        field: "interpreter reset reason",
                        tag,
                    }),
                })
                .transpose()?;
            let affected_cell = reader
                .optional()?
                .then(|| reader.string(MAX_ID_BYTES))
                .transpose()?;
            let last_acknowledged = reader
                .optional()?
                .then(|| match reader.u8()? {
                    1 => Ok(InterpreterAcknowledgedBoundary::Launch),
                    2 => Ok(InterpreterAcknowledgedBoundary::SourceAccepted),
                    3 => Ok(InterpreterAcknowledgedBoundary::ChildResultAccepted),
                    4 => Ok(InterpreterAcknowledgedBoundary::Terminal),
                    5 => Ok(InterpreterAcknowledgedBoundary::Join),
                    tag => Err(RuntimeManifestError::UnknownTag {
                        field: "interpreter acknowledged boundary",
                        tag,
                    }),
                })
                .transpose()?;
            InterpreterObservation::new(
                generation,
                availability,
                reset_reason,
                affected_cell,
                last_acknowledged,
            )
        })
        .transpose()?;
    let durability = match reader.u8()? {
        1 => RuntimeDurability::Required,
        2 => RuntimeDurability::BestEffort,
        3 => RuntimeDurability::Disabled,
        tag => {
            return Err(RuntimeManifestError::UnknownTag {
                field: "durability",
                tag,
            });
        }
    };
    reader.finish()?;
    Ok(RuntimeManifestBundleMetadata {
        provenance,
        topology,
        lifecycle,
        interpreter_observation,
        jobs,
        durability,
    })
}

#[allow(
    clippy::too_many_arguments,
    reason = "the arguments are the closed manifest groups"
)]
pub(crate) fn assemble_bundle_manifest(
    metadata: RuntimeManifestBundleMetadata,
    harness_plan: ReconstructiveComponent,
    provider_configuration: CompiledProviderConfiguration,
    canonical: RuntimeCanonicalState,
    binding: ModelRouteBinding,
    tools: ToolRuntimeManifest,
    ledger: RuntimeOperationLedger,
    family_artifact: Option<FamilyArtifact>,
) -> Result<RuntimeManifest, RuntimeManifestError> {
    RuntimeManifest::new(
        metadata.provenance,
        metadata.topology,
        metadata.lifecycle,
        harness_plan,
        provider_configuration,
        canonical,
        binding,
        tools,
        ledger,
        family_artifact,
        metadata.interpreter_observation,
        metadata.jobs,
        metadata.durability,
    )
}

pub(crate) fn encode_bundle_provider_configuration(
    value: &CompiledProviderConfiguration,
) -> Result<Vec<u8>, RuntimeManifestError> {
    let payload = serde_json::to_vec(value)
        .map_err(|_| RuntimeManifestError::InvalidProviderConfiguration)?;
    if payload.is_empty() || payload.len().saturating_add(12) > MAX_RUNTIME_COMPONENT_BYTES {
        return Err(RuntimeManifestError::ComponentSize);
    }
    let mut bytes = Vec::with_capacity(payload.len().saturating_add(12));
    bytes.extend_from_slice(&PROVIDER_CONFIGURATION_MAGIC);
    bytes.extend_from_slice(&RUNTIME_PROVIDER_CONFIGURATION_CODEC_REVISION.to_be_bytes());
    bytes.extend_from_slice(&payload);
    Ok(bytes)
}

pub(crate) fn decode_bundle_provider_configuration(
    bytes: &[u8],
) -> Result<CompiledProviderConfiguration, RuntimeManifestError> {
    if bytes.len() <= 12 || bytes.len() > MAX_RUNTIME_COMPONENT_BYTES {
        return Err(RuntimeManifestError::ComponentSize);
    }
    if bytes[..8] != PROVIDER_CONFIGURATION_MAGIC {
        return Err(RuntimeManifestError::InvalidMagic);
    }
    let revision = u32::from_be_bytes(
        bytes[8..12]
            .try_into()
            .map_err(|_| RuntimeManifestError::Malformed)?,
    );
    if revision != RUNTIME_PROVIDER_CONFIGURATION_CODEC_REVISION {
        return Err(RuntimeManifestError::UnsupportedVersion { found: revision });
    }
    serde_json::from_slice(&bytes[12..])
        .map_err(|_| RuntimeManifestError::InvalidProviderConfiguration)
}

macro_rules! bundle_value_codec {
    ($encode:ident, $decode:ident, $ty:ty, $write:ident, $read:ident) => {
        pub(crate) fn $encode(value: &$ty) -> Result<Vec<u8>, RuntimeManifestError> {
            let mut writer = Writer::default();
            writer.$write(value)?;
            Ok(writer.bytes)
        }

        pub(crate) fn $decode(bytes: &[u8]) -> Result<$ty, RuntimeManifestError> {
            let mut reader = Reader::new(bytes);
            let value = reader.$read()?;
            reader.finish()?;
            Ok(value)
        }
    };
}

bundle_value_codec!(
    encode_bundle_stable_prefix,
    decode_bundle_stable_prefix,
    StableRequestPrefix,
    stable_prefix,
    stable_prefix
);
bundle_value_codec!(
    encode_bundle_history_segment,
    decode_bundle_history_segment,
    HistorySegment,
    segment,
    segment
);
bundle_value_codec!(
    encode_bundle_binding,
    decode_bundle_binding,
    ModelRouteBinding,
    binding,
    binding
);
bundle_value_codec!(
    encode_bundle_tools,
    decode_bundle_tools,
    ToolRuntimeManifest,
    tools,
    tools
);
bundle_value_codec!(
    encode_bundle_family_artifact,
    decode_bundle_family_artifact,
    FamilyArtifact,
    family_artifact,
    family_artifact
);

pub(crate) fn encode_bundle_component(
    component: &ReconstructiveComponent,
) -> Result<Vec<u8>, RuntimeManifestError> {
    let mut writer = Writer::default();
    writer.component(component)?;
    Ok(writer.bytes)
}

pub(crate) fn decode_bundle_component(
    bytes: &[u8],
) -> Result<ReconstructiveComponent, RuntimeManifestError> {
    let expected =
        RuntimeComponentKind::from_tag(*bytes.first().ok_or(RuntimeManifestError::Malformed)?)?;
    let mut reader = Reader::new(bytes);
    let value = reader.component(expected)?;
    reader.finish()?;
    Ok(value)
}

pub(crate) fn encode_bundle_command_record(
    record: &DurableCommandRecord,
) -> Result<Vec<u8>, RuntimeManifestError> {
    encode_command_record_body(record)
}

pub(crate) fn decode_bundle_command_record(
    bytes: &[u8],
) -> Result<DurableCommandRecord, RuntimeManifestError> {
    let mut reader = Reader::new(bytes);
    let record = reader.command_record()?;
    reader.finish()?;
    Ok(record)
}

/// Encodes the persisted record, naming each component by its object digest.
pub(crate) fn encode_bundle_operation_record(
    record: &DurableOperationRecord,
) -> Result<Vec<u8>, RuntimeManifestError> {
    OperationRecordWire::new(record, crate::checkpoint_bundle::component_object_digest)?.encode()
}

pub(super) fn encode_command_record_body(
    record: &DurableCommandRecord,
) -> Result<Vec<u8>, RuntimeManifestError> {
    let mut writer = Writer::default();
    writer.command_record(record)?;
    Ok(writer.bytes)
}

pub(super) fn ledger_prefix_identity(
    kind: &[u8],
    previous: Option<Digest>,
    record: &[u8],
) -> Result<Digest, RuntimeManifestError> {
    let previous_present = [u8::from(previous.is_some())];
    Digest::sha256(
        DigestDomain::Blob,
        1,
        &[
            b"plakous-agent/runtime-ledger-prefix-v1",
            kind,
            &previous_present,
            previous.as_ref().map_or(&[][..], |digest| digest.bytes()),
            record,
        ],
    )
    .map_err(|_| RuntimeManifestError::InvalidDigest)
}

pub(crate) fn encode_bundle_digest(digest: Digest) -> Result<Vec<u8>, RuntimeManifestError> {
    let mut writer = Writer::default();
    writer.digest(digest)?;
    Ok(writer.bytes)
}

pub(crate) fn decode_bundle_digest(bytes: &[u8]) -> Result<Digest, RuntimeManifestError> {
    let mut reader = Reader::new(bytes);
    let digest = reader.digest(None)?;
    reader.finish()?;
    Ok(digest)
}
