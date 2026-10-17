//! Edits to whole stories: the [`Delta`] an edit made (diffed from the blocks before and after),
//! applying one, and taking one back. Only the blocks an edit touches are taken apart; blocks an
//! edit didn't touch are the same `Arc`s before and after, which is how the diff skips them.

use std::collections::HashMap;
use std::sync::Arc;

use wordcraft_doc::{Block, Blocks, CharProps};

use super::delta::{Builder, Comp, Delta, Item, Map, apply_segs, attrs_json, diff_map, invert_segs, props_json, push_item, to_json};
use super::seq::{El, ParaAttrs, Seg, block_len, blocks_from_segs, push_els, push_segs};
use super::{Error, Result};

/// The units of `blocks`.
pub(crate) fn story_len(blocks: &[Arc<Block>]) -> usize {
    blocks.iter().map(|b| block_len(b)).fold(0usize, usize::saturating_add)
}

/// The edit that turns story `old` into `new`.
pub(crate) fn diff_story(old: &Blocks, new: &Blocks, revs: &Revs) -> Result<Delta> {
    let same = |a: &Arc<Block>, b: &Arc<Block>| Arc::ptr_eq(a, b) || a == b;
    let mut pre = 0;
    while pre < old.len() && pre < new.len() && same(&old[pre], &new[pre]) {
        pre += 1;
    }
    let mut suf = 0;
    while suf < old.len() - pre && suf < new.len() - pre && same(&old[old.len() - 1 - suf], &new[new.len() - 1 - suf]) {
        suf += 1;
    }
    let (om, nm) = (&old[pre..old.len() - suf], &new[pre..new.len() - suf]);
    let mut out = Builder::default();
    out.retain(story_len(&old[..pre]));
    if om.is_empty() && nm.is_empty() {
        return Ok(out.done());
    }
    // Blocks the edit left alone in between (the same `Arc`, in order) split the rest into gaps.
    let anchors = shared_blocks(om, nm);
    let (mut oi, mut ni) = (0, 0);
    for (ao, an) in anchors.into_iter().chain(std::iter::once((om.len(), nm.len()))) {
        diff_gap(&om[oi..ao], &nm[ni..an], revs, &mut out)?;
        if let Some(b) = om.get(ao) {
            out.retain(block_len(b));
        }
        oi = ao + 1;
        ni = an + 1;
    }
    Ok(out.done())
}

/// Pairs (old index, new index) of the same block (`Arc`) in both, in order, as many as can be.
fn shared_blocks(old: &[Arc<Block>], new: &[Arc<Block>]) -> Vec<(usize, usize)> {
    let at: HashMap<*const Block, usize> = old.iter().enumerate().map(|(i, b)| (Arc::as_ptr(b), i)).collect();
    let pairs: Vec<(usize, usize)> = new.iter().enumerate().filter_map(|(j, b)| at.get(&Arc::as_ptr(b)).map(|&i| (i, j))).collect();
    // The longest run of pairs whose old indexes rise too.
    let mut tails: Vec<usize> = Vec::new();
    let mut prev: Vec<Option<usize>> = vec![None; pairs.len()];
    for (k, &(i, _)) in pairs.iter().enumerate() {
        let pos = tails.partition_point(|&t| pairs.get(t).is_some_and(|p| p.0 < i));
        if pos > 0 {
            prev[k] = tails.get(pos - 1).copied();
        }
        if pos == tails.len() {
            tails.push(k);
        } else if let Some(t) = tails.get_mut(pos) {
            *t = k;
        }
    }
    let mut out = Vec::with_capacity(tails.len());
    let mut k = tails.last().copied();
    while let Some(i) = k {
        if let Some(&p) = pairs.get(i) {
            out.push(p);
        }
        k = prev.get(i).copied().flatten();
    }
    out.reverse();
    out
}

/// The steps turning blocks `old` into `new`: the units they share at the start and the end stay
/// (with any formatting change), the rest is deleted and inserted.
fn diff_gap(old: &[Arc<Block>], new: &[Arc<Block>], revs: &Revs, out: &mut Builder) -> Result<()> {
    if old.is_empty() && new.is_empty() {
        return Ok(());
    }
    let mut oe = Vec::new();
    let mut ne = Vec::new();
    push_els(old, &mut oe);
    push_els(new, &mut ne);
    let mut pre = 0;
    while pre < oe.len() && pre < ne.len() && oe[pre].same_unit(&ne[pre]) {
        pre += 1;
    }
    let mut suf = 0;
    while suf < oe.len() - pre && suf < ne.len() - pre && oe[oe.len() - 1 - suf].same_unit(&ne[ne.len() - 1 - suf]) {
        suf += 1;
    }
    let mut memo = PatchMemo { revs, last: None };
    for (a, b) in oe[..pre].iter().zip(&ne[..pre]) {
        change(a, b, &mut memo, out)?;
    }
    out.delete(oe.len() - pre - suf);
    out.push(Comp::Insert { items: items_of(&ne[pre..ne.len() - suf])? });
    for (a, b) in oe[oe.len() - suf..].iter().zip(&ne[ne.len() - suf..]) {
        change(a, b, &mut memo, out)?;
    }
    Ok(())
}

