//! Session banner: arm, dismiss, and paint on `App`.

use super::{App, Frame, Rect};
use crate::tui::launch_intro::{self, BannerFacts, LaunchIntro};

impl App {
    /// Banner is armed. The composer stays live underneath; messages land below.
    #[must_use]
    pub(crate) fn showing_launch_intro(&self) -> bool {
        self.launch_intro.is_some() && self.viewed_agent_id().is_none()
    }

    /// Arm the session banner. There is no setting: the wordmark is always
    /// armed, and `session_banner_rows` is what declines to paint it in a pane
    /// too small to hold it. `plak resume`'s picker is the first surface, so
    /// the banner arms after that picker closes rather than under it.
    pub(crate) fn arm_launch_intro(&mut self) {
        if self.launch_resume_picker {
            return;
        }
        self.launch_intro = Some(LaunchIntro::open());
        self.mark_dirty();
    }

    #[cfg(test)]
    pub(crate) fn arm_launch_intro_cells(&mut self) {
        self.launch_intro = Some(LaunchIntro::open());
        self.mark_dirty();
    }

    pub(crate) fn dismiss_launch_intro(&mut self) {
        if self.launch_intro.take().is_none() {
            return;
        }
        self.mark_dirty();
    }

    pub(super) fn refresh_launch_binary_id(&mut self) {
        if self
            .launch_intro
            .as_mut()
            .is_some_and(LaunchIntro::refresh_binary_id)
        {
            self.mark_dirty();
        }
    }

    /// Rows the banner occupies at the top of the session document, or zero
    /// when it is not armed or the pane cannot hold the mark.
    pub(super) fn session_banner_rows(&self, area: Rect) -> usize {
        if self.showing_launch_intro() && launch_intro::fits(area) {
            usize::from(launch_intro::BANNER_HEIGHT)
        } else {
            0
        }
    }

    pub(super) fn draw_launch_intro(
        &mut self,
        frame: &mut Frame<'_>,
        area: Rect,
        skip_rows: usize,
    ) {
        if !self.showing_launch_intro() {
            return;
        }
        launch_intro::draw(
            frame,
            area,
            skip_rows,
            BannerFacts {
                light: self.theme.id.is_light(),
                version: self.version,
                session: self.session_id.as_deref(),
                binary_id: self.launch_intro.as_ref().and_then(|intro| intro.binary_id),
            },
        );
    }
}
