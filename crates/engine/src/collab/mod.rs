//! Real-time co-editing (`docs/co-editing.md`).
//!
//! Copies of one document stay the same by exchanging an [`Op`] per edit, diffed from the
//! document before and after it, so every command is shared without code of its own:
//! - each story (the body, every header, footer, note, comment and text box) as a [`Delta`] over
//!   its flat sequence of characters, paragraph marks and tables ([`seq`]);
//! - parts added and removed;
//! - everything else (settings, styles, lists, comments, pictures…) as changed keys ([`fields`]).
//!
//! No I/O here: the host moves ops between people ([`Replica`] is one person's copy).

mod delta;
mod fields;
mod replica;
mod seq;
mod shared;
mod story;

pub use delta::{Comp, Delta, Item, compose, transform};
pub use fields::Fields;
pub use replica::{Incoming, Outgoing, Replica, Shown};
pub use shared::Other;
pub(crate) use shared::{Shared, run_shared};

use std::collections::BTreeMap;

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::Value as Json;
use wordcraft_doc::{Document, Part, PartKind, Path, Pos, Revision, StoryRef, TextBody};

use delta::Map;

/// A co-editing error: an edit that can't be shared or placed.
#[derive(Debug, Clone, PartialEq)]
pub struct Error(pub String);

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<Error> for crate::CmdError {
    fn from(e: Error) -> Self {
        crate::CmdError::Failed(e.0)
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// The largest edit shared with the others (its op as JSON). A bigger one (pasting a book full of
/// pictures) is taken back with a message to make it in smaller steps: the server and every other
/// copy would have to swallow it whole.
pub const MAX_SHARED_OP_BYTES: usize = 8 << 20;

/// The edit of one story.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StoryEdit {
    pub story: StoryRef,
    pub delta: Delta,
}

/// A part (header, footer, note, comment text, text box) an edit made, with its content.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NewPart {
    pub id: u32,
    pub kind: PartKind,
    #[serde(default)]
    pub body: TextBody,
    pub items: Vec<Item>,
}

/// One edit as the others receive it.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Op {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub added: Vec<NewPart>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stories: Vec<StoryEdit>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub fields: Fields,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub removed: Vec<u32>,
    /// The tracked-change revisions (author, date) the payloads name, by their index here: each
    /// copy numbers its document's revisions its own way.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub revs: Vec<Json>,
}

impl Op {
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty() && self.fields.is_empty() && self.stories.iter().all(|s| s.delta.is_noop())
    }

    fn story(&self, s: StoryRef) -> Option<&Delta> {
        self.stories.iter().find(|e| e.story == s).map(|e| &e.delta)
    }
}

/// The edit that turned `before` into `after`.
pub fn capture(before: &Document, after: &Document) -> Result<Op> {
    let revs_ctx = story::Revs::new(&before.revisions, &after.revisions);
    let mut op = Op::default();
    if !same_blocks(&before.body, &after.body) {
        push_story(&mut op, StoryRef::Body, story::diff_story(&before.body, &after.body, &revs_ctx)?);
    }
    for (id, pb) in &after.parts {
        match before.parts.get(id) {
            Some(pa) => {
                if !same_blocks(&pa.blocks, &pb.blocks) {
                    push_story(&mut op, StoryRef::Part(*id), story::diff_story(&pa.blocks, &pb.blocks, &revs_ctx)?);
                }
            }
            None => op.added.push(NewPart { id: *id, kind: pb.kind, body: pb.body, items: story::story_items(&pb.blocks)? }),
        }
    }
    op.removed = before.parts.keys().filter(|id| !after.parts.contains_key(id)).copied().collect();
    op.fields = fields::diff_fields(before, after)?;
    // Revisions by value: index `i` of `after.revisions` becomes the op's own number.
    let mut revs: Vec<Json> = Vec::new();
    for_each_payload(&mut op, &mut |m| {
        remap_revs(m, &mut |i| {
            let r = after.revisions.get(i as usize).and_then(|r| serde_json::to_value(r).ok()).unwrap_or(Json::Null);
            let at = revs.iter().position(|x| *x == r).unwrap_or_else(|| {
                revs.push(r);
                revs.len() - 1
            });
            u32::try_from(at).unwrap_or(u32::MAX)
        })
    });
    op.revs = revs;
    Ok(op)
}

fn push_story(op: &mut Op, story: StoryRef, delta: Delta) {
    if !delta.is_noop() {
        op.stories.push(StoryEdit { story, delta });
    }
}

fn same_blocks(a: &wordcraft_doc::Blocks, b: &wordcraft_doc::Blocks) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| std::sync::Arc::ptr_eq(x, y))
}

