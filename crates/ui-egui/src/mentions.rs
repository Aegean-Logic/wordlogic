//! `@mentions` in comments, as in Microsoft 365: typing `@` in a comment or a reply lists people
//! from the host's cloud (Nextcloud) to pick, picking puts `@Name` in the text (kept in the
//! document as Word keeps it, a `Mention`-styled link), and posting tells the people newly
//! mentioned (a notification and an email, sent by the host). Someone who can't open the document
//! is only told after the user agrees to share it with them ("Share and notify").
//!
//! Ported from GridLogic (`gridcraft/crates/ui-egui/src/mentions.rs`). The host answers two
//! operations besides the Share dialog's ([`crate::cloud::Cloud`]):
//!
//! - `people.search` `{query}` → `[{id, name, email?, profile?}]`: people who can be mentioned
//!   (`profile` is a web page about them, the link when they have no email address)
//! - `mention.notify` `{people: [id], text, anchor}` → `{notified: [id], noAccess: [{id, name}]}`
//!
//! and `share.info` says whether mentions work here (`mentions`).

use std::collections::HashMap;

use egui::{Color32, Key, Modifiers, RichText, Ui, vec2};
use serde::Deserialize;
use serde_json::{Value as Json, json};
use wordcraft_engine::cmd::mention::Found;

use crate::WordApp;
use crate::cloud::{Cloud, next_request_id};
use crate::theme::Tokens;

/// Someone picked into a comment: the `mentions` param of the comment commands.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Person {
    pub name: String,
    /// Their Nextcloud user id; empty for mentions read from the document.
    pub id: String,
    pub link: String,
}

/// What a comment box holds while it's being written: the text and the people picked into it.
#[derive(Clone, Debug, Default)]
pub struct Draft {
    pub text: String,
    pub picked: Vec<Person>,
}

impl Draft {
    /// A draft of a comment as it is, its mentions picked already.
    pub fn new(text: &str, found: &[Found]) -> Draft {
        let mut picked: Vec<Person> = vec![];
        for f in found {
            if !picked.iter().any(|p| p.link == f.link) {
                picked.push(Person { name: f.name.clone(), id: String::new(), link: f.link.clone() });
            }
        }
        Draft { text: text.to_string(), picked }
    }

