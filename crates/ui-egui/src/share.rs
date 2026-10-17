//! The Share dialog: share the document with people, or by link, through the host's cloud
//! (Nextcloud), as Word's Share does through OneDrive. The host does the talking ([`Cloud`]);
//! this module is only the dialog, the same in the browser and on the desktop.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use egui::{RichText, vec2};
use serde::Deserialize;
use serde_json::{Value as Json, json};

use crate::WordApp;
use crate::theme::{Tokens, regular};

/// Answers from the cloud: (request id, result).
pub type CloudInbox = Arc<Mutex<Vec<(u64, Result<Json, String>)>>>;

/// The host's cloud, for sharing. `request(id, op, params)` starts an operation and returns at
/// once; its answer arrives in `inbox` under the same id (the host asks for a repaint). All
/// operations are about the document that's open:
///
/// - `share.info` `{}` → `{name, signedIn, inCloud, canShare, account?, server?, link?}`; `link`
///   when the document was opened from a shared link (its address), by someone who can't share it
/// - `share.search` `{query}` → `[{label, detail?, type, shareWith}]`, `type` one of `user`,
///   `group`, `email`, `remote`
/// - `share.list` `{}` → `[{id, type, shareWith, label, edit, url?, password, expiration?}]`,
///   `type` also `link`
/// - `share.add` `{type, shareWith, edit, note}` → a share
/// - `share.update` `{id, edit?, password?, expiration?}` → the share
/// - `share.remove` `{id}` → `{}`
/// - `share.link` `{edit, password?, expiration?}` → the link share (with `url`), made or updated
/// - `share.upload` `{}` → info: puts a document that isn't in the cloud yet there (desktop)
/// - `account.signIn` `{server}` → info: signs in to the cloud at that address (desktop)
///
/// With `files` (the desktop), the File view's operations too ([`crate::cloud_files`]):
///
/// - `account.info` `{}` → `{signedIn, account?, server?}`; `account.signOut` `{}` → the same
/// - `files.list` `{path}` → `{path, items: [{name, path, folder, size}]}`
/// - `files.open` `{path, openPaths}` → `{localPath, alreadyOpen?}`: a copy here, tied to the
///   cloud file (open it like any file; saving it saves to the cloud)
/// - `files.saveAs` `{folder, name, replace}` → `{localPath, name}` (save the document there; it
///   goes to the cloud), or `{exists: true, name}`
pub struct Cloud {
    pub request: Box<dyn Fn(u64, &str, Json)>,
    pub inbox: CloudInbox,
    /// The File view can open and save cloud files (account, Open, Save As).
    pub files: bool,
    /// The cloud file a local file is the copy of (`folder/name.docx`), if it is one: such
    /// documents AutoSave by default.
    pub location: Option<Box<dyn Fn(&str) -> Option<String>>>,
}

impl Cloud {
    /// A cloud for sharing only (the browser's: the page owns the files).
    pub fn sharing(request: Box<dyn Fn(u64, &str, Json)>, inbox: CloudInbox) -> Cloud {
        Cloud { request, inbox, files: false, location: None }
    }

    /// The cloud file `local` is the copy of, if any.
    pub fn location_of(&self, local: &str) -> Option<String> {
        self.location.as_ref().and_then(|f| f(local))
    }
}

