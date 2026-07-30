// Adapted from xai-org/grok-build's scrollback viewport state.
// Copyright 2023-2026 SpaceXAI. Licensed under Apache-2.0.
// Modified for WeCode: extracted the generic scroll/follow state from the pager.

#[derive(Debug)]
pub(crate) struct ViewportState {
    scroll_offset: usize,
    total_height: usize,
    viewport_height: u16,
    follow_mode: bool,
}

impl Default for ViewportState {
    fn default() -> Self {
        Self {
            scroll_offset: 0,
            total_height: 0,
            viewport_height: 0,
            follow_mode: true,
        }
    }
}

impl ViewportState {
    pub(crate) fn update_layout(&mut self, total_height: usize, viewport_height: u16) {
        self.total_height = total_height;
        self.viewport_height = viewport_height;
        if self.follow_mode {
            self.scroll_offset = self.max_scroll_offset();
        } else {
            self.scroll_offset = self.scroll_offset.min(self.max_scroll_offset());
        }
    }

    pub(crate) fn scroll_up(&mut self, rows: u16) {
        self.scroll_offset = self.scroll_offset.saturating_sub(rows as usize);
        self.follow_mode = false;
    }

    pub(crate) fn scroll_down(&mut self, rows: u16) {
        let max_offset = self.max_scroll_offset();
        let before = self.scroll_offset;
        self.scroll_offset = self
            .scroll_offset
            .saturating_add(rows as usize)
            .min(max_offset);

        // Match Grok Pager's explicit overscroll gesture: landing at the
        // bottom does not re-enable follow until the next downward event.
        if rows > 0 && self.scroll_offset == before && self.scroll_offset >= max_offset {
            self.follow_mode = true;
        }
    }

    pub(crate) fn goto_top(&mut self) {
        self.scroll_offset = 0;
        self.follow_mode = false;
    }

    pub(crate) fn goto_bottom(&mut self) {
        self.scroll_offset = self.max_scroll_offset();
        self.follow_mode = true;
    }

    pub(crate) fn offset(&self) -> usize {
        self.scroll_offset
    }

    pub(crate) fn max_scroll_offset(&self) -> usize {
        self.total_height
            .saturating_sub(self.viewport_height as usize)
    }

    pub(crate) fn page_scroll_rows(&self) -> u16 {
        self.viewport_height.saturating_sub(2).max(1)
    }

    #[cfg(test)]
    pub(crate) fn is_follow_mode(&self) -> bool {
        self.follow_mode
    }
}

#[cfg(test)]
mod tests {
    use super::ViewportState;

    #[test]
    fn follow_mode_tracks_content_growth() {
        let mut viewport = ViewportState::default();
        viewport.update_layout(100, 20);
        assert_eq!(viewport.offset(), 80);

        viewport.update_layout(120, 20);
        assert_eq!(viewport.offset(), 100);
        assert!(viewport.is_follow_mode());
    }

    #[test]
    fn manual_scroll_preserves_position_when_content_grows() {
        let mut viewport = ViewportState::default();
        viewport.update_layout(100, 20);
        viewport.scroll_up(10);
        assert_eq!(viewport.offset(), 70);

        viewport.update_layout(120, 20);
        assert_eq!(viewport.offset(), 70);
        assert!(!viewport.is_follow_mode());
    }

    #[test]
    fn downward_overscroll_reengages_follow() {
        let mut viewport = ViewportState::default();
        viewport.update_layout(100, 20);
        viewport.scroll_up(10);
        viewport.scroll_down(10);
        assert_eq!(viewport.offset(), 80);
        assert!(!viewport.is_follow_mode());

        viewport.scroll_down(1);
        assert!(viewport.is_follow_mode());
    }

    #[test]
    fn offsets_are_not_limited_to_u16() {
        let mut viewport = ViewportState::default();
        viewport.update_layout(100_000, 20);
        assert_eq!(viewport.offset(), 99_980);
        assert_eq!(viewport.max_scroll_offset(), 99_980);
    }
}