    /// The `mentions` param of `review.newComment`, `review.reply` and `review.editComment`.
    pub fn people(&self) -> Json {
        json!(self.picked.iter().map(|p| json!({"name": p.name, "id": p.id, "link": p.link})).collect::<Vec<_>>())
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct Info {
    /// Mentions work: the document is in the cloud and the user is signed in, not a link guest.
    mentions: bool,
    signed_in: bool,
    in_cloud: bool,
    link: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
struct Who {
    id: String,
    name: String,
    #[serde(default)]
    email: Option<String>,
    #[serde(default)]
    profile: Option<String>,
}

impl Who {
    /// Who this is in the document: a link to their address, else to their profile page.
    fn person(&self) -> Option<Person> {
        let link = match (&self.email, &self.profile) {
            (Some(e), _) if e.contains('@') => format!("mailto:{e}"),
            (_, Some(p)) if p.starts_with("https://") || p.starts_with("http://") => p.clone(),
            _ => return None,
        };
        Some(Person { name: self.name.clone(), id: self.id.clone(), link })
    }
}

/// A notification to send (again): what was written, where, and to whom.
#[derive(Clone, Debug)]
struct Notice {
    text: String,
    anchor: String,
    people: Vec<Who>,
}

#[derive(Clone, Debug)]
enum Asked {
    Info,
    Search(String),
    Notify(Notice),
    Share(Who),
}

/// The people list under one comment box.
struct Popup {
    editor: egui::Id,
    /// Where the `@` is, in characters.
    at: usize,
    query: String,
    typed_at: f64,
    searched: Option<String>,
    found: Vec<Who>,
    highlight: usize,
    /// Where it was drawn: clicking in it takes the focus off the box without closing it.
    rect: Option<egui::Rect>,
}

/// Mentions state of the app: one picker at a time, and the notifications under way.
#[derive(Default)]
pub struct Mentions {
    info: Option<Info>,
    asked: HashMap<u64, Asked>,
    popup: Option<Popup>,
    /// An `@` the user closed the list for (Escape, or a pick): it stays closed there.
    dismissed: Option<(egui::Id, usize)>,
    /// People who can't open the document: asking whether to share it with them.
    share_ask: Option<Notice>,
    /// Shares being made before notifying: the notice, how many are left, who was shared with.
    sharing: Option<(Notice, usize, Vec<Who>)>,
    /// A comment to open once the document is here (a mention's link), and when that was first
    /// tried.
    go_to: Option<(String, Option<f64>)>,
}

/// Waiting for the user to stop typing before searching.
const SEARCH_DELAY: f64 = 0.25;
/// People listed at most.
const MAX_LISTED: usize = 8;
/// A comment's text sent with its notification, at most (characters).
const MAX_NOTICE_CHARS: usize = 1000;
/// How long a mention's link keeps trying to find its comment while the document arrives
/// (seconds): a shared document can come after the first file.
const GO_TO_PATIENCE: f64 = 20.0;

impl Mentions {
    fn ask(&mut self, cloud: &Cloud, op: &str, params: Json, what: Asked) {
        let id = next_request_id();
        self.asked.insert(id, what);
        (cloud.request)(id, op, params);
    }

    fn ensure_info(&mut self, cloud: &Cloud) {
        if self.info.is_none() && !self.asked.values().any(|a| matches!(a, Asked::Info)) {
            self.ask(cloud, "share.info", json!({}), Asked::Info);
        }
    }

    /// Opens the comment with durable id `anchor` (`review.goToComment`) once the document has it.
    pub fn go_to_comment(&mut self, anchor: &str) {
        let anchor = anchor.trim();
        if !anchor.is_empty() {
            self.go_to = Some((anchor.chars().take(64).collect(), None));
        }
    }

    /// The people list is open under the box `id`: its losing focus is a click in the list.
    pub fn picking(&self, id: egui::Id) -> bool {
        self.popup.as_ref().is_some_and(|p| p.editor == id)
    }
}

/// The `@query` being typed before the caret (`cursor`, in characters): the `@`'s position and
/// what follows it. An `@` counts at the start of a word only (not in `ann@example.com`).
fn token(text: &str, cursor: usize) -> Option<(usize, String)> {
    let before: Vec<char> = text.chars().take(cursor).collect();
    let lo = before.len().saturating_sub(41);
    let mut i = before.len();
    while i > lo {
        i -= 1;
        let c = *before.get(i)?;
        if c == '\n' {
            return None;
        }
        if c == '@' {
            let starts_word = i.checked_sub(1).and_then(|j| before.get(j)).is_none_or(|b| b.is_whitespace() || "([{\"'".contains(*b));
            let query: String = before.get(i + 1..)?.iter().collect();
            if !starts_word || query.starts_with(' ') || query.contains("  ") {
                return None;
            }
            return Some((i, query));
        }
    }
    None
}

/// Replaces the `@query` at `at..cursor` (characters) with `@Name ` and returns the new caret.
fn insert_pick(text: &mut String, at: usize, cursor: usize, name: &str) -> usize {
    let byte = |n: usize| text.char_indices().nth(n).map(|(b, _)| b).unwrap_or(text.len());
    let (a, b) = (byte(at), byte(cursor.max(at)));
    let ins = format!("@{name} ");
    text.replace_range(a..b, &ins);
    at + ins.chars().count()
}

/// The character spans of each picked person's first `@Name` in `text` (as the engine finds them).
fn spans(text: &str, picked: &[Person]) -> Vec<(usize, usize)> {
    let mut out: Vec<(usize, usize)> = vec![];
    for p in picked {
        let needle = format!("@{}", p.name);
        let mut from = 0;
        while let Some(i) = text.get(from..).and_then(|t| t.find(&needle)).map(|i| i + from) {
            let end = i + needle.len();
            let next = text.get(end..).and_then(|t| t.chars().next());
            let (a, n) = (text.get(..i).map(|t| t.chars().count()).unwrap_or(0), needle.chars().count());
            if !out.iter().any(|(x, y)| a < x + y && *x < a + n) && !next.is_some_and(char::is_alphanumeric) {
                out.push((a, n));
                break;
            }
            from = end;
        }
    }
    out.sort();
    out
}

/// Lays out `text` with the character spans `marks` in the accent colour.
pub fn job(text: &str, marks: &[(usize, usize)], font: egui::FontId, color: Color32, accent: Color32, wrap: f32) -> egui::text::LayoutJob {
    let mut job = egui::text::LayoutJob::default();
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let byte = |n: usize| chars.get(n).map(|(b, _)| *b).unwrap_or(text.len());
    let mut pos = 0usize;
    for (start, len) in marks {
        let (a, b) = (byte(*start), byte(start.saturating_add(*len)));
        if a < pos || b <= a {
            continue;
        }
        job.append(text.get(pos..a).unwrap_or(""), 0.0, egui::TextFormat::simple(font.clone(), color));
        job.append(text.get(a..b).unwrap_or(""), 0.0, egui::TextFormat::simple(font.clone(), accent));
        pos = b;
    }
    job.append(text.get(pos..).unwrap_or(""), 0.0, egui::TextFormat::simple(font, color));
    job.wrap.max_width = wrap;
    job
}

/// The colour mentions are drawn in.
pub fn accent(t: &Tokens) -> Color32 {
    t.accent_text
}

/// A comment or reply box that offers people to mention after `@`. `draft` keeps the text and the
/// people picked. While [`Mentions::picking`] is true for `id`, a lost focus is a click in the list,
/// not the end of editing.
pub fn editor(app: &mut WordApp, ui: &mut Ui, id: egui::Id, draft: &mut Draft, font: Option<egui::FontId>, hint: &str) -> egui::Response {
    let WordApp { services, mentions, .. } = app;
    let cloud = services.cloud.as_ref();
    let ctx = ui.ctx().clone();
    let now = ctx.input(|i| i.time);
    let focused = ctx.memory(|m| m.has_focus(id));

    // Keys for the list, before the box sees them.
    let mut pick: Option<Who> = None;
    if focused && let Some(p) = mentions.popup.as_mut().filter(|p| p.editor == id) {
        let n = p.found.len();
        if n > 0 && ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::ArrowDown)) {
            p.highlight = (p.highlight + 1) % n;
        }
        if n > 0 && ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::ArrowUp)) {
            p.highlight = (p.highlight + n - 1) % n;
        }
        if n > 0 && ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Enter) || i.consume_key(Modifiers::NONE, Key::Tab)) {
            pick = p.found.get(p.highlight).cloned();
        }
        if ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape)) {
            mentions.dismissed = Some((id, p.at));
            mentions.popup = None;
        }
    }
    if let Some(who) = pick.take() {
        apply_pick(&ctx, mentions, id, draft, &who);
    }

    // The box, with picked people drawn as mentions.
    let t = Tokens::get(&ctx);
    let accent = accent(&t);
    let picked = draft.picked.clone();
    let font2 = font.clone();
    let mut layouter = move |ui: &Ui, buf: &dyn egui::TextBuffer, wrap: f32| {
        let s = buf.as_str();
        let font = font2.clone().unwrap_or_else(|| egui::TextStyle::Body.resolve(ui.style()));
        let color = ui.visuals().text_color();
        ui.fonts_mut(|f| f.layout_job(job(s, &spans(s, &picked), font, color, accent, wrap)))
    };
    let mut edit = egui::TextEdit::multiline(&mut draft.text).desired_rows(1).desired_width(f32::INFINITY).hint_text(hint);
    if let Some(f) = font {
        edit = edit.font(f);
    }
    let output = edit.id(id).layouter(&mut layouter).show(ui);
    let response = output.response.response.clone();

    // Is an `@name` being typed?
    let cursor: Option<usize> = output.cursor_range.map(|r| r.primary.index.into());
    let typing = if response.has_focus() { cursor.and_then(|c| token(&draft.text, c)) } else { None };
    if let Some((_, at)) = mentions.dismissed
        && draft.text.chars().nth(at) != Some('@')
    {
        mentions.dismissed = None;
    }
    match typing {
        Some((at, query)) if mentions.dismissed != Some((id, at)) => {
            if let Some(c) = cloud {
                mentions.ensure_info(c);
            }
            let caret = cursor.map(|c| output.galley.pos_from_cursor(egui::text::CCursor::new(c)));
            let anchor = caret.map(|r| output.galley_pos + r.left_bottom().to_vec2()).unwrap_or(response.rect.left_bottom());
            match mentions.popup.as_mut().filter(|p| p.editor == id && p.at == at) {
                Some(p) if p.query == query => {}
                Some(p) => {
                    p.query = query;
                    p.typed_at = now;
                }
                None => {
                    mentions.popup = Some(Popup { editor: id, at, query, typed_at: now, searched: None, found: vec![], highlight: 0, rect: None });
                }
            }
            if let Some(p) = mentions.popup.as_mut() {
                p.rect = Some(egui::Rect::from_min_size(anchor + vec2(0.0, 2.0), p.rect.map(|r| r.size()).unwrap_or(vec2(260.0, 40.0))));
            }
        }
        _ => {
            // Clicking a person takes the focus off the box: the list stays while the pointer is on it.
            let on_list =
                mentions.popup.as_ref().and_then(|p| p.rect).zip(ctx.pointer_latest_pos()).is_some_and(|(r, pos)| r.expand(4.0).contains(pos));
            if mentions.popup.as_ref().is_some_and(|p| p.editor == id) && (response.has_focus() || !on_list) {
                mentions.popup = None;
            }
        }
    }

    // Searching once typing pauses.
    if let (Some(c), Some(p)) = (cloud, mentions.popup.as_mut().filter(|p| p.editor == id)) {
        let enabled = mentions.info.as_ref().is_some_and(|i| i.mentions);
        if enabled && p.searched.as_deref() != Some(p.query.as_str()) {
            if now - p.typed_at >= SEARCH_DELAY {
                let q = p.query.trim().to_string();
                p.searched = Some(p.query.clone());
                let rid = next_request_id();
                mentions.asked.insert(rid, Asked::Search(q.clone()));
                (c.request)(rid, "people.search", json!({"query": q}));
            } else {
                ctx.request_repaint_after(std::time::Duration::from_millis(80));
            }
        }
    }

    // The list.
    if let Some(who) = list(&ctx, mentions, id, cloud.is_some(), &t) {
        apply_pick(&ctx, mentions, id, draft, &who);
    }
    response
}

