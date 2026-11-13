//! Live mail delivery and turn cancellation for bound children.

use crate::driver::ledger::CommandLedger as _;

use crate::driver::ledger::{BatchRule, CommandIntent, CommandKind};

use super::{
    Arc, ChildAssignmentStartKind, ChildBoundBind, ChildReopen, ChildReopenMode, ChildStartFailure,
    CommandId, FromStr, Future, LiveRelease, Pin, SpawnParent, TreeDriver, TurnId,
    assignment_start_prompt, mark_restored_lost, prompt_composition, release_startup,
    reopen_spawned_child, spawn_child_start_composition, start_child_turn,
};

/// A delivery that found a turn the lost owner left: the turn was settled
/// as lost and the mail stays unread.
const LOST_TURN_SETTLED: &str = "the child's turn was lost with its host and is settled; the mail stays unread for its continuation";

/// Reminder (1) then force (2) after a Completed turn with no `submit_result`.
/// Never copies assistant text into `output_json`.
pub(in crate::agent) fn submit_harness_prompt(nudge: u8, turn_token: u64, schema: &str) -> String {
    crate::child_prompts::SubmitNudge::after(nudge).render(turn_token, schema)
}

/// A follow-up that reached an ended turn leaves its mail for the waiter.
pub(in crate::agent) fn turn_ended_or(
    error: String,
) -> crate::agent_tree_tools::MailDeliveryFailure {
    if error == crate::agent::live::TURN_ENDED {
        crate::agent_tree_tools::MailDeliveryFailure::TurnEnded
    } else {
        error.into()
    }
}

fn mail_command_id(message_id: &str, kind: &str) -> Result<CommandId, String> {
    CommandId::new(format!("plak:mail:{message_id}:{kind}")).map_err(|error| error.to_string())
}

pub(in crate::agent) struct ProductLiveCanceller {
    pub(in crate::agent) runtime: Arc<crate::topology_runtime::ProductTopologyRuntime>,
}

impl ProductLiveCanceller {
    pub(in crate::agent) fn cancel_agents(
        &self,
        agent_ids: Vec<String>,
        signalled: tokio::sync::oneshot::Sender<()>,
    ) -> Pin<
        Box<
            dyn Future<
                    Output = Result<
                        crate::agent_tree_tools::LiveCancelOutcome,
                        crate::agent_tree_tools::LiveCancelFailure,
                    >,
                > + Send
                + '_,
        >,
    > {
        Box::pin(async move {
            let join = self.runtime.begin_cancel_live_many(&agent_ids).await;
            let _ = signalled.send(());
            let report = join.await;
            if report.is_clean() {
                return Ok(crate::agent_tree_tools::LiveCancelOutcome {
                    cancelled_ids: report.cancelled,
                    not_live_ids: report.not_live,
                });
            }
            Err(crate::agent_tree_tools::LiveCancelFailure {
                message: report
                    .first_failure
                    .unwrap_or_else(|| "child turn could not be cancelled".to_owned()),
                failed_ids: report.failed,
                joiner_expired_ids: report.joiner_expired,
                owner_failures: report.owner_failures,
                cancelled_ids: report.cancelled,
                not_live_ids: report.not_live,
            })
        })
    }
}

pub(in crate::agent) struct ProductMailDeliverer {
    pub(in crate::agent) parent: SpawnParent,
    pub(in crate::agent) runtime: Arc<crate::topology_runtime::ProductTopologyRuntime>,
}

