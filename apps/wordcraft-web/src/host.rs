//! The `postMessage` bridge to a same-origin parent page (`?host=parent`), for pages that embed
//! WordCraft in an `<iframe>` (such as a Nextcloud app). Messages are plain objects with a `type`:
//!
//! - `wordcraft:ready` (to the parent): `{ version, ack }`, the app is listening; send a file now.
//! - `wordcraft:open` (from the parent): `{ name, bytes }`, bytes an `ArrayBuffer` or
//!   `Uint8Array`. It opens like a document picked with File › Open or dropped on the window.
//!   `name` must be a document WordCraft opens ([`acks::DOC_EXTS`]); without one it opens as
//!   `Document.docx`. Empty bytes open a new blank document that saves under that name.
//! - `wordcraft:save` (to the parent): `{ name, bytes }` (a transferred `ArrayBuffer`) for every
//!   file WordCraft would otherwise download: Save and Save As, and Export as PDF or picture.
//!   Save writes a Word document in its own format (.docx, .docm, .dotx, .dotm) and anything else
//!   (.odt, .rtf, .doc, .txt…) as .docx under the same name.
//! - `wordcraft:dirty` (to the parent): `{ dirty }` whenever unsaved changes appear or go away.
//! - `wordcraft:failed` (to the parent): `{ error }` when the app couldn't start.
//!
//! With `&ack=1` the parent confirms every save (`acks`): `wordcraft:save` also carries an `id`,
//! and the parent answers `wordcraft:saved { id, name? }` once it stored the file or
//! `wordcraft:saveFailed { id, error }` when it couldn't. Until then the save is in flight, as one
//! in the browser's save picker is: a Save Changes prompt's New or Open waits for it, and the
//! document is reported dirty. A failure leaves the document unsaved and is shown to the user.
//! `wordcraft:ready` says `{ ack: true }` when this is on, so a parent can tell a WordCraft that
//! ignores the flag. Without it, a document counts as saved as soon as it's handed over.
//!
//! A document saved through the page takes the name it was stored under (the page's `name` in
//! `wordcraft:saved` when it gave one), so the next Save goes to the same file.
//!
//! Co-editing (the page runs the session with the server; [`collab_msg`] checks what it sends):
//!
//! - `wordcraft:ready` says `{ collab: true }`: this build can co-edit.
//! - `wordcraft:collab:start` (from the parent): `{ client (or clientId), seq, readOnly?, user?,
//!   color?, state? }`, after `wordcraft:open`: the document just opened is the shared one as of
//!   log position `seq`. With `state` (a checkpoint's snapshot, `Session::collab_load_snapshot`)
//!   the snapshot is, and replaces the file just opened. `readOnly`: this person can follow but
//!   not edit.
//! - `wordcraft:collab:entries` (from the parent): `{ json }`, the server's log entries in order
//!   (JSON text of `collab::Incoming`s).
//! - `wordcraft:collab:op` (to the parent): `{ json }`, this person's next op for the server
//!   (`collab::Outgoing`). The next comes once this one is back in the log.
//! - `wordcraft:collab:checkpoint` (from the parent): `{ id }`; answered with
//!   `wordcraft:collab:checkpointData { id, seq, name, bytes, state }`: the shared document
//!   (without this person's unconfirmed edits) in the opened file's format, its log position, and
//!   its state snapshot (text) for people who join from that save.
//! - `wordcraft:collab:selection` (to the parent): `{ json }`, this person's position
//!   (`{anchor, focus}`, `Session::collab_presence`) whenever it changes;
//!   `wordcraft:collab:presence` (from the parent): `{ json }`, the others'
//!   (`[{anchor, focus, client, name, color}]`), drawn in the document.
//! - `wordcraft:collab:stop` (from the parent) ends sharing; `wordcraft:collab:error` (to the
//!   parent) `{ error }` says this copy can't follow the log and must rejoin.
//!
//! Co-editing messages are handled as they arrive, not at the next frame, so a hidden tab (which
//! draws none) keeps up with the others and answers checkpoint requests ([`Bridge::attach`]).
//!
//! Sharing (the Share dialog's cloud is the page, which has the user's Nextcloud session):
//!
//! - `wordcraft:cloud` (to the parent): `{ id, op, params }`, an operation of
//!   `wordcraft_ui_egui::share::Cloud` (`params` as JSON text).
//! - `wordcraft:cloud:reply` (from the parent): `{ id, ok, json?, error? }`, its answer (`json`
//!   as JSON text when `ok`, else `error`).
//!
//! Desktop (with `desktop=1` in the page's URL):
//!
//! - `wordcraft:openDesktop` (to the parent): Open in Desktop App was chosen; the page opens the
//!   document in WordLogic on this computer (a `wordlogic://open` link).
//!
//! Only the parent window on the page's own origin is heard or answered. In host mode the page
//! starts on a blank document instead of the Start screen or a sample, and the parent guards
//! leaving the page.
//!
//! Adding a message: a [`FromPage`] variant, its case in [`Bridge::parse`] and in
//! [`Bridge::handle`].

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::rc::{Rc, Weak};

