use super::{Command, NATIVE_RUN_MAX_PROMPT_BYTES, RunEffort, RunPrompt, parse_oauth_account};

impl RunPrompt {
    /// Joins the words and reads every image; a refusal names the file.
    fn into_draft(self) -> Result<(String, Vec<plakous_host_protocol::DraftImage>), String> {
        if self.images.len() > plakous_hosted::attachments::MAX_IMAGES {
            return Err(format!(
                "run accepts at most {} --image files",
                plakous_hosted::attachments::MAX_IMAGES
            ));
        }
        let images = self
            .images
            .iter()
            .map(|path| {
                plakous_hosted::attachments::read_image(path).map_err(|error| error.to_string())
            })
            .collect::<Result<Vec<_>, _>>()?;
        let text = self.words.join(" ");
        plakous_hosted::attachments::check_message_budget(&text, &images)
            .map_err(|error| format!("run refused: {error}"))?;
        for image in images
            .iter()
            .filter(|image| image.source_dimensions.is_some())
        {
            eprintln!(
                "plak: attached {}",
                plakous_hosted::attachments::chip(image)
            );
        }
        Ok((text, images))
    }
}

pub(super) fn finish_continue_session(
    session_id: String,
    prompt: RunPrompt,
    print_session_id: bool,
    resume_tree: bool,
    limits: plakous_hosted::runtime::RunBounds,
) -> Result<Command, String> {
    if session_id.trim().is_empty() {
        return Err("run --session requires an exact session uuid".to_owned());
    }
    let (prompt, images) = prompt.into_draft()?;
    if prompt.trim().is_empty() && images.is_empty() {
        return Err("run requires a non-empty prompt after `--`".to_owned());
    }
    if prompt.len() > NATIVE_RUN_MAX_PROMPT_BYTES {
        return Err("run prompt exceeds the maximum supported size".to_owned());
    }
    Ok(Command::ContinueSession {
        limits,
        session_id,
        prompt: plakous_host_protocol::Draft::new(prompt, images),
        print_session_id,
        resume_tree,
    })
}

#[allow(
    clippy::too_many_arguments,
    reason = "Run finishing receives the independent clap binding fields, prompt, output choice, and existing RunBounds without inventing a second binding type."
)]
pub(super) fn finish_run(
    account: String,
    model: String,
    effort: Option<String>,
    composition: Option<String>,
    protocol: Option<String>,
    prompt: RunPrompt,
    print_session_id: bool,
    limits: plakous_hosted::runtime::RunBounds,
) -> Result<Command, String> {
    if model.is_empty() {
        return Err("run requires --model".to_owned());
    }
    let (prompt, images) = prompt.into_draft()?;
    if let Some(provider) = plakous_hosted::binding::PlatformProvider::for_account(&account) {
        if protocol.is_some() {
            return Err("--protocol is available only with --account openrouter".into());
        }
        let effort = effort
            .as_deref()
            .map(|value| RunEffort::parse(value).ok_or_else(crate::command::run_effort_usage))
            .transpose()?
            .map(RunEffort::as_reasoning_effort);
        let composition = composition
            .as_deref()
            .map(crate::product_config::ProductCompositionSelection::parse)
            .transpose()?
            .unwrap_or(crate::product_config::ProductCompositionSelection::FollowCatalog);
        crate::platform_catalog::validate(provider, &model, effort, composition)?;
        if (prompt.trim().is_empty() && images.is_empty())
            || prompt.len() > NATIVE_RUN_MAX_PROMPT_BYTES
        {
            return Err("run requires a non-empty prompt within the supported size bound".into());
        }
        return Ok(Command::RunPlatform {
            limits,
            provider,
            model,
            effort: effort.ok_or_else(crate::command::run_effort_usage)?,
            composition,
            prompt: plakous_host_protocol::Draft::new(prompt, images),
            print_session_id,
        });
    }
    if account == "openrouter" {
        let effort = effort
            .map(|value| {
                RunEffort::parse(&value)
                    .map(RunEffort::as_reasoning_effort)
                    .ok_or_else(crate::command::run_effort_usage)
            })
            .transpose()?;
        let composition = composition
            .map(|value| crate::product_config::ProductCompositionSelection::parse(&value))
            .transpose()?
            .unwrap_or(crate::product_config::ProductCompositionSelection::FollowCatalog);
        if composition == crate::product_config::ProductCompositionSelection::NativePtc {
            return Err(plakous_hosted::binding::OPENROUTER_NATIVE_PTC_REFUSAL.to_owned());
        }
        let selection = protocol
            .as_deref()
            .map(crate::product_config::ProductProtocolSelection::parse)
            .transpose()?
            .unwrap_or_default();
        let wire = crate::product_config::ProductWireBinding::resolve(selection);
        if prompt.trim().is_empty() && images.is_empty() {
            return Err("run requires a non-empty prompt after `--`".to_owned());
        }
        if prompt.len() > NATIVE_RUN_MAX_PROMPT_BYTES {
            return Err("run prompt exceeds the maximum supported size".to_owned());
        }
        return Ok(Command::RunOpenRouter {
            limits,
            model,
            effort,
            composition,
            wire,
            prompt: plakous_host_protocol::Draft::new(prompt, images),
            print_session_id,
        });
    }
    if protocol.is_some() {
        return Err("--protocol is available only with --account openrouter".to_owned());
    }
    let provider = parse_oauth_account(&account)?;
    // No silent default: the catalog decides whether an effort is required, and
    // `plak run` says so by name when it is (see `PlakousApplication::run`).
    let effort = effort
        .map(|value| RunEffort::parse(&value).ok_or_else(crate::command::run_effort_usage))
        .transpose()?;
    let composition = composition
        .map(|value| crate::product_config::ProductCompositionSelection::parse(&value))
        .transpose()?;
    if prompt.trim().is_empty() && images.is_empty() {
        return Err("run requires a non-empty prompt after `--`".to_owned());
    }
    if prompt.len() > NATIVE_RUN_MAX_PROMPT_BYTES {
        return Err("run prompt exceeds the maximum supported size".to_owned());
    }
    Ok(Command::Run {
        limits,
        provider,
        model,
        effort: effort.map(RunEffort::as_reasoning_effort),
        composition,
        prompt: plakous_host_protocol::Draft::new(prompt, images),
        print_session_id,
    })
}