/// A request id no other cloud request in this app uses (answers share one inbox).
pub fn next_request_id() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct Info {
    name: String,
    signed_in: bool,
    in_cloud: bool,
    can_share: bool,
    account: Option<String>,
    /// The server last signed in to, to sign in again.
    server: Option<String>,
    /// Opened from this shared link (a guest: they can pass the link on, nothing more).
    link: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Sharee {
    label: String,
    #[serde(default)]
    detail: Option<String>,
    #[serde(rename = "type")]
    kind: String,
    share_with: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Share {
    id: Json,
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    label: String,
    #[serde(default)]
    edit: bool,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    password: bool,
    #[serde(default)]
    expiration: Option<String>,
}

/// What a request in flight was for.
#[derive(Clone, Debug)]
enum Asked {
    Info,
    Search(String),
    List,
    Add,
    Update,
    Remove,
    Link { copy: bool },
    Upload,
    SignIn,
}

/// Waiting for the user to stop typing before searching.
const SEARCH_DELAY: f64 = 0.3;

pub struct ShareDialog {
    info: Option<Info>,
    /// The Nextcloud address typed to sign in.
    server: String,
    query: String,
    query_changed_at: f64,
    searched: String,
    suggestions: Vec<Sharee>,
    recipients: Vec<Sharee>,
    edit: bool,
    message: String,
    shares: Vec<Share>,
    manage: bool,
    link_settings: bool,
    link_edit: bool,
    link_password: String,
    link_expiry: String,
    status: Option<(String, bool)>,
    asked: HashMap<u64, Asked>,
    adding: usize,
}

impl ShareDialog {
    /// Opens the dialog and asks the cloud about the document.
    pub fn open(app: &WordApp) -> Option<ShareDialog> {
        app.services.cloud.as_ref()?;
        let mut d = ShareDialog {
            info: None,
            server: String::new(),
            query: String::new(),
            query_changed_at: 0.0,
            searched: String::new(),
            suggestions: Vec::new(),
            recipients: Vec::new(),
            edit: true,
            message: String::new(),
            shares: Vec::new(),
            manage: false,
            link_settings: false,
            // A new link lets people edit, as OneDrive's "anyone with the link" does; the list
            // replaces this with what an existing link really allows.
            link_edit: true,
            link_password: String::new(),
            link_expiry: String::new(),
            status: None,
            asked: HashMap::new(),
            adding: 0,
        };
        d.ask(app, "share.info", json!({}), Asked::Info);
        Some(d)
    }

    fn ask(&mut self, app: &WordApp, op: &str, params: Json, what: Asked) {
        let Some(cloud) = &app.services.cloud else { return };
        let id = next_request_id();
        self.asked.insert(id, what);
        (cloud.request)(id, op, params);
    }

    fn busy(&self) -> bool {
        self.asked.values().any(|a| !matches!(a, Asked::Search(_)))
    }

    /// Takes the cloud's answers to this dialog's requests.
    fn take_answers(&mut self, app: &WordApp, ctx: &egui::Context) {
        let Some(cloud) = &app.services.cloud else { return };
        let answers: Vec<(u64, Result<Json, String>)> = {
            let mut inbox = cloud.inbox.lock().unwrap_or_else(|e| e.into_inner());
            let (mine, rest): (Vec<_>, Vec<_>) = inbox.drain(..).partition(|(id, _)| self.asked.contains_key(id));
            *inbox = rest;
            mine
        };
        for (id, answer) in answers {
            let Some(what) = self.asked.remove(&id) else { continue };
            let answer = match answer {
                Ok(a) => a,
                Err(e) => {
                    if !matches!(what, Asked::Search(_)) {
                        self.status = Some((e, true));
                    }
                    if matches!(what, Asked::Add) {
                        self.adding = self.adding.saturating_sub(1);
                    }
                    continue;
                }
            };
            match what {
                Asked::Info | Asked::Upload | Asked::SignIn => {
                    let info: Info = serde_json::from_value(answer).unwrap_or_default();
                    let ready = info.signed_in && info.in_cloud && info.link.is_none();
                    if !matches!(what, Asked::Info) {
                        // "Uploading…" or "Finish signing in…" is done.
                        self.status = None;
                    }
                    if self.server.is_empty() {
                        self.server = info.server.clone().unwrap_or_default();
                    }
                    self.info = Some(info);
                    if ready {
                        self.ask(app, "share.list", json!({}), Asked::List);
                    }
                }
                Asked::Search(q) => {
                    if q == self.query.trim() {
                        let found: Vec<Sharee> = serde_json::from_value(answer).unwrap_or_default();
                        self.suggestions = found.into_iter().filter(|s| !self.recipients.contains(s)).take(8).collect();
                    }
                }
                Asked::List => {
                    self.shares = serde_json::from_value(answer).unwrap_or_default();
                    if let Some(link) = self.shares.iter().find(|s| s.kind == "link") {
                        self.link_edit = link.edit;
                        self.link_expiry = link.expiration.clone().unwrap_or_default();
                    }
                }
                Asked::Add => {
                    self.adding = self.adding.saturating_sub(1);
                    if self.adding == 0 {
                        self.status = Some((tl!("Shared. They'll get a notification.").into(), false));
                        self.recipients.clear();
                        self.message.clear();
                        self.ask(app, "share.list", json!({}), Asked::List);
                    }
                }
                Asked::Update | Asked::Remove => {
                    if matches!(what, Asked::Remove) {
                        self.status = Some((tl!("Access removed.").into(), false));
                    }
                    self.ask(app, "share.list", json!({}), Asked::List)
                }
                Asked::Link { copy } => {
                    if copy && let Some(url) = answer.get("url").and_then(Json::as_str) {
                        ctx.copy_text(url.to_string());
                        let edit = answer.get("edit").and_then(Json::as_bool).unwrap_or(self.link_edit);
                        let said = if edit { tl!("Link copied. Anyone with it can edit.") } else { tl!("Link copied. Anyone with it can view.") };
                        self.status = Some((said.into(), false));
                    } else {
                        self.status = Some((tl!("Link settings saved.").into(), false));
                    }
                    self.link_password.clear();
                    self.ask(app, "share.list", json!({}), Asked::List);
                }
            }
        }
    }
}

/// Draws the Share dialog, if open.
pub fn show(app: &mut WordApp, ctx: &egui::Context) {
    let Some(mut d) = app.share.take() else { return };
    d.take_answers(app, ctx);
    let t = Tokens::get(ctx);
    let title = match &d.info {
        Some(i) if !i.name.is_empty() => format!("{} \u{201c}{}\u{201d}", tl!("Share"), i.name),
        _ => tl!("Share").to_string(),
    };
    let mut open = true;
    // 500 points wide, or the window's width in a narrow one (a phone, a side-by-side browser).
    let width = (ctx.content_rect().width() - 32.0).clamp(260.0, 500.0);
    egui::Window::new(title)
        .id(egui::Id::new("share-dialog"))
        .collapsible(false)
        .resizable(false)
        .default_width(width)
        .max_width(width)
        .constrain(true)
        .anchor(egui::Align2::CENTER_CENTER, vec2(0.0, -40.0))
        .open(&mut open)
        .show(ctx, |ui| {
            ui.set_min_width(width);
            ui.set_max_width(width);
            body(app, ui, &mut d, &t);
        });
    // Searching once typing pauses.
    let now = ctx.input(|i| i.time);
    let q = d.query.trim().to_string();
    if q != d.searched && now - d.query_changed_at >= SEARCH_DELAY {
        d.searched = q.clone();
        if q.is_empty() {
            d.suggestions.clear();
        } else {
            d.ask(app, "share.search", json!({"query": q}), Asked::Search(q));
        }
    } else if q != d.searched {
        ctx.request_repaint_after(std::time::Duration::from_millis(100));
    }
    if d.busy() {
        ctx.request_repaint_after(std::time::Duration::from_millis(200));
    }
    if open && !ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
        app.share = Some(d);
    }
}

fn body(app: &mut WordApp, ui: &mut egui::Ui, d: &mut ShareDialog, t: &Tokens) {
    let Some(info) = d.info.clone() else {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label(tl!("Checking the document…"));
        });
        status(ui, d, t);
        return;
    };
    if !info.signed_in {
        ui.label(tl!("Sign in to Nextcloud to share this document with people or by link."));
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            ui.label(tl!("Server"));
            labelled(
                ui.add(
                    egui::TextEdit::singleline(&mut d.server)
                        .hint_text("cloud.example.com")
                        .desired_width((ui.available_width() - 8.0).clamp(120.0, 260.0)),
                ),
                tl!("Nextcloud server address"),
            );
        });
        ui.add_space(4.0);
        let ready = !d.busy() && !d.server.trim().is_empty();
        if ui.add_enabled(ready, egui::Button::new(tl!("Sign in to Nextcloud…"))).clicked() {
            d.status = Some((tl!("Finish signing in in your browser…").into(), false));
            let server = d.server.trim().to_string();
            d.ask(app, "account.signIn", json!({"server": server}), Asked::SignIn);
        }
        status(ui, d, t);
        return;
    }
    if !info.in_cloud {
        let account = info.account.as_deref().map(|a| format!(" ({a})")).unwrap_or_default();
        ui.label(format!(
            "{}{account}.",
            tl!("This document isn't in Nextcloud yet. Upload it to share it: people you share with open the copy in Nextcloud")
        ));
        ui.add_space(8.0);
        if ui.add_enabled(!d.busy(), egui::Button::new(tl!("Upload to Nextcloud"))).clicked() {
            d.status = Some((tl!("Uploading…").into(), false));
            d.ask(app, "share.upload", json!({}), Asked::Upload);
        }
        status(ui, d, t);
        return;
    }
    if let Some(link) = info.link.as_ref().filter(|_| !info.can_share) {
        ui.label(tl!("You opened this document from a shared link. Anyone you give the link to can open it as you did."));
        ui.add_space(8.0);
        if crate::widgets::icon_text_button(ui, "link", tl!("Copy link"), regular(12.0)).on_hover_text(link.as_str()).clicked() {
            ui.ctx().copy_text(link.clone());
            d.status = Some((tl!("Link copied.").into(), false));
        }
        status(ui, d, t);
        return;
    }
    if !info.can_share {
        ui.label(tl!("You can't share this document: it was shared with you without permission to share it on."));
        people_with_access(app, ui, d, t, false);
        return;
    }

    // Who to share with: chips for the people picked, then the search box.
    egui::Frame::new().stroke(egui::Stroke::new(1.0, t.input_border)).corner_radius(4.0).inner_margin(6.0).show(ui, |ui| {
        ui.horizontal_wrapped(|ui| {
            let mut remove = None;
            for (i, r) in d.recipients.iter().enumerate() {
                // ×, not ✕: every font has it, the web build's included.
                let chip = ui.add(egui::Button::new(format!("{}  ×", r.label)).fill(t.checked).corner_radius(10.0)).on_hover_text(format!(
                    "{} {}",
                    tl!("Remove"),
                    r.label
                ));
                if chip.clicked() {
                    remove = Some(i);
                }
            }
            if let Some(i) = remove {
                d.recipients.remove(i);
            }
            let edit = labelled(
                ui.add(
                    egui::TextEdit::singleline(&mut d.query)
                        .hint_text(tl!("Add a name, group, or email"))
                        // What the permission list beside it leaves.
                        .desired_width((ui.available_width() - 130.0).clamp(80.0, 260.0))
                        .frame(egui::Frame::NONE),
                ),
                tl!("Add a name, group, or email"),
            );
            if edit.changed() {
                d.query_changed_at = ui.ctx().input(|i| i.time);
            }
            // Enter picks the first suggestion, or an email address as typed.
            if edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                let typed = d.query.trim().to_string();
                let pick = d.suggestions.first().cloned().or_else(|| {
                    typed.contains('@').then(|| Sharee { label: typed.clone(), detail: None, kind: "email".into(), share_with: typed.clone() })
                });
                if let Some(p) = pick {
                    d.recipients.push(p);
                    d.query.clear();
                    d.suggestions.clear();
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                permission_combo(ui, "share-permission", tl!("Permission for people"), &mut d.edit);
            });
        });
    });
    // Suggestions.
    if !d.suggestions.is_empty() {
        egui::Frame::new().fill(t.menu).stroke(egui::Stroke::new(1.0, t.border)).inner_margin(4.0).show(ui, |ui| {
            let mut picked = None;
            for (i, s) in d.suggestions.iter().enumerate() {
                let kind = match s.kind.as_str() {
                    "group" => tl!("Group"),
                    "email" => tl!("Email"),
                    "remote" => tl!("Other server"),
                    _ => tl!("Person"),
                };
                let text = match &s.detail {
                    Some(detail) if !detail.is_empty() => format!("{}  ·  {detail}", s.label),
                    _ => s.label.clone(),
                };
                if ui.add(egui::Button::new(format!("{text}   ({kind})")).frame(false)).clicked() {
                    picked = Some(i);
                }
            }
            if let Some(i) = picked {
                let s = d.suggestions.remove(i);
                d.recipients.push(s);
                d.query.clear();
                d.suggestions.clear();
            }
        });
    }
    ui.add_space(6.0);
    labelled(
        ui.add(egui::TextEdit::multiline(&mut d.message).hint_text(tl!("Add a message")).desired_rows(4).desired_width(f32::INFINITY)),
        tl!("Add a message"),
    );
    ui.add_space(8.0);

    // The link, and what it lets people do: in plain sight, as in Word, so Copy link never
    // copies a permission nobody saw. Changing it changes an existing link at once.
    ui.horizontal(|ui| {
        let (r, _) = ui.allocate_exact_size(vec2(16.0, 16.0), egui::Sense::hover());
        crate::icons::paint(ui.painter(), r, "link", t.text_dim, t.accent);
        ui.label(tl!("Anyone with the link"));
        let before = d.link_edit;
        permission_combo(ui, "link-permission", tl!("Link permission"), &mut d.link_edit);
        if d.link_edit != before
            && let Some(link) = d.shares.iter().find(|s| s.kind == "link")
        {
            let id = link.id.clone();
            let can = if d.link_edit { tl!("Link changed: anyone with it can edit.") } else { tl!("Link changed: anyone with it can view.") };
            d.status = Some((can.into(), false));
            d.ask(app, "share.update", json!({"id": id, "edit": d.link_edit}), Asked::Update);
        }
    });
    ui.add_space(6.0);

    // Footer: who has access, the link, and Send.
    ui.horizontal(|ui| {
        people_summary(ui, d, t);
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let send = ui.add_enabled(
                !d.recipients.is_empty() && !d.busy(),
                egui::Button::new(RichText::new(format!("  {}  ", tl!("Send"))).color(t.on_accent)).fill(t.accent),
            );
            if send.clicked() {
                let note = d.message.trim().to_string();
                let recipients = d.recipients.clone();
                d.adding = recipients.len();
                for r in recipients {
                    d.ask(app, "share.add", json!({"type": r.kind, "shareWith": r.share_with, "edit": d.edit, "note": note}), Asked::Add);
                }
            }
            if ui.button(tl!("Link settings…")).clicked() {
                d.link_settings = !d.link_settings;
            }
            let busy = d.busy();
            if ui.add_enabled_ui(!busy, |ui| crate::widgets::icon_text_button(ui, "link", tl!("Copy link"), regular(12.0))).inner.clicked() {
                let params = link_params(d);
                d.ask(app, "share.link", params, Asked::Link { copy: true });
            }
        });
    });
    if d.link_settings {
        link_settings(app, ui, d, t);
    }
    if d.manage {
        people_with_access(app, ui, d, t, true);
    }
    status(ui, d, t);
}

