use std::borrow::Cow;
use std::ops::{Index, IndexMut};
use std::sync::Arc;

use helix_stdx::Range;
use ropey::Rope;
use smallvec::SmallVec;

use crate::indent::indent_level_for_line;
use crate::movement::Direction;
use crate::snippets::elaborate;
use crate::snippets::TabstopIdx;
use crate::snippets::{Snippet, SnippetElement, Transform};
use crate::{selection, Selection, Tendril, Transaction};

#[derive(Debug, Clone, PartialEq)]
pub enum TabstopKind {
    Choice { choices: Arc<[Tendril]> },
    Placeholder,
    Empty,
    Transform(Arc<Transform>),
}

#[derive(Debug, PartialEq)]
pub struct Tabstop {
    pub ranges: SmallVec<[Range; 1]>,
    pub parent: Option<TabstopIdx>,
    pub kind: TabstopKind,
}

impl Tabstop {
    pub fn has_placeholder(&self) -> bool {
        matches!(
            self.kind,
            TabstopKind::Choice { .. } | TabstopKind::Placeholder
        )
    }

    pub fn selection(
        &self,
        direction: Direction,
        primary_idx: usize,
        snippet_ranges: usize,
    ) -> Selection {
        Selection::new(
            self.ranges
                .iter()
                .map(|&range| {
                    let mut range = selection::Range::new(range.start, range.end);
                    if direction == Direction::Backward {
                        range = range.flip()
                    }
                    range
                })
                .collect(),
            primary_idx * (self.ranges.len() / snippet_ranges),
        )
    }
}

#[derive(Debug, Default, PartialEq)]
pub struct RenderedSnippet {
    pub tabstops: Vec<Tabstop>,
    pub ranges: Vec<Range>,
}

impl RenderedSnippet {
    pub fn first_selection(&self, direction: Direction, primary_idx: usize) -> Selection {
        self.tabstops[0].selection(direction, primary_idx, self.ranges.len())
    }
}

impl Index<TabstopIdx> for RenderedSnippet {
    type Output = Tabstop;
    fn index(&self, index: TabstopIdx) -> &Tabstop {
        &self.tabstops[index.0]
    }
}

impl IndexMut<TabstopIdx> for RenderedSnippet {
    fn index_mut(&mut self, index: TabstopIdx) -> &mut Tabstop {
        &mut self.tabstops[index.0]
    }
}

impl Snippet {
    pub fn prepare_render(&self) -> RenderedSnippet {
        let tabstops =
            self.tabstops()
                .map(|tabstop| Tabstop {
                    ranges: SmallVec::new(),
                    parent: tabstop.parent,
                    kind: match &tabstop.kind {
                        elaborate::TabstopKind::Choice { choices } => TabstopKind::Choice {
                            choices: choices.clone(),
                        },
                        // start out as empty the first non-empty placeholder will change this to a aplaceholder automatically
                        elaborate::TabstopKind::Empty
                        | elaborate::TabstopKind::Placeholder { .. } => TabstopKind::Empty,
                        elaborate::TabstopKind::Transform(transform) => {
                            TabstopKind::Transform(transform.clone())
                        }
                    },
                })
                .collect();
        RenderedSnippet {
            tabstops,
            ranges: Vec::new(),
        }
    }

    pub fn render_at(
        &self,
        snippet: &mut RenderedSnippet,
        newline_with_offset: &str,
        resolve_var: &mut VariableResolver,
        pos: usize,
    ) -> (Tendril, usize) {
        let mut ctx = SnippetRender {
            dst: snippet,
            src: self,
            newline_with_offset,
            text: Tendril::new(),
            off: pos,
            resolve_var,
        };
        ctx.render_elements(self.elements());
        let end = ctx.off;
        let text = ctx.text;
        snippet.ranges.push(Range { start: pos, end });
        (text, end - pos)
    }

    pub fn render(
        &self,
        doc: &Rope,
        selection: &Selection,
        change_range: impl FnMut(&selection::Range) -> (usize, usize),
        ctx: &mut SnippetRenderCtx,
    ) -> (Transaction, Selection, RenderedSnippet) {
        let mut snippet = self.prepare_render();
        let mut off = 0;
        let (transaction, selection) = Transaction::change_by_selection_ignore_overlapping(
            doc,
            selection,
            change_range,
            |replacement_start, replacement_end| {
                let line_idx = doc.char_to_line(replacement_start);
                let indent_level =
                    indent_level_for_line(doc.line(line_idx), ctx.tab_width, ctx.indent_width)
                        * ctx.indent_width;

                let newline_with_offset = format!(
                    "{line_ending}{blank:indent_level$}",
                    line_ending = ctx.line_ending,
                    blank = ""
                );
                let (replacement, replacement_len) = self.render_at(
                    &mut snippet,
                    &newline_with_offset,
                    &mut ctx.resolve_var,
                    (replacement_start as i128 + off) as usize,
                );
                off +=
                    replacement_start as i128 - replacement_end as i128 + replacement_len as i128;

                Some(replacement)
            },
        );
        (transaction, selection, snippet)
    }
}

