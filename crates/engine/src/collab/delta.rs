//! Edits to one story as a list of steps over its flat sequence ([`super::seq`]): keep, insert,
//! delete, or change the formatting of what's kept. Payloads are JSON, so a field the model gains
//! later travels without changes here.
//!
//! Formatting changes are shallow patches: the keys of a unit's formatting that changed, `null`
//! for a key that went. Two people changing different keys of the same text (one bolds, one
//! colours) both keep theirs; the same key, the later one wins.

use serde::{Deserialize, Serialize};
use serde_json::Value as Json;

use super::seq::{ParaAttrs, Seg, SegCursor};
use super::{Error, Result};

pub type Map = serde_json::Map<String, Json>;

/// Inserted content.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "camelCase")]
pub enum Item {
    /// Characters sharing one formatting (`p`, the character formatting as JSON).
    #[serde(rename = "s")]
    Text {
        s: String,
        #[serde(default, skip_serializing_if = "Map::is_empty")]
        p: Map,
    },
    /// An inline object (`o`) with its formatting.
    #[serde(rename = "o")]
    Obj {
        o: Json,
        #[serde(default, skip_serializing_if = "Map::is_empty")]
        p: Map,
    },
    /// A paragraph mark: [`attrs_json`].
    #[serde(rename = "e")]
    End {
        #[serde(default, skip_serializing_if = "Map::is_empty")]
        a: Map,
    },
    #[serde(rename = "b")]
    Table { v: Json },
}

