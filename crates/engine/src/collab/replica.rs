//! One person's copy of a shared document.
//!
//! A server (the host's business; nothing here does I/O) orders everyone's ops into one log. Each
//! copy keeps the document as of its last log entry (`confirmed`), at most one op of its own the
//! server hasn't echoed yet (`inflight`), and the edits made since (`buffer`). What the person sees
//! is `confirmed` with their pending edits on top.
//!
//! Convergence: log op `n` was made against log position `base`, so it's rebased over the log
//! entries `base+1 .. n-1` (its author hadn't seen them) before it's applied. That's a pure
//! function of the log, so every copy, the author's included, applies the same thing and ends
//! with the same document. An op the rebase or the apply refuses becomes a no-op everywhere.
//!
//! Undo is each person's own: an edit's inverse goes on their undo stack, is rebased over
//! everything that happens afterwards (where someone changed the same thing later, their change
//! stays), and undoing sends it to everyone like any other edit.

use std::collections::VecDeque;

use serde::{Deserialize, Serialize};
use wordcraft_doc::Document;

use super::{Error, Op, Result, apply, compose_ops, invert, rebase};

/// Log entries kept for rebasing late ops. An op made against an older position can't be
/// placed; its author has to rejoin from a fresh copy.
const LOG_KEEP: usize = 4096;
/// Undo steps kept.
const UNDO_KEEP: usize = 100;
/// Edits sent together at most (typing between two round trips goes as one op).
const BATCH: usize = 64;

/// An op on its way to the server.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Outgoing {
    pub client: u64,
    /// This client's count of its ops, so the echo can be matched.
    pub client_seq: u64,
    /// The last log position the op's author had applied.
    pub base: u64,
    pub op: Op,
}

/// A log entry from the server.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Incoming {
    pub seq: u64,
    pub client: u64,
    pub client_seq: u64,
    pub base: u64,
    pub op: Op,
}

#[derive(Clone, Debug)]
struct Pending {
    client_seq: u64,
    base: u64,
    /// What the server got (set when it goes out).
    original: Op,
    /// Rebased over what arrived since, for showing it on top of `confirmed`.
    current: Op,
    /// `current` was rebased over someone else's op since it was sent.
    rebased: bool,
}

/// An undo or redo step: the op that takes an edit back, and the edit's name.
#[derive(Clone, Debug)]
struct Step {
    op: Op,
    label: String,
}

/// What [`Replica::receive`] did to what's shown.
#[derive(Debug)]
pub enum Shown {
    /// Nothing (this person's own op came back as it was shown).
    Same,
    /// This op was applied to it.
    Applied(Op),
    /// It was rebuilt; this op turns the old into the new.
    Rebuilt(Op),
}

pub struct Replica {
    client: u64,
    confirmed: Document,
    seq: u64,
    log: VecDeque<(u64, Op)>,
    inflight: Option<Pending>,
    buffer: VecDeque<Pending>,
    next_client_seq: u64,
    undo: Vec<Step>,
    redo: Vec<Step>,
    /// Someone else's op went straight onto what's shown while this person had edits pending: what's
    /// shown may differ from confirmed + pending until the next rebuild.
    drift: bool,
}

impl Replica {
    /// Joins at log position `seq` with the document as of that position. `client` comes from the
    /// server: nonzero and unique in the session.
    pub fn new(client: u64, doc: Document, seq: u64) -> Result<Replica> {
        if client == 0 {
            return Err(Error("co-editing: the client id must not be 0".into()));
        }
        Ok(Replica {
            client,
            confirmed: doc,
            seq,
            log: VecDeque::new(),
            inflight: None,
            buffer: VecDeque::new(),
            next_client_seq: 1,
            undo: Vec::new(),
            redo: Vec::new(),
            drift: false,
        })
    }

    pub fn client(&self) -> u64 {
        self.client
    }

    /// The last log position applied.
    pub fn seq(&self) -> u64 {
        self.seq
    }

    /// The document as of [`Replica::seq`], without this person's pending edits.
    pub fn confirmed(&self) -> &Document {
        &self.confirmed
    }