pub type VariableResolver = dyn FnMut(&str) -> Option<Cow<str>>;
pub struct SnippetRenderCtx {
    pub resolve_var: Box<VariableResolver>,
    pub tab_width: usize,
    pub indent_width: usize,
    pub line_ending: &'static str,
}

struct SnippetRender<'a> {
    dst: &'a mut RenderedSnippet,
    src: &'a Snippet,
    newline_with_offset: &'a str,
    text: Tendril,
    off: usize,
    resolve_var: &'a mut VariableResolver,
}

impl SnippetRender<'_> {
    fn render_elements(&mut self, elements: &[SnippetElement]) {
        for element in elements {
            self.render_element(element)
        }
    }

    fn render_element(&mut self, element: &SnippetElement) {
        match *element {
            SnippetElement::Tabstop { idx } => self.render_tabstop(idx),
            SnippetElement::Variable {
                ref name,
                ref default,
                ref transform,
            } => {
                if let Some(val) = (self.resolve_var)(name) {
                    if let Some(transform) = transform {
                        transform.apply(&val, &mut self.text);
                    } else {
                        self.push_str(&val)
                    }
                } else if let Some(default) = default {
                    self.render_elements(default)
                }
            }
            SnippetElement::Text(ref text) => {
                let mut lines = text
                    .split('\n')
                    .map(|it| it.strip_suffix('\r').unwrap_or(it));
                let first_line = lines.next().unwrap();
                self.push_str(first_line);
                for line in lines {
                    // hacks: all supported indentation (tabs and spaces) and newline
                    // chars are ascii so we can use len()
                    debug_assert!(self.newline_with_offset.is_ascii());
                    self.push_str(self.newline_with_offset);
                    self.push_str(line);
                }
            }
        }
    }

    fn push_str(&mut self, text: &str) {
        self.text.push_str(text);
        self.off += text.chars().count();
    }

    fn render_tabstop(&mut self, tabstop: TabstopIdx) {
        let start = self.off;
        let end = match &self.src[tabstop].kind {
            elaborate::TabstopKind::Placeholder { default } if !default.is_empty() => {
                self.render_elements(default);
                self.dst[tabstop].kind = TabstopKind::Placeholder;
                self.off
            }
            _ => start,
        };
        self.dst[tabstop].ranges.push(Range { start, end });
    }
}

#[cfg(test)]
mod tests {
    use helix_stdx::Range;

    use crate::snippets::render::Tabstop;
    use crate::snippets::Snippet;

    use super::TabstopKind;

    fn assert_snippet(snippet: &str, expect: &str, tabstops: &[Tabstop]) {
        let snippet = Snippet::parse(snippet).unwrap();
        let mut rendered_snippet = snippet.prepare_render();
        let rendered_text = snippet
            .render_at(&mut rendered_snippet, "\t\n", &mut |_| None, 0)
            .0;
        assert_eq!(rendered_text, expect);
        assert_eq!(&rendered_snippet.tabstops, tabstops);
        assert_eq!(
            rendered_snippet.ranges.last().unwrap().end,
            rendered_text.chars().count()
        );
        assert_eq!(rendered_snippet.ranges.last().unwrap().start, 0)
    }

    #[test]
    fn rust_macro() {
        assert_snippet(
            "macro_rules! ${1:name} {\n    ($3) => {\n        $2\n    };\n}",
            "macro_rules! name {\t\n    () => {\t\n        \t\n    };\t\n}",
            &[
                Tabstop {
                    ranges: vec![Range { start: 13, end: 17 }].into(),
                    parent: None,
                    kind: TabstopKind::Placeholder,
                },
                Tabstop {
                    ranges: vec![Range { start: 42, end: 42 }].into(),
                    parent: None,
                    kind: TabstopKind::Empty,
                },
                Tabstop {
                    ranges: vec![Range { start: 26, end: 26 }].into(),
                    parent: None,
                    kind: TabstopKind::Empty,
                },
                Tabstop {
                    ranges: vec![Range { start: 53, end: 53 }].into(),
                    parent: None,
                    kind: TabstopKind::Empty,
                },
            ],
        );
    }
}