/// A text box's name for screen readers (its hint text isn't one).
fn labelled(r: egui::Response, name: &str) -> egui::Response {
    r.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::TextEdit, true, name));
    r
}

/// "Can edit" or "Can view"; `name` says what for (for screen readers, with the choice).
fn permission_combo(ui: &mut egui::Ui, id: &str, name: &str, edit: &mut bool) {
    let shown = if *edit { tl!("Can edit") } else { tl!("Can view") };
    let r = egui::ComboBox::from_id_salt(id).selected_text(shown).width(96.0).show_ui(ui, |ui| {
        ui.selectable_value(edit, true, tl!("Can edit"));
        ui.selectable_value(edit, false, tl!("Can view"));
    });
    let label = format!("{name}: {shown}");
    r.response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::ComboBox, true, &label));
}

fn link_params(d: &ShareDialog) -> Json {
    let mut p = json!({"edit": d.link_edit});
    if !d.link_password.is_empty() {
        p["password"] = Json::String(d.link_password.clone());
    }
    p["expiration"] = if d.link_expiry.trim().is_empty() { Json::Null } else { Json::String(d.link_expiry.trim().to_string()) };
    p
}

/// Initials of the people with access; clicking opens the list.
fn people_summary(ui: &mut egui::Ui, d: &mut ShareDialog, t: &Tokens) {
    let people: Vec<&Share> = d.shares.iter().filter(|s| s.kind != "link").collect();
    let text = match people.len() {
        0 => tl!("Only you have access").to_string(),
        1 => format!("{} {}", tl!("Shared with"), people.first().map_or("", |p| p.label.as_str())),
        n => format!("{} {n} {}", tl!("Shared with"), tl!("people")),
    };
    let link = d.shares.iter().any(|s| s.kind == "link");
    let text = if link { format!("{text} · {}", tl!("link")) } else { text };
    if ui.add(egui::Button::new(RichText::new(text).color(t.text_dim)).frame(false)).on_hover_text(tl!("Manage access")).clicked() {
        d.manage = !d.manage;
    }
}

