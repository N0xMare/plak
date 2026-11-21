//! Best-effort, post-commit presentation notices. No recipient owns a store write.

use std::os::unix::net::UnixDatagram;
use std::path::PathBuf;

use plakous_recursive_subagents_store::TreeChange;

use crate::home::paths::PlakPaths;

enum Change<'a> {
    Record(&'a str),
    Tree(&'a str, TreeChange<'a>),
    Input(&'static str),
}

pub(crate) fn product(paths: &PlakPaths, id: &str) {
    if !paths.probing_stores() {
        send(paths, Change::Record(id));
    }
}

pub(crate) fn tree(paths: &PlakPaths, id: &str, change: TreeChange<'_>) {
    send(paths, Change::Tree(id, change));
}

pub(crate) fn configuration(paths: &PlakPaths) {
    inputs(paths, "config");
}

pub(crate) fn inputs(paths: &PlakPaths, kind: &'static str) {
    send(paths, Change::Input(kind));
}

fn send(paths: &PlakPaths, event: Change<'_>) {
    if local::deliver(paths, &event) {
        return;
    }
    let (kind, id) = match event {
        Change::Record(id) => ("record", id),
        Change::Tree(id, TreeChange::Custody) => ("custody", id),
        Change::Tree(id, TreeChange::Saved(_) | TreeChange::Deleted) => ("tree", id),
        Change::Input(kind) => (kind, "changed"),
    };
    external(paths, kind, id);
}

pub(crate) fn socket(paths: &PlakPaths) -> PathBuf {
    paths.host_socket().with_extension("dir")
}

fn external(paths: &PlakPaths, kind: &str, id: &str) {
    let path = socket(paths);
    if !path.exists() {
        return;
    }
    if let Ok(sender) = UnixDatagram::unbound()
        && sender.set_nonblocking(true).is_ok()
    {
        let _ = sender.send_to(format!("{kind} {id}").as_bytes(), path);
    }
}

#[cfg(feature = "web")]
pub(crate) mod local {
    use std::collections::{BTreeMap, BTreeSet, VecDeque};
    use std::sync::{Arc, Condvar, LazyLock, Mutex, PoisonError, Weak};
    use std::time::Instant;

    use plakous_hosted::orchestration::OrchestrationSnapshot;
    use plakous_hosted::runtime::{ResumeAfterLoss, operator_attention::PendingTreeWork};
    use plakous_recursive_subagents::tree::ProductAgentTree;

    use super::{Change, PathBuf, PlakPaths, TreeChange};

    #[derive(PartialEq, Eq)]
    pub(crate) struct TreeFacts {
        pub(crate) revision: u64,
        pub(crate) roster: OrchestrationSnapshot,
        pub(crate) hold: PendingTreeWork,
        pub(crate) resume: PendingTreeWork,
    }

    impl TreeFacts {
        pub(crate) fn of(tree: &ProductAgentTree) -> Self {
            Self {
                revision: tree.revision,
                roster: tree.current_epoch().clone(),
                hold: PendingTreeWork::read(tree, ResumeAfterLoss::Hold),
                resume: PendingTreeWork::read(tree, ResumeAfterLoss::Continue),
            }
        }
    }

    pub(crate) enum Notice {
        Record(String),
        Tree(String, Option<Box<TreeFacts>>),
        InspectTree(String),
        Custody(String),
        Configuration,
        Visibility,
        Revalidate,
        Gap,
        #[cfg(test)]
        Barrier(std::sync::mpsc::SyncSender<()>),
    }

    pub(crate) enum TreeUpdate {
        Image(Option<Box<TreeFacts>>),
        Inspect,
    }

    #[derive(Default)]
    pub(crate) struct Batch {
        pub(crate) records: BTreeSet<String>,
        pub(crate) trees: BTreeMap<String, TreeUpdate>,
        pub(crate) custody: BTreeSet<String>,
        pub(crate) revalidate: bool,
        pub(crate) gap: bool,
        pub(crate) configuration: bool,
        pub(crate) visibility: bool,
        #[cfg(test)]
        pub(crate) barriers: Vec<std::sync::mpsc::SyncSender<()>>,
    }

    impl Batch {
        fn add(&mut self, notice: Notice) {
            match notice {
                Notice::Record(id) => {
                    self.records.insert(id);
                }
                Notice::Tree(id, facts) => {
                    self.trees.insert(id, TreeUpdate::Image(facts));
                }
                Notice::InspectTree(id) => {
                    self.trees.insert(id, TreeUpdate::Inspect);
                }
                Notice::Custody(id) => {
                    self.custody.insert(id);
                }
                Notice::Configuration => self.configuration = true,
                Notice::Visibility => self.visibility = true,
                Notice::Revalidate => self.revalidate = true,
                Notice::Gap => self.gap = true,
                #[cfg(test)]
                Notice::Barrier(sender) => self.barriers.push(sender),
            }
        }
    }

    #[derive(Default)]
    struct Pending {
        local: Option<Batch>,
        external: VecDeque<Notice>,
        closed: bool,
    }

    #[derive(Default)]
    pub(crate) struct Mailbox {
        pending: Mutex<Pending>,
        ready: Condvar,
    }

    impl Mailbox {
        pub(crate) fn take_until(&self, deadline: Instant) -> Option<Batch> {
            let pending = self.pending.lock().unwrap_or_else(PoisonError::into_inner);
            let (mut pending, _) = self
                .ready
                .wait_timeout_while(
                    pending,
                    deadline.saturating_duration_since(Instant::now()),
                    |state| !state.closed && state.local.is_none() && state.external.is_empty(),
                )
                .unwrap_or_else(PoisonError::into_inner);
            if pending.closed {
                return None;
            }
            let mut batch = pending.local.take().unwrap_or_default();
            let external = std::mem::take(&mut pending.external);
            drop(pending);
            for notice in external {
                batch.add(notice);
            }
            Some(batch)
        }
    }

    pub(crate) struct Inbox(Arc<Mailbox>);

    static INBOXES: LazyLock<Mutex<BTreeMap<PathBuf, Weak<Inbox>>>> =
        LazyLock::new(|| Mutex::new(BTreeMap::new()));

    impl Inbox {
        pub(crate) fn register(paths: &PlakPaths) -> (Arc<Self>, Arc<Mailbox>) {
            let mailbox = Arc::new(Mailbox::default());
            let inbox = Arc::new(Self(Arc::clone(&mailbox)));
            let mut boxes = INBOXES.lock().unwrap_or_else(PoisonError::into_inner);
            boxes.retain(|_, inbox| inbox.strong_count() > 0);
            boxes.insert(paths.product_database(), Arc::downgrade(&inbox));
            (inbox, mailbox)
        }

        pub(crate) fn send(&self, notice: Notice) {
            let mut pending = self
                .0
                .pending
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            pending.local.get_or_insert_with(Batch::default).add(notice);
            self.0.ready.notify_one();
        }

        pub(crate) fn send_external(&self, notice: Notice) {
            let mut pending = self
                .0
                .pending
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if pending.external.len() < 256 {
                pending.external.push_back(notice);
                self.0.ready.notify_one();
            }
        }

        #[cfg(test)]
        pub(crate) fn settle(&self) {
            let (send, receive) = std::sync::mpsc::sync_channel(1);
            self.send(Notice::Barrier(send));
            assert!(
                receive
                    .recv_timeout(std::time::Duration::from_secs(10))
                    .is_ok()
            );
        }
    }

    impl Drop for Inbox {
        fn drop(&mut self) {
            self.0
                .pending
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .closed = true;
            self.0.ready.notify_one();
        }
    }

    pub(super) fn deliver(paths: &PlakPaths, event: &Change<'_>) -> bool {
        let inbox = INBOXES
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&paths.product_database())
            .and_then(Weak::upgrade);
        if let Some(inbox) = inbox {
            inbox.send(match event {
                Change::Record(id) => Notice::Record((*id).to_owned()),
                Change::Tree(id, change) => match change {
                    TreeChange::Saved(tree) => {
                        Notice::Tree((*id).to_owned(), Some(Box::new(TreeFacts::of(tree))))
                    }
                    TreeChange::Deleted => Notice::Tree((*id).to_owned(), None),
                    TreeChange::Custody => Notice::Custody((*id).to_owned()),
                },
                Change::Input("config") => Notice::Configuration,
                Change::Input("visibility") => Notice::Visibility,
                Change::Input(_) => Notice::Revalidate,
            });
            true
        } else {
            false
        }
    }
}

#[cfg(not(feature = "web"))]
mod local {
    pub(super) const fn deliver(_paths: &super::PlakPaths, _event: &super::Change<'_>) -> bool {
        false
    }
}
