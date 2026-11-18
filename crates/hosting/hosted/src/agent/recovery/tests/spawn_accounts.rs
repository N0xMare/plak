use super::*;
use crate::agent::children::runtime_factory::{
    ChildRuntimeFactory, ChildRuntimeFuture, ChildRuntimeRequest,
};
use crate::agent::children::{
    ProductChildController, ProductChildSessionBinder, ProductLiveCanceller, ProductMailDeliverer,
};
use crate::agent_tree::TopologyError;
use crate::agent_tree_protocol::TemplateName;
use crate::agent_tree_tools::{
    ChildController, ChildServices, SpawnBindError, SpawnBound, SpawnStart,
};
use crate::orchestration::RosterBinding;
use plakous::gateway::custody::{
    AccountFacts, CredentialCustodian, CredentialFields, CustodyError, CustodyFuture, CustodySlot,
    CustodyStatus, ExpectedFence, RefreshRequest,
};
use plakous::model::{Digest, DigestDomain};
use plakous_recursive_subagents_store::SqliteTreeStore;

struct ReadySubscriptions;

impl CredentialCustodian for ReadySubscriptions {
    fn facts<'a>(&'a self, slot: &'a CustodySlot) -> CustodyFuture<'a, AccountFacts> {
        Box::pin(async move {
            let mut facts = AccountFacts::signed_out(slot.clone());
            facts.status = CustodyStatus::Ready;
            Ok(facts)
        })
    }

    fn refresh<'a>(
        &'a self,
        _slot: &'a CustodySlot,
        _request: RefreshRequest,
    ) -> CustodyFuture<'a, AccountFacts> {
        Box::pin(async { Err(CustodyError::Internal) })
    }

    fn fields<'a>(
        &'a self,
        _slot: &'a CustodySlot,
        _expected: &'a ExpectedFence,
    ) -> CustodyFuture<'a, CredentialFields> {
        Box::pin(async { Err(CustodyError::Internal) })
    }
}

/// An offline provider that seals the exact account material passed by the
/// product into its real runtime binding, rather than sharing one identity.
struct AccountBoundChild(PathBuf);

impl ChildRuntimeFactory for AccountBoundChild {
    fn bind<'a>(&'a self, request: ChildRuntimeRequest<'a>) -> ChildRuntimeFuture<'a> {
        Box::pin(async move {
            let controller =
                BoundaryController::quiet_from_evidence_path(CrashBoundary::None, &self.0).unwrap();
            let script = serde_json::from_value(json!({
                "version": 1, "repeat_last_step": true,
                "steps": [{"output": {"text": "finished"}}],
            }))
            .unwrap();
            Ok(
                plakous_offline_crash_harness::projected_scripted_runtime_binding_with(
                    Path::new(request.path),
                    controller,
                    Arc::new(script),
                    None,
                    request
                        .account
                        .expect("the child binds its frozen account")
                        .as_bytes(),
                    10_000_000,
                )
                .unwrap(),
            )
        })
    }
}

async fn fixture(tree_id: &str) -> (Fixture, crate::sessions::SessionRecord) {
    let mut fixture = Fixture::new(tree_id);
    let SpawnParent::Api(parent) = &mut fixture.parent else {
        unreachable!()
    };
    let custody = plakous::custody::Custody::gateway(
        plakous::custody::Custody::local().transport(),
        Arc::new(ReadySubscriptions),
    );
    parent.state_roots = parent
        .state_roots
        .clone()
        .with_subscription_custody(custody)
        .with_child_runtime_factory(Arc::new(AccountBoundChild(fixture.evidence.clone())));
    fixture
        .runtime
        .remember_rejoin_spec(Arc::new(fixture.parent.clone()));
    fixture
        .runtime
        .mutate_then_emit(false, |tree| {
            tree.pin_bind_hints_for_test(crate::orchestration::BindHints::from_names(
                &["chatgpt", "supergrok"],
                &[],
            ));
            for (name, alias) in [("codex-review", "chatgpt"), ("grok-review", "supergrok")] {
                let mut binding = RosterBinding::leaf(alias, "offline-loopback-v1");
                binding.composition = Some(plakous_host_protocol::CompositionSelection::Direct);
                tree.pin_template(name, binding);
            }
            Ok(())
        })
        .await
        .unwrap();
    let mut head = crate::sessions::SessionRecord::new(fixture.root.path().to_path_buf());
    head.node = crate::sessions::SessionNode::genesis(tree_id);
    head.account = Some("claude".to_owned());
    choose(&fixture, &mut head, "codex", "codex");
    choose(&fixture, &mut head, "grok", "grok");
    (fixture, head)
}

fn choose(fixture: &Fixture, head: &mut crate::sessions::SessionRecord, kind: &str, account: &str) {
    head.account_choices
        .insert(kind.to_owned(), account.to_owned());
    fixture
        .parent
        .state_roots()
        .session_records()
        .save(head)
        .unwrap();
}