fn link_settings(app: &mut WordApp, ui: &mut egui::Ui, d: &mut ShareDialog, t: &Tokens) {
    egui::Frame::new().stroke(egui::Stroke::new(1.0, t.border)).corner_radius(4.0).inner_margin(8.0).show(ui, |ui| {
        ui.label(RichText::new(tl!("Link settings")).strong());
        ui.horizontal(|ui| {
            ui.label(tl!("Password"));
            labelled(
                ui.add(egui::TextEdit::singleline(&mut d.link_password).password(true).hint_text(tl!("none")).desired_width(160.0)),
                tl!("Link password"),
            );
        });
        ui.horizontal(|ui| {
            ui.label(tl!("Expires"));
            labelled(
                ui.add(egui::TextEdit::singleline(&mut d.link_expiry).hint_text(tl!("YYYY-MM-DD, or empty")).desired_width(160.0)),
                tl!("Link expiry date"),
            );
        });
        ui.horizontal(|ui| {
            if ui.add_enabled(!d.busy(), egui::Button::new(tl!("Apply"))).clicked() {
                let params = link_params(d);
                d.ask(app, "share.link", params, Asked::Link { copy: false });
            }
            if let Some(link) = d.shares.iter().find(|s| s.kind == "link")
                && ui.button(tl!("Stop sharing the link")).clicked()
            {
                let id = link.id.clone();
                d.ask(app, "share.remove", json!({"id": id}), Asked::Remove);
            }
        });
    });
}

