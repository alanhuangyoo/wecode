// Adapted from OpenAI Codex's TUI Markdown renderer.
// Copyright 2025 OpenAI. Licensed under Apache-2.0.
// Modified for WeCode: removed Codex file citations and updated pulldown-cmark to 0.13.

use pulldown_cmark::{CodeBlockKind, CowStr, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Span, Text};

#[derive(Clone, Debug)]
struct IndentContext {
    prefix: Vec<Span<'static>>,
    marker: Option<Vec<Span<'static>>>,
    is_list: bool,
}

impl IndentContext {
    fn new(prefix: Vec<Span<'static>>, marker: Option<Vec<Span<'static>>>, is_list: bool) -> Self {
        Self {
            prefix,
            marker,
            is_list,
        }
    }
}

pub(crate) fn render_markdown_text(input: &str) -> Text<'static> {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TASKLISTS);
    let parser = Parser::new_ext(input, options);
    let mut writer = Writer::new(parser);
    writer.run();
    while writer
        .text
        .lines
        .last()
        .is_some_and(|line| line.spans.is_empty())
    {
        writer.text.lines.pop();
    }
    writer.text
}

struct Writer<'a, I>
where
    I: Iterator<Item = Event<'a>>,
{
    iter: I,
    text: Text<'static>,
    inline_styles: Vec<Style>,
    indent_stack: Vec<IndentContext>,
    list_indices: Vec<Option<u64>>,
    link: Option<String>,
    needs_newline: bool,
    pending_marker_line: bool,
    in_paragraph: bool,
    in_code_block: bool,
}

