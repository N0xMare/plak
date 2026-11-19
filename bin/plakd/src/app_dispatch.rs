use std::io;

use crate::command::Command;
use crate::commands::application::{AccountOps, PlakousApplication};
use crate::presentation::CredentialState;
use crate::{
    CliResult, ProcessOutcome, Services, carry, commands, execute_command, product_config,
};

#[allow(
    clippy::too_many_lines,
    reason = "Application command routing preserves one explicit handler for each account, catalog, usage, and one-shot run verb."
)]
pub(super) fn execute_application_command(
    command: Command,
    services: &Services,
    backend: &mut PlakousApplication,
    stdin: &mut dyn io::BufRead,
    stdout: &mut dyn io::Write,
    stderr: &mut dyn io::Write,
) -> CliResult<ProcessOutcome> {
    // Every turn-running arm admits `config.toml` first, before the
    // compatibility prompt, a credential read or any session state.
    let admit = |stderr: &mut dyn io::Write| {
        commands::support::admit_product_config(services.paths()?, stderr)
    };
    match command {
        Command::Run {
            limits,
            provider,
            model,
            effort,
            composition,
            prompt,
            print_session_id,
            accounts,
        } => {
            let config = admit(stderr)?;
            if !crate::test_hooks::offline_model_selected() {
                let status = AccountOps::offline_status(backend, &provider)?;
                if status.credential != CredentialState::RefreshRequired {
                    commands::support::require_ready(&provider, status.credential)?;
                }
                commands::login::ensure_compatibility(backend, &provider, stdin, stderr)?;
            }
            backend.run(
                &config,
                &provider,
                &model,
                effort,
                composition,
                prompt,
                print_session_id,
                stdout,
                stderr,
                limits,
                &accounts,
            )
        }
        Command::RunOpenRouter {
            account,
            limits,
            model,
            effort,
            composition,
            wire,
            prompt,
            print_session_id,
            accounts,
        } => backend.run_api(
            &admit(stderr)?,
            account,
            &model,
            effort,
            composition,
            product_config::ProductTargetSelection::OpenRouter {
                wire,
                routing: product_config::ProductOpenRouterRoutingSelection::GatewayManaged,
            },
            prompt,
            print_session_id,
            stdout,
            stderr,
            limits,
            &accounts,
        ),
        Command::RunPlatform {
            account,
            limits,
            provider,
            model,
            effort,
            composition,
            prompt,
            print_session_id,
            accounts,
        } => backend.run_api(
            &admit(stderr)?,
            account,
            &model,
            Some(effort),
            composition,
            product_config::ProductTargetSelection::Platform { provider },
            prompt,
            print_session_id,
            stdout,
            stderr,
            limits,
            &accounts,
        ),
        Command::ContinueSession {
            limits,
            session_id,
            prompt,
            print_session_id,
        } => backend.continue_session(
            &admit(stderr)?,
            // A session a carry moved on runs on its successor (spec 02 §2.6).
            &carry::work_id(services, &session_id, stderr)?,
            prompt,
            print_session_id,
            stdout,
            stderr,
            limits,
        ),
        command => execute_command(
            command,
            services,
            Some(backend),
            stdin,
            stdout,
            stderr,
            &mut commands::login_flow::SystemLoginUi,
        ),
    }
}
