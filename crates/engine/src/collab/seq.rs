//! A story as one flat sequence, the way co-editing sees it.
//!
//! Each character of a paragraph is one unit (an inline object's U+FFFC included), the paragraph
//! mark that ends it is one more, and a table is one unit of its own. Splitting a paragraph is
//! inserting a mark and joining two is deleting one, so typing in a paragraph someone else splits
//! or joins at the same moment still lands where it was typed.
//!
//! [`Seg`] is the owned form an edit is applied to (only the paragraphs it touches are taken apart
//! and put back together); [`El`] is the borrowed, unit-by-unit form diffs walk.

use std::sync::{Arc, LazyLock};

use wordcraft_doc::para::{OBJ, next_rev};
use wordcraft_doc::{Block, Blocks, CharProps, InlineObject, ParaProps, Paragraph, Run, SectionProps, Table};

/// What a paragraph mark carries: the paragraph's formatting, its mark's character formatting
/// and, for the last paragraph of a section, the section's settings.
#[derive(Clone, Debug, PartialEq, Default)]
pub(crate) struct ParaAttrs {
    pub props: ParaProps,
    pub mark: CharProps,
    pub section: Option<Box<SectionProps>>,
}

impl ParaAttrs {
    pub fn of(p: &Paragraph) -> ParaAttrs {
        ParaAttrs { props: p.props.clone(), mark: p.mark.clone(), section: p.section.clone() }
    }
}

/// A piece of a story: text sharing one formatting, an inline object, a paragraph mark or a table.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Seg {
    /// Never empty, and holds no U+FFFC of an object (those are [`Seg::Obj`]).
    Text {
        text: String,
        props: CharProps,
    },
    Obj {
        obj: InlineObject,
        props: CharProps,
    },
    End(ParaAttrs),
    Table(Table),
}

impl Seg {
    /// Its length in units.
    pub fn len(&self) -> usize {
        match self {
            Seg::Text { text, .. } => text.chars().count(),
            _ => 1,
        }
    }
}

/// A block's length in units: its characters and its mark, or 1 for a table.
pub(crate) fn block_len(b: &Block) -> usize {
    match b {
        Block::Para(p) => p.text.chars().count() + 1,
        Block::Table(_) => 1,
    }
}

static NO_PROPS: LazyLock<CharProps> = LazyLock::new(CharProps::default);

/// The formatting of each byte range of `p`, in order: its runs, then (for a paragraph whose runs
/// fall short of its text) no formatting for the rest.
fn run_spans(p: &Paragraph) -> Vec<(usize, usize, &CharProps)> {
    let mut out = Vec::with_capacity(p.runs.len() + 1);
    let mut at = 0usize;
    for Run { len, props } in &p.runs {
        let end = at.saturating_add(*len).min(p.text.len());
        if end > at {
            out.push((at, end, props));
        }
        at = end;
    }
    if at < p.text.len() {
        out.push((at, p.text.len(), &*NO_PROPS));
    }
    out
}

/// Appends paragraph `p` as segments.
pub(crate) fn push_para_segs(p: &Paragraph, out: &mut Vec<Seg>) {
    let mut objects = p.objects.iter();
    for (start, end, props) in run_spans(p) {
        let Some(slice) = p.text.get(start..end) else { continue };
        let mut text = String::new();
        for c in slice.chars() {
            if c == OBJ
                && let Some(obj) = objects.next()
            {
                if !text.is_empty() {
                    out.push(Seg::Text { text: std::mem::take(&mut text), props: props.clone() });
                }
                out.push(Seg::Obj { obj: obj.clone(), props: props.clone() });
            } else {
                text.push(c);
            }
        }
        if !text.is_empty() {
            out.push(Seg::Text { text, props: props.clone() });
        }
    }
    out.push(Seg::End(ParaAttrs::of(p)));
}

/// Appends `blocks` as segments.
pub(crate) fn push_segs(blocks: &[Arc<Block>], out: &mut Vec<Seg>) {
    for b in blocks {
        match &**b {
            Block::Para(p) => push_para_segs(p, out),
            Block::Table(t) => out.push(Seg::Table(t.clone())),
        }
    }
}

