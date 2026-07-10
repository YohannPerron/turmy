use ratatui::{
    layout::Rect,
    text::{Line, Span},
};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Pane {
    #[default]
    Jobs,
    Details,
    Output,
}

impl Pane {
    pub fn next(self) -> Self {
        match self {
            Self::Jobs => Self::Details,
            Self::Details => Self::Output,
            Self::Output => Self::Jobs,
        }
    }

    pub fn previous(self) -> Self {
        match self {
            Self::Jobs => Self::Output,
            Self::Details => Self::Jobs,
            Self::Output => Self::Details,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PaneViewport {
    area: Rect,
    content_width: usize,
    horizontal_offset: usize,
}

impl PaneViewport {
    pub fn update(&mut self, area: Rect, content_width: usize) {
        self.area = area;
        self.content_width = content_width;
        self.clamp();
    }

    pub fn area(self) -> Rect {
        self.area
    }

    pub fn contains(self, column: u16, row: u16) -> bool {
        column >= self.area.x
            && column < self.area.x.saturating_add(self.area.width)
            && row >= self.area.y
            && row < self.area.y.saturating_add(self.area.height)
    }

    pub fn visible_width(self) -> usize {
        self.area.width.saturating_sub(2) as usize
    }

    pub fn horizontal_offset(self) -> usize {
        self.horizontal_offset
    }

    pub fn max_horizontal_offset(self) -> usize {
        self.content_width.saturating_sub(self.visible_width())
    }

    pub fn scroll_left_by(&mut self, amount: usize) -> bool {
        let previous = self.horizontal_offset;
        self.horizontal_offset = self.horizontal_offset.saturating_sub(amount);
        previous != self.horizontal_offset
    }

    pub fn scroll_right_by(&mut self, amount: usize) -> bool {
        let previous = self.horizontal_offset;
        self.horizontal_offset = self
            .horizontal_offset
            .saturating_add(amount)
            .min(self.max_horizontal_offset());
        previous != self.horizontal_offset
    }

    pub fn reset_horizontal(&mut self) -> bool {
        let changed = self.horizontal_offset != 0;
        self.horizontal_offset = 0;
        changed
    }

    fn clamp(&mut self) {
        self.horizontal_offset = self.horizontal_offset.min(self.max_horizontal_offset());
    }
}

pub fn display_width(text: &str) -> usize {
    UnicodeWidthStr::width(text)
}

pub fn clip_line(line: &Line<'_>, offset: usize, width: usize) -> Line<'static> {
    let mut source_column = 0usize;
    let mut visible_columns = 0usize;
    let mut spans = Vec::new();

    'spans: for span in &line.spans {
        let mut content = String::new();

        for grapheme in span.content.graphemes(true) {
            let grapheme_width = display_width(grapheme);
            let grapheme_start = source_column;
            let grapheme_end = source_column.saturating_add(grapheme_width);
            source_column = grapheme_end;

            if grapheme_end <= offset {
                continue;
            }

            if grapheme_start < offset {
                let remainder = grapheme_end.saturating_sub(offset);
                let spaces = remainder.min(width.saturating_sub(visible_columns));
                content.extend(std::iter::repeat_n(' ', spaces));
                visible_columns = visible_columns.saturating_add(spaces);
            } else if visible_columns.saturating_add(grapheme_width) <= width {
                content.push_str(grapheme);
                visible_columns = visible_columns.saturating_add(grapheme_width);
            } else {
                if !content.is_empty() {
                    spans.push(Span::styled(content, span.style));
                }
                break 'spans;
            }

            if visible_columns >= width {
                break;
            }
        }

        if !content.is_empty() {
            spans.push(Span::styled(content, span.style));
        }
        if visible_columns >= width {
            break;
        }
    }

    Line {
        style: line.style,
        alignment: line.alignment,
        spans,
    }
}

#[cfg(test)]
pub fn wrap_line(text: &str, first_width: usize, continuation_width: usize) -> Vec<String> {
    if text.is_empty() {
        return vec![String::new()];
    }
    if first_width == 0 && continuation_width == 0 {
        return vec![text.to_string()];
    }

    let mut lines = Vec::new();
    let mut current = String::new();
    let mut current_width = 0usize;
    let mut available_width = if first_width == 0 {
        continuation_width
    } else {
        first_width
    };

    for grapheme in text.graphemes(true) {
        let grapheme_width = display_width(grapheme);
        if available_width > 0
            && !current.is_empty()
            && current_width.saturating_add(grapheme_width) > available_width
        {
            lines.push(std::mem::take(&mut current));
            current_width = 0;
            available_width = continuation_width;
        }

        current.push_str(grapheme);
        current_width = current_width.saturating_add(grapheme_width);

        if available_width > 0 && current_width >= available_width {
            lines.push(std::mem::take(&mut current));
            current_width = 0;
            available_width = continuation_width;
        }
    }

    if !current.is_empty() {
        lines.push(current);
    }
    lines
}

#[cfg(test)]
mod tests {
    use ratatui::style::{Color, Style};

    use super::*;

    #[test]
    fn pane_focus_cycles_in_both_directions() {
        assert_eq!(Pane::Jobs.next(), Pane::Details);
        assert_eq!(Pane::Details.next(), Pane::Output);
        assert_eq!(Pane::Output.next(), Pane::Jobs);
        assert_eq!(Pane::Jobs.previous(), Pane::Output);
        assert_eq!(Pane::Output.previous(), Pane::Details);
    }

    #[test]
    fn viewport_clamps_offsets_to_content() {
        let mut viewport = PaneViewport::default();
        viewport.update(Rect::new(0, 0, 12, 5), 30);

        assert_eq!(viewport.visible_width(), 10);
        assert!(viewport.scroll_right_by(100));
        assert_eq!(viewport.horizontal_offset(), 20);

        viewport.update(Rect::new(0, 0, 22, 5), 15);
        assert_eq!(viewport.horizontal_offset(), 0);
        assert!(!viewport.scroll_left_by(1));
    }

    #[test]
    fn clips_styled_lines_by_display_columns() {
        let yellow = Style::default().fg(Color::Yellow);
        let blue = Style::default().fg(Color::Blue);
        let line = Line::from(vec![
            Span::styled("ab测试", yellow),
            Span::styled("🚀cd", blue),
        ]);

        let clipped = clip_line(&line, 2, 6);
        let content = clipped
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();

        assert_eq!(display_width(&content), 6);
        assert_eq!(content, "测试🚀");
        assert_eq!(clipped.spans[0].style, yellow);
        assert_eq!(clipped.spans[1].style, blue);
    }

    #[test]
    fn replaces_a_partially_clipped_wide_grapheme_with_space() {
        let line = Line::raw("测A");
        let clipped = clip_line(&line, 1, 2);

        assert_eq!(clipped.to_string(), " A");
        assert_eq!(clipped.width(), 2);
    }

    #[test]
    fn wraps_text_by_display_width() {
        assert_eq!(wrap_line("ab测试cd", 4, 3), ["ab测", "试c", "d"]);
        assert_eq!(wrap_line("", 4, 2), [""]);
        assert_eq!(wrap_line("123456789", 4, 2), ["1234", "56", "78", "9"]);
        assert_eq!(wrap_line("123456789", 4, 0), ["1234", "56789"]);
        assert_eq!(wrap_line("123456789", 0, 0), ["123456789"]);
    }
}
