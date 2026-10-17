//! Live co-editing for a document opened from Nextcloud: the desktop joins the same session as
//! the browser editor (`wordcraft-nextcloud`'s `/apps/wordcraft/collab/*`, signed in with the app
//! password) and does what the page's `collab.js` does there. Edits go out one at a time,
//! everyone's come back in the server's order, this person's caret travels with each poll, and an
//! editor saves the shared document to the file now and then (a checkpoint), with the engine's
//! state snapshot of it, so people joining later start from exactly the shared document.
//!
//! A worker thread talks to the server; the UI thread ([`Coedit::poll`]) moves edits between it
//! and the engine. Ported from GridLogic's `apps/gridcraft/src/nextcloud/coedit.rs`; the
//! difference is the state snapshot: joining at a checkpoint that kept one loads it
//! (`Session::collab_load_snapshot`) instead of the file, and checkpoints send one.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value as Json, json};
use wordcraft_engine::collab::{Incoming, Op};
use wordcraft_engine::doc::Document;
use wordcraft_ui_egui::WordApp;

use super::{Connection, State, encode, lock, mtime, persist, tie_key};

/// Save the shared document after this many log entries, or this long after the first unsaved
/// one (as the browser does); after a Save here, at most this often.
const CHECKPOINT_OPS: u64 = 20;
const CHECKPOINT_SECONDS: f64 = 30.0;
const SAVE_CHECKPOINT_SECONDS: f64 = 10.0;
/// How often to ask for others' edits.
const POLL: Duration = Duration::from_millis(600);
/// How long quitting waits for the worker to leave the session.
const LEAVE_WAIT: Duration = Duration::from_secs(2);

/// Someone in the session, as the server lists them.
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Member {
    client: u64,
    #[serde(default)]
    user_id: String,
    #[serde(default)]
    display_name: String,
    #[serde(default = "yes")]
    can_edit: bool,
}

fn yes() -> bool {
    true
}

impl Member {
    fn name(&self) -> &str {
        if self.display_name.is_empty() { &self.user_id } else { &self.display_name }
    }
}

/// What joining gave: the session the downloaded file belongs to.
#[derive(Clone, Debug)]
pub(super) struct Joined {
    session_id: u64,
    client: u64,
    seq: u64,
    entries: Vec<Incoming>,
    members: Vec<Member>,
    can_edit: bool,
    /// The shared document as of `seq` (the checkpoint's state snapshot), when the session keeps
    /// one: opened instead of the file, whose re-read ids would differ.
    state: Option<String>,
    /// The file's ETag as downloaded (no quotes): the session's checkpoint.
    etag: String,
    /// The file, relative to the user's files.
    remote: String,
}

impl Joined {
    /// Leaves the session without having shared anything (the document never opened).
    pub(super) fn leave(&self, state: &Arc<Mutex<State>>) {
        let (state, body) = (state.clone(), json!({"sessionId": self.session_id, "client": self.client}));
        let _ = std::thread::Builder::new().name("nextcloud leave".into()).spawn(move || {
            if let Ok(nc) = Connection::open(&state) {
                let _ = nc.app("POST", "collab/leave", &body);
            }
        });
    }
}

/// Log entries as the server sends them (each op as JSON text); an op that doesn't parse
/// becomes a no-op in its place.
fn entries(v: Option<&Json>) -> Vec<Incoming> {
    v.and_then(Json::as_array)
        .into_iter()
        .flatten()
        .filter_map(|e| {
            let num = |k: &str| e.get(k).and_then(Json::as_u64);
            let op: Op = e.get("op").and_then(Json::as_str).and_then(|t| serde_json::from_str(t).ok()).unwrap_or_default();
            Some(Incoming { seq: num("seq")?, client: num("client")?, client_seq: num("client_seq")?, base: num("base")?, op })
        })
        .collect()
}