/// Each document's tracked-change revisions by value. Copies of a shared document number their
/// revisions their own way, and a revision with the same author, date and kind as another may
/// share its number, so formatting is compared with revisions by value.
pub(crate) struct Revs {
    old: Vec<u32>,
    new: Vec<u32>,
}

impl Revs {
    pub fn new(old: &[wordcraft_doc::Revision], new: &[wordcraft_doc::Revision]) -> Revs {
        let mut ids: HashMap<String, u32> = HashMap::new();
        let mut id = |r: &wordcraft_doc::Revision| {
            let key = serde_json::to_string(r).unwrap_or_default();
            let n = u32::try_from(ids.len()).unwrap_or(u32::MAX);
            *ids.entry(key).or_insert(n)
        };
        let old = old.iter().map(&mut id).collect();
        let new = new.iter().map(&mut id).collect();
        Revs { old, new }
    }

    /// `b` as a patch over `a`: the keys whose values differ once revisions are compared by
    /// value, with `b`'s values (`null` where `b` has none).
    fn patch(&self, a: &Map, b: &Map) -> Map {
        let (mut ca, mut cb) = (a.clone(), b.clone());
        super::remap_revs(&mut ca, &mut |i| self.old.get(i as usize).copied().unwrap_or(u32::MAX));
        super::remap_revs(&mut cb, &mut |i| self.new.get(i as usize).copied().unwrap_or(u32::MAX));
        diff_map(&ca, &cb).keys().map(|k| (k.clone(), b.get(k).cloned().unwrap_or(serde_json::Value::Null))).collect()
    }
}

/// The last formatting patch worked out: neighbouring characters share their run's formatting.
struct PatchMemo<'r> {
    revs: &'r Revs,
    last: Option<(*const CharProps, *const CharProps, Map)>,
}

impl PatchMemo<'_> {
    fn patch(&mut self, a: &CharProps, b: &CharProps) -> Result<Map> {
        if let Some((pa, pb, m)) = &self.last
            && std::ptr::eq(*pa, a)
            && std::ptr::eq(*pb, b)
        {
            return Ok(m.clone());
        }
        let m = if a == b { Map::new() } else { self.revs.patch(&props_json(a)?, &props_json(b)?) };
        self.last = Some((a, b, m.clone()));
        Ok(m)
    }
}

/// The step for one unit kept between the versions: a keep, or its change.
fn change(a: &El<'_>, b: &El<'_>, memo: &mut PatchMemo<'_>, out: &mut Builder) -> Result<()> {
    match (a, b) {
        (El::Ch(_, pa), El::Ch(_, pb)) => out.push(Comp::Format { n: 1, p: memo.patch(pa, pb)? }),
        (El::Obj(oa, pa), El::Obj(ob, pb)) => {
            let p = memo.patch(pa, pb)?;
            match (oa == ob, p.is_empty()) {
                (true, _) => out.push(Comp::Format { n: 1, p }),
                (false, true) => out.push(Comp::Obj { o: to_json(ob)? }),
                // Both changed: one step can't say that, so the object goes and comes back.
                (false, false) => {
                    out.delete(1);
                    out.push(Comp::Insert { items: items_of(std::slice::from_ref(b))? });
                }
            }
        }
        (El::End(pa), El::End(pb)) => {
            let (aa, ab) = (ParaAttrs::of(pa), ParaAttrs::of(pb));
            if aa == ab {
                out.retain(1);
            } else {
                out.push(Comp::Para { p: memo.revs.patch(&attrs_json(&aa)?, &attrs_json(&ab)?) });
            }
        }
        (El::Table(ta), El::Table(tb)) => {
            let wrap = |t: &wordcraft_doc::Table| -> Result<Map> { Ok(std::iter::once(("t".to_string(), to_json(t)?)).collect()) };
            if ta == tb || memo.revs.patch(&wrap(ta)?, &wrap(tb)?).is_empty() {
                out.retain(1);
            } else {
                out.push(Comp::Table { v: to_json(tb)? });
            }
        }
        _ => return Err(Error("co-editing: diff paired different kinds of unit".into())),
    }
    Ok(())
}

