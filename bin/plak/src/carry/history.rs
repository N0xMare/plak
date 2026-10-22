//! Plak-owned export-v8 vocabulary. Never serialize canonical runtime values here.
use base64::Engine as _;
use plakous::model::{
    Actor, AudioBlock, CallId, CanonicalHistory, CitationBlock, CitationRange, CitationSource,
    CitationTarget, CitationUnit, CompactionBlock, CompactionReplayBinding, ContentBlock,
    ConversationItem, DataClassification, Digest, DigestDomain, DocumentBlock, EntryId,
    HistorySegment, HistorySegmentKind, ImageBlock, ImageDetailHint, McpServerId, MediaSource,
    ModelFamilyId, ModelId, NamespaceId, OpaqueBlock, OpaqueFallback, ProtocolId, Provenance,
    ReasoningBlock, ReasoningVisibility, RefusalBlock, ReplayEnvelope, ReplayKindId, ReplayScope,
    SummarizedTurns, TextBlock, TextPhase, ToolCallBlock, ToolCallState, ToolCallerLineage,
    ToolCorrelation, ToolExecutionOwner, ToolName, ToolResultBlock, ToolResultKind, ToolResultPart,
    ToolResultUpdateBlock,
};
use plakous::sessions::PortableSessionHistory;
use serde::{Deserialize, Serialize};

use super::ImportError;

