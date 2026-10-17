//! One person's copy of a shared document.
//!
//! A server (the host's business; nothing here does I/O) orders everyone's ops into one log. Each
//! copy keeps the document as of its last log entry (`confirmed`) and at most one op of its own
//! the server hasn't echoed yet (`inflight`). What the person sees (`shown`, the session's
//! document) is the truth for them: the next op sent is always the difference between what the
//! server will have once the op in flight is in (`assumed`) and what's shown. Whatever happens on
//! the way (an op that lands differently than this copy assumed, one the server refused), the next
//! op puts it right, and nothing typed is dropped.
//!
//! Convergence: log op `n` was made against log position `base`, so it's rebased over the log
//! entries `base+1 .. n-1` (its author hadn't seen them) before it's applied. That's a pure
//! function of the log, so every copy, the author's included, applies the same thing and ends
//! with the same document. An op the rebase or the apply refuses becomes a no-op everywhere. The
//! log keeps each op as applied (with the paragraph marks applying it may add), so later ops are
//! placed after those too.
//!
//! Undo is each person's own: an edit's inverse goes on their undo stack, is rebased over
//! everything that happens afterwards (where someone changed the same thing later, their change
//! stays), and undoing sends it to everyone like any other edit.

use std::collections::VecDeque;

use serde::{Deserialize, Serialize};
use wordcraft_doc::Document;

use super::{Error, Op, Result, apply, capture, compose_ops, invert, rebase};

/// Log entries kept for rebasing late ops. An op made against an older position can't be
/// placed; its author has to rejoin from a fresh copy.
const LOG_KEEP: usize = 4096;
/// Undo steps kept.
const UNDO_KEEP: usize = 100;

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
struct Inflight {
    client_seq: u64,
    /// Rebased over what arrived since it was sent: how it will land.
    current: Op,
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
    /// Nothing (this person's own op came back).
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
    inflight: Option<Inflight>,
    /// `confirmed` with the op in flight: what this person's edits since are made on. Kept
    /// sharing blocks with what's shown where they're the same, so diffing them is cheap.
    assumed: Document,
    next_client_seq: u64,
    undo: Vec<Step>,
    redo: Vec<Step>,
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
            assumed: doc.clone(),
            confirmed: doc,
            seq,
            log: VecDeque::new(),
            inflight: None,
            next_client_seq: 1,
            undo: Vec::new(),
            redo: Vec::new(),
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

    /// Edits of this person's (in `shown`) the server hasn't confirmed yet.
    pub fn has_pending(&self, shown: &Document) -> bool {
        self.inflight.is_some() || capture(&self.assumed, shown).map_or(true, |op| !op.is_empty())
    }

    pub fn can_undo(&self, redo: bool) -> bool {
        if redo { !self.redo.is_empty() } else { !self.undo.is_empty() }
    }

    /// The undo (or redo) steps' names, newest first.
    pub fn labels(&self, redo: bool) -> Vec<&str> {
        let s = if redo { &self.redo } else { &self.undo };
        s.iter().rev().map(|s| s.label.as_str()).collect()
    }

    /// This person's edit `op` (already on what they see, which was `before`): its inverse goes
    /// on the undo stack. `join`: it continues the last undo step (typing). The edit itself goes
    /// to the others with the next [`Replica::outgoing`].
    pub fn local(&mut self, before: &Document, op: Op, label: &str, join: bool) -> Result<()> {
        let inverse = invert(before, &op)?;
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
        Ok(())
    }

    /// Takes back this person's last edit (or puts back the last one taken back) on `shown`.
    /// `None`: nothing to take back.
    pub fn undo(&mut self, shown: &mut Document, redo: bool) -> Result<Option<Op>> {
        let stack = if redo { &mut self.redo } else { &mut self.undo };
        let Some(step) = stack.pop() else { return Ok(None) };
        let mut next = shown.clone();
        let applied = apply(&mut next, &step.op)?;
        let inverse = invert(shown, &applied)?;
        *shown = next;
        for s in self.undo.iter_mut().chain(self.redo.iter_mut()) {
            s.op = rebase(&s.op, &applied, true);
        }
        let back = Step { op: inverse, label: step.label };
        if redo {
            self.undo.push(back)
        } else {
            self.redo.push(back)
        }
        Ok(Some(applied))
    }

