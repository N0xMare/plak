//! Child route construction follows the parent's scoped state roots.
use super::super::{SessionToolFence, SpawnParent};
use plakous::{model::OutputBoundDisposition, sessions::SessionRuntimeBinding};

pub(crate) struct ChildRuntimeRequest<'a> {
    pub parent: &'a SpawnParent,
    pub agent_id: &'a str,
    pub path: &'a str,
    pub account: Option<&'a str>,
    pub tuple_id: Option<&'a str>,
    pub effort: Option<&'a str>,
    pub opening_durable_session: bool,
    pub fence: SessionToolFence,
    pub created_output_bound: Option<OutputBoundDisposition>,
}
pub(crate) trait ChildRuntimeFactory: Send + Sync {
    fn bind<'a>(&'a self, request: ChildRuntimeRequest<'a>) -> ChildRuntimeFuture<'a>;
}
pub(crate) struct ProductionChildRuntimeFactory;
impl ChildRuntimeFactory for ProductionChildRuntimeFactory {
    fn bind<'a>(&'a self, request: ChildRuntimeRequest<'a>) -> ChildRuntimeFuture<'a> {
        Box::pin(super::bind::production_child_runtime(request))
    }
}

pub(crate) type ChildRuntimeFuture<'a> =
    std::pin::Pin<Box<dyn Future<Output = Result<SessionRuntimeBinding, String>> + Send + 'a>>;

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "deterministic scoped route fixtures")]
mod tests {
    use super::*;
    use crate::agent::{ProductStateRoots, api_topology_parent};
    use crate::runtime::test_kit::SwitchableChildRuntime;
    use std::sync::Arc;

    fn parent(roots: ProductStateRoots, path: &std::path::Path) -> SpawnParent {
        let record = crate::sessions::SessionRecord::new(path.to_path_buf());
        SpawnParent::Api(api_topology_parent(
            path.to_path_buf(),
            "scoped-factory".into(),
            &record,
            path.to_path_buf(),
            roots,
            "offline".into(),
            None,
            crate::binding::ApiTarget::OpenRouter {
                wire: crate::binding::ResolvedProtocol::Chat,
            },
            "scoped-factory".into(),
            false,
            1,
            false,
        ))
    }
    async fn bind(parent: &SpawnParent, opening: bool) -> String {
        match super::super::bind::child_session_runtime_binding(
            parent,
            "a2",
            "/work/a2",
            None,
            None,
            None,
            opening,
            SessionToolFence::Full,
            None,
        )
        .await
        {
            Err(error) => error,
            Ok(_) => panic!("the fixture route must refuse before provider resolution"),
        }
    }
    #[tokio::test]
    async fn child_routes_are_scoped_to_roots_and_survive_new_child_scoping() {
        let home = tempfile::tempdir().unwrap();
        let shared = ProductStateRoots::with_doubles(
            home.path().join("first"),
            home.path().join("state-first"),
        );
        let first_home = shared.paths()[0].clone();
        let first_binding = SwitchableChildRuntime::new(Arc::new(move |id, path, roots| {
            assert_eq!(roots.paths()[0], first_home);
            Err(format!("first:{id}:{path}"))
        }));
        let second_binding =
            SwitchableChildRuntime::new(Arc::new(|id, path, _| Err(format!("second:{id}:{path}"))));
        let first = first_binding.with_roots(shared.clone());
        let second = second_binding.with_roots(shared);
        let first_parent = parent(first.clone(), home.path());
        let second_parent = parent(second, home.path());
        for opening in [false, true] {
            let (one, two) =
                tokio::join!(bind(&first_parent, opening), bind(&second_parent, opening));
            assert_eq!(one, "first:a2:/work/a2");
            assert_eq!(two, "second:a2:/work/a2");
        }
        {
            let _replacement = first_binding.install(Arc::new(|_, _, _| Err("replacement".into())));
            assert_eq!(bind(&first_parent, false).await, "replacement");
            assert_eq!(bind(&second_parent, false).await, "second:a2:/work/a2");
        }
        let older = first_binding.install(Arc::new(|_, _, _| Err("older".into())));
        let newer = first_binding.install(Arc::new(|_, _, _| Err("newer".into())));
        drop(older);
        assert_eq!(bind(&first_parent, false).await, "newer");
        drop(newer);
        assert_eq!(bind(&first_parent, false).await, "first:a2:/work/a2");
    }
}