fn people_with_access(app: &mut WordApp, ui: &mut egui::Ui, d: &mut ShareDialog, t: &Tokens, manage: bool) {
    ui.add_space(6.0);
    egui::Frame::new().stroke(egui::Stroke::new(1.0, t.border)).corner_radius(4.0).inner_margin(8.0).show(ui, |ui| {
        ui.label(RichText::new(tl!("People with access")).strong());
        if d.shares.is_empty() {
            ui.label(RichText::new(tl!("Only you")).color(t.text_dim));
        }
        let mut change: Option<(Json, bool)> = None;
        let mut remove: Option<Json> = None;
        egui::ScrollArea::vertical().max_height(180.0).show(ui, |ui| {
            for s in &d.shares {
                ui.horizontal(|ui| {
                    let who = if s.kind == "link" { tl!("Anyone with the link").to_string() } else { s.label.clone() };
                    let extra = match (s.password, &s.expiration) {
                        (true, Some(e)) => format!(" ({}, {} {e})", tl!("password"), tl!("until")),
                        (true, None) => format!(" ({})", tl!("password")),
                        (false, Some(e)) => format!(" ({} {e})", tl!("until")),
                        _ => String::new(),
                    };
                    ui.label(format!("{who}{extra}"));
                    if !manage {
                        return;
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button(tl!("Remove")).clicked() {
                            remove = Some(s.id.clone());
                        }
                        let mut edit = s.edit;
                        let name =
                            if s.kind == "link" { tl!("Link permission").to_string() } else { format!("{} {}", tl!("Permission for"), s.label) };
                        permission_combo(ui, &format!("access-{}", s.id), &name, &mut edit);
                        if edit != s.edit {
                            change = Some((s.id.clone(), edit));
                        }
                        if let Some(url) = &s.url
                            && ui.button(tl!("Copy")).on_hover_text(url).clicked()
                        {
                            ui.ctx().copy_text(url.clone());
                        }
                    });
                });
            }
        });
        if let Some((id, edit)) = change {
            d.ask(app, "share.update", json!({"id": id, "edit": edit}), Asked::Update);
        }
        if let Some(id) = remove {
            d.ask(app, "share.remove", json!({"id": id}), Asked::Remove);
        }
    });
}

fn status(ui: &mut egui::Ui, d: &ShareDialog, t: &Tokens) {
    if let Some((text, error)) = &d.status {
        ui.add_space(6.0);
        let color = if *error { t.red } else { t.text_dim };
        ui.label(RichText::new(text).color(color));
    }
}