    /// Edits of this person's the server hasn't confirmed yet.
    pub fn has_pending(&self) -> bool {
        self.inflight.is_some() || !self.buffer.is_empty()
    }

    pub fn can_undo(&self, redo: bool) -> bool {
        if redo { !self.redo.is_empty() } else { !self.undo.is_empty() }
    }

    /// The undo (or redo) steps' names, newest first.
    pub fn labels(&self, redo: bool) -> Vec<&str> {
        let s = if redo { &self.redo } else { &self.undo };
        s.iter().rev().map(|s| s.label.as_str()).collect()
    }

    /// This person's edit `op` (already on what they see, which was `before`): queued for the
    /// others, its inverse on the undo stack. `join`: it continues the last undo step (typing).
    pub fn local(&mut self, before: &Document, op: Op, label: &str, join: bool) -> Result<()> {
        let inverse = invert(before, &op)?;
        self.push_local(op, inverse, label, join);
        Ok(())
    }

    fn push_local(&mut self, op: Op, inverse: Op, label: &str, join: bool) {
        self.redo.clear();
        // Typing on: one step takes back this edit and then the ones before it (the top step
        // applies where this edit started, so it's put together before rebasing).
        let mut step = Step { op: inverse, label: label.to_string() };
        if join
            && let Some(top) = self.undo.last()
            && let Some(both) = compose_ops(&step.op, &top.op)
        {
            step = Step { op: both, label: top.label.clone() };
            self.undo.pop();
        }
        // Older steps now come before this edit: where it changed the same thing, it stays.
        for s in self.undo.iter_mut() {
            s.op = rebase(&s.op, &op, true);
        }
        self.undo.push(step);
        if self.undo.len() > UNDO_KEEP {
            self.undo.remove(0);
        }
        self.buffer.push_back(Pending { client_seq: 0, base: 0, original: Op::default(), current: op, rebased: false });
    }

    /// Takes back this person's last edit (or puts back the last one taken back) on `shown`, and
    /// queues that for the others. False: nothing to take back.
    pub fn undo(&mut self, shown: &mut Document, redo: bool) -> Result<Option<Op>> {
        let stack = if redo { &mut self.redo } else { &mut self.undo };
        let Some(step) = stack.pop() else { return Ok(None) };
        let mut next = shown.clone();
        apply(&mut next, &step.op)?;
        let inverse = invert(shown, &step.op)?;
        *shown = next;
        for s in self.undo.iter_mut().chain(self.redo.iter_mut()) {
            s.op = rebase(&s.op, &step.op, true);
        }
        let back = Step { op: inverse, label: step.label };
        if redo {
            self.undo.push(back)
        } else {
            self.redo.push(back)
        }
        self.buffer.push_back(Pending { client_seq: 0, base: 0, original: Op::default(), current: step.op.clone(), rebased: false });
        Ok(Some(step.op))
    }

    /// The next op to send, if none is waiting for its echo: the edits made since, as one op
    /// where they can be put together.
    pub fn outgoing(&mut self) -> Option<Outgoing> {
        if self.inflight.is_some() {
            return None;
        }
        let mut p = self.buffer.pop_front()?;
        let mut joined = 1;
        while joined < BATCH
            && let Some(next) = self.buffer.front()
            && let Some(both) = compose_ops(&p.current, &next.current)
        {
            p.current = both;
            self.buffer.pop_front();
            joined += 1;
        }
        p.client_seq = self.next_client_seq;
        self.next_client_seq += 1;
        p.base = self.seq;
        p.original = p.current.clone();
        p.rebased = false;
        let out = Outgoing { client: self.client, client_seq: p.client_seq, base: p.base, op: p.original.clone() };
        self.inflight = Some(p);
        Some(out)
    }

    /// Takes back an op sent but refused by the server for good (too large, no permission): the
    /// others never get it, so this person's copy drops it too. What's shown is rebuilt.
    pub fn drop_inflight(&mut self, shown: &mut Document) -> Option<Op> {
        self.inflight.take()?;
        self.rebuild(shown)
    }