use wasm_bindgen::JsCast as _;
use wasm_bindgen::JsValue;
use wasm_bindgen::closure::Closure;
use wordcraft_ui_egui::async_saves::AsyncSaves;
use wordcraft_ui_egui::share::{Cloud, CloudInbox};
use wordcraft_ui_egui::{Inbox, Presence, Services, WordApp};

use crate::acks::{self, Reply, SaveAcks};
use crate::collab_msg::{self, Fields};

/// The longest user name taken from `?author=` (the engine caps it again).
const MAX_AUTHOR_CHARS: usize = 200;
/// The most save answers held between two frames; more are dropped.
const MAX_REPLIES: usize = 256;
/// The most open requests held between two frames; more are dropped.
const MAX_OPENS: usize = 4;
/// The largest document the page can send (as a picture pasted or dropped).
const MAX_OPEN_BYTES: u32 = 200 << 20;
/// The longest strings read from a message, in UTF-16 units: longer ones are ignored unread.
const MAX_TYPE_UNITS: u32 = 64;
const MAX_NAME_UNITS: u32 = 4096;
const MAX_ERROR_UNITS: u32 = 64 * 1024;
/// Log batches, state snapshots and presence lists (JSON text).
const MAX_JSON_UNITS: u32 = collab_msg::MAX_JSON_BYTES as u32;
/// A person's name and colour.
const MAX_SHORT_UNITS: u32 = 1024;
/// The most co-editing messages held for the app; past this the page has to rejoin.
const MAX_COLLAB_QUEUE: usize = 4096;
/// The largest answer to a sharing request taken from the page (JSON text).
const MAX_CLOUD_REPLY_UNITS: u32 = 4 << 20;
/// The most sharing answers held between two frames.
const MAX_CLOUD_REPLIES: usize = 256;

/// A message from the page, checked.
enum FromPage {
    /// `wordcraft:open`: a document to open (its name checked by [`acks::open_name`]).
    Open { name: String, bytes: Vec<u8> },
    /// `wordcraft:saved` / `wordcraft:saveFailed` (`&ack=1` only).
    Reply(Reply),
    /// `wordcraft:collab:*`.
    Collab(collab_msg::FromPage),
    /// `wordcraft:cloud:reply`: the answer to the Share dialog's request `id`.
    CloudReply { id: u64, answer: Result<serde_json::Value, String> },
}

#[derive(Clone)]
pub struct Bridge {
    window: web_sys::Window,
    parent: web_sys::Window,
    origin: String,
    /// The app's saves that finish later; with `&ack=1` each save to the parent is one.
    saves: AsyncSaves,
    /// Without `&ack=1`: names of the files handed to the parent since the last frame.
    saved: Rc<RefCell<Vec<String>>>,
    /// The unsaved-changes state last reported to the parent.
    reported: Rc<Cell<Option<bool>>>,
    /// `&ack=1`: saves the parent hasn't answered yet; `None` when the parent doesn't answer.
    acks: Option<Rc<RefCell<SaveAcks>>>,
    /// The parent's answers since the last frame.
    replies: Rc<RefCell<Vec<Reply>>>,
    /// Co-editing messages from the parent not handled yet, in order.
    collab: Rc<RefCell<VecDeque<collab_msg::FromPage>>>,
    /// This person's position as last told to the parent (JSON).
    position: Rc<RefCell<String>>,
    /// The app, for handling co-editing messages as they arrive ([`Bridge::attach`]), and the
    /// files the page sent that it hasn't opened yet.
    app: Rc<RefCell<Option<(Weak<RefCell<WordApp>>, Inbox)>>>,
    /// The page's answers to the Share dialog's requests.
    cloud: CloudInbox,
    /// Asks for another frame when co-editing changes what's shown.
    ctx: Rc<RefCell<Option<egui::Context>>>,
}

