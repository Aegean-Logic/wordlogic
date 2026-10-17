//! Co-editing: the others in a shared document, drawn where they are. Each person's selection is
//! a translucent highlight in their colour, under this person's own; their caret is a bar in
//! their colour with a small tag carrying their name, as Word for the web shows co-authors.
//! Positions come from the session ([`wordcraft_engine::Session::collab_others`]), which keeps
//! them with the text between the host's reports; names and colours from [`WordApp::presence`].

use egui::{Align2, Color32, Pos2, Rect, Stroke, pos2, vec2};
use wordcraft_doc::Pos;
use wordcraft_layout::DocLayout;

use crate::WordApp;
use crate::canvas::page_screen_scale;
use crate::theme::semibold;

/// The most highlight rectangles drawn for one person (a select-all in a long document).
const MAX_RECTS: usize = 4000;
/// How opaque a selection highlight is.
const SELECTION_ALPHA: u8 = 56;
/// The name tag's text size, its padding, and its longest name in characters.
const TAG_TEXT: f32 = 10.0;
const TAG_PAD: f32 = 3.0;
const TAG_CHARS: usize = 24;
/// How long a person's name shows on their caret after they move or type (milliseconds).
const TAG_SHOW_MS: f64 = 3000.0;
/// The flag a caret carries the rest of the time, and how near the pointer shows the name.
const FLAG: f32 = 5.0;
const HOVER: f32 = 6.0;

/// The others' positions with their names and colours, where this copy can show them (a
/// position in a part this copy doesn't have yet isn't drawn).
fn others(app: &WordApp) -> Vec<(&crate::Presence, Pos, Pos)> {
    let doc = &app.session.doc;
    app.presence
        .iter()
        .zip(app.session.collab_others())
        .filter(|(_, o)| doc.story(o.anchor.story).is_some() && doc.story(o.focus.story).is_some())
        .map(|(p, o)| (p, doc.clamp(&o.anchor), doc.clamp(&o.focus)))
        .collect()
}

/// A page-space rectangle on screen.
fn on_screen(layout: &DocLayout, rects: &[Rect], scale: f32, page: usize, r: wordcraft_geom::Rect) -> Option<Rect> {
    let pr = rects.get(page)?;
    let s = layout.pages.get(page).map_or(scale, |p| page_screen_scale(*pr, p, scale));
    Some(Rect::from_min_size(pos2(pr.min.x + r.x * s, pr.min.y + r.y * s), vec2(r.w * s, r.h * s)))
}

/// The others' selections, in their colours (before this person's own is drawn).
pub(crate) fn paint_selections(app: &WordApp, painter: &egui::Painter, layout: &DocLayout, rects: &[Rect], scale: f32) {
    let clip = painter.clip_rect();
    for (p, anchor, focus) in others(app) {
        if anchor == focus || anchor.story != focus.story {
            continue;
        }
        let [r, g, b, _] = p.color.to_array();
        let fill = Color32::from_rgba_unmultiplied(r, g, b, SELECTION_ALPHA);
        for (page, pr) in layout.selection_rects(&app.session.doc, &anchor, &focus, app.session.page_hint).into_iter().take(MAX_RECTS) {
            if let Some(sr) = on_screen(layout, rects, scale, page, pr).filter(|sr| sr.intersects(clip)) {
                painter.rect_filled(sr, 0.0, fill);
            }
        }
    }
}

/// The others' carets (after this person's own). Each carries its person's name for a few seconds
/// after they move or type, and while the pointer is over it; otherwise a small flag in their
/// colour, so names don't sit over the text.
pub(crate) fn paint_carets(app: &WordApp, painter: &egui::Painter, layout: &DocLayout, rects: &[Rect], scale: f32) {
    let clip = painter.clip_rect();
    let now = crate::now_ms();
    let hover = painter.ctx().pointer_hover_pos();
    let mut fade_in: Option<f64> = None;
    for (p, _, focus) in others(app) {
        let Some(c) = layout.caret_on(&focus, app.session.page_hint) else { continue };
        let Some(pr) = rects.get(c.page) else { continue };
        let s = layout.pages.get(c.page).map_or(scale, |page| page_screen_scale(*pr, page, scale));
        let x = (pr.min.x + c.x * s).round() + 0.5;
        let y0 = pr.min.y + c.top * s;
        let y1 = y0 + c.height.max(1.0) * s;
        if !Rect::from_min_max(pos2(x - 1.0, y0), pos2(x + 1.0, y1)).intersects(clip) {
            continue;
        }
        let bar = if c.width > 0.0 {
            // Turned text: the caret lies across the page.
            [pos2(x, y0.round() + 0.5), pos2(x + c.width * s, y0.round() + 0.5)]
        } else {
            [pos2(x, y0), pos2(x, y1)]
        };
        painter.line_segment(bar, Stroke::new(2.0, p.color));
        let since = app.presence_moved.get(&p.client).map_or(f64::INFINITY, |(_, _, t)| now - t);
        let hovered = hover.is_some_and(|h| Rect::from_min_max(pos2(x - HOVER, y0 - HOVER), pos2(x + HOVER, y1 + HOVER)).contains(h));
        if hovered || (0.0..TAG_SHOW_MS).contains(&since) {
            tag(painter, bar[0], &p.name, p.color);
            if !hovered {
                let left = TAG_SHOW_MS - since;
                fade_in = Some(fade_in.map_or(left, |f| f.min(left)));
            }
        } else {
            // Just a flag at the top of the caret.
            painter.rect_filled(Rect::from_min_size(pos2(x - 1.0, bar[0].y - FLAG), vec2(FLAG, FLAG)), 0.0, p.color);
        }
    }
    // Take the names down when their time is up.
    if let Some(ms) = fade_in {
        painter.ctx().request_repaint_after(std::time::Duration::from_millis(ms.clamp(0.0, TAG_SHOW_MS) as u64 + 50));
    }
}