impl<'a, I> Writer<'a, I>
where
    I: Iterator<Item = Event<'a>>,
{
    fn new(iter: I) -> Self {
        Self {
            iter,
            text: Text::default(),
            inline_styles: Vec::new(),
            indent_stack: Vec::new(),
            list_indices: Vec::new(),
            link: None,
            needs_newline: false,
            pending_marker_line: false,
            in_paragraph: false,
            in_code_block: false,
        }
    }

    fn run(&mut self) {
        while let Some(event) = self.iter.next() {
            self.handle_event(event);
        }
    }

    fn handle_event(&mut self, event: Event<'a>) {
        match event {
            Event::Start(tag) => self.start_tag(tag),
            Event::End(tag) => self.end_tag(tag),
            Event::Text(text) => self.text(text),
            Event::Code(code) => self.code(code),
            Event::SoftBreak => self.soft_break(),
            Event::HardBreak => self.hard_break(),
            Event::Rule => {
                if !self.text.lines.is_empty() {
                    self.push_blank_line();
                }
                self.push_line(Line::from("---").dim());
                self.needs_newline = true;
            }
            Event::Html(html) => self.html(html, false),
            Event::InlineHtml(html) => self.html(html, true),
            Event::TaskListMarker(checked) => {
                self.push_span(Span::from(if checked { "[x] " } else { "[ ] " }));
            }
            Event::InlineMath(math) | Event::DisplayMath(math) => self.text(math),
            Event::FootnoteReference(_) => {}
        }
    }

    fn start_tag(&mut self, tag: Tag<'a>) {
        match tag {
            Tag::Paragraph => self.start_paragraph(),
            Tag::Heading { level, .. } => self.start_heading(level),
            Tag::BlockQuote(_) => self.start_blockquote(),
            Tag::CodeBlock(kind) => {
                let indent = match kind {
                    CodeBlockKind::Fenced(_) => None,
                    CodeBlockKind::Indented => Some(Span::from(" ".repeat(4))),
                };
                self.start_codeblock(indent);
            }
            Tag::List(start) => self.start_list(start),
            Tag::Item => self.start_item(),
            Tag::Emphasis => self.push_inline_style(Style::new().italic()),
            Tag::Strong => self.push_inline_style(Style::new().bold()),
            Tag::Strikethrough => self.push_inline_style(Style::new().crossed_out()),
            Tag::Link { dest_url, .. } => self.push_link(dest_url.to_string()),
            _ => {}
        }
    }

    fn end_tag(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => self.end_paragraph(),
            TagEnd::Heading(_) => self.end_heading(),
            TagEnd::BlockQuote(_) => self.end_blockquote(),
            TagEnd::CodeBlock => self.end_codeblock(),
            TagEnd::List(_) => self.end_list(),
            TagEnd::Item => {
                self.indent_stack.pop();
                self.pending_marker_line = false;
            }
            TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough => {
                self.pop_inline_style();
            }
            TagEnd::Link => self.pop_link(),
            _ => {}
        }
    }

    fn start_paragraph(&mut self) {
        if self.needs_newline {
            self.push_blank_line();
        }
        self.push_line(Line::default());
        self.needs_newline = false;
        self.in_paragraph = true;
    }

    fn end_paragraph(&mut self) {
        self.needs_newline = true;
        self.in_paragraph = false;
        self.pending_marker_line = false;
    }

    fn start_heading(&mut self, level: HeadingLevel) {
        if self.needs_newline {
            self.push_line(Line::default());
            self.needs_newline = false;
        }
        let heading_style = match level {
            HeadingLevel::H1 => Style::new().bold().underlined(),
            HeadingLevel::H2 => Style::new().bold(),
            HeadingLevel::H3 => Style::new().bold().italic(),
            HeadingLevel::H4 | HeadingLevel::H5 | HeadingLevel::H6 => Style::new().italic(),
        };
        let content = format!("{} ", "#".repeat(level as usize));
        self.push_line(Line::from(Span::styled(content, heading_style)));
        self.push_inline_style(heading_style);
        self.needs_newline = false;
    }

    fn end_heading(&mut self) {
        self.needs_newline = true;
        self.pop_inline_style();
    }

    fn start_blockquote(&mut self) {
        if self.needs_newline {
            self.push_blank_line();
            self.needs_newline = false;
        }
        self.indent_stack
            .push(IndentContext::new(vec![Span::from("> ")], None, false));
    }

    fn end_blockquote(&mut self) {
        self.indent_stack.pop();
        self.needs_newline = true;
    }

    fn text(&mut self, text: CowStr<'a>) {
        if self.pending_marker_line {
            self.push_line(Line::default());
        }
        self.pending_marker_line = false;
        if self.in_code_block
            && !self.needs_newline
            && self
                .text
                .lines
                .last()
                .is_some_and(|line| !line.spans.is_empty())
        {
            self.push_line(Line::default());
        }
        for (index, line) in text.lines().enumerate() {
            if self.needs_newline {
                self.push_line(Line::default());
                self.needs_newline = false;
            }
            if index > 0 {
                self.push_line(Line::default());
            }
            let span = Span::styled(
                line.to_owned(),
                self.inline_styles.last().copied().unwrap_or_default(),
            );
            self.push_span(span);
        }
        self.needs_newline = false;
    }

    fn code(&mut self, code: CowStr<'a>) {
        if self.pending_marker_line {
            self.push_line(Line::default());
            self.pending_marker_line = false;
        }
        self.push_span(Span::from(code.into_string()).dim());
    }

    fn html(&mut self, html: CowStr<'a>, inline: bool) {
        self.pending_marker_line = false;
        for (index, line) in html.lines().enumerate() {
            if self.needs_newline {
                self.push_line(Line::default());
                self.needs_newline = false;
            }
            if index > 0 {
                self.push_line(Line::default());
            }
            let style = self.inline_styles.last().copied().unwrap_or_default();
            self.push_span(Span::styled(line.to_owned(), style));
        }
        self.needs_newline = !inline;
    }

    fn hard_break(&mut self) {
        self.push_line(Line::default());
    }

    fn soft_break(&mut self) {
        self.push_line(Line::default());
    }

    fn start_list(&mut self, index: Option<u64>) {
        if self.list_indices.is_empty() && self.needs_newline {
            self.push_line(Line::default());
        }
        self.list_indices.push(index);
    }

    fn end_list(&mut self) {
        self.list_indices.pop();
        self.needs_newline = true;
    }

    fn start_item(&mut self) {
        self.pending_marker_line = true;
        let depth = self.list_indices.len();
        let is_ordered = self.list_indices.last().is_some_and(Option::is_some);
        let width = depth * 4 - 3;
        let marker = if let Some(last_index) = self.list_indices.last_mut() {
            match last_index {
                None => Some(vec![Span::from(" ".repeat(width - 1) + "- ")]),
                Some(index) => {
                    *index += 1;
                    Some(vec![format!("{:width$}. ", *index - 1).light_blue()])
                }
            }
        } else {
            None
        };
        let indent_prefix = if depth == 0 {
            Vec::new()
        } else {
            let indent_len = if is_ordered { width + 2 } else { width + 1 };
            vec![Span::from(" ".repeat(indent_len))]
        };
        self.indent_stack
            .push(IndentContext::new(indent_prefix, marker, true));
        self.needs_newline = false;
    }

    fn start_codeblock(&mut self, indent: Option<Span<'static>>) {
        if !self.text.lines.is_empty() {
            self.push_blank_line();
        }
        self.in_code_block = true;
        self.indent_stack.push(IndentContext::new(
            vec![indent.unwrap_or_default()],
            None,
            false,
        ));
        self.needs_newline = true;
    }

    fn end_codeblock(&mut self) {
        self.needs_newline = true;
        self.in_code_block = false;
        self.indent_stack.pop();
    }

    fn push_inline_style(&mut self, style: Style) {
        let current = self.inline_styles.last().copied().unwrap_or_default();
        self.inline_styles.push(current.patch(style));
    }

    fn pop_inline_style(&mut self) {
        self.inline_styles.pop();
    }

    fn push_link(&mut self, destination: String) {
        self.link = Some(destination);
    }

    fn pop_link(&mut self) {
        if let Some(link) = self.link.take() {
            self.push_span(" (".into());
            self.push_span(link.cyan().underlined());
            self.push_span(")".into());
        }
    }

    fn push_line(&mut self, mut line: Line<'static>) {
        let was_pending = self.pending_marker_line;
        let mut spans = self.current_prefix_spans();
        spans.append(&mut line.spans);
        let blockquote_active = self
            .indent_stack
            .iter()
            .any(|context| context.prefix.iter().any(|span| span.content.contains('>')));
        let style = if blockquote_active {
            Style::new().green()
        } else {
            line.style
        };
        self.text.lines.push(Line::from_iter(spans).style(style));
        if was_pending {
            self.pending_marker_line = false;
        }
    }

    fn push_span(&mut self, span: Span<'static>) {
        if let Some(last) = self.text.lines.last_mut() {
            last.push_span(span);
        } else {
            self.push_line(Line::from(span));
        }
    }

    fn push_blank_line(&mut self) {
        if self.indent_stack.iter().all(|context| context.is_list) {
            self.text.lines.push(Line::default());
        } else {
            self.push_line(Line::default());
        }
    }

    fn current_prefix_spans(&self) -> Vec<Span<'static>> {
        let mut prefix = Vec::new();
        let last_marker_index = if self.pending_marker_line {
            self.indent_stack
                .iter()
                .enumerate()
                .rev()
                .find_map(|(index, context)| context.marker.as_ref().map(|_| index))
        } else {
            None
        };
        let last_list_index = self
            .indent_stack
            .iter()
            .rposition(|context| context.is_list);

        for (index, context) in self.indent_stack.iter().enumerate() {
            if self.pending_marker_line {
                if Some(index) == last_marker_index
                    && let Some(marker) = &context.marker
                {
                    prefix.extend(marker.iter().cloned());
                    continue;
                }
                if context.is_list && last_marker_index.is_some_and(|marker| marker > index) {
                    continue;
                }
            } else if context.is_list && Some(index) != last_list_index {
                continue;
            }
            prefix.extend(context.prefix.iter().cloned());
        }
        prefix
    }
}

#[cfg(test)]
mod tests {
    use super::render_markdown_text;
    use ratatui::style::Modifier;

    fn visible_text(markdown: &str) -> String {
        render_markdown_text(markdown)
            .lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect()
    }

    #[test]
    fn renders_common_inline_markdown() {
        let rendered = render_markdown_text("**bold** and `code` and *italic*");
        let visible = visible_text("**bold** and `code` and *italic*");
        assert_eq!(visible, "bold and code and italic");

        let bold = rendered.lines[0]
            .spans
            .iter()
            .find(|span| span.content == "bold")
            .expect("bold span");
        assert!(bold.style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn renders_nested_lists_and_links() {
        let visible = visible_text("- one\n  1. [two](https://example.com)");
        assert!(visible.contains("- one"));
        assert!(visible.contains("1. two (https://example.com)"));
    }
}