impl Bridge {
    /// `None` when the page isn't framed (there is no parent to talk to). `ack`: the parent
    /// confirms saves (`&ack=1`); `saves`: the app's saves that finish later.
    pub fn new(ack: bool, saves: AsyncSaves) -> Option<Self> {
        let window = web_sys::window()?;
        let parent = window.parent().ok().flatten()?;
        if js_sys::Object::is(&parent, &window) {
            return None;
        }
        let origin = window.location().origin().ok()?;
        Some(Self {
            window,
            parent,
            origin,
            saves,
            saved: Rc::default(),
            reported: Rc::default(),
            acks: ack.then(Rc::default),
            replies: Rc::default(),
            collab: Rc::default(),
            position: Rc::default(),
            app: Rc::default(),
            cloud: CloudInbox::default(),
            ctx: Rc::default(),
        })
    }

    /// Send saved and exported files to the parent instead of downloading them, and hear files
    /// from it. The page is also the Share dialog's cloud: it has the user's Nextcloud session.
    pub fn connect(&self, services: &mut Services, inbox: Inbox, ctx: egui::Context) {
        *self.ctx.borrow_mut() = Some(ctx.clone());
        self.listen(inbox, ctx);
        let bridge = self.clone();
        services.download = Some(Box::new(move |name: &str, bytes: &[u8]| bridge.post_save(name, bytes)));
        let bridge = self.clone();
        services.cloud = Some(Cloud::sharing(
            Box::new(move |id, op, params| {
                let params = params.to_string();
                // Ids stay far below 2^53, so a JavaScript number holds them exactly.
                bridge.send(&[
                    ("type", "wordcraft:cloud".into()),
                    ("id", JsValue::from_f64(id as f64)),
                    ("op", op.into()),
                    ("params", params.as_str().into()),
                ]);
            }),
            self.cloud.clone(),
        ));
    }

    /// `desktop=1`: the page can open the document in the desktop app
    /// (`wordcraft:openDesktop`, to the page): the ribbon offers Open in Desktop App.
    pub fn offer_desktop(&self, services: &mut Services) {
        let bridge = self.clone();
        services.open_in_desktop = Some(Box::new(move || {
            bridge.send(&[("type", "wordcraft:openDesktop".into())]);
        }));
    }

    /// Handle co-editing messages as they arrive rather than at the next frame, so a hidden tab
    /// (which draws none) keeps up with the others and answers checkpoint requests.
    pub fn attach(&self, app: &Rc<RefCell<WordApp>>, inbox: Inbox) {
        *self.app.borrow_mut() = Some((Rc::downgrade(app), inbox));
    }

    /// Co-editing messages now, if the app is free and has opened every file the page sent (a
    /// session starts on the file that came just before it).
    fn sync_collab_now(&self) {
        let Some((app, inbox)) = self.app.borrow().clone() else { return };
        let opened = inbox.lock().map(|q| q.is_empty()).unwrap_or(false);
        if let (true, Some(app)) = (opened, app.upgrade())
            && let Ok(mut app) = app.try_borrow_mut()
        {
            self.sync_collab(&mut app);
        }
    }

    /// `?author=`: the host's name for the user signs new comments and tracked changes.
    pub fn set_author(app: &mut WordApp, author: &str) {
        let author: String = author.trim().chars().take(MAX_AUTHOR_CHARS).collect();
        if !author.is_empty()
            && let Err(e) = app.session.run("file.setAuthor", &serde_json::json!({"name": author}))
        {
            log::warn!("?author= not used: {e}");
        }
    }

