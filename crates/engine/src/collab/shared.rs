//! Co-editing through a [`Session`]: once started, every edit (from any command path: ribbon,
//! keys, agents) goes into the outgoing queue, and the host feeds the server's log back in.

use serde_json::{Value, json};
use wordcraft_doc::{Document, Pos};

use super::{Error, FlatPos, Incoming, MAX_SHARED_OP_BYTES, Op, Outgoing, Replica, Shown, capture, flat_pos, id_space, map_flat, pos_at};
use crate::{CmdError, Selection, Session};

/// The shared document of a [`Session`] and this person's copy of it.
pub(crate) struct Shared {
    pub(crate) replica: Replica,
    /// The document as last shared: the next edit is what changed since.
    pub(crate) shown: Document,
    /// This person can view the document but not edit it.
    pub(crate) read_only: bool,
    /// [`Session::document_id`] of the shared document: opening another one ends sharing.
    pub(crate) document_id: u64,
}

const READ_ONLY: &str = "You can view this document but not edit it.";

/// Runs a command on a shared document: Undo and Redo take back this person's own edits, and
/// any other change is shared with the others.
pub(crate) fn run_shared(s: &mut Session, id: &str, params: &Value) -> Result<Value, CmdError> {
    match id {
        "edit.undo" => return s.collab_undo(false, 1),
        "edit.redo" => return s.collab_undo(true, 1),
        "edit.undoTo" => {
            let n = crate::p::u64(params, "count").ok_or_else(|| CmdError::Params("`count` (whole number, 1 or more) is required".into()))?;
            if n == 0 {
                return Err(CmdError::Params("`count` must be 1 or more".into()));
            }
            return s.collab_undo(false, usize::try_from(n).unwrap_or(usize::MAX));
        }
        _ => {}
    }
    let client = s.collab.as_ref().map_or(0, |c| c.replica.client());
    let before = s.undo_counts();
    let typing = matches!(id, "text.insert" | "equation.type");
    s.collab_depth += 1;
    let r = wordcraft_doc::with_id_space(Some(id_space(client)), || s.run_inner(id, params));
    s.collab_depth -= 1;
    let joined = typing && s.undo_counts() == before;
    let label = if typing { "Typing".to_string() } else { s.registry.get(id).map_or_else(|| id.to_string(), |c| c.label.to_string()) };
    match s.share_edit(&label, joined) {
        Ok(()) => r,
        Err(e) => {
            s.status = e.to_string();
            Err(e)
        }
    }
}

impl Session {
    /// Shares the document, which is the shared document as of log position `seq`; `client` is
    /// this person's id from the server.
    pub fn collab_start(&mut self, client: u64, seq: u64) -> Result<(), CmdError> {
        let replica = Replica::new(client, self.doc.clone(), seq)?;
        self.collab = Some(Box::new(Shared { replica, shown: self.doc.clone(), read_only: false, document_id: self.document_id() }));
        Ok(())
    }

    /// Opens the shared document from a checkpoint's snapshot ([`super::snapshot`]): the exact
    /// copy the others have, which re-reading the saved file wouldn't give. Call before
    /// [`Session::collab_start`]; the document counts as saved.
    pub fn collab_load_snapshot(&mut self, snapshot: &str) -> Result<(), CmdError> {
        let doc = super::from_snapshot(snapshot)?;
        let path = self.path.clone();
        let name = self.file_name.clone();
        self.set_document(doc);
        self.path = path;
        self.file_name = name;
        self.dirty = false;
        Ok(())
    }

    /// The shared document as everyone has it (without this person's unconfirmed edits) and its
    /// log position, as a snapshot: what a checkpoint keeps beside the saved file.
    pub fn collab_snapshot(&self) -> Option<Result<(u64, String), CmdError>> {
        let c = self.collab.as_ref()?;
        Some(super::snapshot(c.replica.confirmed()).map(|s| (c.replica.seq(), s)).map_err(Into::into))
    }

    /// The shared document as everyone has it and its log position: what a checkpoint saves.
    pub fn collab_confirmed(&self) -> Option<(u64, &Document)> {
        self.collab.as_ref().map(|c| (c.replica.seq(), c.replica.confirmed()))
    }

    /// This person can view the shared document but not edit it: an edit is taken back with a
    /// message (selecting, scrolling and copying still work).
    pub fn collab_set_read_only(&mut self, read_only: bool) {
        if let Some(c) = self.collab.as_mut() {
            c.read_only = read_only;
        }
    }

    pub fn collab_read_only(&self) -> bool {
        self.collab.as_ref().is_some_and(|c| c.read_only)
    }

    pub fn collab_active(&self) -> bool {
        self.collab.is_some()
    }

    pub fn collab_stop(&mut self) {
        self.collab = None;
    }

    /// This person's client id while the document is shared.
    pub fn collab_client(&self) -> Option<u64> {
        self.collab.as_ref().map(|c| c.replica.client())
    }

    /// The last log position applied.
    pub fn collab_seq(&self) -> Option<u64> {
        self.collab.as_ref().map(|c| c.replica.seq())
    }

    /// This person has edits the server hasn't confirmed.
    pub fn collab_pending(&self) -> bool {
        self.collab.as_ref().is_some_and(|c| c.replica.has_pending())
    }

    /// The next op to send to the server (one at a time: the next once this one comes back).
    pub fn collab_outgoing(&mut self) -> Option<Outgoing> {
        self.collab_flush();
        self.collab.as_mut().and_then(|c| c.replica.outgoing())
    }