/// Joins the session for `file_id`, pinned to the version downloaded (`etag`), with the state
/// snapshot for where it starts when the session keeps one. `None`: the session saved a newer
/// version since (or replaced the snapshot); download again.
pub(super) fn join(nc: &Connection, file_id: u64, etag: &str, remote: &str) -> Result<Option<Joined>, String> {
    let etag = etag.trim_start_matches("W/").trim_matches('"').to_string();
    let r = nc.app("POST", "collab/join", &json!({"fileId": file_id, "etag": etag}))?;
    if r.get("retry").and_then(Json::as_bool) == Some(true) {
        return Ok(None);
    }
    let num = |k: &str| r.get(k).and_then(Json::as_u64).ok_or_else(|| format!("Nextcloud's co-editing answer had no {k}."));
    let (session_id, client, seq) = (num("sessionId")?, num("client")?, num("seq")?);
    let state = if r.get("hasState").and_then(Json::as_bool) == Some(true) {
        let a = nc.app_raw("GET", &format!("collab/state?sessionId={session_id}&client={client}&seq={seq}"), &Json::Null)?;
        if a.status != 200 {
            // Saved again in between (that save replaced the snapshot): start over from the newer
            // file.
            let _ = nc.app("POST", "collab/leave", &json!({"sessionId": session_id, "client": client}));
            return Ok(None);
        }
        Some(String::from_utf8(a.body).map_err(|_| "Nextcloud's copy of the shared document isn't text.".to_string())?)
    } else {
        None
    };
    Ok(Some(Joined {
        session_id,
        client,
        seq,
        entries: entries(r.get("entries")),
        members: r.get("members").cloned().and_then(|m| serde_json::from_value(m).ok()).unwrap_or_default(),
        can_edit: r.get("canEdit").and_then(Json::as_bool) != Some(false),
        state,
        etag,
        remote: remote.to_string(),
    }))
}

/// A checkpoint for the worker to write: the shared document at `seq`, saved as `name`.
struct Job {
    seq: u64,
    doc: Document,
    name: String,
    password: Option<String>,
    opts: wordcraft_engine::io::ExportOptions,
}

enum Event {
    /// This copy can't follow the session any more: open the file again.
    Rejoin,
    /// The session can't go on (the file changed outside it).
    End(String),
    /// The server refused this person's edits for good (they can only view now).
    Refused(String),
    Notice(String, bool),
}

/// Between the UI thread and the worker.
#[derive(Default)]
struct Link {
    /// This person's edits, in order: (client sequence number, base, op as JSON text).
    outbox: VecDeque<(u64, u64, String)>,
    entries: Vec<Incoming>,
    members: Option<Vec<Member>>,
    checkpoint_seq: u64,
    checkpoint_etag: String,
    /// This person's position (JSON), for the next poll.
    selection: String,
    job: Option<Job>,
    writing: bool,
    events: Vec<Event>,
    stop: bool,
}

fn link(l: &Mutex<Link>) -> MutexGuard<'_, Link> {
    l.lock().unwrap_or_else(|e| e.into_inner())
}

/// What became of the session this frame.
pub(super) enum Status {
    Going,
    /// Another document was opened, or the session ended: sharing is over.
    Over,
    /// Open the file again and rejoin.
    Rejoin,
}