type Result<T> = std::result::Result<T, ImportError>;
fn invalid(error: impl std::fmt::Display) -> ImportError {
    ImportError::Invalid(format!("invalid carried history: {error}"))
}
macro_rules! tags {
    ($to:ident, $from:ident, $ty:ident, {$($variant:ident => $tag:literal),+ $(,)?}) => {
        fn $to(value: $ty) -> Result<String> {
            Ok(match value { $($ty::$variant => $tag,)+ _ => return Err(invalid("unsupported content tag")), }.to_owned())
        }
        fn $from(value: &str) -> Result<$ty> {
            match value { $($tag => Ok($ty::$variant),)+ _ => Err(invalid(format!("unknown {} tag {value:?}", stringify!($ty)))) }
        }
    };
}
tags!(actor_out, actor_in, Actor, {Developer=>"developer", System=>"system", User=>"user", Assistant=>"assistant", Tool=>"tool", Harness=>"harness"});
tags!(origin_out, origin_in, Provenance, {UserInput=>"user_input", ModelOutput=>"model_output", ToolExecution=>"tool_execution", Instruction=>"instruction", Imported=>"imported"});
tags!(group_out, group_in, HistorySegmentKind, {Conversation=>"conversation", RecentIntent=>"recent_intent", Steering=>"steering", ToolExchange=>"tool_exchange", ReasoningReplay=>"reasoning_replay", Compaction=>"compaction"});
tags!(phase_out, phase_in, TextPhase, {Commentary=>"commentary", Final=>"final"});
tags!(detail_out, detail_in, ImageDetailHint, {Auto=>"auto", Low=>"low", High=>"high", Original=>"original"});
tags!(visibility_out, visibility_in, ReasoningVisibility, {Visible=>"visible", Summary=>"summary", Omitted=>"omitted", Redacted=>"redacted", OpaqueReplay=>"opaque_replay"});
tags!(state_out, state_in, ToolCallState, {Requested=>"requested", Pending=>"pending", Completed=>"completed", Failed=>"failed"});
tags!(result_out, result_in, ToolResultKind, {Success=>"success", Failure=>"failure"});
tags!(unit_out, unit_in, CitationUnit, {UnicodeScalar=>"unicode_scalar", Utf8Byte=>"utf8_byte", Utf16CodeUnit=>"utf16_code_unit"});
tags!(classification_out, classification_in, DataClassification, {Public=>"public", Sensitive=>"sensitive", Secret=>"secret"});

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct History {
    replay_account: String,
    replay_model: String,
    #[serde(deserialize_with = "bounded_groups")]
    groups: Vec<Group>,
}
#[derive(Serialize)]
struct Group {
    kind: String,
    messages: Vec<Message>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Message {
    id: String,
    actor: String,
    origin: String,
    #[serde(default, deserialize_with = "bounded_subject")]
    presentation_subject: Option<String>,
    #[serde(deserialize_with = "bounded_blocks")]
    blocks: Vec<Block>,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Source {
    Bytes { bytes: String },
    Uri { uri: String },
    DataUrl { data_url: String },
}
impl Source {
    fn export(source: &MediaSource) -> Result<Self> {
        if let Some(bytes) = source.as_bytes() {
            return Ok(Self::Bytes {
                bytes: base64::engine::general_purpose::STANDARD.encode(bytes),
            });
        }
        if let Some(uri) = source.as_uri() {
            return Ok(Self::Uri {
                uri: uri.to_owned(),
            });
        }
        if let Some(data_url) = source.as_data_url() {
            return Ok(Self::DataUrl {
                data_url: data_url.to_owned(),
            });
        }
        Err(invalid("unsupported media source"))
    }
    fn import(self) -> Result<MediaSource> {
        match self {
            Self::Bytes { bytes } => {
                if bytes.len() > (16 * 1024 * 1024_usize).div_ceil(3) * 4 {
                    return Err(invalid("oversized media bytes"));
                }
                MediaSource::bytes(
                    base64::engine::general_purpose::STANDARD
                        .decode(bytes)
                        .map_err(invalid)?,
                )
            }
            Self::Uri { uri } => {
                let parsed = url::Url::parse(&uri).map_err(invalid)?;
                if parsed.scheme() != "https" || parsed.host_str().is_none() {
                    return Err(invalid("media URI must use https"));
                }
                MediaSource::uri(uri)
            }
            Self::DataUrl { data_url } => MediaSource::data_url(data_url),
        }
        .map_err(invalid)
    }
}
#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Owner {
    Local {},
    Mcp { server: String },
    Provider { namespace: String },
}
impl Owner {
    fn export(owner: &ToolExecutionOwner) -> Result<Self> {
        Ok(match owner {
            ToolExecutionOwner::Local => Self::Local {},
            ToolExecutionOwner::Mcp { server } => Self::Mcp {
                server: server.as_str().to_owned(),
            },
            ToolExecutionOwner::ProviderHosted { provider_kind } => Self::Provider {
                namespace: provider_kind.as_str().to_owned(),
            },
            _ => return Err(invalid("unsupported execution owner")),
        })
    }
    fn import(self) -> Result<ToolExecutionOwner> {
        Ok(match self {
            Self::Local {} => ToolExecutionOwner::Local,
            Self::Mcp { server } => ToolExecutionOwner::Mcp {
                server: McpServerId::new(server).map_err(invalid)?,
            },
            Self::Provider { namespace } => ToolExecutionOwner::ProviderHosted {
                provider_kind: NamespaceId::new(namespace).map_err(invalid)?,
            },
        })
    }
}
#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Caller {
    Direct {},
    Provider {
        namespace: String,
        execution_id: String,
    },
    Local {
        operation: u64,
        generation: u64,
        cell: String,
        sequence: u32,
        child: String,
        depth: u16,
    },
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Correlation {
    remote_call: Option<String>,
    container: Option<String>,
    caller: Caller,
}
fn call_id(id: &str) -> Result<CallId> {
    CallId::try_from(uuid::Uuid::parse_str(id).map_err(invalid)?).map_err(invalid)
}
fn entry_id(id: &str) -> Result<EntryId> {
    EntryId::try_from(uuid::Uuid::parse_str(id).map_err(invalid)?).map_err(invalid)
}
impl Correlation {
    fn export(c: &ToolCorrelation) -> Self {
        let caller = match c.caller_lineage() {
            ToolCallerLineage::Direct => Caller::Direct {},
            ToolCallerLineage::ProviderProgrammatic { kind, execution_id } => Caller::Provider {
                namespace: kind.as_str().to_owned(),
                execution_id: execution_id.clone(),
            },
            ToolCallerLineage::LocalProgrammatic {
                evaluation_operation_id,
                interpreter_generation,
                cell_id,
                request_sequence,
                child_call_id,
                depth,
            } => Caller::Local {
                operation: *evaluation_operation_id,
                generation: *interpreter_generation,
                cell: cell_id.clone(),
                sequence: *request_sequence,
                child: child_call_id.to_string(),
                depth: *depth,
            },
        };
        Self {
            remote_call: c.remote_call_id().map(str::to_owned),
            container: c.container_id().map(str::to_owned),
            caller,
        }
    }
    fn import(self) -> Result<ToolCorrelation> {
        let caller = match self.caller {
            Caller::Direct {} => ToolCallerLineage::Direct,
            Caller::Provider {
                namespace,
                execution_id,
            } => ToolCallerLineage::provider_programmatic(
                NamespaceId::new(namespace).map_err(invalid)?,
                execution_id,
            )
            .map_err(invalid)?,
            Caller::Local {
                operation,
                generation,
                cell,
                sequence,
                child,
                depth,
            } => ToolCallerLineage::local_programmatic(
                operation,
                generation,
                cell,
                sequence,
                call_id(&child)?,
                depth,
            )
            .map_err(invalid)?,
        };
        ToolCorrelation::new(self.remote_call, self.container, caller).map_err(invalid)
    }
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Replay {
    protocol: String,
    family: Option<String>,
    scope: String,
    model: Option<String>,
    namespace: String,
    version: u32,
    classification: String,
    payload: String,
}
impl Replay {
    fn export(r: &ReplayEnvelope) -> Result<Self> {
        let (scope, model) = match r.scope() {
            ReplayScope::Protocol => ("protocol", None),
            ReplayScope::ModelFamily => ("family", None),
            ReplayScope::ExactModel(model) => ("model", Some(model.as_str().to_owned())),
            _ => return Err(invalid("unsupported replay scope")),
        };
        Ok(Self {
            protocol: r.protocol().as_str().to_owned(),
            family: r.model_family().map(|f| f.as_str().to_owned()),
            scope: scope.to_owned(),
            model,
            namespace: r.kind().as_str().to_owned(),
            version: r.version(),
            classification: classification_out(r.classification())?,
            payload: r.payload().get().to_owned(),
        })
    }
    fn import(self) -> Result<ReplayEnvelope> {
        let scope = match (self.scope.as_str(), self.model) {
            ("protocol", None) => ReplayScope::Protocol,
            ("family", None) => ReplayScope::ModelFamily,
            ("model", Some(model)) => {
                ReplayScope::ExactModel(ModelId::new(model).map_err(invalid)?)
            }
            _ => return Err(invalid("invalid replay scope")),
        };
        ReplayEnvelope::new(
            ProtocolId::new(self.protocol).map_err(invalid)?,
            self.family
                .map(ModelFamilyId::new)
                .transpose()
                .map_err(invalid)?,
            scope,
            ReplayKindId::new(self.namespace).map_err(invalid)?,
            self.version,
            classification_in(&self.classification)?,
            serde_json::value::RawValue::from_string(self.payload).map_err(invalid)?,
        )
        .map_err(invalid)
    }
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct NativeBinding {
    protocol: String,
    family: Option<String>,
    model: String,
    account: [u8; 32],
    account_version: u32,
}
impl NativeBinding {
    fn export(b: &CompactionReplayBinding) -> Result<Self> {
        if b.account().domain() != DigestDomain::ModelBinding {
            return Err(invalid("unexpected native replay account domain"));
        }
        Ok(Self {
            protocol: b.protocol().as_str().to_owned(),
            family: b.model_family().map(|f| f.as_str().to_owned()),
            model: b.model().as_str().to_owned(),
            account: *b.account().bytes(),
            account_version: b.account().version(),
        })
    }
    fn import(self) -> Result<CompactionReplayBinding> {
        Ok(CompactionReplayBinding::new(
            ProtocolId::new(self.protocol).map_err(invalid)?,
            self.family
                .map(ModelFamilyId::new)
                .transpose()
                .map_err(invalid)?,
            ModelId::new(self.model).map_err(invalid)?,
            Digest::from_sha256_bytes(
                DigestDomain::ModelBinding,
                self.account_version,
                self.account,
            )
            .map_err(invalid)?,
        ))
    }
}
#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Fallback {
    Omit {},
    Summary { text: String },
    CompatibleReplay {},
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Citation {
    item: String,
    block: u32,
    source: String,
    annotation: String,
    uri: Option<String>,
    title: Option<String>,
    range: Option<Range>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Range {
    start: u64,
    end: u64,
    unit: String,
}
#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Block {
    Text {
        text: String,
        phase: Option<String>,
    },
    Image {
        mime: String,
        source: Source,
        detail: Option<String>,
    },
    Document {
        mime: String,
        source: Source,
    },
    Audio {
        mime: String,
        source: Source,
    },
    Call {
        id: String,
        name: String,
        canonical_name: String,
        owner: Owner,
        state: String,
        correlation: Correlation,
        arguments: String,
    },
    Result {
        id: String,
        owner: Owner,
        outcome: String,
        #[serde(deserialize_with = "bounded_result_parts")]
        parts: Vec<Block>,
        correlation: Correlation,
    },
    Update {
        id: String,
        owner: Owner,
        text: String,
        phase: Option<String>,
        correlation: Correlation,
    },
    Reasoning {
        visibility: String,
        text: Option<String>,
        replay: Option<Replay>,
    },
    Refusal {
        text: String,
    },
    Citation {
        citation: Citation,
    },
    Compaction {
        summary: String,
        turns: Option<[u64; 2]>,
        native: Option<String>,
        binding: Option<NativeBinding>,
    },
    Opaque {
        replay: Replay,
        fallback: Fallback,
    },
}
impl Block {
    #[allow(
        clippy::too_many_lines,
        reason = "exhaustive neutral content conversion"
    )]
    fn export(block: &ContentBlock) -> Result<Self> {
        Ok(match block {
            ContentBlock::Text(b) => Self::Text {
                text: b.text().to_owned(),
                phase: b.phase().map(phase_out).transpose()?,
            },
            ContentBlock::Image(b) => Self::Image {
                mime: b.media_type().to_owned(),
                source: Source::export(b.source())?,
                detail: b.detail().map(detail_out).transpose()?,
            },
            ContentBlock::Document(b) => Self::Document {
                mime: b.media_type().to_owned(),
                source: Source::export(b.source())?,
            },
            ContentBlock::Audio(b) => Self::Audio {
                mime: b.media_type().to_owned(),
                source: Source::export(b.source())?,
            },
            ContentBlock::ToolCall(b) => Self::Call {
                id: b.call_id().to_string(),
                name: b.exposed_name().as_str().to_owned(),
                canonical_name: b.canonical_name().as_str().to_owned(),
                owner: Owner::export(b.owner())?,
                state: state_out(b.state())?,
                correlation: Correlation::export(b.correlation()),
                arguments: b.raw_arguments().to_owned(),
            },
            ContentBlock::ToolResult(b) => Self::Result {
                id: b.call_id().to_string(),
                owner: Owner::export(b.owner())?,
                outcome: result_out(b.kind())?,
                parts: b
                    .parts()
                    .iter()
                    .map(|p| match p {
                        ToolResultPart::Text(b) => Self::export(&ContentBlock::Text(b.clone())),
                        ToolResultPart::Image(b) => Self::export(&ContentBlock::Image(b.clone())),
                        ToolResultPart::Document(b) => {
                            Self::export(&ContentBlock::Document(b.clone()))
                        }
                        ToolResultPart::Audio(b) => Self::export(&ContentBlock::Audio(b.clone())),
                        ToolResultPart::Opaque(b) => Self::export(&ContentBlock::Opaque(b.clone())),
                        _ => Err(invalid("unsupported tool result part")),
                    })
                    .collect::<Result<_>>()?,
                correlation: Correlation::export(b.correlation()),
            },
            ContentBlock::ToolResultUpdate(b) => Self::Update {
                id: b.call_id().to_string(),
                owner: Owner::export(b.owner())?,
                text: b.text().text().to_owned(),
                phase: b.text().phase().map(phase_out).transpose()?,
                correlation: Correlation::export(b.correlation()),
            },
            ContentBlock::Reasoning(b) => Self::Reasoning {
                visibility: visibility_out(b.visibility())?,
                text: b.text().map(str::to_owned),
                replay: b.replay().map(Replay::export).transpose()?,
            },
            ContentBlock::Refusal(b) => Self::Refusal {
                text: b.text().to_owned(),
            },
            ContentBlock::Citation(b) => Self::Citation {
                citation: Citation {
                    item: b.target().item().to_string(),
                    block: b.target().block_index(),
                    source: b.source().source_id().to_owned(),
                    annotation: b.source().annotation_id().to_owned(),
                    uri: b.source().uri().map(str::to_owned),
                    title: b.source().title().map(str::to_owned),
                    range: b
                        .range()
                        .map(|r| {
                            Ok::<_, ImportError>(Range {
                                start: r.start(),
                                end: r.end(),
                                unit: unit_out(r.unit())?,
                            })
                        })
                        .transpose()?,
                },
            },
            ContentBlock::Compaction(b) => Self::Compaction {
                summary: b.summary().to_owned(),
                turns: b.summarized_turns().map(|t| [t.prompts(), t.turns()]),
                native: b.native_item().map(str::to_owned),
                binding: b.native_binding().map(NativeBinding::export).transpose()?,
            },
            ContentBlock::Opaque(b) => Self::Opaque {
                replay: Replay::export(b.replay())?,
                fallback: match b.fallback() {
                    OpaqueFallback::Omit => Fallback::Omit {},
                    OpaqueFallback::SemanticSummary(text) => {
                        Fallback::Summary { text: text.clone() }
                    }
                    OpaqueFallback::CompatibleReplay => Fallback::CompatibleReplay {},
                    _ => return Err(invalid("unsupported opaque fallback")),
                },
            },
            _ => return Err(invalid("unsupported content block")),
        })
    }
    #[allow(
        clippy::too_many_lines,
        reason = "exhaustive neutral content reconstruction"
    )]
    fn import(self) -> Result<ContentBlock> {
        Ok(match self {
            Self::Text { text, phase } => ContentBlock::Text(
                TextBlock::new(text, phase.as_deref().map(phase_in).transpose()?)
                    .map_err(invalid)?,
            ),
            Self::Image {
                mime,
                source,
                detail,
            } => ContentBlock::Image(
                ImageBlock::with_detail(
                    mime,
                    source.import()?,
                    detail.as_deref().map(detail_in).transpose()?,
                )
                .map_err(invalid)?,
            ),
            Self::Document { mime, source } => {
                ContentBlock::Document(DocumentBlock::new(mime, source.import()?).map_err(invalid)?)
            }
            Self::Audio { mime, source } => {
                ContentBlock::Audio(AudioBlock::new(mime, source.import()?).map_err(invalid)?)
            }
            Self::Call {
                id,
                name,
                canonical_name,
                owner,
                state,
                correlation,
                arguments,
            } => ContentBlock::ToolCall(
                ToolCallBlock::new(
                    call_id(&id)?,
                    ToolName::new(name).map_err(invalid)?,
                    owner.import()?,
                    state_in(&state)?,
                    correlation.import()?,
                    arguments,
                )
                .map_err(invalid)?
                .with_canonical_name(ToolName::new(canonical_name).map_err(invalid)?),
            ),
            Self::Result {
                id,
                owner,
                outcome,
                parts,
                correlation,
            } => ContentBlock::ToolResult(
                ToolResultBlock::new(
                    call_id(&id)?,
                    owner.import()?,
                    result_in(&outcome)?,
                    parts
                        .into_iter()
                        .map(|p| match p.import()? {
                            ContentBlock::Text(b) => Ok(ToolResultPart::Text(b)),
                            ContentBlock::Image(b) => Ok(ToolResultPart::Image(b)),
                            ContentBlock::Document(b) => Ok(ToolResultPart::Document(b)),
                            ContentBlock::Audio(b) => Ok(ToolResultPart::Audio(b)),
                            ContentBlock::Opaque(b) => Ok(ToolResultPart::Opaque(b)),
                            _ => Err(invalid("invalid tool result part")),
                        })
                        .collect::<Result<_>>()?,
                    correlation.import()?,
                )
                .map_err(invalid)?,
            ),
            Self::Update {
                id,
                owner,
                text,
                phase,
                correlation,
            } => ContentBlock::ToolResultUpdate(
                ToolResultUpdateBlock::new(
                    call_id(&id)?,
                    owner.import()?,
                    TextBlock::new(text, phase.as_deref().map(phase_in).transpose()?)
                        .map_err(invalid)?,
                    correlation.import()?,
                )
                .map_err(invalid)?,
            ),
            Self::Reasoning {
                visibility,
                text,
                replay,
            } => ContentBlock::Reasoning(
                ReasoningBlock::new(
                    visibility_in(&visibility)?,
                    text,
                    replay.map(Replay::import).transpose()?,
                )
                .map_err(invalid)?,
            ),
            Self::Refusal { text } => {
                ContentBlock::Refusal(RefusalBlock::new(text).map_err(invalid)?)
            }
            Self::Citation { citation: c } => ContentBlock::Citation(CitationBlock::new(
                CitationTarget::new(entry_id(&c.item)?, c.block),
                CitationSource::new(c.source, c.annotation, c.uri, c.title).map_err(invalid)?,
                c.range
                    .map(|r| CitationRange::new(r.start, r.end, unit_in(&r.unit)?).map_err(invalid))
                    .transpose()?,
            )),
            Self::Compaction {
                summary,
                turns,
                native,
                binding,
            } => {
                let mut b = CompactionBlock::new(summary).map_err(invalid)?;
                if let Some([prompts, turns]) = turns {
                    b = b.with_summarized_turns(
                        SummarizedTurns::new(prompts, turns)
                            .ok_or_else(|| invalid("invalid summarized turns"))?,
                    );
                }
                if let Some(native) = native {
                    b = b.with_native_item(native);
                }
                if let Some(binding) = binding {
                    b = b.with_native_binding(binding.import()?);
                }
                ContentBlock::Compaction(b)
            }
            Self::Opaque { replay, fallback } => ContentBlock::Opaque(
                OpaqueBlock::new(
                    replay.import()?,
                    match fallback {
                        Fallback::Omit {} => OpaqueFallback::Omit,
                        Fallback::Summary { text } => OpaqueFallback::SemanticSummary(text),
                        Fallback::CompatibleReplay {} => OpaqueFallback::CompatibleReplay,
                    },
                )
                .map_err(invalid)?,
            ),
        })
    }
}
impl std::fmt::Debug for History {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("History")
            .field("groups", &self.groups.len())
            .finish_non_exhaustive()
    }
}