fn request(template: &str, prompt: &str) -> serde_json::Value {
    serde_json::to_value(SpawnAgentRequest::test_named(
        template,
        Tier::ReadOnly,
        "review",
        prompt,
    ))
    .unwrap()
}

async fn spawn(fixture: &Fixture, template: &str, prompt: &str) -> String {
    let (ok, receipt) = fixture
        .invoke("spawn_agent", request(template, prompt))
        .await;
    assert!(ok, "{receipt}");
    receipt["agent_id"].as_str().unwrap().to_owned()
}

fn assert_bound(fixture: &Fixture, child: &str, account: &str) -> SessionLocator {
    let locator: SessionLocator = fixture
        .runtime
        .read(|tree| {
            let node = tree.node(child).unwrap();
            assert_eq!(node.account, account);
            node.session_locator.as_deref().unwrap().parse().unwrap()
        })
        .unwrap();
    let head =
        plakous::sessions::rehearsal::inspect_binding_presentations_sqlite(fixture.database())
            .unwrap()
            .into_iter()
            .find(|head| {
                head.locator.session_id() == locator.session_id()
                    && head.locator.branch_id() == locator.branch_id()
            })
            .unwrap();
    let presentation = head.presentation.unwrap().unwrap();
    let stored = presentation
        .configuration()
        .unwrap()
        .effective()
        .authority()
        .account_binding();
    let expected = Digest::sha256(DigestDomain::ModelBinding, 1, &[account.as_bytes()]).unwrap();
    assert_eq!(stored, expected, "{child} runtime binding");
    locator
}

async fn await_finished(fixture: &Fixture, child: &str) {
    tokio::time::timeout(Duration::from_secs(30), async {
        while fixture.attempt_status(child) == AttemptStatus::Running {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        fixture.attempt_status(child),
        AttemptStatus::CompletedWithoutSubmission
    );
}

async fn reattach(fixture: &mut Fixture, store: Arc<dyn TreeStore>, mut tree: ProductAgentTree) {
    tree.set_vcs(fixture.vcs.clone());
    fixture.runtime =
        crate::topology_runtime::insert_root_with(&fixture.tree_id, tree, store, None)
            .await
            .unwrap();
    fixture
        .runtime
        .remember_rejoin_spec(Arc::new(fixture.parent.clone()));
}

#[tokio::test]
async fn alias_cards_freeze_the_default_and_named_accounts_through_followup() {
    let (mut fixture, mut head) = fixture("alias-spawn-through-followup").await;
    let epoch = fixture
        .runtime
        .read(|tree| tree.current_epoch().clone())
        .unwrap();
    let codex = spawn(&fixture, "codex-review", "default for codex").await;
    await_finished(&fixture, &codex).await;
    assert_bound(&fixture, &codex, "codex");
    let grok = spawn(&fixture, "grok-review", "default for grok").await;
    await_finished(&fixture, &grok).await;
    assert_bound(&fixture, &grok, "grok");

    choose(&fixture, &mut head, "codex", "codex/work");
    choose(&fixture, &mut head, "grok", "grok/work");
    let codex_work = spawn(&fixture, "codex-review", "named for codex").await;
    await_finished(&fixture, &codex_work).await;
    assert_bound(&fixture, &codex_work, "codex/work");
    let grok_work = spawn(&fixture, "grok-review", "named for grok").await;
    await_finished(&fixture, &grok_work).await;
    let prior = assert_bound(&fixture, &grok_work, "grok/work");

    choose(&fixture, &mut head, "grok", "grok/other");
    crate::topology_runtime::force_release(&fixture.tree_id).await;
    let store = fixture.store.clone();
    let tree = ProductAgentTree::reopen(store.load(&fixture.tree_id).unwrap().unwrap());
    reattach(&mut fixture, store, tree).await;
    let (ok, receipt) = fixture
        .invoke(
            "send_agent_message",
            json!({
                "agent_id": grok_work, "purpose": "followup", "message": "review the revision",
            }),
        )
        .await;
    assert!(ok, "{receipt}");
    assert_eq!(receipt["disposition"], "started");
    await_finished(&fixture, &grok_work).await;
    assert_eq!(
        assert_bound(&fixture, &grok_work, "grok/work").session_id(),
        prior.session_id()
    );
    let other = spawn(&fixture, "grok-review", "new choice").await;
    await_finished(&fixture, &other).await;
    assert_bound(&fixture, &other, "grok/other");
    assert_bound(&fixture, &grok, "grok");
    assert_bound(&fixture, &codex, "codex");
    fixture
        .runtime
        .read(|tree| {
            assert_eq!(tree.current_epoch().digest, epoch.digest);
            assert_eq!(tree.current_epoch().roster, epoch.roster);
        })
        .unwrap();
    crate::topology_runtime::force_release(&fixture.tree_id).await;
}

// The real product admits and resolves the spawn. The cut holds just before
// the binder can reserve startup, so no child lease survives the caller.
struct CutAfterReceipt {
    product: ProductChildController,
    reached: Arc<tokio::sync::Notify>,
}

type Answer<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

impl ChildController for CutAfterReceipt {
    fn services(&self) -> ChildServices {
        ChildServices::BIND
    }

    fn admit_spawn(&self, caller: &str, request: &SpawnAgentRequest) -> Result<(), TopologyError> {
        self.product.admit_spawn(caller, request)
    }

    fn spawn_parent_composition(
        &self,
        caller: &str,
    ) -> Option<plakous_host_protocol::CompositionSelection> {
        self.product.spawn_parent_composition(caller)
    }

    fn admit_binding<'a>(
        &'a self,
        template: &'a TemplateName,
        binding: &'a RosterBinding,
    ) -> Answer<'a, Result<(), TopologyError>> {
        self.product.admit_binding(template, binding)
    }

    fn resolve_account<'a>(
        &'a self,
        card: &'a str,
    ) -> Answer<'a, Result<Option<String>, TopologyError>> {
        self.product.resolve_account(card)
    }

    fn bind(&self, _start: SpawnStart) -> Answer<'_, Result<SpawnBound, SpawnBindError>> {
        Box::pin(async {
            self.reached.notify_one();
            std::future::pending().await
        })
    }
}

