use super::{Future, Stage};

type Hook = std::pin::Pin<Box<dyn Future<Output = ()>>>;
thread_local! {
    pub(in crate::runtime::host_loop) static HOOK: std::cell::RefCell<Option<(Stage, Hook)>> = const { std::cell::RefCell::new(None) };
    pub(in crate::runtime::host_loop::prompt) static MAIL_ERROR: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) };
}