/// Applies `op` to `doc`. On an error `doc` may be half changed: apply to a copy.
pub fn apply(doc: &mut Document, op: &Op) -> Result<()> {
    let mut op = op.clone();
    // The op's revisions get this document's numbers (the same revision keeps its number). A
    // number the op doesn't define names no revision here either.
    if has_payloads(&op) {
        let mut map = Vec::with_capacity(op.revs.len());
        for r in &op.revs {
            let rev: Revision = serde_json::from_value(r.clone()).unwrap_or_default();
            let at = match doc.revisions.iter().position(|x| *x == rev) {
                Some(i) => i,
                None => {
                    doc.revisions.push(rev);
                    doc.revisions.len() - 1
                }
            };
            map.push(u32::try_from(at).unwrap_or(u32::MAX));
        }
        for_each_payload(&mut op, &mut |m| remap_revs(m, &mut |i| map.get(i as usize).copied().unwrap_or(u32::MAX)));
    }
    for p in &op.added {
        if doc.parts.contains_key(&p.id) {
            return Err(Error(format!("co-editing: part {} already exists", p.id)));
        }
        let blocks = story::story_from_items(&p.items)?;
        doc.parts.insert(p.id, Part { kind: p.kind, blocks, body: p.body });
    }
    for e in &op.stories {
        let blocks = doc.story_mut(e.story).map_err(|_| Error(format!("co-editing: no story {:?}", e.story)))?;
        story::apply_story(blocks, &e.delta)?;
    }
    fields::apply_fields(doc, &op.fields)?;
    for id in &op.removed {
        doc.parts.remove(id);
    }
    doc.ensure_nonempty();
    Ok(())
}

fn has_payloads(op: &Op) -> bool {
    !op.added.is_empty() || !op.stories.is_empty() || !op.fields.is_empty()
}

/// The edit that takes `op` back, given the document it was applied to.
pub fn invert(before: &Document, op: &Op) -> Result<Op> {
    let mut inv = Op::default();
    for e in &op.stories {
        let blocks = before.story(e.story).ok_or_else(|| Error(format!("co-editing: no story {:?}", e.story)))?;
        push_story(&mut inv, e.story, story::invert_story(blocks, &e.delta)?);
    }
    inv.fields = fields::invert_fields(before, &op.fields)?;
    inv.removed = op.added.iter().map(|p| p.id).collect();
    for id in &op.removed {
        if let Some(p) = before.parts.get(id) {
            inv.added.push(NewPart { id: *id, kind: p.kind, body: p.body, items: story::story_items(&p.blocks)? });
        }
    }
    // The payloads name `before`'s revisions by its numbers: give the op its own.
    let mut revs: Vec<Json> = Vec::new();
    for_each_payload(&mut inv, &mut |m| {
        remap_revs(m, &mut |i| {
            let r = before.revisions.get(i as usize).and_then(|r| serde_json::to_value(r).ok()).unwrap_or(Json::Null);
            let at = revs.iter().position(|x| *x == r).unwrap_or_else(|| {
                revs.push(r);
                revs.len() - 1
            });
            u32::try_from(at).unwrap_or(u32::MAX)
        })
    });
    inv.revs = revs;
    Ok(inv)
}

/// `a` rebased to apply after `b` (both made against the same document). `a_first`: `a` is the
/// earlier in the log, so where both change the same thing `b`'s change stays.
pub fn rebase(a: &Op, b: &Op, a_first: bool) -> Op {
    let mut out = Op { added: a.added.clone(), removed: a.removed.clone(), revs: a.revs.clone(), ..Op::default() };
    for e in &a.stories {
        if let StoryRef::Part(id) = e.story
            && b.removed.contains(&id)
        {
            continue;
        }
        let delta = match b.story(e.story) {
            Some(theirs) => delta::transform(&e.delta, theirs, a_first),
            None => e.delta.clone(),
        };
        push_story(&mut out, e.story, delta);
    }
    // A part both removed goes once.
    out.removed.retain(|id| !b.removed.contains(id));
    out.fields = fields::transform_fields(&a.fields, &b.fields, a_first);
    out
}

