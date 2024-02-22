use std::borrow::Cow;
use std::ops::{Index, IndexMut};
use std::sync::Arc;

use helix_stdx::Range;
use ropey::Rope;
use smallvec::SmallVec;

use crate::indent::indent_level_for_line;
use crate::snippets::elaborate::SnippetElement;
use crate::snippets::elaborate::{self, Transform};
use crate::snippets::TabstopIdx;
use crate::{selection, Selection, Tendril, Transaction};

#[derive(Debug)]
pub enum TabstopKind {
    Choice { choices: Arc<[Tendril]> },
    Placeholder,
    Empty,
    Transform(Arc<Transform>),
}

#[derive(Debug)]
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
}

#[derive(Debug, Default)]
pub struct Snippet {
    pub tabstops: Vec<Tabstop>,
    pub ranges: Vec<Range>,
}

impl Index<TabstopIdx> for Snippet {
    type Output = Tabstop;
    fn index(&self, index: TabstopIdx) -> &Tabstop {
        &self.tabstops[index.0]
    }
}

impl IndexMut<TabstopIdx> for Snippet {
    fn index_mut(&mut self, index: TabstopIdx) -> &mut Tabstop {
        &mut self.tabstops[index.0]
    }
}

impl elaborate::Snippet {
    pub fn prepare_render(&self) -> Snippet {
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
        Snippet {
            tabstops,
            ranges: Vec::new(),
        }
    }

    pub fn render_at(
        &self,
        snippet: &mut Snippet,
        newline_with_offset: &str,
        resolve_var: impl FnMut(&str) -> Option<Cow<str>>,
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

    pub fn render<R: FnMut(&str) -> Option<Cow<str>>>(
        &self,
        doc: &Rope,
        selection: &Selection,
        change_range: impl FnMut(&selection::Range) -> (usize, usize),
        ctx: &mut SnippetRenderCtx<R>,
    ) -> (Transaction, Selection, Snippet) {
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

pub struct SnippetRenderCtx<R> {
    pub resolve_var: R,
    pub tab_width: usize,
    pub indent_width: usize,
    pub line_ending: &'static str,
}

struct SnippetRender<'a, R> {
    dst: &'a mut Snippet,
    src: &'a elaborate::Snippet,
    newline_with_offset: &'a str,
    text: Tendril,
    off: usize,
    resolve_var: R,
}

impl<R: FnMut(&str) -> Option<Cow<str>>> SnippetRender<'_, R> {
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

// impl Snippet {
//     pub fn new(snippet: &elaborate::Snippet) -> Snippet {
//         Snippet {
//             tabstops: snippet.tabstops().map(|tabstop| Tabstop { ranges:SmallVec::new() , parent:tabstop.parent , transform: if let Tr  }).collect(),
//         }
//     }

//     pub fn add_tabstop(
//         &mut self,
//         ranges: SmallVec<[Range; 1]>,
//         num_cursors: usize,
//         parent: Option<TabstopId>,
//     ) -> TabstopId {
//         let has_placeholder = ranges.iter().any(|range| !range.is_empty());
//         let id = self.tabstops.len();
//         self.tabstops.push(Tabstop {
//             ranges,
//             has_placeholder,
//             num_cursors,
//             parent: parent.map_or(usize::MAX, |id| id.0),
//         });
//         TabstopId(id)
//     }
// }
