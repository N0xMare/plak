mod actual_host;
mod fixture;
mod product_release;
mod protocol;

use std::io::{self, BufRead, BufReader};

use protocol::{ChildReply, ControlCommand, PROTOCOL_VERSION, write_reply};

pub(super) const CHILD_SENTINEL: &str = "PLAK_PRIVATE_CRASH_CHILD";
pub(super) const REPLY_PREFIX: &str = "PLAK-CRASH-REPLY ";
pub(super) const CHILD_TEST_ARGS: &[&str] = &[
    "--ignored",
    "--exact",
    "product_crash_tests::product_crash_child",
    "--nocapture",
    "--test-threads=1",
];

#[tokio::test]
#[ignore = "process child entrypoint used only by the private crash matrices"]
async fn product_crash_child() {
    if std::env::var_os(CHILD_SENTINEL).is_none() {
        return;
    }
    let exit = Box::pin(child_main()).await;
    assert_eq!(exit, 0, "private product crash child failed");
}

async fn child_main() -> i32 {
    let command = match read_command() {
        Ok(command) => command,
        Err(message) => {
            let _ = write_reply(&ChildReply::Failed {
                version: PROTOCOL_VERSION,
                message: message.to_owned(),
            });
            return 2;
        }
    };
    if command.version() != PROTOCOL_VERSION {
        let _ = write_reply(&ChildReply::Failed {
            version: PROTOCOL_VERSION,
            message: "unsupported control protocol version".to_owned(),
        });
        return 2;
    }
    if write_reply(&ChildReply::Started {
        version: PROTOCOL_VERSION,
    })
    .is_err()
    {
        return 2;
    }
    let outcome = match command {
        ControlCommand::ProductRun {
            home,
            workspace,
            state,
            workflow,
            cut,
            ..
        } => {
            Box::pin(fixture::run_product_fixture(
                &home, &workspace, &state, workflow, cut,
            ))
            .await
        }
        ControlCommand::ProductProbe { home, state, .. } => {
            fixture::probe_product_fixture(&home, &state)
        }
        ControlCommand::ProductRecover {
            home,
            workspace,
            state,
            ..
        } => Box::pin(fixture::recover_product_fixture(&home, &workspace, &state)).await,
        ControlCommand::ActualHostRun {
            home,
            workspace,
            state,
            workflow,
            cut,
            ..
        } => fixture::run_actual_host_fixture(&home, &workspace, &state, workflow, cut).await,
        ControlCommand::ActualHostRecover {
            home,
            workspace,
            state,
            next_prompt,
            ..
        } => fixture::recover_actual_host_fixture(&home, &workspace, &state, next_prompt),
    };
    match outcome {
        Ok(report) => write_reply(&ChildReply::ProductProbed {
            version: PROTOCOL_VERSION,
            report: Box::new(report),
        })
        .map_or(2, |()| 0),
        Err(error) => {
            let _ = write_reply(&ChildReply::Failed {
                version: PROTOCOL_VERSION,
                message: error.to_string().chars().take(512).collect(),
            });
            1
        }
    }
}

fn read_command() -> Result<ControlCommand, &'static str> {
    let mut line = String::new();
    BufReader::new(io::stdin())
        .read_line(&mut line)
        .map_err(|_| "control input could not be read")?;
    if line.is_empty() {
        return Err("control input ended before one command");
    }
    serde_json::from_str(&line).map_err(|_| "control input was not valid versioned JSON")
}