impl Item {
    pub fn len(&self) -> usize {
        match self {
            Item::Text { s, .. } => s.chars().count(),
            _ => 1,
        }
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// One step of a [`Delta`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "k")]
pub enum Comp {
    /// Keep `n` units.
    #[serde(rename = "r")]
    Retain { n: usize },
    #[serde(rename = "i")]
    Insert { items: Vec<Item> },
    #[serde(rename = "d")]
    Delete { n: usize },
    /// Patch the character formatting of `n` characters or objects.
    #[serde(rename = "f")]
    Format { n: usize, p: Map },
    /// Patch a paragraph mark's attributes ([`attrs_json`] keys).
    #[serde(rename = "p")]
    Para { p: Map },
    /// Replace an inline object (formatting aside).
    #[serde(rename = "o")]
    Obj { o: Json },
    /// Replace a table.
    #[serde(rename = "b")]
    Table { v: Json },
}

impl Comp {
    /// Units of the old sequence it covers (0 for an insert).
    pub fn input_len(&self) -> usize {
        match self {
            Comp::Retain { n } | Comp::Delete { n } | Comp::Format { n, .. } => *n,
            Comp::Insert { .. } => 0,
            Comp::Para { .. } | Comp::Obj { .. } | Comp::Table { .. } => 1,
        }
    }
    /// Units of the new sequence it makes (0 for a delete).
    pub fn output_len(&self) -> usize {
        match self {
            Comp::Retain { n } | Comp::Format { n, .. } => *n,
            Comp::Insert { items } => items.iter().map(Item::len).sum(),
            Comp::Delete { .. } => 0,
            Comp::Para { .. } | Comp::Obj { .. } | Comp::Table { .. } => 1,
        }
    }
    fn is_insert(&self) -> bool {
        matches!(self, Comp::Insert { .. })
    }
}

/// The steps of an edit to one story. A trailing keep is implied.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Delta(pub Vec<Comp>);

impl Delta {
    /// Nothing changes.
    pub fn is_noop(&self) -> bool {
        self.0.iter().all(|c| matches!(c, Comp::Retain { .. }))
    }

    /// Where the first change is, and the end of what it reads of the old sequence (an insert
    /// reads nothing, so it ends where it starts).
    pub fn span(&self) -> Option<(usize, usize)> {
        let mut at = 0usize;
        let mut start = None;
        let mut end = 0usize;
        for c in &self.0 {
            if let Comp::Retain { n } = c {
                at = at.saturating_add(*n);
                continue;
            }
            start.get_or_insert(at);
            at = at.saturating_add(c.input_len());
            end = at;
        }
        start.map(|s| (s, end.max(s)))
    }

    /// The same edit made `offset` units further on.
    pub fn shifted(mut self, offset: usize) -> Delta {
        if offset > 0 {
            self.0.insert(0, Comp::Retain { n: offset });
            self.0 = Builder::from(self.0).done().0;
        }
        self
    }

    /// The edit with its leading keep taken off, and that keep's length.
    pub fn split_lead(&self) -> (usize, &[Comp]) {
        match self.0.first() {
            Some(Comp::Retain { n }) => (*n, self.0.get(1..).unwrap_or(&[])),
            _ => (0, &self.0),
        }
    }

    /// Where `at`, a unit position before the edit, is after it. `stay` keeps it in front of an
    /// insert made right there (someone else's text typed at this person's caret goes after it).
    pub fn map_pos(&self, at: usize, stay: bool) -> usize {
        let mut old = 0usize;
        let mut new = 0usize;
        for c in &self.0 {
            match c {
                Comp::Insert { .. } => {
                    if old < at || (old == at && !stay) {
                        new = new.saturating_add(c.output_len());
                    }
                }
                Comp::Delete { n } => {
                    if old.saturating_add(*n) > at {
                        // `at` was deleted: it lands where the deletion was.
                        return new;
                    }
                    old = old.saturating_add(*n);
                }
                _ => {
                    let n = c.input_len();
                    if old.saturating_add(n) > at {
                        return new.saturating_add(at.saturating_sub(old));
                    }
                    old = old.saturating_add(n);
                    new = new.saturating_add(n);
                }
            }
        }
        new.saturating_add(at.saturating_sub(old))
    }
}

/// Appends steps, merging neighbours of the same kind and dropping empty ones.
#[derive(Default)]
pub(crate) struct Builder(Vec<Comp>);

impl Builder {
    pub fn from(comps: Vec<Comp>) -> Builder {
        let mut b = Builder::default();
        for c in comps {
            b.push(c);
        }
        b
    }

    pub fn retain(&mut self, n: usize) {
        self.push(Comp::Retain { n });
    }
    pub fn delete(&mut self, n: usize) {
        self.push(Comp::Delete { n });
    }

    pub fn push(&mut self, c: Comp) {
        match &c {
            Comp::Retain { n } | Comp::Delete { n } | Comp::Format { n, .. } if *n == 0 => return,
            Comp::Insert { items } if items.is_empty() => return,
            Comp::Format { p, n } if p.is_empty() => return self.retain(*n),
            Comp::Para { p } if p.is_empty() => return self.retain(1),
            _ => {}
        }
        // An insert goes before a delete at the same place (the same result, one shape).
        if c.is_insert()
            && let Some(Comp::Delete { n }) = self.0.last()
        {
            let n = *n;
            self.0.pop();
            self.push(c);
            self.delete(n);
            return;
        }
        match (self.0.last_mut(), c) {
            (Some(Comp::Retain { n: a }), Comp::Retain { n: b }) => *a += b,
            (Some(Comp::Delete { n: a }), Comp::Delete { n: b }) => *a += b,
            (Some(Comp::Format { n: a, p: pa }), Comp::Format { n: b, p: pb }) if *pa == pb => *a += b,
            (Some(Comp::Insert { items: a }), Comp::Insert { items: b }) => {
                for it in b {
                    push_item(a, it);
                }
            }
            (_, c) => self.0.push(c),
        }
    }

    /// The steps, without a trailing keep.
    pub fn done(mut self) -> Delta {
        while matches!(self.0.last(), Some(Comp::Retain { .. })) {
            self.0.pop();
        }
        Delta(self.0)
    }
}

/// Appends an item, joining text to text with the same formatting.
pub(crate) fn push_item(items: &mut Vec<Item>, it: Item) {
    if let Item::Text { s, p } = &it {
        if s.is_empty() {
            return;
        }
        if let Some(Item::Text { s: s0, p: p0 }) = items.last_mut()
            && p0 == p
        {
            s0.push_str(s);
            return;
        }
    }
    items.push(it);
}

/// Splits items after `n` units.
fn split_items(items: Vec<Item>, n: usize) -> (Vec<Item>, Vec<Item>) {
    let mut head = Vec::new();
    let mut tail = Vec::new();
    let mut left = n;
    for it in items {
        let len = it.len();
        if left == 0 {
            tail.push(it);
        } else if len <= left {
            left -= len;
            head.push(it);
        } else if let Item::Text { s, p } = it {
            let cut = s.char_indices().nth(left).map_or(s.len(), |(i, _)| i);
            let (a, b) = s.split_at(cut);
            head.push(Item::Text { s: a.to_string(), p: p.clone() });
            tail.push(Item::Text { s: b.to_string(), p });
            left = 0;
        } else {
            tail.push(it);
        }
    }
    (head, tail)
}

/// Walks a list of steps, handing out pieces of a given length.
struct Iter {
    comps: std::collections::VecDeque<Comp>,
}

impl Iter {
    fn new(comps: &[Comp]) -> Iter {
        Iter { comps: comps.iter().cloned().collect() }
    }
    fn has(&self) -> bool {
        !self.comps.is_empty()
    }
    fn peek_insert(&self) -> bool {
        self.comps.front().is_some_and(Comp::is_insert)
    }
    /// Units of the next step: what it reads, or for an insert what it makes; `usize::MAX` once
    /// the steps are done (an implied keep).
    fn peek_len(&self) -> usize {
        match self.comps.front() {
            Some(c) if c.is_insert() => c.output_len(),
            Some(c) => c.input_len(),
            None => usize::MAX,
        }
    }
    /// The next step, cut to at most `n` units (a keep once the steps are done).
    fn next(&mut self, n: usize) -> Comp {
        let Some(c) = self.comps.pop_front() else { return Comp::Retain { n } };
        let len = if c.is_insert() { c.output_len() } else { c.input_len() };
        if n >= len {
            return c;
        }
        let (head, tail) = match c {
            Comp::Retain { n: m } => (Comp::Retain { n }, Comp::Retain { n: m - n }),
            Comp::Delete { n: m } => (Comp::Delete { n }, Comp::Delete { n: m - n }),
            Comp::Format { n: m, p } => (Comp::Format { n, p: p.clone() }, Comp::Format { n: m - n, p }),
            Comp::Insert { items } => {
                let (a, b) = split_items(items, n);
                (Comp::Insert { items: a }, Comp::Insert { items: b })
            }
            // One unit long: never cut.
            other => return other,
        };
        self.comps.push_front(tail);
        head
    }
    fn next_all(&mut self) -> Comp {
        self.next(usize::MAX)
    }
}

/// `p` without the keys `q` sets.
fn minus_keys(p: &Map, q: &Map) -> Map {
    p.iter().filter(|(k, _)| !q.contains_key(*k)).map(|(k, v)| (k.clone(), v.clone())).collect()
}

/// `q` applied over `p` (later keys win).
fn merge(p: &Map, q: &Map) -> Map {
    let mut out = p.clone();
    for (k, v) in q {
        out.insert(k.clone(), v.clone());
    }
    out
}

/// Applies a shallow patch: a key set to `null` goes.
pub(crate) fn patch_map(m: &mut Map, p: &Map) {
    for (k, v) in p {
        if v.is_null() {
            m.remove(k);
        } else {
            m.insert(k.clone(), v.clone());
        }
    }
}

/// The patch that turns `old` into `new`.
pub(crate) fn diff_map(old: &Map, new: &Map) -> Map {
    let mut p = Map::new();
    for (k, v) in new {
        if old.get(k) != Some(v) {
            p.insert(k.clone(), v.clone());
        }
    }
    for k in old.keys() {
        if !new.contains_key(k) {
            p.insert(k.clone(), Json::Null);
        }
    }
    p
}

/// The patch that takes `patch` back on `old`: each key's old value, `null` where it had none.
pub(crate) fn undo_patch(old: &Map, patch: &Map) -> Map {
    patch.keys().map(|k| (k.clone(), old.get(k).cloned().unwrap_or(Json::Null))).collect()
}

/// `a` rebased to apply after `b`, both made against the same story. `a_first`: `a` comes first
/// in the log, so where both change the same thing `b` wins, and where both insert at one place
/// `a`'s text goes first.
pub fn transform(a: &Delta, b: &Delta, a_first: bool) -> Delta {
    let mut ai = Iter::new(&a.0);
    let mut bi = Iter::new(&b.0);
    let mut out = Builder::default();
    while ai.has() {
        if ai.peek_insert() && (a_first || !bi.peek_insert()) {
            out.push(ai.next_all());
            continue;
        }
        if bi.peek_insert() {
            out.retain(bi.next_all().output_len());
            continue;
        }
        let n = ai.peek_len().min(bi.peek_len());
        let ac = ai.next(n);
        let bc = bi.next(n);
        match (ac, bc) {
            (_, Comp::Delete { .. }) => {}
            (Comp::Delete { n }, _) => out.delete(n),
            (Comp::Retain { n }, _) => out.retain(n),
            (Comp::Format { n, p }, Comp::Format { p: q, .. }) => out.push(Comp::Format { n, p: if a_first { minus_keys(&p, &q) } else { p } }),
            (Comp::Para { p }, Comp::Para { p: q }) => out.push(Comp::Para { p: if a_first { minus_keys(&p, &q) } else { p } }),
            (Comp::Obj { .. }, Comp::Obj { .. }) | (Comp::Table { .. }, Comp::Table { .. }) if a_first => out.retain(1),
            (c, _) => out.push(c),
        }
    }
    out.done()
}

/// `a` then `b` as one edit, or `None` when one step can't say both (an object replaced and
/// reformatted in one go): send them one after the other then.
pub fn compose(a: &Delta, b: &Delta) -> Option<Delta> {
    let mut ai = Iter::new(&a.0);
    let mut bi = Iter::new(&b.0);
    let mut out = Builder::default();
    while ai.has() || bi.has() {
        if bi.peek_insert() {
            out.push(bi.next_all());
            continue;
        }
        if matches!(ai.comps.front(), Some(Comp::Delete { .. })) {
            out.push(ai.next_all());
            continue;
        }
        if !bi.has() {
            out.push(ai.next_all());
            continue;
        }
        let n = ai.peek_len().min(bi.peek_len());
        let ac = ai.next(n);
        let bc = bi.next(n);
        match (ac, bc) {
            (ac, Comp::Retain { .. }) => out.push(ac),
            (Comp::Insert { .. }, Comp::Delete { .. }) => {}
            (_, Comp::Delete { n }) => out.delete(n),
            (Comp::Retain { .. }, bc) => out.push(bc),
            (Comp::Insert { items }, bc) => out.push(Comp::Insert { items: change_items(items, &bc)? }),
            (Comp::Format { n, p }, Comp::Format { p: q, .. }) => out.push(Comp::Format { n, p: merge(&p, &q) }),
            (Comp::Para { p }, Comp::Para { p: q }) => out.push(Comp::Para { p: merge(&p, &q) }),
            (Comp::Obj { .. }, Comp::Obj { o }) => out.push(Comp::Obj { o }),
            (Comp::Table { .. }, Comp::Table { v }) => out.push(Comp::Table { v }),
            _ => return None,
        }
    }
    Some(out.done())
}

/// Inserted items with a later change to them made part of them.
fn change_items(items: Vec<Item>, c: &Comp) -> Option<Vec<Item>> {
    let mut out = Vec::with_capacity(items.len());
    for it in items {
        let it = match (it, c) {
            (Item::Text { s, mut p }, Comp::Format { p: q, .. }) => {
                patch_map(&mut p, q);
                Item::Text { s, p }
            }
            (Item::Obj { o, mut p }, Comp::Format { p: q, .. }) => {
                patch_map(&mut p, q);
                Item::Obj { o, p }
            }
            (Item::Obj { p, .. }, Comp::Obj { o }) => Item::Obj { o: o.clone(), p },
            (Item::End { mut a }, Comp::Para { p }) => {
                patch_map(&mut a, p);
                Item::End { a }
            }
            (Item::Table { .. }, Comp::Table { v }) => Item::Table { v: v.clone() },
            _ => return None,
        };
        push_item(&mut out, it);
    }
    Some(out)
}

// --- JSON forms of the model's pieces -------------------------------------------------------

fn to_map<T: Serialize>(v: &T) -> Result<Map> {
    match serde_json::to_value(v).map_err(|e| Error(format!("co-editing: {e}")))? {
        Json::Object(m) => Ok(m),
        _ => Ok(Map::new()),
    }
}

fn from_map<T: serde::de::DeserializeOwned>(m: Map) -> Result<T> {
    serde_json::from_value(Json::Object(m)).map_err(|e| Error(format!("co-editing: bad formatting in an edit: {e}")))
}

pub(crate) fn props_json(p: &wordcraft_doc::CharProps) -> Result<Map> {
    to_map(p)
}

/// A paragraph mark's attributes as one flat map: `p.<key>` for the paragraph's formatting,
/// `m.<key>` for the mark's character formatting and `s` for the section settings, so a patch can
/// change one paragraph setting without touching the others.
pub(crate) fn attrs_json(a: &ParaAttrs) -> Result<Map> {
    let mut out = Map::new();
    for (k, v) in to_map(&a.props)? {
        out.insert(format!("p.{k}"), v);
    }
    for (k, v) in to_map(&a.mark)? {
        out.insert(format!("m.{k}"), v);
    }
    if let Some(s) = &a.section {
        out.insert("s".into(), serde_json::to_value(s).map_err(|e| Error(format!("co-editing: {e}")))?);
    }
    Ok(out)
}

pub(crate) fn attrs_from_json(m: Map) -> Result<ParaAttrs> {
    let mut props = Map::new();
    let mut mark = Map::new();
    let mut section = None;
    for (k, v) in m {
        if let Some(k) = k.strip_prefix("p.") {
            props.insert(k.to_string(), v);
        } else if let Some(k) = k.strip_prefix("m.") {
            mark.insert(k.to_string(), v);
        } else if k == "s" && !v.is_null() {
            section = Some(Box::new(serde_json::from_value(v).map_err(|e| Error(format!("co-editing: bad section in an edit: {e}")))?));
        }
    }
    Ok(ParaAttrs { props: from_map(props)?, mark: from_map(mark)?, section })
}

pub(crate) fn seg_item(seg: &Seg) -> Result<Item> {
    Ok(match seg {
        Seg::Text { text, props } => Item::Text { s: text.clone(), p: props_json(props)? },
        Seg::Obj { obj, props } => Item::Obj { o: to_json(obj)?, p: props_json(props)? },
        Seg::End(a) => Item::End { a: attrs_json(a)? },
        Seg::Table(t) => Item::Table { v: to_json(t)? },
    })
}

pub(crate) fn to_json<T: Serialize>(v: &T) -> Result<Json> {
    serde_json::to_value(v).map_err(|e| Error(format!("co-editing: {e}")))
}

fn from_json<T: serde::de::DeserializeOwned>(v: Json, what: &str) -> Result<T> {
    serde_json::from_value(v).map_err(|e| Error(format!("co-editing: bad {what} in an edit: {e}")))
}

fn item_seg(it: &Item) -> Result<Seg> {
    Ok(match it {
        Item::Text { s, p } => {
            if s.is_empty() || s.contains(wordcraft_doc::para::OBJ) {
                return Err(Error("co-editing: inserted text is empty or holds an object mark".into()));
            }
            Seg::Text { text: s.clone(), props: from_map(p.clone())? }
        }
        Item::Obj { o, p } => Seg::Obj { obj: from_json(o.clone(), "object")?, props: from_map(p.clone())? },
        Item::End { a } => Seg::End(attrs_from_json(a.clone())?),
        Item::Table { v } => Seg::Table(from_json(v.clone(), "table")?),
    })
}

/// Applies steps to segments (the old sequence; anything after the steps is kept).
pub(crate) fn apply_segs(segs: Vec<Seg>, comps: &[Comp]) -> Result<Vec<Seg>> {
    let short = || Error("co-editing: an edit runs past the end of the text".into());
    let mut src = SegCursor::new(segs);
    let mut out = Vec::new();
    for c in comps {
        match c {
            Comp::Retain { n } => out.extend(src.take(*n).ok_or_else(short)?),
            Comp::Insert { items } => {
                for it in items {
                    out.push(item_seg(it)?);
                }
            }
            Comp::Delete { n } => {
                src.take(*n).ok_or_else(short)?;
            }
            Comp::Format { n, p } => {
                for seg in src.take(*n).ok_or_else(short)? {
                    out.push(match seg {
                        Seg::Text { text, props } => Seg::Text { text, props: patched(&props, p)? },
                        Seg::Obj { obj, props } => Seg::Obj { obj, props: patched(&props, p)? },
                        _ => return Err(Error("co-editing: character formatting for a paragraph mark or table".into())),
                    });
                }
            }
            Comp::Para { p } => match src.take(1).ok_or_else(short)?.pop() {
                Some(Seg::End(a)) => {
                    let mut m = attrs_json(&a)?;
                    patch_map(&mut m, p);
                    out.push(Seg::End(attrs_from_json(m)?));
                }
                _ => return Err(Error("co-editing: paragraph settings for something not a paragraph mark".into())),
            },
            Comp::Obj { o } => match src.take(1).ok_or_else(short)?.pop() {
                Some(Seg::Obj { props, .. }) => out.push(Seg::Obj { obj: from_json(o.clone(), "object")?, props }),
                _ => return Err(Error("co-editing: an object change for something not an object".into())),
            },
            Comp::Table { v } => match src.take(1).ok_or_else(short)?.pop() {
                Some(Seg::Table(_)) => out.push(Seg::Table(from_json(v.clone(), "table")?)),
                _ => return Err(Error("co-editing: a table change for something not a table".into())),
            },
        }
    }
    out.extend(src.rest());
    Ok(out)
}

fn patched(props: &wordcraft_doc::CharProps, p: &Map) -> Result<wordcraft_doc::CharProps> {
    let mut m = props_json(props)?;
    patch_map(&mut m, p);
    from_map(m)
}

/// The steps that take `comps` back, given the segments they were applied to.
pub(crate) fn invert_segs(segs: Vec<Seg>, comps: &[Comp]) -> Result<Vec<Comp>> {
    let short = || Error("co-editing: an edit runs past the end of the text".into());
    let mut src = SegCursor::new(segs);
    let mut out = Builder::default();
    for c in comps {
        match c {
            Comp::Retain { n } => {
                src.take(*n).ok_or_else(short)?;
                out.retain(*n);
            }
            Comp::Insert { .. } => out.delete(c.output_len()),
            Comp::Delete { n } => {
                let mut items = Vec::new();
                for seg in src.take(*n).ok_or_else(short)? {
                    push_item(&mut items, seg_item(&seg)?);
                }
                out.push(Comp::Insert { items });
            }
            Comp::Format { n, p } => {
                for seg in src.take(*n).ok_or_else(short)? {
                    let props = match &seg {
                        Seg::Text { props, .. } | Seg::Obj { props, .. } => props,
                        _ => return Err(Error("co-editing: character formatting for a paragraph mark or table".into())),
                    };
                    out.push(Comp::Format { n: seg.len(), p: undo_patch(&props_json(props)?, p) });
                }
            }
            Comp::Para { p } => match src.take(1).ok_or_else(short)?.pop() {
                Some(Seg::End(a)) => out.push(Comp::Para { p: undo_patch(&attrs_json(&a)?, p) }),
                _ => return Err(Error("co-editing: paragraph settings for something not a paragraph mark".into())),
            },
            Comp::Obj { .. } => match src.take(1).ok_or_else(short)?.pop() {
                Some(Seg::Obj { obj, .. }) => out.push(Comp::Obj { o: to_json(&obj)? }),
                _ => return Err(Error("co-editing: an object change for something not an object".into())),
            },
            Comp::Table { .. } => match src.take(1).ok_or_else(short)?.pop() {
                Some(Seg::Table(t)) => out.push(Comp::Table { v: to_json(&t)? }),
                _ => return Err(Error("co-editing: a table change for something not a table".into())),
            },
        }
    }
    Ok(out.done().0)
}
