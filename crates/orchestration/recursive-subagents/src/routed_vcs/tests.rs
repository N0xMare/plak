use super::*;
use crate::remote_vcs::RemoteVcs;
use plakous::tools::runner::{RunnerCall, RunnerCallFuture, RunnerLink};
use serde_json::json;
use std::sync::atomic::{AtomicUsize, Ordering};

const HOME: &str = "/root-repo";
const MIRROR: &str = "/copies/peer/tree";
const COPY: &str = "/copies/peer/tree/.plak/worktrees/c1";

fn offline(runner: &str) -> Arc<dyn VcsOps> {
    Arc::new(RemoteVcs::with_link(|| None, runner.into(), "tree".into()))
}

#[test]
fn restore_route_validates_mirror_layout_and_never_dials() {
    let calls = Arc::new(AtomicUsize::new(0));
    let count = Arc::clone(&calls);
    let routed = RoutedVcs::with_resolver(offline("home"), "tree", HOME.into(), move |id| {
        count.fetch_add(1, Ordering::Relaxed);
        offline(id)
    });
    for bad in [
        "copies/peer/tree/.plak/worktrees/c1",
        "/copies/other/tree/.plak/worktrees/c1",
        "/copies/peer/wrong/.plak/worktrees/c1",
        "/copies/peer/tree/.plak/other/c1",
        "/copies/escape/../peer/tree/.plak/worktrees/c1",
    ] {
        assert!(
            routed.restore_route("peer", Path::new(bad)).is_err(),
            "{bad}"
        );
    }
    let root_copy = Path::new("/root-repo/.plak/worktrees/same-runner");
    routed.restore_route("home", root_copy).unwrap();
    assert_eq!(routed.path_runner(root_copy).as_deref(), Some("home"));
    assert_eq!(calls.load(Ordering::Relaxed), 0);
    routed.restore_route("peer", Path::new(COPY)).unwrap();
    assert_eq!(routed.path_runner(Path::new(COPY)).as_deref(), Some("peer"));
    assert_eq!(routed.placement_ready(Path::new(COPY)), Some(false));
}

#[test]
fn restore_route_refuses_overlap_without_replacing_the_first_route() {
    let routed = RoutedVcs::with_resolver(offline("home"), "tree", HOME.into(), offline);
    assert!(
        routed
            .restore_route("peer", Path::new("/root-repo/peer/tree/.plak/worktrees/c1"))
            .is_err()
    );
    routed.restore_route("peer", Path::new(COPY)).unwrap();
    let nested = Path::new("/copies/peer/tree/other/tree/.plak/worktrees/c2");
    assert!(routed.restore_route("other", nested).is_err());
    assert_eq!(routed.path_runner(nested).as_deref(), Some("peer"));
    assert!(routed.transfer_port("other").is_none());
}

struct MirrorLink;

impl RunnerLink for MirrorLink {
    fn call(&self, call: RunnerCall) -> RunnerCallFuture {
        assert_eq!(
            call.payload(),
            &json!({"op":"transfer_root","runner_id":"peer","tree_id":"tree"})
        );
        Box::pin(std::future::ready(Ok(json!({"Ok": MIRROR}))))
    }
}

#[tokio::test]
async fn with_resolver_registers_the_mirror_only_after_transfer_admission() {
    let routed = RoutedVcs::with_resolver(offline("home"), "tree", HOME.into(), |id| {
        if id == "peer" {
            Arc::new(RemoteVcs::new(
                Arc::new(MirrorLink),
                id.into(),
                "tree".into(),
            ))
        } else {
            offline(id)
        }
    });
    assert!(routed.transfer_port("peer").is_none());
    assert!(routed.prepare_transfer("unready").await.is_err());
    assert!(routed.transfer_port("unready").is_none());
    routed.prepare_transfer("peer").await.unwrap();
    assert_eq!(routed.transfer_mirror("peer"), Some(PathBuf::from(MIRROR)));
    assert_eq!(routed.path_runner(Path::new(COPY)).as_deref(), Some("peer"));
    assert_eq!(routed.path_runner(Path::new(HOME)).as_deref(), Some("home"));
}