/// A shared document (the UI thread's side).
pub(super) struct Coedit {
    /// [`wordcraft_engine::Session::document_id`] of the shared document.
    document: u64,
    /// The local copy (its record's key) and the Nextcloud file.
    pub(super) local: String,
    pub(super) remote: String,
    client: u64,
    can_edit: bool,
    link: Arc<Mutex<Link>>,
    members: Vec<Member>,
    first_unsaved_at: Option<f64>,
    last_save_checkpoint: f64,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl Coedit {
    /// Shares the document showing (just opened from the downloaded file) in the joined session:
    /// from the session's state snapshot when it keeps one, then catching up on the log.
    pub(super) fn start(app: &mut WordApp, joined: Joined, local: String, state: Arc<Mutex<State>>, ctx: &egui::Context) -> Result<Coedit, String> {
        let started = Self::share(app, &joined);
        if let Err(e) = started {
            app.session.collab_stop();
            joined.leave(&state);
            return Err(e);
        }
        let last_seq = joined.entries.last().map_or(joined.seq, |e| e.seq.max(joined.seq));
        if !joined.can_edit {
            app.status("You can view this document but not edit it.");
        }
        let link = Arc::new(Mutex::new(Link { checkpoint_seq: joined.seq, checkpoint_etag: joined.etag.clone(), ..Link::default() }));
        let worker = Worker {
            state,
            link: link.clone(),
            ctx: ctx.clone(),
            session_id: joined.session_id,
            client: joined.client,
            last_seq,
            remote: joined.remote.clone(),
            local: local.clone(),
            sent_selection: String::new(),
        };
        let handle = std::thread::Builder::new().name("nextcloud co-editing".into()).spawn(move || worker.run()).map_err(|e| e.to_string());
        let handle = match handle {
            Ok(h) => h,
            Err(e) => {
                app.session.collab_stop();
                return Err(e);
            }
        };
        let c = Coedit {
            document: app.session.document_id(),
            local,
            remote: joined.remote,
            client: joined.client,
            can_edit: joined.can_edit,
            link,
            members: joined.members,
            first_unsaved_at: None,
            last_save_checkpoint: f64::NEG_INFINITY,
            worker: Some(handle),
        };
        c.say_who(app, &[]);
        Ok(c)
    }

    /// The engine's side of joining.
    fn share(app: &mut WordApp, joined: &Joined) -> Result<(), String> {
        if let Some(snapshot) = &joined.state {
            app.session.collab_load_snapshot(snapshot).map_err(|e| e.to_string())?;
        }
        app.session.collab_start(joined.client, joined.seq).map_err(|e| e.to_string())?;
        app.session.collab_set_read_only(!joined.can_edit);
        for e in joined.entries.iter().filter(|e| e.seq > joined.seq) {
            app.session.collab_receive(e.clone()).map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    /// Stops sharing: the worker leaves the session.
    pub(super) fn stop(&self, app: &mut WordApp) {
        link(&self.link).stop = true;
        if app.session.collab_client() == Some(self.client) {
            app.session.collab_stop();
        }
    }

    /// Waits a moment for the worker to have left the session (when quitting).
    pub(super) fn join_worker(mut self) {
        let Some(handle) = self.worker.take() else { return };
        let until = std::time::Instant::now() + LEAVE_WAIT;
        while !handle.is_finished() && std::time::Instant::now() < until {
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Every frame: everyone's edits in, this person's out, positions both ways, checkpoints.
    /// `just_saved`: the document was saved here this frame (Save or AutoSave).
    pub(super) fn poll(&mut self, app: &mut WordApp, now: f64, just_saved: bool) -> Status {
        if app.session.document_id() != self.document || app.session.collab_client() != Some(self.client) {
            // Another document replaced the shared one in this window.
            return Status::Over;
        }
        let (entries, members, events) = {
            let mut l = link(&self.link);
            (std::mem::take(&mut l.entries), l.members.take(), std::mem::take(&mut l.events))
        };
        for e in entries {
            if app.session.collab_receive(e).is_err() {
                return Status::Rejoin;
            }
        }
        for event in events {
            match event {
                Event::Rejoin => return Status::Rejoin,
                Event::End(text) => {
                    app.cloud_files.notice = Some(("Co-editing stopped".into(), text));
                    return Status::Over;
                }
                Event::Refused(text) => {
                    app.session.collab_refused();
                    app.session.collab_set_read_only(true);
                    self.can_edit = false;
                    app.cloud_files.notice = Some(("Nextcloud".into(), text));
                }
                Event::Notice(text, true) => app.cloud_files.notice = Some(("Nextcloud".into(), text)),
                Event::Notice(text, false) => app.status(text),
            }
        }
        if let Some(m) = members
            && m != self.members
        {
            let before = std::mem::replace(&mut self.members, m);
            self.say_who(app, &before);
        }
        while let Some(out) = app.session.collab_outgoing() {
            let op = serde_json::to_string(&out.op).unwrap_or_else(|_| "{}".into());
            link(&self.link).outbox.push_back((out.client_seq, out.base, op));
        }
        // This person's caret and selection, for the others (the same JSON the browser sends).
        let selection = app.session.collab_presence().to_string();
        {
            let mut l = link(&self.link);
            if l.selection != selection {
                l.selection = selection;
            }
        }
        self.maybe_checkpoint(app, now, just_saved);
        Status::Going
    }

    /// Who came and went, on the status bar.
    fn say_who(&self, app: &mut WordApp, before: &[Member]) {
        let others = |list: &[Member]| -> Vec<(u64, String)> {
            list.iter().filter(|m| m.client != self.client).map(|m| (m.client, m.name().to_string())).collect()
        };
        let (was, now) = (others(before), others(&self.members));
        let came: Vec<&str> = now.iter().filter(|p| !was.contains(p)).map(|(_, n)| n.as_str()).collect();
        let went: Vec<&str> = was.iter().filter(|p| !now.contains(p)).map(|(_, n)| n.as_str()).collect();
        if !came.is_empty() {
            app.status(format!("Editing together with {}", came.join(", ")));
        } else if !went.is_empty() {
            app.status(format!("{} stopped editing", went.join(", ")));
        }
    }

    /// The editor with the lowest client id saves when enough has changed; a Save here saves
    /// (an editor's, at most every few seconds, as AutoSave saves often).
    fn maybe_checkpoint(&mut self, app: &WordApp, now: f64, just_saved: bool) {
        if !self.can_edit {
            return;
        }
        let Some((seq, doc)) = app.session.collab_confirmed() else { return };
        let mut l = link(&self.link);
        if seq <= l.checkpoint_seq {
            self.first_unsaved_at = None;
            return;
        }
        let first = *self.first_unsaved_at.get_or_insert(now);
        let writer = self.members.iter().filter(|m| m.can_edit).map(|m| m.client).min().unwrap_or(self.client) == self.client;
        let due = writer && (seq - l.checkpoint_seq >= CHECKPOINT_OPS || now - first >= CHECKPOINT_SECONDS);
        let asked = just_saved && now - self.last_save_checkpoint >= SAVE_CHECKPOINT_SECONDS;
        if (due || asked) && l.job.is_none() && !l.writing {
            if asked {
                self.last_save_checkpoint = now;
            }
            let name = self.remote.rsplit('/').next().unwrap_or("Document.docx").to_string();
            let password = app.session.password.as_ref().map(|p| p.as_str().to_string());
            let opts = wordcraft_engine::cmd::file::export_options(&app.session, &json!({}));
            l.job = Some(Job { seq, doc: doc.clone(), name, password, opts });
        }
    }
}

/// The worker thread's side.
struct Worker {
    state: Arc<Mutex<State>>,
    link: Arc<Mutex<Link>>,
    ctx: egui::Context,
    session_id: u64,
    client: u64,
    last_seq: u64,
    remote: String,
    local: String,
    sent_selection: String,
}

impl Worker {
    fn run(mut self) {
        let nc = match Connection::open(&self.state) {
            Ok(nc) => nc,
            Err(e) => return self.event(Event::End(e)),
        };
        loop {
            if link(&self.link).stop {
                let _ = nc.app("POST", "collab/leave", &json!({"sessionId": self.session_id, "client": self.client}));
                return;
            }
            if !self.push(&nc) {
                return;
            }
            let job = link(&self.link).job.take();
            if let Some(job) = job {
                link(&self.link).writing = true;
                let ok = self.checkpoint(&nc, job);
                link(&self.link).writing = false;
                if !ok {
                    return;
                }
            }
            match self.sync(&nc) {
                Some(true) => continue,
                Some(false) => self.wait(POLL),
                None => return,
            }
        }
    }

    /// Sleeps, waking early when sharing stops or an edit is waiting to go.
    fn wait(&self, d: Duration) {
        let until = std::time::Instant::now() + d;
        while std::time::Instant::now() < until {
            {
                let l = link(&self.link);
                if l.stop || !l.outbox.is_empty() || l.job.is_some() {
                    return;
                }
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    fn event(&self, e: Event) {
        link(&self.link).events.push(e);
        self.ctx.request_repaint();
    }

    /// Sends queued edits in order. False when the session is over for this copy.
    fn push(&self, nc: &Connection) -> bool {
        loop {
            let Some((client_seq, base, op)) = link(&self.link).outbox.front().cloned() else { return true };
            let body = json!({"sessionId": self.session_id, "client": self.client, "clientSeq": client_seq, "base": base, "op": op});
            match nc.app_status("POST", "collab/push", &body) {
                Ok((200..=299, _)) => {
                    link(&self.link).outbox.pop_front();
                }
                Ok((403, _)) => {
                    link(&self.link).outbox.clear();
                    self.event(Event::Refused("You can view this document but not edit it: your changes aren't shared.".into()));
                }
                Ok((409, _)) => {
                    self.event(Event::Rejoin);
                    return false;
                }
                Ok((400, body)) => {
                    // Refused for good (too large, or malformed): this copy now differs from
                    // everyone else's, so start again from the file.
                    let why = body.get("error").and_then(Json::as_str).unwrap_or("it was refused").to_string();
                    self.event(Event::Notice(format!("An edit couldn't be shared: {why}"), true));
                    self.event(Event::Rejoin);
                    return false;
                }
                // Offline or a hiccup: the server recognises a retried edit.
                _ => {
                    std::thread::sleep(Duration::from_secs(1));
                    return !link(&self.link).stop;
                }
            }
        }
    }

    /// Others' edits, who's here, and where the file was last saved. `Some(true)`: more to
    /// fetch at once; `None`: the session is over for this copy.
    fn sync(&mut self, nc: &Connection) -> Option<bool> {
        let selection = link(&self.link).selection.clone();
        let sel = if selection != self.sent_selection { selection.clone() } else { String::new() };
        let query = format!("collab/sync?sessionId={}&client={}&since={}&sel={}", self.session_id, self.client, self.last_seq, encode(&sel));
        match nc.app_status("GET", &query, &Json::Null) {
            Ok((200..=299, r)) => {
                if r.get("rejoin").and_then(Json::as_bool) == Some(true) {
                    self.event(Event::Rejoin);
                    return None;
                }
                if !sel.is_empty() {
                    self.sent_selection = selection;
                }
                let new = entries(r.get("entries"));
                let more = !new.is_empty();
                if let Some(last) = new.last() {
                    self.last_seq = last.seq;
                }
                let mut l = link(&self.link);
                l.entries.extend(new);
                l.members = r.get("members").cloned().and_then(|m| serde_json::from_value(m).ok());
                let seq = r.get("checkpointSeq").and_then(Json::as_u64).unwrap_or(0);
                if seq > l.checkpoint_seq {
                    l.checkpoint_seq = seq;
                }
                if let Some(etag) = r.get("checkpointEtag").and_then(Json::as_str).filter(|e| !e.is_empty() && *e != l.checkpoint_etag) {
                    l.checkpoint_etag = etag.to_string();
                    let etag = etag.to_string();
                    drop(l);
                    self.settle(&etag);
                }
                self.ctx.request_repaint();
                Some(more)
            }
            Ok((409, _)) => {
                self.event(Event::Rejoin);
                None
            }
            _ => Some(false),
        }
    }

    /// The local copy's record follows the session's saves: once sharing ends, opening the file
    /// again fetches Nextcloud's version rather than sending this copy.
    fn settle(&self, etag: &str) {
        let mut s = lock(&self.state);
        if let Some(key) = tie_key(&s.stored.files, &self.local)
            && let Some(t) = s.stored.files.get_mut(&key)
        {
            t.etag = Some(format!("\"{etag}\""));
            t.synced = mtime(std::path::Path::new(&key));
            persist(&mut s);
        }
    }

    /// Saves the shared document to the file, with its state snapshot. False when the session
    /// can't go on.
    fn checkpoint(&mut self, nc: &Connection, job: Job) -> bool {
        let before = link(&self.link).checkpoint_etag.clone();
        let saved = wordcraft_engine::io::save_bytes_with(&job.name, &job.doc, job.password.as_deref(), &job.opts)
            .and_then(|bytes| wordcraft_engine::collab::snapshot(&job.doc).map(|state| (bytes, state)).map_err(|e| e.to_string()));
        let (bytes, state) = match saved {
            Ok(b) => b,
            Err(e) => {
                self.event(Event::Notice(format!("Couldn't save the shared document: {e}"), true));
                return true;
            }
        };
        let headers = [("If-Match", format!("\"{before}\"")), ("Content-Type", "application/octet-stream".to_string())];
        let r = match nc.send("PUT", &nc.dav(&self.remote), &headers, bytes) {
            Ok(r) => r,
            Err(_) => return true,
        };
        if r.status == 412 {
            // Someone else in the session saved first (fine), or the file changed outside it.
            let _ = self.sync(nc);
            if link(&self.link).checkpoint_etag != before {
                return true;
            }
            self.event(Event::End(
                "The file was changed outside this editing session in Nextcloud. Open it again to see it; edits since the last save weren't written to it.".into(),
            ));
            return false;
        }
        if !(200..300).contains(&r.status) {
            return true;
        }
        let etag = r.etag.as_deref().unwrap_or("").trim_start_matches("W/").trim_matches('"').to_string();
        let body = json!({"sessionId": self.session_id, "client": self.client, "seq": job.seq, "etag": etag, "state": state});
        if nc.app_status("POST", "collab/checkpoint", &body).is_ok_and(|(s, _)| (200..300).contains(&s)) {
            let mut l = link(&self.link);
            l.checkpoint_seq = l.checkpoint_seq.max(job.seq);
            l.checkpoint_etag = etag.clone();
            drop(l);
            self.settle(&etag);
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entries_read_like_the_page_does() {
        let e = entries(Some(&json!([
            {"seq": 4, "client": 2, "client_seq": 1, "base": 3, "op": "{}"},
            {"seq": 5, "client": 3, "client_seq": 1, "base": 4, "op": "not json"},
            {"client": 3}
        ])));
        assert_eq!(e.len(), 2, "an entry without a position is dropped");
        assert_eq!(e[1].op, Op::default(), "an op that doesn't parse is a no-op");
    }

    #[test]
    fn members_read_like_the_server_lists_them() {
        let m: Vec<Member> =
            serde_json::from_value(json!([{"client": 2, "userId": "ana", "displayName": "", "selection": "{}", "canEdit": false}, {"client": 3, "userId": "bo", "displayName": "Bo Lee"}]))
                .unwrap();
        assert_eq!((m[0].name(), m[0].can_edit), ("ana", false));
        assert_eq!((m[1].name(), m[1].can_edit), ("Bo Lee", true));
    }
}