#[tokio::test]
async fn spawn_account_receipt_replays_after_relaunch() {
    let (mut fixture, mut head) = fixture("spawn-account-receipt-cut").await;
    choose(&fixture, &mut head, "grok", "grok/work");
    let mut tree = fixture.runtime.read(ProductAgentTree::clone).unwrap();
    crate::topology_runtime::force_release(&fixture.tree_id).await;
    let database = fixture.root.path().join("tree.sqlite3");
    let store = Arc::new(SqliteTreeStore::open(&database).unwrap());
    tree.acknowledge_revision(Revision::published(0));
    tree.acknowledge_revision(store.save_in_custody(&tree).unwrap());
    reattach(&mut fixture, store.clone(), tree).await;

    let reached = Arc::new(tokio::sync::Notify::new());
    let arguments = request("grok-review", "replay the exact receipt");
    let operation = "spawn-account-cut";
    let task = tokio::spawn({
        let runtime = fixture.runtime.clone();
        let parent = fixture.parent.clone();
        let reached = reached.clone();
        let arguments = arguments.clone();
        async move {
            let controller = ProductChildController {
                binder: ProductChildSessionBinder {
                    parent: parent.clone(),
                },
                deliverer: ProductMailDeliverer {
                    parent,
                    runtime: runtime.clone(),
                },
                canceller: ProductLiveCanceller {
                    runtime: runtime.clone(),
                },
                _residency: None,
            };
            crate::agent_tree_tools::invoke_granted_topology_tool_on(
                runtime.driver().clone(),
                "root".into(),
                "spawn_agent",
                &arguments,
                operation.into(),
                crate::agent_tree_tools::TopologyHostPorts {
                    controller: Some(Arc::new(CutAfterReceipt {
                        product: controller,
                        reached,
                    })),
                    ..Default::default()
                },
            )
            .await
        }
    });
    tokio::time::timeout(Duration::from_secs(30), reached.notified())
        .await
        .unwrap();
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());

    let persisted = ProductAgentTree::reopen(store.load(&fixture.tree_id).unwrap().unwrap());
    let receipt: crate::agent_tree::SpawnAgentOutput = serde_json::from_str(
        persisted.operations()[operation]
            .terminal_response
            .as_deref()
            .unwrap(),
    )
    .unwrap();
    let child = persisted.node(&receipt.agent_id).unwrap();
    assert_eq!(child.account, "grok/work");
    assert!(
        child.session_locator.is_none(),
        "the binder did not run before the cut"
    );

    crate::topology_runtime::force_release(&fixture.tree_id).await;
    choose(&fixture, &mut head, "grok", "grok/other");
    let store = Arc::new(SqliteTreeStore::open(&database).unwrap());
    let tree = ProductAgentTree::reopen(store.load(&fixture.tree_id).unwrap().unwrap());
    reattach(&mut fixture, store, tree).await;
    let replay = crate::agent::invoke_topology_tool_on_hub(
        &fixture.tree_id,
        None,
        "spawn_agent",
        &arguments,
        operation,
    )
    .await
    .unwrap();
    assert!(replay.succeeded, "{}", replay.detail);
    assert_eq!(
        serde_json::from_str::<crate::agent_tree::SpawnAgentOutput>(&replay.detail).unwrap(),
        receipt
    );
    await_finished(&fixture, &receipt.agent_id).await;
    assert_bound(&fixture, &receipt.agent_id, "grok/work");
    crate::topology_runtime::force_release(&fixture.tree_id).await;
}