/// `a` then `b` as one op, or `None` when they can't be put together (send them one by one).
pub fn compose_ops(a: &Op, b: &Op) -> Option<Op> {
    // A part `b` makes with an id `a` just freed: the order matters, keep them apart.
    if b.added.iter().any(|p| a.removed.contains(&p.id)) {
        return None;
    }
    let mut b = b.clone();
    let shift = u32::try_from(a.revs.len()).ok()?;
    if shift > 0 {
        for_each_payload(&mut b, &mut |m| remap_revs(m, &mut |i| i.saturating_add(shift)));
    }
    let mut added = a.added.clone();
    let mut stories = a.stories.clone();
    for e in &b.stories {
        // An edit of a part `a` made goes into what `a` makes.
        if let StoryRef::Part(id) = e.story
            && let Some(p) = added.iter_mut().find(|p| p.id == id)
        {
            let made = delta::compose(&Delta(vec![Comp::Insert { items: p.items.clone() }]), &e.delta)?;
            p.items = match made.0.as_slice() {
                [Comp::Insert { items }] => items.clone(),
                [] => Vec::new(),
                _ => return None,
            };
            continue;
        }
        match stories.iter_mut().find(|s| s.story == e.story) {
            Some(s) => s.delta = delta::compose(&s.delta, &e.delta)?,
            None => stories.push(e.clone()),
        }
    }
    added.extend(b.added.iter().cloned());
    let mut removed = a.removed.clone();
    for id in &b.removed {
        stories.retain(|s| s.story != StoryRef::Part(*id));
        // A part `a` made and `b` removed never existed for the others.
        match added.iter().position(|p| p.id == *id) {
            Some(i) => {
                added.remove(i);
            }
            None => removed.push(*id),
        }
    }
    stories.retain(|s| !s.delta.is_noop());
    Some(Op { added, stories, fields: fields::compose_fields(&a.fields, &b.fields), removed, revs: a.revs.iter().chain(&b.revs).cloned().collect() })
}

/// Runs `f` on every JSON payload of `op` that can name revisions.
fn for_each_payload(op: &mut Op, f: &mut dyn FnMut(&mut Map)) {
    let items = |items: &mut Vec<Item>, f: &mut dyn FnMut(&mut Map)| {
        for it in items {
            match it {
                Item::Text { p, .. } => f(p),
                Item::Obj { o, p } => {
                    f(p);
                    json_maps(o, f);
                }
                Item::End { a } => f(a),
                Item::Table { v } => json_maps(v, f),
            }
        }
    };
    for p in &mut op.added {
        items(&mut p.items, f);
    }
    for e in &mut op.stories {
        for c in &mut e.delta.0 {
            match c {
                Comp::Insert { items: its } => items(its, f),
                Comp::Format { p, .. } | Comp::Para { p } => f(p),
                Comp::Obj { o } => json_maps(o, f),
                Comp::Table { v } => json_maps(v, f),
                Comp::Retain { .. } | Comp::Delete { .. } => {}
            }
        }
    }
    for m in fields::payloads_mut(&mut op.fields) {
        f(m);
    }
}

/// Runs `f` on `v` if it's an object (the remapper walks inside).
fn json_maps(v: &mut Json, f: &mut dyn FnMut(&mut Map)) {
    if let Json::Object(m) = v {
        f(m);
    } else if let Json::Array(a) = v {
        for x in a {
            json_maps(x, f);
        }
    }
}

/// Renumbers the revision references in `m` and everything inside it: a run's `ins` and `del`,
/// and the `rev` of a tracked formatting or numbering change. Keys may carry a prefix
/// (`m.ins`: a paragraph mark's).
pub(super) fn remap_revs(m: &mut Map, f: &mut dyn FnMut(u32) -> u32) {
    for (k, v) in m.iter_mut() {
        let base = k.rsplit('.').next().unwrap_or(k);
        match base {
            "ins" | "del" => {
                if let Some(n) = v.as_u64().and_then(|n| u32::try_from(n).ok()) {
                    *v = Json::from(f(n));
                }
            }
            "fmtChange" | "numChange" => {
                if let Some(r) = v.get_mut("rev")
                    && let Some(n) = r.as_u64().and_then(|n| u32::try_from(n).ok())
                {
                    *r = Json::from(f(n));
                }
                if let Some(Json::Object(old)) = v.get_mut("old") {
                    remap_revs(old, f);
                }
            }
            _ => remap_json(v, f),
        }
    }
}

fn remap_json(v: &mut Json, f: &mut dyn FnMut(u32) -> u32) {
    match v {
        Json::Object(m) => remap_revs(m, f),
        Json::Array(a) => {
            for x in a {
                remap_json(x, f);
            }
        }
        _ => {}
    }
}

// --- Snapshots --------------------------------------------------------------------------------