    /// The server refused the op in flight for good: drop it here too.
    pub fn collab_refused(&mut self) {
        let Some(mut c) = self.collab.take() else { return };
        if let Some(fix) = c.replica.drop_inflight(&mut self.doc) {
            self.follow(&c.shown, &fix);
        }
        c.shown = self.doc.clone();
        self.collab = Some(c);
    }

    /// Applies the server's next log entry. An error means this copy can't follow the log (an
    /// entry missing or out of order): rejoin from a fresh copy.
    pub fn collab_receive(&mut self, msg: Incoming) -> Result<(), CmdError> {
        self.collab_flush();
        let Some(mut c) = self.collab.take() else { return Err(CmdError::Failed("co-editing: no shared document".into())) };
        let r = c.replica.receive(msg, &mut self.doc);
        if let Ok(Shown::Applied(op) | Shown::Rebuilt(op)) = &r {
            self.follow(&c.shown, op);
        }
        c.shown = self.doc.clone();
        self.collab = Some(c);
        r.map(|_| ()).map_err(Into::into)
    }

    /// Where this person's selection is, for showing it to the others.
    pub fn collab_presence(&self) -> Value {
        json!({"anchor": self.sel.anchor, "focus": self.sel.focus})
    }

    /// Keeps the selection where it was through someone else's `op` (just applied).
    fn follow(&mut self, old: &Document, op: &Op) {
        let map = |p: &Pos| flat_pos(old, p).map(|f| map_flat(op, &f, true));
        let (a, f): (Option<FlatPos>, Option<FlatPos>) = (map(&self.sel.anchor), map(&self.sel.focus));
        if let (Some(a), Some(f)) = (a, f) {
            self.sel = Selection { anchor: pos_at(&self.doc, &a), focus: pos_at(&self.doc, &f) };
        }
        self.widened = None;
        self.column = None;
        self.also_selected.clear();
        self.clamp_selection();
        self.touch_quiet();
    }

    /// Shares a change made outside a command (a host writing to the document directly).
    fn collab_flush(&mut self) {
        if self.collab.is_some()
            && let Err(e) = self.share_edit("Edit", false)
        {
            self.status = e.to_string();
        }
    }

    /// After a command on the shared document: the change goes to the others. If it can't be
    /// shared, it's taken back rather than left on this screen only.
    pub(crate) fn share_edit(&mut self, label: &str, join: bool) -> Result<(), CmdError> {
        let Some(mut c) = self.collab.take() else { return Ok(()) };
        if self.document_id() != c.document_id {
            // Another document replaced the shared one: it isn't shared.
            return Ok(());
        }
        let shared = capture(&c.shown, &self.doc).and_then(|op| {
            if op.is_empty() {
                return Ok(None);
            }
            if c.read_only {
                return Err(Error(READ_ONLY.into()));
            }
            let size = serde_json::to_vec(&op).map(|v| v.len()).unwrap_or(usize::MAX);
            if size > MAX_SHARED_OP_BYTES {
                return Err(Error(format!(
                    "That change is too large to share with the others ({:.1} MB). Make it in smaller steps.",
                    size as f64 / 1_048_576.0
                )));
            }
            Ok(Some(op))
        });
        let r = match shared {
            Ok(None) => Ok(()),
            Ok(Some(op)) => {
                #[cfg(test)]
                {
                    let mut again = c.shown.clone();
                    let applied = super::apply(&mut again, &op);
                    let left = applied.as_ref().ok().and_then(|_| capture(&again, &self.doc).ok());
                    assert!(
                        left.as_ref().is_some_and(Op::is_empty),
                        "{label}: the op doesn't reproduce the edit ({applied:?}); left over: {}",
                        serde_json::to_string(&left).unwrap_or_default()
                    );
                }
                let r = c.replica.local(&c.shown, op, label, join);
                if r.is_ok() {
                    c.shown = self.doc.clone();
                }
                r
            }
            Err(e) => Err(e),
        };
        if r.is_err() {
            self.doc = c.shown.clone();
            self.clamp_selection();
            self.touch_quiet();
        }
        self.collab = Some(c);
        r.map_err(Into::into)
    }

    /// Undo (or redo) in a shared document: this person's own last `count` edits, for everyone.
    /// Their edits since stay.
    pub(crate) fn collab_undo(&mut self, redo: bool, count: usize) -> Result<Value, CmdError> {
        self.collab_flush();
        let Some(mut c) = self.collab.take() else { return Ok(Value::Null) };
        if c.read_only {
            self.collab = Some(c);
            return Err(CmdError::Disabled(READ_ONLY.into()));
        }
        let mut done = 0;
        let mut last: Option<Op> = None;
        let mut err = None;
        while done < count {
            match c.replica.undo(&mut self.doc, redo) {
                Ok(Some(op)) => {
                    done += 1;
                    last = Some(op);
                }
                Ok(None) => break,
                Err(e) => {
                    err = Some(e);
                    break;
                }
            }
        }
        // The caret goes where the last step changed something.
        if let Some(op) = &last
            && let Some(e) = op.stories.first()
            && let Some((start, _)) = e.delta.span()
        {
            let at = pos_at(&self.doc, &FlatPos { story: e.story, at: start, inner: None });
            self.sel = Selection::caret(at);
        }
        c.shown = self.doc.clone();
        self.collab = Some(c);
        if done > 0 {
            self.touch();
            self.clamp_selection();
        }
        if let Some(e) = err {
            return Err(e.into());
        }
        if done == 0 {
            let what = if redo { "redo" } else { "undo" };
            return Err(CmdError::Disabled(format!("Nothing of yours to {what}.")));
        }
        Ok(json!({"undone": done, "depth": self.undo_depth()}))
    }
}