    pub fn post_ready(&self) {
        self.send(&[
            ("type", "wordcraft:ready".into()),
            ("version", env!("CARGO_PKG_VERSION").into()),
            ("ack", self.acks.is_some().into()),
            ("collab", true.into()),
        ]);
    }

    pub fn post_failed(&self, error: &str) {
        self.send(&[("type", "wordcraft:failed".into()), ("error", error.into())]);
    }

    /// After each frame: answers to saves settle them (with `&ack=1`), a document stored through
    /// the page takes the name it was stored under, and the parent hears when unsaved changes
    /// appear or go away.
    pub fn sync(&self, app: &mut WordApp, ctx: &egui::Context) {
        let current = app.session.document_id();
        let mut unconfirmed = false;
        // Saves that ended this frame: the app reads how at the start of the next one, and a
        // failed one only then marks the document unsaved again.
        let mut settling = false;
        if let Some(acks) = &self.acks {
            let mut acks = acks.borrow_mut();
            acks.bind(current);
            let replies = std::mem::take(&mut *self.replies.borrow_mut());
            for reply in replies {
                if let Some((doc, name)) = acks.apply(reply)
                    && doc == current
                {
                    take_name(app, name);
                }
            }
            let finished = acks.take_finished();
            settling = !finished.is_empty();
            for (ticket, outcome) in finished {
                self.saves.finish(ticket, outcome);
            }
            if settling {
                ctx.request_repaint();
            }
            unconfirmed = acks.unconfirmed(current);
        } else {
            let saved = std::mem::take(&mut *self.saved.borrow_mut());
            if let Some(name) = saved.into_iter().rev().find(|name| wordcraft_ui_egui::keeps_everything(name)) {
                take_name(app, name);
            }
        }
        self.sync_collab(app);
        if settling {
            return;
        }
        let dirty = app.session.dirty || unconfirmed;
        if self.reported.get() != Some(dirty) && self.send(&[("type", "wordcraft:dirty".into()), ("dirty", dirty.into())]) {
            self.reported.set(Some(dirty));
        }
    }

    /// Co-editing: the page's messages in order, then this person's position and next op.
    fn sync_collab(&self, app: &mut WordApp) {
        let queued: Vec<collab_msg::FromPage> = self.collab.borrow_mut().drain(..).collect();
        let mut changed = false;
        for msg in queued {
            match msg {
                collab_msg::FromPage::Start { client, seq, read_only, user, color: _, state } => {
                    // A checkpoint's snapshot is the shared document exactly; the file read back
                    // wouldn't number its parts, comments and lists the same way.
                    let loaded = match &state {
                        Some(snapshot) => app.session.collab_load_snapshot(snapshot),
                        None => Ok(()),
                    };
                    match loaded.and_then(|()| app.session.collab_start(client, seq)) {
                        Ok(()) => {
                            app.session.collab_set_read_only(read_only);
                            self.position.borrow_mut().clear();
                            log::info!("co-editing as client {client} ({}) from log position {seq}", user.as_deref().unwrap_or("?"));
                        }
                        Err(e) => self.post_collab_error(&e.to_string()),
                    }
                    changed = true;
                }
                collab_msg::FromPage::Entries(entries) => {
                    for entry in entries {
                        if let Err(e) = app.session.collab_receive(entry) {
                            app.session.collab_stop();
                            app.presence.clear();
                            self.post_collab_error(&e.to_string());
                            break;
                        }
                        changed = true;
                    }
                }
                collab_msg::FromPage::Checkpoint { id } => self.post_checkpoint(app, id),
                collab_msg::FromPage::Stop => {
                    app.session.collab_stop();
                    app.presence.clear();
                    changed = true;
                }
                collab_msg::FromPage::Presence(people) => {
                    let people = people
                        .into_iter()
                        .map(|s| {
                            let [r, g, b] = s.color;
                            (Presence { client: s.client, name: s.name, color: egui::Color32::from_rgb(r, g, b) }, s.anchor, s.focus)
                        })
                        .collect();
                    app.set_presence(people);
                    changed = true;
                }
            }
        }
        self.post_position(app);
        if let Some(out) = app.session.collab_outgoing() {
            match serde_json::to_string(&out) {
                Ok(json) => {
                    self.send(&[("type", "wordcraft:collab:op".into()), ("json", json.into())]);
                }
                Err(e) => self.post_collab_error(&e.to_string()),
            }
        }
        if changed && let Some(ctx) = self.ctx.borrow().as_ref() {
            ctx.request_repaint();
        }
    }