impl ProductMailDeliverer {
    pub(in crate::agent) fn withdraw_followup(
        &self,
        driver: TreeDriver,
        agent_id: String,
        message_id: String,
        receipt: Option<(String, crate::agent_tree_protocol::TopologyErrorCode)>,
    ) -> Pin<
        Box<dyn Future<Output = Result<(), crate::agent_tree_protocol::TopologyError>> + Send + '_>,
    > {
        Box::pin(async move {
            use crate::agent_tree_protocol::{TopologyError, TopologyErrorCode};
            use crate::topology_runtime::LiveChildHandle as _;
            let refused =
                |message| TopologyError::new(TopologyErrorCode::OperationRecoveryRequired, message);
            let (path, locator) = driver.read(|tree| {
                let node = tree.node(&agent_id)?;
                Ok::<_, TopologyError>((
                    node.path.display().to_string(),
                    node.session_locator.clone(),
                ))
            })??;
            let locator = locator.ok_or_else(|| refused("child session is missing".to_owned()))?;
            let startup = self
                .runtime
                .reserve_startup_admitted(&agent_id, |_| Ok(()))
                .await
                .map_err(refused)?;
            let opened = reopen_spawned_child(
                &self.parent,
                &agent_id,
                &path,
                &locator,
                "followup-withdrawal",
                ChildReopenMode::OWNER,
            )
            .await;
            let (attached, recovered) = match opened {
                Ok(ChildReopen::Attached { session, recovered }) => (*session, recovered),
                other => {
                    self.runtime
                        .finish_startup(&agent_id, &startup, LiveRelease::default())
                        .await;
                    return Err(refused(match other {
                        Err(error) => error.message,
                        _ => "child session requires recovery".to_owned(),
                    }));
                }
            };
            // Both the startup slot and the exact session lease remain held
            // through the tail read and tree CAS: no Start can race this check.
            let session_id = attached.locator().session_id();
            let result = if recovered.is_some() {
                Err(refused("child session has an admitted turn".to_owned()))
            } else {
                match crate::agent::ledger::feed_bounds(&attached.sessions, session_id).await {
                    Ok((_, tail)) => {
                        driver
                            .withdraw_unstarted_followup(
                                &agent_id,
                                &message_id,
                                &session_id.to_string(),
                                tail,
                                receipt,
                            )
                            .await
                    }
                    Err(error) => Err(refused(error)),
                }
            };
            if let Some(recovered) = recovered {
                let child = crate::agent::live::ChildActorHandle::without_observation(
                    &self.runtime.driver().release_owners(),
                    attached,
                    recovered.turn_id(),
                    recovered.control(),
                );
                self.runtime
                    .finish_startup(
                        &agent_id,
                        &startup,
                        Box::new(child)
                            .cancel_and_release(self.runtime.driver().clone(), None)
                            .await,
                    )
                    .await;
            } else {
                release_startup(&self.runtime, &agent_id, &startup, attached).await;
            }
            result
        })
    }

    pub(in crate::agent) fn deliver(
        &self,
        request: crate::agent_tree_tools::MailDeliveryRequest,
    ) -> Pin<
        Box<
            dyn Future<
                    Output = Result<
                        crate::agent_tree_tools::MailDeliveryAck,
                        crate::agent_tree_tools::MailDeliveryFailure,
                    >,
                > + Send
                + '_,
        >,
    > {
        Box::pin(async move { self.deliver_mail(request).await })
    }

    async fn deliver_mail(
        &self,
        request: crate::agent_tree_tools::MailDeliveryRequest,
    ) -> Result<
        crate::agent_tree_tools::MailDeliveryAck,
        crate::agent_tree_tools::MailDeliveryFailure,
    > {
        match request.disposition {
            crate::agent_tree::MailDisposition::Started => self.start_child(request).await,
            crate::agent_tree::MailDisposition::Queued => self.follow_up_child(request).await,
            crate::agent_tree::MailDisposition::Steered => self.steer_child(request).await,
        }
    }