    /// Applies the server's next log entry and shows the result in `shown` (with this person's
    /// pending edits on top).
    pub fn receive(&mut self, msg: Incoming, shown: &mut Document) -> Result<Shown> {
        if msg.seq != self.seq + 1 {
            return Err(Error(format!("co-editing: expected log entry {}, got {}", self.seq + 1, msg.seq)));
        }
        let own = msg.client == self.client;
        if own && self.inflight.as_ref().is_none_or(|p| p.client_seq != msg.client_seq) {
            return Err(Error("co-editing: an echo of an op this copy didn't send".into()));
        }
        // Without the entries its author hadn't seen, this copy can't place the op the way the
        // others do: rejoin from a fresh copy rather than diverge.
        let oldest = self.log.front().map_or(self.seq + 1, |(s, _)| *s);
        if msg.base < self.seq && msg.base + 1 < oldest {
            return Err(Error("co-editing: the op is older than this copy's log; rejoin the session".into()));
        }
        let (op, doc) = match self.place(&msg) {
            Ok(placed) => placed,
            // Refused the same way everywhere: a no-op in the log.
            Err(_) => (Op::default(), self.confirmed.clone()),
        };
        self.confirmed = doc;
        self.seq = msg.seq;
        self.log.push_back((msg.seq, op.clone()));
        while self.log.len() > LOG_KEEP {
            self.log.pop_front();
        }
        if own {
            let rebased = self.inflight.take().is_some_and(|p| p.rebased) || self.drift;
            self.drift = false;
            // What's shown has it already, unless someone else's op changed it on its way.
            return Ok(match rebased.then(|| self.rebuild(shown)).flatten() {
                Some(fix) => Shown::Rebuilt(fix),
                None => Shown::Same,
            });
        }
        // This person's pending edits now go after the new entry; the entry, as it lands after
        // them, goes onto what's shown.
        let mut theirs = op;
        for p in self.inflight.iter_mut().chain(self.buffer.iter_mut()) {
            let mine = p.current.clone();
            p.current = rebase(&mine, &theirs, false);
            p.rebased = true;
            theirs = rebase(&theirs, &mine, true);
        }
        for s in self.undo.iter_mut().chain(self.redo.iter_mut()) {
            s.op = rebase(&s.op, &theirs, true);
        }
        let mut next = shown.clone();
        if apply(&mut next, &theirs).is_ok() {
            *shown = next;
            self.drift |= self.has_pending();
            return Ok(Shown::Applied(theirs));
        }
        Ok(match self.rebuild(shown) {
            Some(fix) => Shown::Rebuilt(fix),
            None => Shown::Same,
        })
    }

    /// Log entry `msg` rebased over the entries its author hadn't seen, and the confirmed document
    /// with it applied.
    fn place(&self, msg: &Incoming) -> Result<(Op, Document)> {
        if msg.base >= msg.seq {
            return Err(Error("co-editing: an op made against a later position".into()));
        }
        let mut op = msg.op.clone();
        for (_, m) in self.log.iter().filter(|(s, _)| *s > msg.base && *s < msg.seq) {
            op = rebase(&op, m, false);
        }
        let mut doc = self.confirmed.clone();
        apply(&mut doc, &op)?;
        Ok((op, doc))
    }

    /// `confirmed` with the pending edits on top: what should be shown.
    #[cfg(test)]
    pub(crate) fn expected(&self) -> Document {
        let mut doc = self.confirmed.clone();
        for p in self.inflight.iter().chain(self.buffer.iter()) {
            let mut next = doc.clone();
            if apply(&mut next, &p.current).is_ok() {
                doc = next;
            }
        }
        doc
    }

    /// Shows `confirmed` with the pending edits: the op that turns what was shown into that, or
    /// `None` when they're the same.
    fn rebuild(&mut self, shown: &mut Document) -> Option<Op> {
        let mut doc = self.confirmed.clone();
        for p in self.inflight.iter().chain(self.buffer.iter()) {
            let mut next = doc.clone();
            if apply(&mut next, &p.current).is_ok() {
                doc = next;
            }
        }
        // Each person's own settings stay theirs.
        doc.settings.zoom = shown.settings.zoom;
        let fix = super::capture(shown, &doc).unwrap_or_default();
        *shown = doc;
        (!fix.is_empty()).then_some(fix)
    }
}