/// Units as inserted items.
fn items_of(els: &[El<'_>]) -> Result<Vec<Item>> {
    let mut items = Vec::new();
    let mut text = String::new();
    let mut props: Option<&CharProps> = None;
    let flush = |text: &mut String, props: Option<&CharProps>, items: &mut Vec<Item>| -> Result<()> {
        if !text.is_empty() {
            let p = props.map(props_json).transpose()?.unwrap_or_default();
            push_item(items, Item::Text { s: std::mem::take(text), p });
        }
        Ok(())
    };
    for el in els {
        match el {
            El::Ch(c, p) => {
                if props.is_some_and(|q| !std::ptr::eq(q, *p) && q != *p) {
                    flush(&mut text, props, &mut items)?;
                }
                props = Some(p);
                text.push(*c);
            }
            El::Obj(o, p) => {
                flush(&mut text, props, &mut items)?;
                items.push(Item::Obj { o: to_json(o)?, p: props_json(p)? });
            }
            El::End(para) => {
                flush(&mut text, props, &mut items)?;
                items.push(Item::End { a: attrs_json(&ParaAttrs::of(para))? });
            }
            El::Table(t) => {
                flush(&mut text, props, &mut items)?;
                items.push(Item::Table { v: to_json(t)? });
            }
        }
    }
    flush(&mut text, props, &mut items)?;
    Ok(items)
}

/// The blocks `delta` touches: (first index, end index, units before the first). An insert at the
/// very end of the story touches the last block.
fn touched(blocks: &[Arc<Block>], delta: &Delta) -> Result<Option<(usize, usize, usize)>> {
    let Some((start, end)) = delta.span() else { return Ok(None) };
    let total = story_len(blocks);
    if end > total || start > total {
        return Err(Error("co-editing: an edit runs past the end of the story".into()));
    }
    let mut at = 0usize;
    let mut first = None;
    let mut last = 0;
    for (i, b) in blocks.iter().enumerate() {
        let len = block_len(b);
        let next = at.saturating_add(len);
        if first.is_none() && (start < next || next == total) {
            first = Some((i, at));
        }
        if first.is_some() {
            last = i;
            if end < next || (end == next && end > start) || next == total {
                break;
            }
        }
        at = next;
    }
    let Some((first, before)) = first else { return Err(Error("co-editing: an edit for an empty story".into())) };
    // One more block, so text left without its paragraph mark joins the paragraph after it.
    let end_ix = (last + 2).min(blocks.len());
    Ok(Some((first, end_ix, before)))
}

/// Applies `delta` to a story.
pub(crate) fn apply_story(blocks: &mut Blocks, delta: &Delta) -> Result<()> {
    let Some((first, end, before)) = touched(blocks, delta)? else { return Ok(()) };
    let mut segs = Vec::new();
    push_segs(&blocks[first..end], &mut segs);
    let (lead, rest) = delta.split_lead();
    let mut comps = vec![Comp::Retain { n: lead - before }];
    comps.extend_from_slice(rest);
    let segs = apply_segs(segs, &comps)?;
    let fresh = blocks_from_segs(segs);
    blocks.splice(first..end, fresh);
    Ok(())
}

/// The edit that takes `delta` back, given the story it was applied to.
pub(crate) fn invert_story(blocks: &Blocks, delta: &Delta) -> Result<Delta> {
    let Some((first, end, before)) = touched(blocks, delta)? else { return Ok(Delta::default()) };
    let mut segs: Vec<Seg> = Vec::new();
    push_segs(&blocks[first..end], &mut segs);
    let (lead, rest) = delta.split_lead();
    let mut comps = vec![Comp::Retain { n: lead - before }];
    comps.extend_from_slice(rest);
    let inv = invert_segs(segs, &comps)?;
    Ok(Delta(inv).shifted(before))
}

/// A whole story as inserted items (a new part's content).
pub(crate) fn story_items(blocks: &[Arc<Block>]) -> Result<Vec<Item>> {
    let mut segs = Vec::new();
    push_segs(blocks, &mut segs);
    let mut items = Vec::new();
    for s in &segs {
        push_item(&mut items, super::delta::seg_item(s)?);
    }
    Ok(items)
}

/// A story made of inserted items.
pub(crate) fn story_from_items(items: &[Item]) -> Result<Blocks> {
    let segs = apply_segs(Vec::new(), &[Comp::Insert { items: items.to_vec() }])?;
    Ok(blocks_from_segs(segs))
}
