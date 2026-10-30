//! Only the terminal's owning thread may restore it or print a panic over it.

use std::panic::PanicHookInfo;
use std::sync::{Mutex, Once};
use std::thread::ThreadId;

static OWNER: Mutex<Option<ThreadId>> = Mutex::new(None);
static LAST_BACKGROUND_PANIC: Mutex<Option<PanicSite>> = Mutex::new(None);

pub(super) fn claim() {
    let current = std::thread::current().id();
    *OWNER
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(current);
}

pub(super) fn release() {
    *OWNER
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
}

pub(super) fn owner() -> Option<ThreadId> {
    *OWNER
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

type Hook = Box<dyn Fn(&PanicHookInfo<'_>) + Send + Sync + 'static>;

#[derive(Clone)]
struct PanicSite {
    thread: String,
    file: String,
    line: u32,
    column: u32,
}

fn make_hook(
    owner: impl Fn() -> Option<ThreadId> + Send + Sync + 'static,
    restore: impl Fn() + Send + Sync + 'static,
    print: Hook,
    report: impl Fn(PanicSite) + Send + Sync + 'static,
) -> Hook {
    Box::new(move |info| {
        let current = std::thread::current();
        match owner() {
            Some(owner) if owner == current.id() => {
                restore();
                print(info);
            }
            Some(_) => {
                let location = info.location();
                report(PanicSite {
                    thread: current.name().unwrap_or("<unnamed>").to_owned(),
                    file: location
                        .map_or("<unknown>", std::panic::Location::file)
                        .to_owned(),
                    line: location.map_or(0, std::panic::Location::line),
                    column: location.map_or(0, std::panic::Location::column),
                });
            }
            None => print(info),
        }
    })
}

pub(super) fn install(restore: fn()) {
    static INSTALL: Once = Once::new();
    INSTALL.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(make_hook(owner, restore, previous, |site| {
            *LAST_BACKGROUND_PANIC
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(site.clone());
            tracing::warn!(
                thread = site.thread.as_str(),
                file = site.file.as_str(),
                line = site.line,
                column = site.column,
                "background thread panicked while TUI owns the terminal"
            );
        }));
    });
}

pub(super) fn take_background_panic_report() -> Option<String> {
    if owner().is_some() {
        return None;
    }
    let site = LAST_BACKGROUND_PANIC
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take()?;
    Some(format!(
        "most recent background panic: thread {:?} at {:?}:{}:{}",
        site.thread, site.file, site.line, site.column
    ))
}

pub(crate) fn report_background_panic() {
    use std::io::Write as _;
    if let Some(report) = take_background_panic_report() {
        let _ = writeln!(std::io::stderr(), "{report}");
    }
}

#[cfg(test)]
mod tests;
