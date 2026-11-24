//! The product file's exact schema objects and admission descriptors.

use plakous_recursive_subagents_store::admission::{Revision, Schema};

use super::GENERATION;

pub(super) const SCHEMA_OBJECTS: &[(&str, &str)] = &[
    (
        "product_schema",
        "CREATE TABLE product_schema(singleton INTEGER PRIMARY KEY CHECK(singleton=1),instance_id TEXT NOT NULL UNIQUE,fingerprint TEXT NOT NULL)",
    ),
    (
        "sessions",
        "CREATE TABLE sessions(id TEXT PRIMARY KEY,revision INTEGER NOT NULL CHECK(revision>0),create_operation_id TEXT NOT NULL UNIQUE,workspace_root TEXT NOT NULL,workspace_identity TEXT NOT NULL,created_at INTEGER NOT NULL CHECK(created_at>=0),updated_at INTEGER NOT NULL CHECK(updated_at>=0),account TEXT,model TEXT,effort TEXT,title TEXT,source_session_id TEXT,source_operation_id TEXT,locator TEXT, composition TEXT NOT NULL DEFAULT 'plakous-default' CHECK(composition IN('direct','plakous-default','native-ptc','js-ptc','py-ptc','py-sdk')), target_selection TEXT NOT NULL DEFAULT '\"native\"', frozen_tools TEXT CHECK(frozen_tools IS NULL OR json_valid(frozen_tools)), carry_intent TEXT CHECK(carry_intent IS NULL OR (json_valid(carry_intent) AND locator IS NULL)), node_tree_id TEXT NOT NULL CHECK(length(node_tree_id) BETWEEN 1 AND 128), node_agent_id TEXT NOT NULL CHECK(length(node_agent_id) BETWEEN 1 AND 128), root_profile TEXT CHECK(root_profile IS NULL OR json_valid(root_profile)), runner_display TEXT CHECK(runner_display IS NULL OR json_valid(runner_display)), title_override TEXT, archived INTEGER NOT NULL CHECK(archived IN (0,1)), account_choices TEXT NOT NULL DEFAULT '{}' CHECK(json_valid(account_choices) AND json_type(account_choices)='object'))",
    ),
    (
        "sessions_workspace_updated",
        "CREATE INDEX sessions_workspace_updated ON sessions(workspace_identity,updated_at DESC,id)",
    ),
    (
        "sessions_carry_source",
        "CREATE INDEX sessions_carry_source ON sessions(source_session_id,source_operation_id)",
    ),
    (
        "sessions_node",
        "CREATE INDEX sessions_node ON sessions(node_tree_id,node_agent_id)",
    ),
    (
        "identity_bindings",
        "CREATE TABLE identity_bindings(identity TEXT PRIMARY KEY,kind TEXT NOT NULL CHECK(kind IN('create','queue','delivery','command','workflow')),owner_session_id TEXT NOT NULL,binding_digest TEXT NOT NULL,terminal_disposition TEXT,terminal_outcome TEXT,CHECK((kind='create' AND terminal_disposition IS NULL AND terminal_outcome IS NULL) OR (kind='queue' AND ((terminal_disposition IS NULL AND terminal_outcome IS NULL) OR (terminal_disposition='consumed' AND terminal_outcome IS NOT NULL) OR (terminal_disposition='deleted' AND terminal_outcome IS NULL))) OR (kind IN('delivery','command') AND terminal_disposition IN('rejected','product_committed') AND terminal_outcome IS NULL) OR (kind='workflow' AND ((terminal_disposition IS NULL AND terminal_outcome IS NULL) OR (terminal_disposition='product_committed' AND terminal_outcome IS NOT NULL) OR (terminal_disposition IN('terminal_no_change','discarded') AND terminal_outcome IS NULL)))))",
    ),
    (
        "session_move_seeds",
        "CREATE TABLE session_move_seeds(session_id TEXT PRIMARY KEY REFERENCES sessions(id) ON DELETE CASCADE,operation TEXT NOT NULL,capsule_digest TEXT NOT NULL CHECK(length(capsule_digest)=64),seed_digest TEXT NOT NULL CHECK(length(seed_digest)=64),seed TEXT NOT NULL CHECK(json_valid(seed)))",
    ),
    (
        "queue_items",
        "CREATE TABLE queue_items(session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,item_id TEXT PRIMARY KEY,revision INTEGER NOT NULL CHECK(revision>0),position INTEGER NOT NULL CHECK(position>=0),text TEXT NOT NULL,submitted_at INTEGER NOT NULL CHECK(submitted_at>=0),UNIQUE(session_id,position))",
    ),
    (
        "queue_attachments",
        "CREATE TABLE queue_attachments(item_id TEXT NOT NULL REFERENCES queue_items(item_id) ON DELETE CASCADE,ordinal INTEGER NOT NULL CHECK(ordinal>=0),name TEXT NOT NULL,media_type TEXT NOT NULL,bytes BLOB NOT NULL,PRIMARY KEY(item_id,ordinal))",
    ),
    (
        "prompt_history",
        "CREATE TABLE prompt_history(session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,position INTEGER NOT NULL CHECK(position>=0),prompt TEXT NOT NULL,PRIMARY KEY(session_id,position))",
    ),
    (
        "submissions",
        "CREATE TABLE submissions(session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,command_id TEXT PRIMARY KEY,kind TEXT NOT NULL CHECK(kind IN('root_start','send_now_steer','host_wake')),queue_item_id TEXT NOT NULL,queue_revision INTEGER NOT NULL CHECK(queue_revision>0),target_turn_id TEXT,canonical_command TEXT NOT NULL,submitted_at_ms INTEGER NOT NULL CHECK(submitted_at_ms>=0),anchor INTEGER NOT NULL CHECK(anchor>=0))",
    ),
    (
        "submissions_session",
        "CREATE INDEX submissions_session ON submissions(session_id)",
    ),
    (
        "workflow_requests",
        "CREATE TABLE workflow_requests(session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,operation_id TEXT PRIMARY KEY,source_locator TEXT NOT NULL,kind TEXT NOT NULL CHECK(kind IN('compact','new','fork','rebind','switch')),compact_reason TEXT,selection TEXT,anchor INTEGER NOT NULL CHECK(anchor>=0))",
    ),
    (
        "workflow_requests_session",
        "CREATE INDEX workflow_requests_session ON workflow_requests(session_id)",
    ),
    (
        "operator_notices",
        "CREATE TABLE operator_notices(session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,notice_id TEXT NOT NULL,anchor INTEGER NOT NULL CHECK(anchor>=0),ordinal INTEGER NOT NULL CHECK(ordinal>=0),row TEXT NOT NULL CHECK(json_valid(row) AND json_type(row,'$.kind')='text'),PRIMARY KEY(session_id,notice_id),UNIQUE(session_id,anchor,ordinal))",
    ),
    (
        "host_intents",
        "CREATE TABLE host_intents(session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,kind TEXT NOT NULL CHECK(kind IN('wake_suppression','pause_cause','interrupt_cause','continuation_decision')),anchor INTEGER NOT NULL CHECK(anchor>=0),ordinal INTEGER NOT NULL CHECK(ordinal>=0),value TEXT NOT NULL CHECK(json_valid(value)),PRIMARY KEY(session_id,kind,anchor,ordinal))",
    ),
    (
        "projector_cursors",
        "CREATE TABLE projector_cursors(session_id TEXT PRIMARY KEY REFERENCES sessions(id) ON DELETE CASCADE,runtime_session_id TEXT NOT NULL,applied_seq INTEGER NOT NULL CHECK(applied_seq>=0))",
    ),
    (
        "session_views",
        "CREATE TABLE session_views(session_id TEXT PRIMARY KEY REFERENCES sessions(id) ON DELETE CASCADE,head_locator TEXT,root_turn_count INTEGER NOT NULL CHECK(root_turn_count>=0),tool_call_count INTEGER NOT NULL CHECK(tool_call_count>=0),compaction_count INTEGER NOT NULL CHECK(compaction_count>=0),state TEXT NOT NULL CHECK(json_valid(state) AND json_type(state,'$.calls') IS NULL AND json_type(state,'$.open_turns') IS NULL AND json_type(state,'$.processes') IS NULL))",
    ),
    (
        "session_call_views",
        "CREATE TABLE session_call_views(session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,call_id TEXT NOT NULL,value TEXT NOT NULL CHECK(json_valid(value)),PRIMARY KEY(session_id,call_id))",
    ),
    (
        "session_open_turn_views",
        "CREATE TABLE session_open_turn_views(session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,turn_id TEXT NOT NULL,value TEXT NOT NULL CHECK(json_valid(value)),PRIMARY KEY(session_id,turn_id))",
    ),
    (
        "session_process_views",
        "CREATE TABLE session_process_views(session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,process_id INTEGER NOT NULL CHECK(process_id>=0),value TEXT NOT NULL CHECK(json_valid(value)),PRIMARY KEY(session_id,process_id))",
    ),
    (
        "transcript_rows",
        "CREATE TABLE transcript_rows(session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,anchor INTEGER NOT NULL CHECK(anchor>=0),lane INTEGER NOT NULL CHECK(lane=0),ordinal INTEGER NOT NULL CHECK(ordinal>=0),row TEXT NOT NULL CHECK(json_valid(row) AND json_type(row,'$.kind')='text'),call_id TEXT,PRIMARY KEY(session_id,anchor,lane,ordinal))",
    ),
    (
        "transcript_rows_call",
        "CREATE UNIQUE INDEX transcript_rows_call ON transcript_rows(session_id,call_id) WHERE call_id IS NOT NULL",
    ),
    (
        "agent_acknowledgements",
        "CREATE TABLE agent_acknowledgements(tree_id TEXT NOT NULL,agent_id TEXT NOT NULL,attempt_id TEXT NOT NULL,attention TEXT NOT NULL,ended_at INTEGER,acknowledged_at INTEGER NOT NULL CHECK(acknowledged_at>=0),CHECK(ended_at IS NULL OR ended_at>=0),PRIMARY KEY(tree_id,agent_id,attempt_id,attention,ended_at))",
    ),
];