/// The exact shared document, for someone joining at a checkpoint (re-reading the saved file
/// wouldn't give back the same part, comment and list numbers). Pictures as base64.
pub fn snapshot(doc: &Document) -> Result<String> {
    let b64 = base64::engine::general_purpose::STANDARD;
    let mut bare = doc.clone();
    let media: BTreeMap<String, String> = std::mem::take(&mut bare.media).iter().map(|(k, v)| (k.clone(), b64.encode(v.as_slice()))).collect();
    let passthrough: BTreeMap<String, String> =
        std::mem::take(&mut bare.passthrough).iter().map(|(k, v)| (k.clone(), b64.encode(v.as_slice()))).collect();
    let v = serde_json::json!({"v": 1, "doc": bare, "media": media, "passthrough": passthrough});
    serde_json::to_string(&v).map_err(|e| Error(format!("co-editing: {e}")))
}

/// The document a [`snapshot`] holds.
pub fn from_snapshot(s: &str) -> Result<Document> {
    #[derive(Deserialize)]
    struct Snap {
        doc: Document,
        #[serde(default)]
        media: BTreeMap<String, String>,
        #[serde(default)]
        passthrough: BTreeMap<String, String>,
    }
    let snap: Snap = serde_json::from_str(s).map_err(|e| Error(format!("co-editing: the shared copy can't be read: {e}")))?;
    let b64 = base64::engine::general_purpose::STANDARD;
    let bytes = |m: BTreeMap<String, String>| -> Result<BTreeMap<String, std::sync::Arc<Vec<u8>>>> {
        m.into_iter()
            .map(|(k, v)| {
                b64.decode(v).map(|b| (k, std::sync::Arc::new(b))).map_err(|_| Error("co-editing: bad picture data in the shared copy".into()))
            })
            .collect()
    };
    let mut doc = snap.doc;
    doc.media = bytes(snap.media)?;
    doc.passthrough = bytes(snap.passthrough)?;
    doc.ensure_nonempty();
    Ok(doc)
}

// --- Positions --------------------------------------------------------------------------------

/// A position as co-editing places it: the unit position in its story, and for a position inside
/// a table, where in the table (the path below the table's block, and the byte offset).
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct FlatPos {
    pub story: StoryRef,
    pub at: usize,
    pub inner: Option<(Vec<u32>, usize)>,
}

pub(crate) fn flat_pos(doc: &Document, p: &Pos) -> Option<FlatPos> {
    let blocks = doc.story(p.story)?;
    let first = *p.path.0.first()? as usize;
    let before = story::story_len(blocks.get(..first)?);
    let block = blocks.get(first)?;
    match &**block {
        wordcraft_doc::Block::Para(para) if p.path.0.len() == 1 => {
            let off = p.off.min(para.text.len());
            let chars = para.text.get(..off).map_or(0, |s| s.chars().count());
            Some(FlatPos { story: p.story, at: before.saturating_add(chars), inner: None })
        }
        _ => Some(FlatPos { story: p.story, at: before, inner: Some((p.path.0.get(1..).unwrap_or(&[]).to_vec(), p.off)) }),
    }
}

pub(crate) fn pos_at(doc: &Document, f: &FlatPos) -> Pos {
    let fallback = || doc.start_of(StoryRef::Body);
    let Some(blocks) = doc.story(f.story) else { return fallback() };
    let mut at = 0usize;
    for (i, b) in blocks.iter().enumerate() {
        let len = seq::block_len(b);
        if f.at < at.saturating_add(len) || i + 1 == blocks.len() {
            return match &**b {
                wordcraft_doc::Block::Para(p) => {
                    let n = f.at.saturating_sub(at);
                    let off = p.text.char_indices().nth(n).map_or(p.text.len(), |(o, _)| o);
                    Pos::new(f.story, Path::top(i), off)
                }
                wordcraft_doc::Block::Table(_) => {
                    let (rest, off) = f.inner.clone().unwrap_or_default();
                    let mut path = vec![u32::try_from(i).unwrap_or(u32::MAX)];
                    path.extend(rest);
                    doc.clamp(&Pos::new(f.story, Path(path), off))
                }
            };
        }
        at = at.saturating_add(len);
    }
    fallback()
}

/// Where `f` is after `op` (applied to the document `f` is in).
pub(crate) fn map_flat(op: &Op, f: &FlatPos, stay: bool) -> FlatPos {
    let Some(d) = op.story(f.story) else { return f.clone() };
    let at = d.map_pos(f.at, stay);
    // A table someone else replaced keeps the place inside it only if it's still a table.
    FlatPos { story: f.story, at, inner: f.inner.clone() }
}

/// The id range new parts, comments and lists come from for client `client`.
pub fn id_space(client: u64) -> wordcraft_doc::IdSpace {
    let lo = 0x1000_0000u32 + (((client % 2048) as u32) << 16);
    wordcraft_doc::IdSpace { lo, hi: lo + 0x1_0000 }
}

#[cfg(test)]
mod tests;