/// Puts `who` in the box at the `@` being typed and closes the list.
fn apply_pick(ctx: &egui::Context, mentions: &mut Mentions, id: egui::Id, draft: &mut Draft, who: &Who) {
    let Some(p) = mentions.popup.take().filter(|p| p.editor == id) else { return };
    let Some(person) = who.person() else { return };
    let mut state = egui::text_edit::TextEditState::load(ctx, id).unwrap_or_default();
    let cursor: usize = state.cursor.char_range().map(|r| r.primary.index.into()).unwrap_or(p.at + 1 + p.query.chars().count());
    let caret = insert_pick(&mut draft.text, p.at, cursor, &who.name);
    if !draft.picked.iter().any(|q| q.link == person.link) {
        draft.picked.push(person);
    }
    state.cursor.set_char_range(Some(egui::text::CCursorRange::one(egui::text::CCursor::new(caret))));
    state.store(ctx, id);
    ctx.memory_mut(|m| m.request_focus(id));
    mentions.dismissed = Some((id, p.at));
}

/// Draws the people list of the box `id`, returning the person clicked.
fn list(ctx: &egui::Context, mentions: &mut Mentions, id: egui::Id, has_cloud: bool, t: &Tokens) -> Option<Who> {
    let info = mentions.info.clone();
    let p = mentions.popup.as_mut().filter(|p| p.editor == id)?;
    let pos = p.rect?.min;
    let mut clicked = None;
    let area = egui::Area::new(id.with("mention-list")).order(egui::Order::Tooltip).fixed_pos(pos).show(ctx, |ui| {
        egui::Frame::popup(ui.style()).show(ui, |ui| {
            ui.set_min_width(240.0);
            ui.set_max_width(320.0);
            let why = |s: &str| RichText::new(s).color(t.text_dim);
            match &info {
                _ if !has_cloud => {
                    ui.label(why("Save the document to Nextcloud to mention people."));
                }
                None => {
                    ui.label(why("Looking for people…"));
                }
                Some(i) if !i.mentions => {
                    let reason = if i.link.is_some() {
                        "People with a link can't mention anyone. Sign in to Nextcloud to mention people."
                    } else if !i.signed_in {
                        "Sign in to Nextcloud to mention people."
                    } else if !i.in_cloud {
                        "Save the document to Nextcloud to mention people."
                    } else {
                        "Mentions aren't available here."
                    };
                    ui.label(why(reason));
                }
                Some(_) if p.found.is_empty() => {
                    let searching = p.searched.as_deref() != Some(p.query.as_str());
                    ui.label(why(if searching { "Looking for people…" } else { "No one found. Keep typing a name or email." }));
                }
                Some(_) => {
                    for (k, who) in p.found.iter().enumerate() {
                        let on = k == p.highlight;
                        let r = egui::Frame::NONE
                            .fill(if on { t.hover } else { Color32::TRANSPARENT })
                            .corner_radius(4.0)
                            .inner_margin(vec2(6.0, 3.0))
                            .show(ui, |ui| {
                                ui.set_width(ui.available_width());
                                ui.vertical(|ui| {
                                    ui.label(RichText::new(&who.name).color(t.text));
                                    let detail = who.email.clone().unwrap_or_else(|| who.id.clone());
                                    if detail != who.name {
                                        ui.label(RichText::new(detail).small().color(t.text_dim));
                                    }
                                });
                            });
                        let r = r.response.interact(egui::Sense::click());
                        if r.hovered() {
                            p.highlight = k;
                        }
                        if r.clicked() {
                            clicked = Some(who.clone());
                        }
                    }
                }
            }
        });
    });
    p.rect = Some(area.response.rect);
    clicked
}