const BASE_FINGERPRINT: &str = "9c64f5a3da9370dfab12e9acd4cd6568bbf2ac4814e165141ef1dc930173fe50";

/// Revision 40 admits the Ultrafast target in the existing JSON column.
/// No existing row or SQL object changes.
pub(crate) const NATIVE_ULTRAFAST_REVISION: Revision = Revision {
    sql: "",
    objects: &[],
    fingerprint: BASE_FINGERPRINT,
};

const PREVIEW_INDEX: &str = "CREATE INDEX transcript_rows_preview ON transcript_rows(session_id,json_extract(row,'$.kind'),anchor,ordinal) WHERE json_extract(row,'$.kind') IN ('assistant','user') AND json_extract(row,'$.text') <> ''";

const DIRECTORY_PREVIEW: Revision = Revision {
    sql: PREVIEW_INDEX,
    objects: &[("transcript_rows_preview", PREVIEW_INDEX)],
    fingerprint: "b59a938e5cda4be7e625516b35ea24836a552240ae58ec648a472f7c197badfd",
};

/// The product file's admission descriptors.
pub(super) const SCHEMA: Schema = Schema {
    noun: "product",
    stem: "product",
    archive: "product.sqlite3 and runtime.sqlite3 together",
    generation: GENERATION,
    objects: SCHEMA_OBJECTS,
    base_fingerprint: BASE_FINGERPRINT,
    revisions: &[NATIVE_ULTRAFAST_REVISION, DIRECTORY_PREVIEW],
};

pub(super) fn schema() -> Schema {
    crate::test_hooks::product_store_schema(SCHEMA)
}
