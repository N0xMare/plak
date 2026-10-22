use super::ProductCapability;
use super::{Recommendation, RouteDoctor};
use crate::product_doctor::CheckKind;
use crate::profiles::{Builtin, BuiltinFile};

pub(super) const RECOMMENDED_MODEL: &str = "claude-sonnet-5";

pub(super) static CLAUDE: ProductCapability = ProductCapability {
    route: &plakous_hosted::capabilities::CLAUDE,
    label: "Claude (Max)",
    short_label: "Claude",
    recommendation: Some(Recommendation {
        accounts: &["claude"],
        model: RECOMMENDED_MODEL,
    }),
    doctor: Some(RouteDoctor::Oauth {
        kind: CheckKind::OauthClaude,
        probe: |probes| probes.claude.clone(),
    }),
    preset: Some((1, &PRESET)),
};

static PRESET: Builtin = Builtin {
    id: "claude-default",
    aliases: &["claude"],
    files: &[
        BuiltinFile {
            role: "root",
            body: include_str!("../../presets/claude/agents/root-agent.toml"),
        },
        BuiltinFile {
            role: "worker",
            body: include_str!("../../presets/claude/agents/worker.toml"),
        },
        BuiltinFile {
            role: "reviewer",
            body: include_str!("../../presets/claude/agents/reviewer.toml"),
        },
        BuiltinFile {
            role: "researcher",
            body: include_str!("../../presets/claude/agents/researcher.toml"),
        },
    ],
};