pub(super) fn finish_agent(
    account: Option<String>,
    model: Option<String>,
    effort: Option<String>,
    composition: Option<String>,
    protocol: Option<String>,
    session: Option<String>,
    resume_tree: bool,
) -> Result<Command, String> {
    if let Some(session_id) = session {
        if account.is_some()
            || model.is_some()
            || effort.is_some()
            || composition.is_some()
            || protocol.is_some()
        {
            return Err(
                "agent --session uses the recorded binding and composition; omit --account/--model/--effort/--composition/--protocol"
                    .to_owned(),
            );
        }
        if session_id.trim().is_empty() {
            return Err("agent --session requires an exact session uuid".to_owned());
        }
        return Ok(Command::Agent {
            session_id: Some(session_id),
            provider: None,
            platform: None,
            wire: None,
            model: None,
            effort: None,
            effort_explicit: false,
            composition: None,
            resume_tree,
            wire_v2: false,
        });
    }
    if resume_tree {
        return Err("agent --resume-tree requires --session".to_owned());
    }
    if let Some(account) = account.as_deref() {
        if let Some(platform) = plakous_hosted::binding::PlatformProvider::for_account(account) {
            if protocol.is_some() {
                return Err("--protocol is available only with --account openrouter".into());
            }
            return finish_platform_agent(account, platform, model, effort, composition);
        }
        if account == "openrouter" {
            return finish_openrouter_agent(model, effort, composition, protocol);
        }
        if protocol.is_some() {
            return Err("--protocol is available only with --account openrouter".to_owned());
        }
    } else if protocol.is_some() {
        return Err("--protocol is available only with --account openrouter".to_owned());
    }
    let provider = account.as_deref().map(parse_oauth_account).transpose()?;
    let effort_explicit = effort.is_some();
    let effort = match effort {
        None => None,
        Some(value) => Some(
            RunEffort::parse(&value)
                .ok_or_else(crate::command::run_effort_usage)?
                .as_reasoning_effort(),
        ),
    };
    let composition = composition
        .map(|value| crate::product_config::ProductCompositionSelection::parse(&value))
        .transpose()?;
    Ok(Command::Agent {
        session_id: None,
        provider,
        platform: None,
        wire: None,
        model,
        effort,
        effort_explicit,
        composition,
        resume_tree: false,
        wire_v2: false,
    })
}

fn finish_openrouter_agent(
    model: Option<String>,
    effort: Option<String>,
    composition: Option<String>,
    protocol: Option<String>,
) -> Result<Command, String> {
    let effort = effort
        .map(|value| {
            RunEffort::parse(&value)
                .map(RunEffort::as_reasoning_effort)
                .ok_or_else(crate::command::run_effort_usage)
        })
        .transpose()?;
    let effort_explicit = effort.is_some();
    let composition = composition
        .map(|value| crate::product_config::ProductCompositionSelection::parse(&value))
        .transpose()?
        .unwrap_or(crate::product_config::ProductCompositionSelection::FollowCatalog);
    if composition == crate::product_config::ProductCompositionSelection::NativePtc {
        return Err(plakous_hosted::binding::OPENROUTER_NATIVE_PTC_REFUSAL.to_owned());
    }
    let selection = protocol
        .as_deref()
        .map(crate::product_config::ProductProtocolSelection::parse)
        .transpose()?
        .unwrap_or_default();
    let wire = crate::product_config::ProductWireBinding::resolve(selection);
    Ok(Command::Agent {
        session_id: None,
        provider: None,
        platform: None,
        wire: Some(wire),
        model,
        effort,
        effort_explicit,
        composition: Some(composition),
        resume_tree: false,
        wire_v2: false,
    })
}

fn finish_platform_agent(
    account: &str,
    platform: plakous_hosted::binding::PlatformProvider,
    model: Option<String>,
    effort: Option<String>,
    composition: Option<String>,
) -> Result<Command, String> {
    let effort = effort
        .as_deref()
        .map(|value| {
            RunEffort::parse(value)
                .map(RunEffort::as_reasoning_effort)
                .ok_or_else(crate::command::run_effort_usage)
        })
        .transpose()?;
    let composition = composition
        .as_deref()
        .map(crate::product_config::ProductCompositionSelection::parse)
        .transpose()?;
    if plakous_hosted::capabilities::require(account).is_ok() {
        if model
            .as_deref()
            .is_some_and(|model| !crate::platform_catalog::enrolled(platform, model))
        {
            return Err("API model is not enrolled for the selected account".into());
        }
        if let (Some(model), Some(composition)) = (model.as_deref(), composition) {
            plakous_hosted::platform::api_composition(platform, model, composition)
                .map_err(str::to_owned)?;
        }
        if let (Some(model), Some(effort)) = (model.as_deref(), effort) {
            plakous_hosted::platform::bundled_catalog(platform, "offline")
                .admit(model, Some(effort))
                .map_err(|error| error.to_string())?;
        }
    }
    Ok(Command::Agent {
        session_id: None,
        provider: None,
        platform: Some(platform),
        wire: None,
        model,
        effort,
        effort_explicit: effort.is_some(),
        composition,
        resume_tree: false,
        wire_v2: false,
    })
}