/// After a comment or reply was posted or edited (`answer` is the command's result, with
/// `mentions`, the people newly mentioned, and `anchor`): tells them, through the cloud.
pub fn posted(app: &mut WordApp, answer: &Json, text: &str) {
    let people: Vec<Who> = answer
        .get("mentions")
        .and_then(Json::as_array)
        .into_iter()
        .flatten()
        .filter_map(|m| Some(Who { id: m.get("id")?.as_str()?.to_string(), name: m.get("name")?.as_str()?.to_string(), email: None, profile: None }))
        .filter(|w| !w.id.is_empty())
        .collect();
    let anchor = answer.get("anchor").and_then(Json::as_str).unwrap_or("").to_string();
    if people.is_empty() || !app.mentions.info.as_ref().is_some_and(|i| i.mentions) {
        return;
    }
    let WordApp { services, mentions, .. } = app;
    let Some(cloud) = services.cloud.as_ref() else { return };
    notify(mentions, cloud, Notice { text: text.chars().take(MAX_NOTICE_CHARS).collect(), anchor, people });
}

fn notify(mentions: &mut Mentions, cloud: &Cloud, n: Notice) {
    let ids: Vec<&str> = n.people.iter().map(|w| w.id.as_str()).collect();
    let params = json!({"people": ids, "text": n.text, "anchor": n.anchor});
    mentions.ask(cloud, "mention.notify", params, Asked::Notify(n));
}

