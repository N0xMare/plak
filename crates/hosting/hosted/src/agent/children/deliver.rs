//! Live mail delivery and turn cancellation for bound children.

use super::{
    Arc, ChildAssignmentStartKind, ChildBoundBind, ChildReopen, ChildReopenMode, ChildStartFailure,
    CommandId, FromStr, Future, LiveRelease, Pin, SpawnParent, TreeDriver, TurnId,
    assignment_start_prompt, bound_child_turn_matches, occupy_recovered_child, prompt_composition,
    rejoin_recovered_start, release_startup, reopen_spawned_child, spawn_child_start_composition,
    start_child_turn,
};

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

pub(super) fn mail_command_id(message_id: &str, kind: &str) -> Result<CommandId, String> {
    CommandId::new(format!("plak:mail:{message_id}:{kind}")).map_err(|error| error.to_string())
}

pub(in crate::agent) struct ProductLiveCanceller {
    pub(in crate::agent) runtime: Arc<crate::topology_runtime::ProductTopologyRuntime>,
}

impl crate::agent_tree_tools::LiveTurnCanceller for ProductLiveCanceller {
    fn cancel_agents(
        &self,
        agent_ids: Vec<String>,
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
            let report = self.runtime.cancel_live_many(&agent_ids).await;
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

impl crate::agent_tree_tools::MailDeliverer for ProductMailDeliverer {
    fn withdraw_followup(
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
                ChildReopenMode::REJOIN,
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
                match crate::agent::result_context::feed_bounds(&attached.sessions, session_id)
                    .await
                {
                    Ok((_, tail)) => driver.withdraw_unstarted_followup(
                        &agent_id,
                        &message_id,
                        &session_id.to_string(),
                        tail,
                        receipt,
                    ),
                    Err(error) => Err(refused(error)),
                }
            };
            if let Some(recovered) = recovered {
                let child = crate::agent::live::ProductLiveChild::new(
                    attached,
                    recovered.turn_id(),
                    recovered.control(),
                );
                self.runtime
                    .finish_startup(
                        &agent_id,
                        &startup,
                        Box::new(child)
                            .cancel_and_release(self.runtime.driver().clone())
                            .await,
                    )
                    .await;
            } else {
                release_startup(&self.runtime, &agent_id, &startup, attached).await;
            }
            result
        })
    }

    fn deliver(
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
}

impl ProductMailDeliverer {
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
            ChildReopenMode::REJOIN,
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
        if let Some(recovered) = recovered {
            return rejoin_recovered_start(
                &self.runtime,
                &request.agent_id,
                &request.message_id,
                attached,
                recovered.turn_id(),
                recovered.control(),
                recovered.result(),
            )
            .await;
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
        ));
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
            prompt,
        )
        .await
        .map_err(|failure| match failure {
            ChildStartFailure::Refused(message) => {
                crate::agent_tree_tools::MailDeliveryFailure::Refused(message)
            }
            ChildStartFailure::SlotOccupied(_) => {
                crate::agent_tree_tools::MailDeliveryFailure::SlotBusy
            }
            ChildStartFailure::Failed(message)
            | ChildStartFailure::HoldEndedAfterAdmission(message) => {
                crate::agent_tree_tools::MailDeliveryFailure::RecoveryRequired(message)
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
        let context = crate::agent::result_context::prepare(
            self.runtime.driver(),
            &request.agent_id,
            command_id.as_str(),
            &request.body,
        )?;
        // Follow-up is on `Agent`, which is not cloneable; the live-map guard
        // stays held for this one child so a concurrent reopen cannot replace it.
        let mut live = self.runtime.live.lock().await;
        let child = live
            .get_mut(&request.agent_id)
            .ok_or_else(|| "child live session is missing after reopen".to_owned())?;
        child
            .follow_up(command_id, context.prompt)
            .await
            .map_err(turn_ended_or)?;
        let turn_id = child.turn_id();
        let cursor =
            crate::agent::live::live_feed_cursor(self.runtime.driver(), &request.agent_id)?;
        let read = child.feed(cursor);
        drop(live);
        if let Some(feed) = read.await? {
            self.runtime.mutate_then_emit(false, |tree| {
                feed.apply(tree, &request.agent_id)
                    .map(|_| ())
                    .map_err(|error| error.message)
            })?;
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
        let context = crate::agent::result_context::prepare(
            self.runtime.driver(),
            &request.agent_id,
            command_id.as_str(),
            &request.body,
        )?;
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
            self.runtime.mutate_then_emit(false, |tree| {
                feed.apply(tree, &request.agent_id)
                    .map(|_| ())
                    .map_err(|error| error.message)
            })?;
        }
        Ok(crate::agent_tree_tools::MailDeliveryAck {
            turn_id: Some(turn_id.to_string()),
        })
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
                return Err(
                    "child turn could not be rejoined after reopen; no replacement start was admitted"
                        .into(),
                );
            }
        };
        if let Some(recovered) = recovered {
            if !bound_child_turn_matches(&self.runtime, &request.agent_id, recovered.turn_id()) {
                let _ = attached.release_opened().await;
                return Err("recovered child turn does not match the bound assignment".into());
            }
            occupy_recovered_child(&self.runtime, request.agent_id.clone(), attached, recovered)
                .await?;
            return Ok(());
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
        Err(
            "child turn could not be rejoined after reopen; no replacement start was admitted"
                .into(),
        )
    }
}
