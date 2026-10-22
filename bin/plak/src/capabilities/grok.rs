use super::ProductCapability;
use super::{OauthDoctor, Recommendation};
use crate::product_doctor::CheckKind;
use crate::profiles::{Builtin, BuiltinFile};

pub(super) const RECOMMENDED_MODEL: &str = "grok-4.6";

pub(super) static GROK: ProductCapability = ProductCapability {
    route: &plakous_hosted::capabilities::GROK,
    label: "Grok (SuperGrok)",
    short_label: "Grok",
    recommendation: Some(Recommendation {
        accounts: &["grok", "supergrok", "xai"],
        model: RECOMMENDED_MODEL,
    }),
    doctor: Some(OauthDoctor {
        kind: CheckKind::OauthXai,
        probe: |probes| probes.xai.clone(),
    }),
    preset: Some((2, &PRESET)),
};

static PRESET: Builtin = Builtin {
    id: "grok-default",
    aliases: &["grok", "supergrok"],
    files: &[
        BuiltinFile {
            role: "root",
            body: include_str!("../../presets/grok/agents/root-agent.toml"),
        },
        BuiltinFile {
            role: "worker",
            body: include_str!("../../presets/grok/agents/worker.toml"),
        },
        BuiltinFile {
            role: "reviewer",
            body: include_str!("../../presets/grok/agents/reviewer.toml"),
        },
        BuiltinFile {
            role: "researcher",
            body: include_str!("../../presets/grok/agents/researcher.toml"),
        },
    ],
};
