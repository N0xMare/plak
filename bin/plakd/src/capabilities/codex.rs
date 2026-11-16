use super::ProductCapability;
use super::{Recommendation, RouteDoctor};
use crate::product_doctor::CheckKind;
use crate::profiles::{Builtin, BuiltinFile};

pub(super) const RECOMMENDED_MODEL: &str = "gpt-5.6-sol";

pub(super) static CODEX: ProductCapability = ProductCapability {
    route: &plakous_hosted::capabilities::CODEX,
    label: "Codex (ChatGPT)",
    short_label: "Codex",
    recommendation: Some(Recommendation {
        accounts: &["codex", "chatgpt"],
        model: RECOMMENDED_MODEL,
    }),
    doctor: Some(RouteDoctor::Oauth {
        kind: CheckKind::OauthCodex,
        probe: |probes| probes.codex.clone(),
    }),
    preset: Some((0, &PRESET)),
};

static PRESET: Builtin = Builtin {
    id: "codex-default",
    aliases: &["codex", "chatgpt"],
    files: &[
        BuiltinFile {
            role: "root",
            body: include_str!("../../presets/codex/agents/root-agent.toml"),
        },
        BuiltinFile {
            role: "worker",
            body: include_str!("../../presets/codex/agents/worker.toml"),
        },
        BuiltinFile {
            role: "reviewer",
            body: include_str!("../../presets/codex/agents/reviewer.toml"),
        },
        BuiltinFile {
            role: "researcher",
            body: include_str!("../../presets/codex/agents/researcher.toml"),
        },
    ],
};