    /// The next op to send: what's shown that the server doesn't have yet. `None` while an op is
    /// on its way (the next goes once it's back) or when there's nothing new.
    pub fn outgoing(&mut self, shown: &Document) -> Option<Outgoing> {
        if self.inflight.is_some() {
            return None;
        }
        let op = capture(&self.assumed, shown).ok().filter(|op| !op.is_empty())?;
        let client_seq = self.next_client_seq;
        self.next_client_seq += 1;
        self.inflight = Some(Inflight { client_seq, current: op.clone() });
        self.assumed = shown.clone();
        Some(Outgoing { client: self.client, client_seq, base: self.seq, op })
    }

    /// The server refused the op in flight for good (too large, no permission): this person's
    /// unconfirmed edits go, and what's shown is the shared document again.
    pub fn drop_inflight(&mut self, shown: &mut Document) -> Option<Op> {
        self.inflight.take()?;
        self.assumed = self.confirmed.clone();
        let fix = capture(shown, &self.confirmed).unwrap_or_default();
        let zoom = shown.settings.zoom;
        *shown = self.confirmed.clone();
        shown.settings.zoom = zoom;
        (!fix.is_empty()).then_some(fix)
    }

    /// Applies the server's next log entry and shows the result in `shown` (with this person's
    /// unconfirmed edits on top).
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
            // In the log now. If it landed differently than assumed (or was refused), what's shown
            // stays as the person made it, and the next op sends the difference.
            self.inflight = None;
            self.assumed = self.confirmed.clone();
            self.share_if_same(shown);
            return Ok(Shown::Same);
        }
        // Someone else's: it goes after the op in flight and this person's edits since.
        let unsent = capture(&self.assumed, shown).unwrap_or_default();
        let mut theirs = op;
        if let Some(p) = self.inflight.as_mut() {
            let mine = p.current.clone();
            p.current = rebase(&mine, &theirs, false);
            theirs = rebase(&theirs, &mine, true);
        }
        let mut assumed = self.assumed.clone();
        self.assumed = match apply(&mut assumed, &theirs) {
            Ok(_) => assumed,
            Err(_) => self.rebuild_assumed(),
        };
        let theirs = rebase(&theirs, &unsent, true);
        for s in self.undo.iter_mut().chain(self.redo.iter_mut()) {
            s.op = rebase(&s.op, &theirs, true);
        }
        let mut next = shown.clone();
        let result = match apply(&mut next, &theirs) {
            Ok(applied) => {
                *shown = next;
                Shown::Applied(applied)
            }
            Err(_) => {
                // Shouldn't happen; show what the server has with this person's edits on top as
                // far as they still apply.
                let mut doc = self.assumed.clone();
                if apply(&mut doc, &rebase(&unsent, &theirs, false)).is_err() {
                    doc = self.assumed.clone();
                }
                doc.settings.zoom = shown.settings.zoom;
                let fix = capture(shown, &doc).unwrap_or_default();
                *shown = doc;
                Shown::Rebuilt(fix)
            }
        };
        self.share_if_same(shown);
        Ok(result)
    }

    /// `confirmed` with the op in flight, rebuilt.
    fn rebuild_assumed(&self) -> Document {
        let mut doc = self.confirmed.clone();
        if let Some(p) = &self.inflight {
            let mut next = doc.clone();
            if apply(&mut next, &p.current).is_ok() {
                doc = next;
            }
        }
        doc
    }

    /// With nothing of this person's unconfirmed, the confirmed document takes what's shown (the
    /// same content), so they share their blocks again and the next diff is cheap.
    fn share_if_same(&mut self, shown: &Document) {
        if self.inflight.is_none() && capture(&self.assumed, shown).is_ok_and(|op| op.is_empty()) {
            self.confirmed = shown.clone();
            self.assumed = shown.clone();
        }
    }

    /// Log entry `msg` rebased over the entries its author hadn't seen, as applied, and the
    /// confirmed document with it.
    fn place(&self, msg: &Incoming) -> Result<(Op, Document)> {
        if msg.base >= msg.seq {
            return Err(Error("co-editing: an op made against a later position".into()));
        }
        let mut op = msg.op.clone();
        for (_, m) in self.log.iter().filter(|(s, _)| *s > msg.base && *s < msg.seq) {
            op = rebase(&op, m, false);
        }
        let mut doc = self.confirmed.clone();
        let op = apply(&mut doc, &op)?;
        Ok((op, doc))
    }
}