    #[allow(
        clippy::too_many_lines,
        reason = "Child start binds the immutable attempt request, runtime attachment, admission acknowledgement, and waiter ownership without duplicate dispatch."
    )]
    async fn start_child(
        &self,
        request: crate::agent_tree_tools::MailDeliveryRequest,
    ) -> Result<
        crate::agent_tree_tools::MailDeliveryAck,
        crate::agent_tree_tools::MailDeliveryFailure,
    > {
        if self.runtime.is_closed() {
            return Err("topology hub is shutting down".into());
        }
        // A Started mail was addressed to a child with no turn. A live slot
        // or a bound turn belongs to a turn some other command admitted: the
        // mail never joins it as a Start (that Start would be acknowledged
        // without ever reaching the turn); the send delivers it there as a
        // follow-up instead. Nothing reopens a session whose slot is still
        // being started or released, and a slot another Start takes while
        // this one reopens is not a fault: the send retries once it is vacant.
        {
            let live = self.runtime.live.lock().await;
            if live.get(&request.agent_id).is_some() {
                return Err(crate::agent_tree_tools::MailDeliveryFailure::TurnEnded);
            }
            if live.contains_key(&request.agent_id) {
                return Err(crate::agent_tree_tools::MailDeliveryFailure::SlotBusy);
            }
        }
        self.refuse_while_recovery_owed(&request.agent_id)?;
        let stored_turn = self
            .runtime
            .read(|tree| {
                Ok::<_, String>(
                    tree.attempt(&request.agent_id)
                        .and_then(|assignment| assignment.turn_id.clone()),
                )
            })
            .map_err(|error| error.message)??;
        if stored_turn.is_some() {
            return Err(crate::agent_tree_tools::MailDeliveryFailure::TurnEnded);
        }
        let (attached, recovered) = match reopen_spawned_child(
            &self.parent,
            &request.agent_id,
            &request.path,
            &request.session_locator,
            "mail-open",
            ChildReopenMode::OWNER,
        )
        .await?
        {
            ChildReopen::Attached { session, recovered } => (*session, recovered),
            ChildReopen::RecoveryRequired(_) => {
                return Err("child session reopen requires explicit recovery".into());
            }
        };
        // The live check above already passed. Another Start can take the
        // slot while this one still has the session open, before
        // `reserve_startup_admitted`. A test forces that window here.
        #[cfg(test)]
        crate::agent::park::hooks::around_reopen_spawned_child(&request.agent_id).await;
        // A turn the open restored is a lost owner's, never rejoined: it is
        // settled as lost and this mail stays unread for the child's
        // continuation or next wake.
        if let Some(recovered) = recovered {
            mark_restored_lost(&self.runtime, &request.agent_id, attached, recovered).await;
            return Err(LOST_TURN_SETTLED.into());
        }
        let (template, tier, workspace_path, bind) = self
            .runtime
            .read(|tree| {
                let node = tree
                    .node(&request.agent_id)
                    .map_err(|error| error.message)?;
                let template = node
                    .template
                    .as_ref()
                    .map(|template| template.as_str().to_owned())
                    .unwrap_or_else(|| "unknown".to_owned());
                let workspace_path = tree
                    .workspace(&node.workspace_id)
                    .filter(|row| row.mode != crate::agent_tree_protocol::WorkspaceMode::Inherit)
                    .map(|_| node.path.display().to_string());
                Ok::<_, String>((
                    template,
                    node.tier,
                    workspace_path,
                    ChildBoundBind::of(node),
                ))
            })
            .map_err(|error| error.message)??;
        // The mail-delivered Start reads the composition off the node's own
        // bound row, the same source `create_spawned_child_session` uses for
        // the spawn Start. Deriving it from a full `spawned_child_spec`
        // instead refused every OpenRouter child here with "spawn child
        // account is not a supported native OAuth account":
        // the exact anti-pattern the note above `spawn_child_start_composition`
        // records, left in place on the assignment path.
        let start_kind = ChildAssignmentStartKind::from_composition(prompt_composition(
            spawn_child_start_composition(
                &self.parent,
                &request.agent_id,
                bind.account.as_deref(),
            )?,
        ))?;
        let prompt = assignment_start_prompt(
            start_kind,
            &template,
            tier,
            &request.description,
            &request.prompt,
            &request.output_schema_json,
            request.turn_token,
            Some(&request.body),
            workspace_path.as_deref(),
        );
        let command_id = mail_command_id(&request.message_id, "start")?;
        let turn_id = start_child_turn(
            &self.runtime,
            &request.agent_id,
            attached,
            command_id,
            super::turns::ChildCommand::mail_start(prompt, request.message_id.clone()),
        )
        .await
        .map_err(|failure| match failure {
            ChildStartFailure::Refused(message) => {
                crate::agent_tree_tools::MailDeliveryFailure::Refused(message.into())
            }
            ChildStartFailure::SlotOccupied(_) => {
                crate::agent_tree_tools::MailDeliveryFailure::SlotBusy
            }
            ChildStartFailure::Failed(message)
            | ChildStartFailure::HoldEndedAfterAdmission(message) => {
                crate::agent_tree_tools::MailDeliveryFailure::RecoveryRequired(message.into())
            }
        })?;
        Ok(crate::agent_tree_tools::MailDeliveryAck {
            turn_id: Some(turn_id.to_string()),
        })
    }

    async fn follow_up_child(
        &self,
        request: crate::agent_tree_tools::MailDeliveryRequest,
    ) -> Result<
        crate::agent_tree_tools::MailDeliveryAck,
        crate::agent_tree_tools::MailDeliveryFailure,
    > {
        self.ensure_live(&request).await?;
        let command_id = mail_command_id(&request.message_id, "follow-up")?;
        let context =
            crate::agent::ledger::ChildLedger::new(self.runtime.driver(), &request.agent_id)
                .prepare(CommandIntent {
                    command_id: command_id.as_str(),
                    kind: CommandKind::Followup,
                    base_text: &request.body,
                    batch: BatchRule::Delivered(&request.message_id),
                })
                .await
                .map_err(String::from)?;
        let (delivery, turn_id, read) = {
            let live = self.runtime.live.lock().await;
            let child = live
                .get(&request.agent_id)
                .ok_or_else(|| "child live session is missing after reopen".to_owned())?;
            let cursor =
                crate::agent::live::live_feed_cursor(self.runtime.driver(), &request.agent_id)?;
            (
                child.follow_up(command_id, context.prompt),
                child.turn_id(),
                child.feed(cursor),
            )
        };
        self.runtime
            .driver()
            .spawn_release_owner(delivery)
            .await
            .map_err(|_| "follow-up delivery task failed".to_owned())?
            .map_err(turn_ended_or)?;
        if let Some(feed) = read.await? {
            let agent_id = request.agent_id.clone();
            self.runtime
                .mutate_then_emit(false, move |tree| {
                    feed.apply(tree, &agent_id)
                        .map(|_| ())
                        .map_err(|error| error.message)
                })
                .await?;
        }
        Ok(crate::agent_tree_tools::MailDeliveryAck {
            turn_id: Some(turn_id.to_string()),
        })
    }

    async fn steer_child(
        &self,
        request: crate::agent_tree_tools::MailDeliveryRequest,
    ) -> Result<
        crate::agent_tree_tools::MailDeliveryAck,
        crate::agent_tree_tools::MailDeliveryFailure,
    > {
        self.ensure_live(&request).await?;
        let command_id = mail_command_id(&request.message_id, "steer")?;
        let context =
            crate::agent::ledger::ChildLedger::new(self.runtime.driver(), &request.agent_id)
                .prepare(CommandIntent {
                    command_id: command_id.as_str(),
                    kind: CommandKind::Followup,
                    base_text: &request.body,
                    batch: BatchRule::Delivered(&request.message_id),
                })
                .await
                .map_err(String::from)?;
        let (control, turn_id) = {
            let live = self.runtime.live.lock().await;
            let child = live
                .get(&request.agent_id)
                .ok_or_else(|| "child live session is missing after reopen".to_owned())?;
            (child.turn_control(), child.turn_id())
        };
        let ack = control
            .steer_with_id(command_id, context.prompt)
            .await
            .map_err(|error| error.to_string())?;
        if let Some(rejection) = ack.rejection() {
            if rejection.kind() == plakous::agent::ControlErrorKind::WrongPhase {
                return Err(crate::agent_tree_tools::MailDeliveryFailure::TurnEnded);
            }
            return Err(rejection.to_string().into());
        }
        let cursor =
            crate::agent::live::live_feed_cursor(self.runtime.driver(), &request.agent_id)?;
        let read = {
            let live = self.runtime.live.lock().await;
            live.get(&request.agent_id).map(|child| child.feed(cursor))
        };
        if let Some(read) = read
            && let Some(feed) = read.await?
        {
            let agent_id = request.agent_id.clone();
            self.runtime
                .mutate_then_emit(false, move |tree| {
                    feed.apply(tree, &agent_id)
                        .map(|_| ())
                        .map_err(|error| error.message)
                })
                .await?;
        }
        Ok(crate::agent_tree_tools::MailDeliveryAck {
            turn_id: Some(turn_id.to_string()),
        })
    }

    /// A child the reopen law owes a `recover_node` takes no turn before it
    /// runs: whether its lost turn rejoins or it is marked is not known yet.
    /// The mail stays durable and the send looks again once that settles.
    fn refuse_while_recovery_owed(
        &self,
        agent_id: &str,
    ) -> Result<(), crate::agent_tree_tools::MailDeliveryFailure> {
        let owed = self
            .runtime
            .read(|tree| tree.recovery_owed(agent_id))
            .map_err(|error| error.message)?;
        if owed {
            return Err(crate::agent_tree_tools::MailDeliveryFailure::SlotBusy);
        }
        Ok(())
    }

    async fn ensure_live(
        &self,
        request: &crate::agent_tree_tools::MailDeliveryRequest,
    ) -> Result<(), crate::agent_tree_tools::MailDeliveryFailure> {
        if self.runtime.is_closed() {
            return Err("topology hub is shutting down".into());
        }
        {
            let live = self.runtime.live.lock().await;
            if live.get(&request.agent_id).is_some() {
                return Ok(());
            }
            // A slot still starting or retiring has no turn to join yet: the
            // send looks again once it changes.
            if live.contains_key(&request.agent_id) {
                return Err(crate::agent_tree_tools::MailDeliveryFailure::SlotBusy);
            }
        }
        self.refuse_while_recovery_owed(&request.agent_id)?;
        let stored_turn = self
            .runtime
            .read(|tree| {
                Ok::<_, String>(
                    tree.attempt(&request.agent_id)
                        .and_then(|assignment| assignment.turn_id.clone()),
                )
            })
            .map_err(|error| error.message)??;
        let Some(stored_turn) = stored_turn else {
            return Err("no live child session; follow-up and steer require a started turn".into());
        };
        let _stored_turn = TurnId::from_str(&stored_turn)
            .map_err(|_| "stored child turn identity is invalid".to_owned())?;
        let (attached, recovered) = match reopen_spawned_child(
            &self.parent,
            &request.agent_id,
            &request.path,
            &request.session_locator,
            "mail-open",
            ChildReopenMode::OWNER,
        )
        .await?
        {
            ChildReopen::Attached { session, recovered } => (*session, recovered),
            ChildReopen::RecoveryRequired(_) => {
                return Err(self.rejoin_refusal(&request.agent_id).into());
            }
        };
        if let Some(recovered) = recovered {
            mark_restored_lost(&self.runtime, &request.agent_id, attached, recovered).await;
            return Err(LOST_TURN_SETTLED.into());
        }
        // Opened + bound turn has no RecoveredTurnHandle. Joining via
        // turn_control would occupy without a waiter and is a restart drain
        // if Start were admitted next. Fail closed; never Start.
        let _ = attached.release_opened().await;
        // An interrupted child's cancelled turn ended before its waiter
        // released it: the send waits for that release, then resumes the
        // child with this mail (spec 03 section 9). A bound turn no waiter
        // watches is released by nobody, so the send fails closed at once
        // instead of waiting out its budget.
        let unwinding = self
            .runtime
            .read(|tree| {
                let interrupted = tree.node(&request.agent_id).is_ok_and(|node| {
                    node.status == crate::agent_tree_protocol::NodeStatus::Interrupted
                });
                let still_bound = tree
                    .attempt(&request.agent_id)
                    .and_then(|attempt| attempt.turn_id.as_deref())
                    == Some(stored_turn.as_str());
                interrupted && (!still_bound || tree.has_turn_waiter(&request.agent_id))
            })
            .unwrap_or(false);
        if unwinding {
            return Err(crate::agent_tree_tools::MailDeliveryFailure::TurnEnded);
        }
        Err(self.rejoin_refusal(&request.agent_id).into())
    }

    fn rejoin_refusal(&self, agent_id: &str) -> String {
        let mut message =
            "child turn could not be rejoined after reopen; no replacement start was admitted"
                .to_owned();
        let attempt = self
            .runtime
            .read(|tree| {
                tree.node(agent_id)
                    .ok()
                    .filter(|node| {
                        node.status == crate::agent_tree_protocol::NodeStatus::Interrupted
                    })
                    .and_then(|node| node.attempt_id.clone())
            })
            .ok()
            .flatten();
        if let Some(attempt) = attempt {
            let acknowledgement =
                self.parent
                    .state_roots()
                    .frozen_presentation()
                    .is_none_or(|frozen| {
                        plakous::model::ToolName::new("send_agent_message")
                            .ok()
                            .and_then(|name| frozen.definition(&name))
                            .is_some_and(|definition| match definition.input() {
                                plakous::model::ToolInputContract::Structured(schema) => {
                                    serde_json::from_slice::<serde_json::Value>(
                                        schema.document().canonical_bytes(),
                                    )
                                    .is_ok_and(|document| {
                                        !document["properties"]["acknowledge_recovery"].is_null()
                                    })
                                }
                                _ => false,
                            })
                    });
            if acknowledgement {
                message.push_str(&format!(
                    "; its parent may send purpose=followup with acknowledge_recovery={attempt} \
                     for a fresh attempt without replaying uncertain work; a tool may have changed this workspace"
                ));
            } else {
                message.push_str("; a tool may have changed this workspace. Ask the operator to Resume/recover the child, or rebind this session to the current tool definitions before acknowledging recovery");
            }
        }
        message
    }
}