/// Builds blocks back from segments. A run of text that no paragraph mark ends (before a table,
/// or at the end) is closed with a plain mark, the same way on every copy, so whatever two edits
/// made at once leave behind is still a well-formed story. Also returns where those marks went,
/// as positions in the segments before them (the edit as applied includes them: see
/// [`super::story::apply_story`]).
pub(crate) fn blocks_from_segs(segs: Vec<Seg>) -> (Blocks, Vec<usize>) {
    let mut out: Blocks = Vec::new();
    let mut cur = Paragraph::new();
    let mut open = false;
    let mut at = 0usize;
    let mut added = Vec::new();
    fn push_text(p: &mut Paragraph, text: &str, props: CharProps) {
        if text.is_empty() {
            return;
        }
        p.text.push_str(text);
        match p.runs.last_mut() {
            Some(last) if last.props == props => last.len += text.len(),
            _ => p.runs.push(Run { len: text.len(), props }),
        }
    }
    fn close(cur: &mut Paragraph, attrs: ParaAttrs, out: &mut Blocks) {
        let mut p = std::mem::take(cur);
        p.props = attrs.props;
        p.mark = attrs.mark;
        p.section = attrs.section;
        p.rev = next_rev();
        out.push(Arc::new(Block::Para(p)));
    }
    for seg in segs {
        let len = seg.len();
        match seg {
            Seg::Text { text, props } => {
                push_text(&mut cur, &text, props);
                open = true;
            }
            Seg::Obj { obj, props } => {
                let mut buf = [0u8; 4];
                push_text(&mut cur, OBJ.encode_utf8(&mut buf), props);
                cur.objects.push(obj);
                open = true;
            }
            Seg::End(attrs) => {
                close(&mut cur, attrs, &mut out);
                open = false;
            }
            Seg::Table(t) => {
                if open {
                    close(&mut cur, ParaAttrs::default(), &mut out);
                    added.push(at);
                    open = false;
                }
                out.push(Arc::new(Block::Table(t)));
            }
        }
        at = at.saturating_add(len);
    }
    if open {
        close(&mut cur, ParaAttrs::default(), &mut out);
        added.push(at);
    }
    (out, added)
}

/// One unit of a story, borrowed.
#[derive(Clone, Copy, Debug)]
pub(crate) enum El<'a> {
    Ch(char, &'a CharProps),
    Obj(&'a InlineObject, &'a CharProps),
    End(&'a Paragraph),
    Table(&'a Table),
}

impl El<'_> {
    /// The same unit in both versions, formatting aside: the same character, an object, a mark
    /// or a table.
    pub fn same_unit(&self, o: &El<'_>) -> bool {
        match (self, o) {
            (El::Ch(a, _), El::Ch(b, _)) => a == b,
            (El::Obj(..), El::Obj(..)) | (El::End(_), El::End(_)) | (El::Table(_), El::Table(_)) => true,
            _ => false,
        }
    }
}

/// Appends the units of `blocks`.
pub(crate) fn push_els<'a>(blocks: &'a [Arc<Block>], out: &mut Vec<El<'a>>) {
    for b in blocks {
        match &**b {
            Block::Para(p) => {
                let mut objects = p.objects.iter();
                for (start, end, props) in run_spans(p) {
                    let Some(slice) = p.text.get(start..end) else { continue };
                    for c in slice.chars() {
                        match (c == OBJ).then(|| objects.next()).flatten() {
                            Some(obj) => out.push(El::Obj(obj, props)),
                            None => out.push(El::Ch(c, props)),
                        }
                    }
                }
                out.push(El::End(p));
            }
            Block::Table(t) => out.push(El::Table(t)),
        }
    }
}

/// Takes segments off the front of a list, `n` units at a time, splitting text where needed.
pub(crate) struct SegCursor {
    segs: std::collections::VecDeque<Seg>,
}

impl SegCursor {
    pub fn new(segs: Vec<Seg>) -> SegCursor {
        SegCursor { segs: segs.into() }
    }

    /// The next `n` units, or `None` if there are fewer left.
    pub fn take(&mut self, mut n: usize) -> Option<Vec<Seg>> {
        let mut out = Vec::new();
        while n > 0 {
            let seg = self.segs.pop_front()?;
            let len = seg.len();
            if len <= n {
                n -= len;
                out.push(seg);
                continue;
            }
            // Only text is longer than one unit.
            let Seg::Text { text, props } = seg else { return None };
            let cut = text.char_indices().nth(n).map_or(text.len(), |(i, _)| i);
            let (head, tail) = text.split_at(cut);
            out.push(Seg::Text { text: head.to_string(), props: props.clone() });
            self.segs.push_front(Seg::Text { text: tail.to_string(), props });
            n = 0;
        }
        Some(out)
    }

    pub fn rest(self) -> Vec<Seg> {
        self.segs.into()
    }
}