/// The name tag at the top of a caret: a small flag in the person's colour.
fn tag(painter: &egui::Painter, top: Pos2, name: &str, color: Color32) {
    let mut shown: String = name.chars().take(TAG_CHARS).collect();
    if name.chars().count() > TAG_CHARS {
        shown.push('…');
    }
    if shown.trim().is_empty() {
        return;
    }
    let galley = painter.layout_no_wrap(shown, semibold(TAG_TEXT), text_on(color));
    let size = galley.size() + vec2(2.0 * TAG_PAD, 2.0 * TAG_PAD - 2.0);
    let rect = Rect::from_min_size(pos2(top.x - 1.0, top.y - size.y), size);
    painter.rect_filled(rect, egui::CornerRadius { nw: 2, ne: 2, se: 2, sw: 0 }, color);
    painter.galley(Align2::LEFT_TOP.anchor_size(rect.min + vec2(TAG_PAD, TAG_PAD - 1.0), galley.size()).min, galley, Color32::WHITE);
}

/// White or near-black text, whichever reads on `bg`.
fn text_on(bg: Color32) -> Color32 {
    let [r, g, b, _] = bg.to_array();
    let luma = 0.299 * f32::from(r) + 0.587 * f32::from(g) + 0.114 * f32::from(b);
    if luma > 160.0 { Color32::from_gray(20) } else { Color32::WHITE }
}

/// The colour a person in a session is drawn in, by client id: the colours of
/// `wordcraft-nextcloud`'s `colorFor` (`src/collab.js`), so the browser's list of who's editing
/// and every WordLogic, browser or desktop, agree.
pub fn color_for(client: u64) -> Color32 {
    const COLORS: [[u8; 3]; 8] = [
        [0xC0, 0x39, 0x2B],
        [0x1F, 0x6F, 0xB2],
        [0x8E, 0x44, 0xAD],
        [0xB9, 0x77, 0x0E],
        [0x14, 0x8F, 0x77],
        [0xAD, 0x14, 0x57],
        [0x2E, 0x7D, 0x32],
        [0x6D, 0x4C, 0x41],
    ];
    let [r, g, b] = COLORS.get(usize::try_from(client.saturating_sub(1) % 8).unwrap_or(0)).copied().unwrap_or(COLORS[0]);
    Color32::from_rgb(r, g, b)
}