/// Opens a mention's comment when the document has it, takes the cloud's answers and draws
/// "Share and notify?" when it's needed. Every frame.
pub fn show(app: &mut WordApp, ctx: &egui::Context) {
    if let Some((anchor, first)) = app.mentions.go_to.clone() {
        let now = ctx.input(|i| i.time);
        let first = first.unwrap_or(now);
        // Found, or given up on.
        if app.run("review.goToComment", json!({"anchor": anchor})).is_ok() || now - first > GO_TO_PATIENCE {
            app.mentions.go_to = None;
        } else {
            app.mentions.go_to = Some((anchor, Some(first)));
            ctx.request_repaint_after(std::time::Duration::from_millis(500));
        }
    }
    let mut problem: Option<String> = None;
    {
        let WordApp { services, mentions, .. } = &mut *app;
        let Some(cloud) = services.cloud.as_ref() else { return };
        let answers: Vec<(u64, Result<Json, String>)> = {
            let mut inbox = cloud.inbox.lock().unwrap_or_else(|e| e.into_inner());
            let (mine, rest): (Vec<_>, Vec<_>) = inbox.drain(..).partition(|(id, _)| mentions.asked.contains_key(id));
            *inbox = rest;
            mine
        };
        for (id, answer) in answers {
            let Some(what) = mentions.asked.remove(&id) else { continue };
            match (what, answer) {
                (Asked::Info, Ok(a)) => mentions.info = Some(serde_json::from_value(a).unwrap_or_default()),
                (Asked::Info, Err(_)) => mentions.info = Some(Info::default()),
                (Asked::Search(q), Ok(a)) => {
                    if let Some(p) = mentions.popup.as_mut().filter(|p| p.query.trim() == q) {
                        let found: Vec<Who> = serde_json::from_value(a).unwrap_or_default();
                        // Someone the document can't link to (no address, no profile) can't be mentioned.
                        p.found = found.into_iter().filter(|w| w.person().is_some()).take(MAX_LISTED).collect();
                        p.highlight = 0;
                    }
                }
                (Asked::Search(_), Err(_)) => {}
                (Asked::Notify(n), Ok(a)) => {
                    let no_access: Vec<Who> = a.get("noAccess").cloned().and_then(|v| serde_json::from_value(v).ok()).unwrap_or_default();
                    if !no_access.is_empty() {
                        mentions.share_ask = Some(Notice { people: no_access, ..n });
                    }
                }
                (Asked::Notify(_), Err(e)) => problem = Some(format!("The people you mentioned couldn't be told: {e}")),
                (Asked::Share(who), result) => {
                    if let Some((_, left, done)) = mentions.sharing.as_mut() {
                        *left = left.saturating_sub(1);
                        match result {
                            Ok(_) => done.push(who),
                            Err(e) => problem = Some(format!("{} couldn't be given access: {e}", who.name)),
                        }
                    }
                    if mentions.sharing.as_ref().is_some_and(|(_, left, _)| *left == 0)
                        && let Some((n, _, done)) = mentions.sharing.take()
                        && !done.is_empty()
                    {
                        notify(mentions, cloud, Notice { people: done, ..n });
                    }
                }
            }
            ctx.request_repaint();
        }
        if !mentions.asked.is_empty() {
            ctx.request_repaint_after(std::time::Duration::from_millis(200));
        }
    }
    if let Some(p) = problem {
        app.status(p);
    }

    // "Share and notify?", as Office asks when someone mentioned can't open the file.
    let WordApp { services, mentions, .. } = app;
    let Some(cloud) = services.cloud.as_ref() else { return };
    let Some(n) = mentions.share_ask.clone() else { return };
    let t = Tokens::get(ctx);
    let names = join_names(&n.people);
    let mut choice: Option<bool> = None;
    let mut open = true;
    egui::Window::new("Share and notify?")
        .id(egui::Id::new("mention-share"))
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, vec2(0.0, -40.0))
        .open(&mut open)
        .show(ctx, |ui| {
            ui.set_max_width(380.0);
            let them = if n.people.len() == 1 { "them" } else { "these people" };
            ui.label(format!("{names} can't open this document. Share it with {them} so they can see your comment?"));
            ui.label(RichText::new("They'll be able to edit it.").small().color(t.text_dim));
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.button("Share and notify").clicked() {
                    choice = Some(true);
                }
                if ui.button("Don't share").clicked() {
                    choice = Some(false);
                }
            });
        });
    if ctx.input(|i| i.key_pressed(Key::Escape)) || !open {
        choice = Some(false);
    }
    match choice {
        Some(true) => {
            mentions.share_ask = None;
            mentions.sharing = Some((n.clone(), n.people.len(), vec![]));
            for who in n.people {
                let params = json!({"type": "user", "shareWith": who.id, "edit": true, "note": ""});
                mentions.ask(cloud, "share.add", params, Asked::Share(who));
            }
        }
        Some(false) => mentions.share_ask = None,
        None => {}
    }
}