    /// Tells the parent where this person is (caret and selection) when it has changed.
    fn post_position(&self, app: &WordApp) {
        if !app.session.collab_active() {
            return;
        }
        let json = app.session.collab_presence().to_string();
        if *self.position.borrow() != json && self.send(&[("type", "wordcraft:collab:selection".into()), ("json", json.as_str().into())]) {
            *self.position.borrow_mut() = json;
        }
    }

    /// The shared document as everyone has it, for the page to save as the session's checkpoint:
    /// in the opened file's format, with its state snapshot.
    fn post_checkpoint(&self, app: &mut WordApp, id: u64) {
        let name = checkpoint_name(app);
        let data = app.session.collab_snapshot().and_then(|snapshot| {
            let (_, doc) = app.session.collab_confirmed()?;
            Some(snapshot.map_err(|e| e.to_string()).and_then(|(seq, state)| Ok((seq, state, wordcraft_engine::io::save_bytes(&name, doc)?))))
        });
        let (seq, state, bytes) = match data {
            Some(Ok(data)) => data,
            Some(Err(e)) => {
                // Not a reason to rejoin: the page's save fails and is tried again later.
                log::error!("co-editing: couldn't write the shared document as {name}: {e}");
                app.status(format!("Couldn't save the shared document: {e}"));
                return;
            }
            None => {
                self.post_collab_error("no shared document to save");
                return;
            }
        };
        let buffer = js_sys::Uint8Array::from(bytes.as_slice()).buffer();
        let fields = [
            ("type", "wordcraft:collab:checkpointData".into()),
            ("id", JsValue::from_f64(id as f64)),
            ("seq", JsValue::from_f64(seq as f64)),
            ("name", name.as_str().into()),
            ("state", state.as_str().into()),
            ("bytes", buffer.clone().into()),
        ];
        if let Some(m) = message(&fields)
            && self.parent.post_message_with_transfer(&m, &self.origin, &js_sys::Array::of1(&buffer)).is_err()
        {
            log::error!("couldn't hand the checkpoint to the page");
        }
    }

    fn post_collab_error(&self, error: &str) {
        log::error!("co-editing: {error}");
        self.send(&[("type", "wordcraft:collab:error".into()), ("error", error.into())]);
    }

    /// The app's `download`: hand the file to the parent. An error means it didn't get there (the
    /// app says so and the document stays unsaved).
    fn post_save(&self, path: &str, bytes: &[u8]) -> Result<(), String> {
        let name = acks::clean_name(path).unwrap_or_else(|| acks::DEFAULT_NAME.to_string());
        // The document itself, not a copy (PDF, picture, plain text): what Save marks saved.
        let document = wordcraft_ui_egui::keeps_everything(&name);
        let buffer = js_sys::Uint8Array::from(bytes).buffer();
        let id = self.acks.as_ref().map(|acks| acks.borrow_mut().begin(&name, document));
        let mut fields = vec![("type", "wordcraft:save".into()), ("name", name.as_str().into()), ("bytes", buffer.clone().into())];
        if let Some(id) = id {
            // Ids stay far below 2^53, so a JavaScript number holds them exactly.
            fields.push(("id", JsValue::from_f64(id as f64)));
        }
        let sent = message(&fields).ok_or_else(|| "couldn't build the message".to_string()).and_then(|message| {
            self.parent
                .post_message_with_transfer(&message, &self.origin, &js_sys::Array::of1(&buffer))
                .map_err(|e| e.as_string().unwrap_or_else(|| format!("{e:?}")))
        });
        match (sent, id, &self.acks) {
            (Err(e), Some(id), Some(acks)) => {
                acks.borrow_mut().forget(id);
                Err(e)
            }
            (Err(e), ..) => Err(e),
            // The app waits for the parent's answer on this ticket.
            (Ok(()), Some(id), Some(acks)) => {
                acks.borrow_mut().started(id, self.saves.start());
                Ok(())
            }
            (Ok(()), ..) => {
                if document {
                    self.saved.borrow_mut().push(name);
                }
                Ok(())
            }
        }
    }