/// A person's initials: the first letters of their first and last names.
pub fn initials(name: &str) -> String {
    let parts: Vec<&str> = name.split_whitespace().collect();
    let first = parts.first().and_then(|p| p.chars().next());
    let last = parts.get(1..).and_then(|rest| rest.last()).and_then(|p| p.chars().next());
    first.into_iter().chain(last).flat_map(char::to_uppercase).take(2).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Presence, Services};
    use serde_json::json;
    use wordcraft_engine::Session;

    fn shared(text: &str) -> WordApp {
        let mut a = WordApp::new(Session::new(wordcraft_doc::Document::from_text(text)), Services::default());
        a.session.collab_start(1, 0).unwrap();
        a
    }

    fn person(client: u64, name: &str) -> Presence {
        Presence { client, name: name.into(), color: Color32::from_rgb(0x1F, 0x6F, 0xB2) }
    }

    #[test]
    fn colours_follow_the_page() {
        assert_eq!(color_for(1), Color32::from_rgb(0xC0, 0x39, 0x2B));
        assert_eq!(color_for(2), Color32::from_rgb(0x1F, 0x6F, 0xB2));
        assert_eq!(color_for(9), color_for(1));
        assert_eq!(color_for(0), color_for(1), "no client 0, but no panic either");
    }

    #[test]
    fn initials_of_names() {
        assert_eq!(initials("Ada Lovelace"), "AL");
        assert_eq!(initials("  ben  "), "B");
        assert_eq!(initials("Jean-Luc de la Tour"), "JT");
        assert_eq!(initials(""), "");
        assert_eq!(initials("ßtraße x"), "SS", "at most two letters");
    }

    #[test]
    fn the_others_are_drawn_where_they_are() {
        let mut a = shared("Hello world\nSecond line");
        a.set_presence(vec![
            (person(2, "Ada Lovelace"), Pos::body(0, 0), Pos::body(0, 5)),
            // Past the end: put inside the document.
            (person(3, "Ben"), Pos::body(9, 99), Pos::body(9, 99)),
        ]);
        assert_eq!(a.presence.len(), 2);
        let shown = others(&a);
        assert_eq!(shown[0].2, Pos::body(0, 5));
        assert_eq!(shown[1].2, Pos::body(1, 11));
        // This person typing in Ada's selection moves its end along (what they type at its
        // start goes after it: Ada didn't type it).
        a.session.sel = wordcraft_engine::Selection::caret(Pos::body(0, 0));
        a.run("text.insert", json!({"text": "Oh, "})).unwrap();
        assert_eq!(others(&a)[0].1, Pos::body(0, 0));
        assert_eq!(others(&a)[0].2, Pos::body(0, 9));
        // Drawn: a highlight and a caret with its name tag.
        let layout = a.session.layout();
        let ctx = egui::Context::default();
        crate::theme::install_fonts(&ctx);
        let screen = Rect::from_min_size(Pos2::ZERO, vec2(2000.0, 4000.0));
        let mut out = ctx.run_ui(egui::RawInput { screen_rect: Some(screen), ..Default::default() }, |ui| {
            let painter = ui.painter().clone();
            let rects: Vec<Rect> = layout.pages.iter().map(|p| Rect::from_min_size(Pos2::ZERO, vec2(p.w, p.h))).collect();
            paint_selections(&a, &painter, &layout, &rects, 1.0);
            paint_carets(&a, &painter, &layout, &rects, 1.0);
        });
        let fills = out.shapes.iter().filter(|s| matches!(&s.shape, egui::Shape::Rect(r) if r.fill.a() == SELECTION_ALPHA)).count();
        let bars = out.shapes.iter().filter(|s| matches!(&s.shape, egui::Shape::LineSegment { .. })).count();
        let names: Vec<String> =
            out.shapes.iter().filter_map(|s| if let egui::Shape::Text(t) = &s.shape { Some(t.galley.text().to_string()) } else { None }).collect();
        out.textures_delta.clear();
        assert!(fills >= 1, "Ada's selection is highlighted");
        assert_eq!(bars, 2, "a caret each");
        assert_eq!(names, ["Ada Lovelace", "Ben"], "names show while they've just moved");
        // A while later, only a flag on each caret: names don't sit over the text.
        for (_, _, t) in a.presence_moved.values_mut() {
            *t -= TAG_SHOW_MS + 1.0;
        }
        let mut later = ctx.run_ui(egui::RawInput { screen_rect: Some(screen), ..Default::default() }, |ui| {
            let painter = ui.painter().clone();
            let rects: Vec<Rect> = layout.pages.iter().map(|p| Rect::from_min_size(Pos2::ZERO, vec2(p.w, p.h))).collect();
            paint_carets(&a, &painter, &layout, &rects, 1.0);
        });
        let texts = later.shapes.iter().filter(|s| matches!(&s.shape, egui::Shape::Text(_))).count();
        let flags =
            later.shapes.iter().filter(|s| matches!(&s.shape, egui::Shape::Rect(r) if r.rect.width() == FLAG && r.rect.height() == FLAG)).count();
        later.textures_delta.clear();
        assert_eq!((texts, flags), (0, 2), "a flag each, no names");
        // Reporting the same place again doesn't bring the name back; moving does.
        let old = a.presence_moved.get(&2).map(|m| m.2);
        a.set_presence(vec![(person(2, "Ada Lovelace"), Pos::body(0, 0), Pos::body(0, 5)), (person(3, "Ben"), Pos::body(1, 2), Pos::body(1, 2))]);
        assert_eq!(a.presence_moved.get(&2).map(|m| m.2), old, "Ada stayed put");
        assert!(a.presence_moved.get(&3).is_some_and(|m| crate::now_ms() - m.2 < TAG_SHOW_MS), "Ben moved");
        // No session: nobody.
        a.session.collab_stop();
        assert!(others(&a).is_empty());
    }
}