impl History {
    pub(super) fn export(history: &PortableSessionHistory) -> Result<Self> {
        let groups = history
            .segments()
            .iter()
            .map(|g| {
                Ok(Group {
                    kind: group_out(g.kind())?,
                    messages: g
                        .items()
                        .iter()
                        .map(|m| {
                            Ok(Message {
                                id: m.id().to_string(),
                                actor: actor_out(m.actor())?,
                                origin: origin_out(m.provenance())?,
                                presentation_subject: m.presentation_subject().map(str::to_owned),
                                blocks: m
                                    .blocks()
                                    .iter()
                                    .map(Block::export)
                                    .collect::<Result<_>>()?,
                            })
                        })
                        .collect::<Result<_>>()?,
                })
            })
            .collect::<Result<_>>()?;
        let history = Self {
            replay_account: history.replay_account().to_owned(),
            replay_model: history.replay_model().as_str().to_owned(),
            groups,
        };
        history.validate_size()?;
        let mut counter = ByteCounter(0);
        serde_json::to_writer(&mut counter, &history).map_err(invalid)?;
        super::validate_size("history", counter.0, 128 * 1024 * 1024)?;
        Ok(history)
    }
    pub(super) fn validate_size(&self) -> Result<()> {
        super::validate_size("history groups", self.groups.len(), 100_000)?;
        let count = self.groups.iter().map(|g| g.messages.len()).sum::<usize>();
        super::validate_size("history messages", count, 100_000)?;
        for message in self.groups.iter().flat_map(|g| &g.messages) {
            if message.actor == "user" {
                for block in &message.blocks {
                    if let Block::Image {
                        source: Source::Bytes { bytes },
                        ..
                    } = block
                    {
                        super::validate_size(
                            "user image",
                            bytes
                                .len()
                                .saturating_div(4)
                                .saturating_mul(3)
                                .saturating_sub(
                                    bytes
                                        .bytes()
                                        .rev()
                                        .take_while(|b| *b == b'=')
                                        .count()
                                        .min(2),
                                ),
                            plakous::agent::MAX_USER_IMAGE_BYTES,
                        )?;
                    }
                }
            }
        }
        Ok(())
    }
    pub(super) fn import(self) -> Result<PortableSessionHistory> {
        self.validate_size()?;
        let segments = self
            .groups
            .into_iter()
            .map(|g| {
                HistorySegment::new(
                    group_in(&g.kind)?,
                    g.messages
                        .into_iter()
                        .map(|m| {
                            ConversationItem::new(
                                entry_id(&m.id)?,
                                actor_in(&m.actor)?,
                                m.blocks
                                    .into_iter()
                                    .map(Block::import)
                                    .collect::<Result<_>>()?,
                                origin_in(&m.origin)?,
                            )
                            .map(|item| {
                                item.with_presentation_subject(m.presentation_subject.as_deref())
                            })
                            .map_err(invalid)
                        })
                        .collect::<Result<_>>()?,
                )
                .map_err(invalid)
            })
            .collect::<Result<Vec<_>>>()?;
        CanonicalHistory::new(segments.clone()).map_err(invalid)?;
        Ok(PortableSessionHistory::new(
            segments,
            self.replay_account,
            ModelId::new(self.replay_model).map_err(invalid)?,
        ))
    }
    pub(super) fn message_count(&self) -> usize {
        self.groups.iter().map(|g| g.messages.len()).sum()
    }
    pub(super) fn summary(&self) -> String {
        let messages = self.message_count();
        format!(
            "{messages} messages, ~{} tokens; the first request rebuilds the prompt cache",
            self.estimated_tokens()
        )
    }
    pub(super) fn estimated_tokens(&self) -> u64 {
        self.groups
            .iter()
            .flat_map(|g| &g.messages)
            .flat_map(|m| &m.blocks)
            .map(Block::estimated_bytes)
            .sum::<usize>()
            .div_ceil(4)
            .try_into()
            .unwrap_or(u64::MAX)
    }
}

