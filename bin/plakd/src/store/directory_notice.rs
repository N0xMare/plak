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
        && sender
            .set_write_timeout(Some(std::time::Duration::from_millis(100)))
            .is_ok()
    {
        let _ = sender.send_to(format!("{kind} {id}").as_bytes(), path);
    }
}

#[cfg(feature = "web")]
pub(crate) mod local {
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, LazyLock, Mutex, PoisonError, Weak, mpsc};

    use plakous_hosted::orchestration::OrchestrationSnapshot;
    use plakous_hosted::runtime::{ResumeAfterLoss, operator_attention::PendingTreeWork};
    use plakous_recursive_subagents::tree::ProductAgentTree;

    use super::{Change, PathBuf, PlakPaths, TreeChange};

    #[derive(PartialEq, Eq)]
    pub(crate) struct TreeFacts {
        pub(crate) roster: OrchestrationSnapshot,
        pub(crate) hold: PendingTreeWork,
        pub(crate) resume: PendingTreeWork,
    }

    impl TreeFacts {
        pub(crate) fn of(tree: &ProductAgentTree) -> Self {
            Self {
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
        Gap,
        Barrier(mpsc::SyncSender<()>),
    }

    pub(crate) struct Inbox {
        send: mpsc::SyncSender<Notice>,
        pub(crate) gap: Arc<AtomicBool>,
    }

    static INBOXES: LazyLock<Mutex<BTreeMap<PathBuf, Weak<Inbox>>>> =
        LazyLock::new(|| Mutex::new(BTreeMap::new()));

    impl Inbox {
        pub(crate) fn register(paths: &PlakPaths) -> (Arc<Self>, mpsc::Receiver<Notice>) {
            let (send, receive) = mpsc::sync_channel(256);
            let inbox = Arc::new(Self {
                send,
                gap: Arc::default(),
            });
            let mut boxes = INBOXES.lock().unwrap_or_else(PoisonError::into_inner);
            boxes.retain(|_, inbox| inbox.strong_count() > 0);
            boxes.insert(paths.product_database(), Arc::downgrade(&inbox));
            (inbox, receive)
        }

        pub(crate) fn send(&self, notice: Notice) {
            if let Err(mpsc::TrySendError::Full(_)) = self.send.try_send(notice) {
                self.gap.store(true, Ordering::Release);
                let _ = self.send.try_send(Notice::Gap);
            }
        }

        pub(crate) fn settle(&self) {
            let (send, receive) = mpsc::sync_channel(1);
            if self.send.send(Notice::Barrier(send)).is_ok() {
                let _ = receive.recv_timeout(std::time::Duration::from_secs(10));
            }
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
                Change::Input(_) => Notice::Gap,
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