    fn listen(&self, inbox: Inbox, ctx: egui::Context) {
        let bridge = self.clone();
        let on_message = Closure::<dyn FnMut(web_sys::MessageEvent)>::new(move |event: web_sys::MessageEvent| {
            if event.origin() != bridge.origin || !event.source().is_some_and(|s| js_sys::Object::is(&s, &bridge.parent)) {
                return;
            }
            if let Some(msg) = bridge.parse(&event.data()) {
                bridge.handle(msg, &inbox);
                ctx.request_repaint();
            }
        });
        if self.window.add_event_listener_with_callback("message", on_message.as_ref().unchecked_ref()).is_ok() {
            // The listener lives as long as the page.
            on_message.forget();
        }
    }

    /// The page's message, checked; `None` for anything this bridge doesn't take.
    fn parse(&self, data: &JsValue) -> Option<FromPage> {
        let kind = string(data, "type", MAX_TYPE_UNITS)?;
        match kind.as_str() {
            "wordcraft:open" => {
                let given = string(data, "name", MAX_NAME_UNITS);
                // A name that is there but isn't a short string (not one left out) is refused.
                if given.is_none() && present(data, "name") {
                    log::warn!("wordcraft:open: `name` must be a string of at most {MAX_NAME_UNITS} characters");
                    return None;
                }
                let Some(name) = acks::open_name(given.as_deref()) else {
                    log::warn!("wordcraft:open: {given:?} isn't a document WordCraft opens");
                    return None;
                };
                let bytes = bytes(data, MAX_OPEN_BYTES)?;
                Some(FromPage::Open { name, bytes })
            }
            "wordcraft:saved" | "wordcraft:saveFailed" if self.acks.is_some() => {
                Reply::parse(&kind, number(data, "id"), string(data, "name", MAX_NAME_UNITS), string(data, "error", MAX_ERROR_UNITS))
                    .map(FromPage::Reply)
            }
            "wordcraft:cloud:reply" => {
                let id = number(data, "id").filter(|v| v.is_finite() && *v >= 0.0 && v.fract() == 0.0 && *v <= 9_007_199_254_740_991.0)?;
                let ok = js_sys::Reflect::get(data, &JsValue::from_str("ok")).ok().and_then(|v| v.as_bool()).unwrap_or(false);
                let answer = if ok {
                    match string(data, "json", MAX_CLOUD_REPLY_UNITS) {
                        Some(text) => serde_json::from_str(&text).map_err(|e| e.to_string()),
                        None if present(data, "json") => Err("the answer was too large".to_string()),
                        None => Ok(serde_json::Value::Null),
                    }
                } else {
                    Err(string(data, "error", MAX_ERROR_UNITS).unwrap_or_else(|| "Nextcloud didn't answer".into()).chars().take(500).collect())
                };
                Some(FromPage::CloudReply { id: id as u64, answer })
            }
            _ if kind.starts_with("wordcraft:collab:") => {
                let flag = |key: &str| js_sys::Reflect::get(data, &JsValue::from_str(key)).ok().and_then(|v| v.as_bool());
                let fields = Fields {
                    client: number(data, "client"),
                    client_id: number(data, "clientId"),
                    seq: number(data, "seq"),
                    id: number(data, "id"),
                    read_only: flag("readOnly"),
                    json: string(data, "json", MAX_JSON_UNITS),
                    user: string(data, "user", MAX_SHORT_UNITS),
                    color: string(data, "color", MAX_SHORT_UNITS),
                    state: string(data, "state", MAX_JSON_UNITS),
                };
                if present(data, "state") && fields.state.is_none() {
                    self.post_collab_error(&format!("{kind}: `state` must be text of at most {MAX_JSON_UNITS} characters"));
                    return None;
                }
                match collab_msg::parse(&kind, fields)? {
                    Ok(msg) => Some(FromPage::Collab(msg)),
                    Err(e) => {
                        self.post_collab_error(&e);
                        None
                    }
                }
            }
            _ => None,
        }
    }

