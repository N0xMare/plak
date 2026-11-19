//! Terminal OAuth workflow over opaque account-operation tokens.

use std::io;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use crate::CliResult;
use crate::oauth_ui;

/// Machine effects belong to terminal presentation, separate from account authority.
pub(crate) trait LoginUi {
    fn copy_url(&mut self, url: &str) -> oauth_ui::CopyResult;
    fn read_secret(
        &mut self,
        input: &mut dyn io::BufRead,
        stderr: &mut dyn io::Write,
        maximum: usize,
    ) -> CliResult<super::secret_input::SecretLine>;
    fn ssh_session_detected(&self) -> bool;
    fn present_url(
        &mut self,
        out: &mut dyn io::Write,
        url: &str,
        account: &str,
        paste: bool,
    ) -> io::Result<()>;
    fn open_browser(&mut self, url: &str) -> bool;
    fn wait_reminder(&mut self, account: String, waiting: Arc<AtomicBool>);
}

pub(crate) struct SystemLoginUi;

impl LoginUi for SystemLoginUi {
    fn copy_url(&mut self, url: &str) -> oauth_ui::CopyResult {
        oauth_ui::try_copy_to_clipboard(url)
    }
    fn read_secret(
        &mut self,
        input: &mut dyn io::BufRead,
        stderr: &mut dyn io::Write,
        maximum: usize,
    ) -> CliResult<super::secret_input::SecretLine> {
        super::secret_input::read_system(input, stderr, maximum)
    }
    fn ssh_session_detected(&self) -> bool {
        oauth_ui::ssh_session_detected()
    }

    fn present_url(
        &mut self,
        out: &mut dyn io::Write,
        url: &str,
        account: &str,
        paste: bool,
    ) -> io::Result<()> {
        oauth_ui::present_authorize_url(out, url, account, paste)
    }

    fn open_browser(&mut self, url: &str) -> bool {
        crate::terminal_application::request_browser_open(url)
    }

    fn wait_reminder(&mut self, account: String, waiting: Arc<AtomicBool>) {
        oauth_ui::spawn_soft_wait_reminder(account, waiting);
    }
}

#[cfg(test)]
#[derive(Default)]
pub(crate) struct TestLoginUi {
    pub(crate) ssh: bool,
    pub(crate) copied_urls: Vec<String>,
    pub(crate) clipboard_available: bool,
    pub(crate) browser_urls: Vec<String>,
    pub(crate) presented_urls: Vec<String>,
    pub(crate) waiting: Option<Arc<AtomicBool>>,
}

#[cfg(test)]
impl LoginUi for TestLoginUi {
    fn copy_url(&mut self, url: &str) -> oauth_ui::CopyResult {
        self.copied_urls.push(url.to_owned());
        if self.clipboard_available {
            oauth_ui::CopyResult::Copied
        } else {
            oauth_ui::CopyResult::Unavailable
        }
    }
    fn read_secret(
        &mut self,
        input: &mut dyn io::BufRead,
        stderr: &mut dyn io::Write,
        maximum: usize,
    ) -> CliResult<super::secret_input::SecretLine> {
        super::secret_input::read_plain(input, stderr, maximum)
    }
    fn ssh_session_detected(&self) -> bool {
        self.ssh
    }
    fn present_url(
        &mut self,
        out: &mut dyn io::Write,
        url: &str,
        _account: &str,
        _paste: bool,
    ) -> io::Result<()> {
        self.presented_urls.push(url.to_owned());
        writeln!(out, "{url}")
    }
    fn open_browser(&mut self, url: &str) -> bool {
        self.browser_urls.push(url.to_owned());
        false
    }
    fn wait_reminder(&mut self, _account: String, waiting: Arc<AtomicBool>) {
        self.waiting = Some(waiting);
    }
}