/// "Ann", "Ann and Bo", "Ann, Bo and Cy".
fn join_names(people: &[Who]) -> String {
    let names: Vec<&str> = people.iter().map(|w| w.name.as_str()).collect();
    match names.split_last() {
        None => String::new(),
        Some((last, [])) => (*last).to_string(),
        Some((last, rest)) => format!("{} and {last}", rest.join(", ")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_at_sign_starts_a_mention_at_a_word_start_only() {
        assert_eq!(token("Hi @an", 6), Some((3, "an".into())));
        assert_eq!(token("mail ann@example.com", 20), None);
        assert_eq!(token("@ ann", 5), None);
        assert_eq!(token("@ann\nx", 6), None);
        // A four-byte character before the `@` (escaped: the source scan checks interface glyphs).
        assert_eq!(token("\u{1F600} @Bø", 5), Some((2, "Bø".into())));
    }

    #[test]
    fn picking_replaces_the_query_with_the_name() {
        let mut s = "Hi @an, see".to_string();
        assert_eq!(insert_pick(&mut s, 3, 6, "Ann Lee"), 12);
        assert_eq!(s, "Hi @Ann Lee , see");
        let mut s = "@".to_string();
        insert_pick(&mut s, 0, 99, "A");
        assert_eq!(s, "@A ");
    }

    #[test]
    fn people_link_to_their_address_or_profile() {
        let w = |email: Option<&str>, profile: Option<&str>| Who {
            id: "ann".into(),
            name: "Ann".into(),
            email: email.map(str::to_string),
            profile: profile.map(str::to_string),
        };
        assert_eq!(w(Some("ann@example.com"), None).person().map(|p| p.link), Some("mailto:ann@example.com".into()));
        assert_eq!(w(None, Some("https://c.example/u/ann")).person().map(|p| p.link), Some("https://c.example/u/ann".into()));
        assert_eq!(w(Some("ann"), Some("javascript:x")).person(), None);
        let d = Draft { text: "@Ann hi".into(), picked: vec![w(Some("a@b.c"), None).person().unwrap_or_default()] };
        assert_eq!(spans(&d.text, &d.picked), vec![(0, 4)]);
        assert_eq!(join_names(&[w(None, None), w(None, None)]), "Ann and Ann");
    }
}
