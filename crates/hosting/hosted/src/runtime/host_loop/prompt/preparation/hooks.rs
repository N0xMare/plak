use super::{Future, Stage};

type Hook = std::pin::Pin<Box<dyn Future<Output = ()>>>;
thread_local! {
    pub(in crate::runtime::host_loop::prompt) static HOOK: std::cell::RefCell<Option<(Stage, Hook)>> = const { std::cell::RefCell::new(None) };
}