    fn handle(&self, msg: FromPage, inbox: &Inbox) {
        match msg {
            FromPage::Open { name, bytes } => {
                let mut queue = inbox.lock().unwrap_or_else(|e| e.into_inner());
                if queue.len() < MAX_OPENS {
                    queue.push((name, bytes));
                } else {
                    log::warn!("wordcraft:open: too many files at once; {name} dropped");
                }
            }
            FromPage::Reply(reply) => {
                let mut replies = self.replies.borrow_mut();
                if replies.len() < MAX_REPLIES {
                    replies.push(reply);
                }
            }
            FromPage::CloudReply { id, answer } => {
                let mut answers = self.cloud.lock().unwrap_or_else(|e| e.into_inner());
                if answers.len() < MAX_CLOUD_REPLIES {
                    answers.push((id, answer));
                }
            }
            FromPage::Collab(msg) => {
                let full = {
                    let mut queue = self.collab.borrow_mut();
                    let full = queue.len() >= MAX_COLLAB_QUEUE;
                    if !full {
                        queue.push_back(msg);
                    }
                    full
                };
                if full {
                    self.post_collab_error("too many messages waiting; rejoin");
                }
                self.sync_collab_now();
            }
        }
    }

    /// Post a message to the parent; false if it couldn't be sent.
    fn send(&self, fields: &[(&str, JsValue)]) -> bool {
        message(fields).is_some_and(|m| self.parent.post_message(&m, &self.origin).is_ok())
    }
}

/// The name of the file the shared document is saved to (the one the page opened), which also
/// gives the format.
fn checkpoint_name(app: &WordApp) -> String {
    app.session
        .path
        .as_ref()
        .and_then(|p| p.to_str())
        .and_then(acks::clean_name)
        .or_else(|| app.session.file_name.as_deref().and_then(acks::clean_name))
        .unwrap_or_else(|| acks::DEFAULT_NAME.to_string())
}

/// The document is now the file `name` in the page's storage: Save goes there next.
fn take_name(app: &mut WordApp, name: String) {
    app.session.path = Some(name.into());
}

fn message(fields: &[(&str, JsValue)]) -> Option<js_sys::Object> {
    let m = js_sys::Object::new();
    for (key, value) in fields {
        js_sys::Reflect::set(&m, &JsValue::from_str(key), value).ok()?;
    }
    Some(m)
}

/// The string field `key`, if it is one of at most `max_units` UTF-16 units.
fn string(data: &JsValue, key: &str, max_units: u32) -> Option<String> {
    let value = js_sys::Reflect::get(data, &JsValue::from_str(key)).ok()?;
    let text = value.dyn_ref::<js_sys::JsString>()?;
    if text.length() > max_units {
        return None;
    }
    text.as_string()
}

/// Whether the field `key` is there (neither missing, `undefined` nor `null`).
fn present(data: &JsValue, key: &str) -> bool {
    js_sys::Reflect::get(data, &JsValue::from_str(key)).is_ok_and(|v| !v.is_undefined() && !v.is_null())
}

fn number(data: &JsValue, key: &str) -> Option<f64> {
    js_sys::Reflect::get(data, &JsValue::from_str(key)).ok()?.as_f64()
}

/// `bytes` as an `ArrayBuffer` or `Uint8Array` of at most `max` bytes.
fn bytes(data: &JsValue, max: u32) -> Option<Vec<u8>> {
    let value = js_sys::Reflect::get(data, &JsValue::from_str("bytes")).ok()?;
    let array = if let Some(array) = value.dyn_ref::<js_sys::Uint8Array>() {
        array.clone()
    } else if value.is_instance_of::<js_sys::ArrayBuffer>() {
        js_sys::Uint8Array::new(&value)
    } else {
        log::warn!("wordcraft:open: `bytes` must be an ArrayBuffer or a Uint8Array");
        return None;
    };
    if array.length() > max {
        log::warn!("wordcraft:open: the document is larger than {} MB; not opened", max >> 20);
        return None;
    }
    Some(array.to_vec())
}