impl Block {
    fn estimated_bytes(&self) -> usize {
        match self {
            Self::Text { text, .. } | Self::Refusal { text } | Self::Update { text, .. } => {
                text.len()
            }
            Self::Call { arguments, .. } => arguments.len(),
            Self::Result { parts, .. } => parts.iter().map(Self::estimated_bytes).sum(),
            Self::Reasoning { text, .. } => text.as_ref().map_or(0, String::len),
            Self::Compaction {
                summary, native, ..
            } => {
                if native.is_some() {
                    4096
                } else {
                    summary.len()
                }
            }
            Self::Opaque {
                fallback: Fallback::Summary { text },
                ..
            } => text.len(),
            Self::Opaque { .. } | Self::Citation { .. } => 0,
            Self::Image { .. } | Self::Audio { .. } | Self::Document { .. } => 4096,
        }
    }
}

fn bounded_blocks<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> std::result::Result<Vec<Block>, D::Error> {
    bounded_sequence(d, 4096)
}
fn bounded_sequence<'de, D, T>(d: D, maximum: usize) -> std::result::Result<Vec<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    struct Items<T>(usize, std::marker::PhantomData<T>);
    impl<'de, T: Deserialize<'de>> serde::de::Visitor<'de> for Items<T> {
        type Value = Vec<T>;
        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("a bounded sequence")
        }
        fn visit_seq<A: serde::de::SeqAccess<'de>>(
            self,
            mut seq: A,
        ) -> std::result::Result<Self::Value, A::Error> {
            let mut items = Vec::new();
            while items.len() < self.0 {
                let Some(item) = seq.next_element()? else {
                    return Ok(items);
                };
                items.push(item);
            }
            if seq.next_element::<serde::de::IgnoredAny>()?.is_some() {
                return Err(serde::de::Error::custom("sequence exceeds carry limit"));
            }
            Ok(items)
        }
    }
    d.deserialize_seq(Items(maximum, std::marker::PhantomData))
}
fn bounded_groups<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> std::result::Result<Vec<Group>, D::Error> {
    use serde::de::{DeserializeSeed, Error, MapAccess, SeqAccess, Visitor};
    struct Groups;
    struct GroupSeed<'a>(&'a mut usize);
    struct Messages<'a>(&'a mut usize);
    impl<'de> DeserializeSeed<'de> for Messages<'_> {
        type Value = Vec<Message>;
        fn deserialize<D: serde::Deserializer<'de>>(
            self,
            d: D,
        ) -> std::result::Result<Self::Value, D::Error> {
            d.deserialize_seq(self)
        }
    }
    impl<'de> Visitor<'de> for Messages<'_> {
        type Value = Vec<Message>;
        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("at most 100000 messages across all groups")
        }
        fn visit_seq<A: SeqAccess<'de>>(
            self,
            mut seq: A,
        ) -> std::result::Result<Self::Value, A::Error> {
            let mut messages = Vec::new();
            while *self.0 > 0 {
                let Some(message) = seq.next_element()? else {
                    return Ok(messages);
                };
                *self.0 -= 1;
                messages.push(message);
            }
            if seq.next_element::<serde::de::IgnoredAny>()?.is_some() {
                return Err(A::Error::custom("history messages exceed 100000"));
            }
            Ok(messages)
        }
    }
    impl<'de> DeserializeSeed<'de> for GroupSeed<'_> {
        type Value = Group;
        fn deserialize<D: serde::Deserializer<'de>>(
            self,
            d: D,
        ) -> std::result::Result<Group, D::Error> {
            d.deserialize_map(self)
        }
    }
    impl<'de> Visitor<'de> for GroupSeed<'_> {
        type Value = Group;
        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("a carry group")
        }
        fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> std::result::Result<Group, A::Error> {
            let (mut kind, mut messages) = (None, None);
            while let Some(key) = map.next_key::<String>()? {
                match key.as_str() {
                    "kind" if kind.is_none() => kind = Some(map.next_value()?),
                    "messages" if messages.is_none() => {
                        messages = Some(map.next_value_seed(Messages(self.0))?);
                    }
                    _ => return Err(A::Error::custom("unknown or duplicate group field")),
                }
            }
            Ok(Group {
                kind: kind.ok_or_else(|| A::Error::missing_field("kind"))?,
                messages: messages.ok_or_else(|| A::Error::missing_field("messages"))?,
            })
        }
    }
    impl<'de> Visitor<'de> for Groups {
        type Value = Vec<Group>;
        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("bounded history groups")
        }
        fn visit_seq<A: SeqAccess<'de>>(
            self,
            mut seq: A,
        ) -> std::result::Result<Self::Value, A::Error> {
            let mut groups = Vec::new();
            let mut remaining = 100_000;
            while groups.len() < 100_000 {
                let Some(group) = seq.next_element_seed(GroupSeed(&mut remaining))? else {
                    return Ok(groups);
                };
                groups.push(group);
            }
            if seq.next_element::<serde::de::IgnoredAny>()?.is_some() {
                return Err(A::Error::custom("history groups exceed 100000"));
            }
            Ok(groups)
        }
    }
    d.deserialize_seq(Groups)
}

fn bounded_subject<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> std::result::Result<Option<String>, D::Error> {
    let subject = Option::<String>::deserialize(d)?;
    if subject.as_ref().is_some_and(|s| s.len() > 4096) {
        return Err(serde::de::Error::custom(
            "presentation subject exceeds 4096 bytes",
        ));
    }
    Ok(subject)
}

struct ByteCounter(usize);
impl std::io::Write for ByteCounter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0 = self.0.saturating_add(bytes.len());
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn bounded_result_parts<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> std::result::Result<Vec<Block>, D::Error> {
    bounded_sequence(d, 1024)
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    reason = "test fixture constructors and assertions"
)]
mod tests;
