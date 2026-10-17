//! Co-editing: scenarios (edits made at the same moment merge as people would expect) and a
//! convergence fuzz (whatever the timing, everyone ends with the same document).

use serde_json::{Value, json};
use wordcraft_doc::{Block, Document, Path, Pos, StoryRef};

use super::*;
use crate::{Selection, Session};

/// A server: the ordered log, and each client's position in it.
struct Server {
    log: Vec<Incoming>,
    /// With `WORDCRAFT_COLLAB_SEED` set: each log position's confirmed document as the first copy
    /// to reach it had it, to find the first entry copies disagree on.
    seen: std::cell::RefCell<std::collections::HashMap<u64, (Value, String)>>,
    check: bool,
}

impl Server {
    fn new() -> Server {
        Server { log: Vec::new(), seen: Default::default(), check: std::env::var("WORDCRAFT_COLLAB_SEED").is_ok() }
    }

    /// Takes what `s` has to send, if anything.
    fn push(&mut self, s: &mut Session) -> bool {
        let Some(out) = s.collab_outgoing() else { return false };
        let seq = self.log.len() as u64 + 1;
        self.log.push(Incoming { seq, client: out.client, client_seq: out.client_seq, base: out.base, op: out.op });
        true
    }

    /// Delivers the next entry `s` hasn't had.
    fn deliver_one(&self, s: &mut Session) -> bool {
        let next = s.collab_seq().unwrap_or(0) as usize;
        match self.log.get(next) {
            Some(e) => {
                s.collab_receive(e.clone()).unwrap_or_else(|e| panic!("receive {next}: {e}"));
                if self.check {
                    let (seq, d) = s.collab_confirmed().unwrap();
                    let c = canonical(d);
                    let t = d.plain_text(StoryRef::Body);
                    let mut seen = self.seen.borrow_mut();
                    match seen.get(&seq) {
                        Some((c0, t0)) => assert!(
                            *c0 == c,
                            "copies disagree after entry {seq} (client {}, base {}):
{t0:?}
vs
{t:?}
op: {}
prev: {:?}",
                            e.client,
                            e.base,
                            serde_json::to_string(&e.op).unwrap(),
                            seen.get(&(seq - 1)).map(|x| &x.1)
                        ),
                        None => {
                            seen.insert(seq, (c, t));
                        }
                    }
                }
                true
            }
            None => false,
        }
    }

    /// Everything sent and delivered, until nobody has anything left.
    fn settle(&mut self, all: &mut [Session]) {
        loop {
            let mut moved = false;
            for s in all.iter_mut() {
                while self.deliver_one(s) {
                    moved = true;
                }
                moved |= self.push(s);
            }
            if !moved {
                break;
            }
        }
    }
}

fn doc(lines: &[&str]) -> Document {
    Document::from_text(&lines.join("\n"))
}

/// Everyone starts from the same document.
fn people(d: &Document, n: u64) -> Vec<Session> {
    (1..=n)
        .map(|c| {
            let mut s = Session::new(d.clone());
            s.author = format!("Person {c}");
            s.collab_start(c, 0).unwrap();
            s
        })
        .collect()
}

fn at(s: &mut Session, block: usize, off: usize) {
    s.sel = Selection::caret(Pos::body(block, off));
}

fn select(s: &mut Session, block: usize, from: usize, to: usize) {
    s.sel = Selection { anchor: Pos::body(block, from), focus: Pos::body(block, to) };
}

fn run(s: &mut Session, id: &str, v: Value) {
    s.run(id, &v).unwrap_or_else(|e| panic!("{id}: {e}"));
}

fn text(s: &Session) -> String {
    s.doc.plain_text(StoryRef::Body)
}

/// A document as co-editing compares it: stories as units, revisions by value, keyed lists by
/// key, without each person's own settings.
fn canonical(d: &Document) -> Value {
    let revs = |mut items: Vec<Item>| -> Value {
        let mut op = Op {
            stories: vec![StoryEdit { story: StoryRef::Body, delta: Delta(vec![Comp::Insert { items: std::mem::take(&mut items) }]) }],
            ..Op::default()
        };
        for_each_payload(&mut op, &mut |m| {
            remap_revs(m, &mut |i| {
                // Hash the revision into the number so equal revisions compare equal.
                let r = d.revisions.get(i as usize).map(|r| format!("{}{}{:?}", r.author, r.date, r.kind)).unwrap_or_default();
                r.bytes().fold(7u32, |h, b| h.wrapping_mul(31).wrapping_add(u32::from(b)))
            })
        });
        // Runs whose revisions turned out equal by value are one run.
        let Some(Comp::Insert { items }) = op.stories.pop().and_then(|s| s.delta.0.into_iter().next()) else { return json!([]) };
        let mut merged = Vec::new();
        for it in items {
            delta::push_item(&mut merged, it);
        }
        serde_json::to_value(&merged).unwrap()
    };
    let mut parts = serde_json::Map::new();
    for (id, p) in &d.parts {
        parts.insert(id.to_string(), json!({"kind": p.kind, "body": p.body, "story": revs(story::story_items(&p.blocks).unwrap())}));
    }
    let mut rest = d.clone();
    rest.body.clear();
    rest.parts.clear();
    rest.revisions.clear();
    rest.settings.zoom = None;
    rest.styles.styles.sort_by(|a, b| a.id.cmp(&b.id));
    rest.numbering.abstracts.sort_by_key(|a| a.id);
    rest.numbering.nums.sort_by_key(|n| n.id);
    rest.sources.sort_by(|a, b| a.tag.cmp(&b.tag));
    rest.custom_props.sort_by(|a, b| a.name.cmp(&b.name));
    json!({"body": revs(story::story_items(&d.body).unwrap()), "parts": parts, "rest": rest})
}

fn assert_same(all: &[Session]) {
    let first = canonical(&all[0].doc);
    for (i, s) in all.iter().enumerate().skip(1) {
        let c = canonical(&s.doc);
        assert!(first == c, "copy {} differs from copy 1:\n{}\nvs\n{}", i + 1, text(&all[0]), text(s));
    }
    // And each shows what the server has.
    for s in all {
        assert!(!s.collab_pending());
        assert_eq!(canonical(&s.doc), canonical(s.collab_confirmed().unwrap().1));
    }
}

#[test]
fn typing_in_different_paragraphs() {
    let mut srv = Server::new();
    let mut p = people(&doc(&["one", "two"]), 2);
    at(&mut p[0], 0, 3);
    run(&mut p[0], "text.insert", json!({"text": " A"}));
    at(&mut p[1], 1, 0);
    run(&mut p[1], "text.insert", json!({"text": "B "}));
    srv.settle(&mut p);
    assert_same(&p);
    assert_eq!(text(&p[0]), "one A\nB two");
}

#[test]
fn typing_at_the_same_place_keeps_both() {
    let mut srv = Server::new();
    let mut p = people(&doc(&["ab"]), 2);
    at(&mut p[0], 0, 1);
    run(&mut p[0], "text.insert", json!({"text": "X"}));
    at(&mut p[1], 0, 1);
    run(&mut p[1], "text.insert", json!({"text": "Y"}));
    srv.settle(&mut p);
    assert_same(&p);
    let t = text(&p[0]);
    assert!(t == "aXYb" || t == "aYXb", "{t}");
}

#[test]
fn enter_while_someone_types_further_on() {
    let mut srv = Server::new();
    let mut p = people(&doc(&["Hello world"]), 2);
    at(&mut p[0], 0, 5);
    run(&mut p[0], "text.newParagraph", json!({}));
    at(&mut p[1], 0, 11);
    run(&mut p[1], "text.insert", json!({"text": "!"}));
    srv.settle(&mut p);
    assert_same(&p);
    assert_eq!(text(&p[0]), "Hello\n world!");
}

#[test]
fn joining_paragraphs_keeps_the_other_persons_typing() {
    let mut srv = Server::new();
    let mut p = people(&doc(&["first", "second"]), 2);
    at(&mut p[0], 1, 0);
    run(&mut p[0], "text.backspace", json!({}));
    at(&mut p[1], 1, 6);
    run(&mut p[1], "text.insert", json!({"text": " line"}));
    srv.settle(&mut p);
    assert_same(&p);
    assert_eq!(text(&p[0]), "firstsecond line");
}

#[test]
fn bold_and_typing_inside_it() {
    let mut srv = Server::new();
    let mut p = people(&doc(&["make this bold"]), 2);
    select(&mut p[0], 0, 5, 9);
    run(&mut p[0], "format.bold", json!({}));
    at(&mut p[1], 0, 7);
    run(&mut p[1], "text.insert", json!({"text": "!!"}));
    srv.settle(&mut p);
    assert_same(&p);
    assert_eq!(text(&p[0]), "make th!!is bold");
    let para = p[0].doc.body[0].as_para().unwrap().clone();
    let bold_at = |off: usize| para.props_of_char(off).bold == Some(true);
    assert!(bold_at(5) && bold_at(10) && !bold_at(0));
}

#[test]
fn bold_and_italic_of_the_same_text_both_stay() {
    let mut srv = Server::new();
    let mut p = people(&doc(&["styled"]), 2);
    select(&mut p[0], 0, 0, 6);
    run(&mut p[0], "format.bold", json!({}));
    select(&mut p[1], 0, 0, 6);
    run(&mut p[1], "format.italic", json!({}));
    srv.settle(&mut p);
    assert_same(&p);
    let props = p[0].doc.body[0].as_para().unwrap().props_of_char(2);
    assert_eq!((props.bold, props.italic), (Some(true), Some(true)));
}

#[test]
fn paragraph_settings_merge_by_setting() {
    let mut srv = Server::new();
    let mut p = people(&doc(&["centred and indented"]), 2);
    at(&mut p[0], 0, 0);
    run(&mut p[0], "para.alignCenter", json!({}));
    at(&mut p[1], 0, 0);
    run(&mut p[1], "para.indent", json!({}));
    srv.settle(&mut p);
    assert_same(&p);
    let props = &p[0].doc.body[0].as_para().unwrap().props;
    assert!(props.align.is_some() && props.indent_left.is_some(), "{props:?}");
}

#[test]
fn undo_takes_back_only_your_own_edit() {
    let mut srv = Server::new();
    let mut p = people(&doc(&["one", "two"]), 2);
    at(&mut p[0], 0, 3);
    run(&mut p[0], "text.insert", json!({"text": " mine"}));
    srv.settle(&mut p);
    at(&mut p[1], 1, 3);
    run(&mut p[1], "text.insert", json!({"text": " theirs"}));
    srv.settle(&mut p);
    run(&mut p[0], "edit.undo", json!({}));
    srv.settle(&mut p);
    assert_same(&p);
    assert_eq!(text(&p[0]), "one\ntwo theirs");
    run(&mut p[0], "edit.redo", json!({}));
    srv.settle(&mut p);
    assert_same(&p);
    assert_eq!(text(&p[0]), "one mine\ntwo theirs");
    // Nothing of person 2's to take back for person 1.
    run(&mut p[0], "edit.undo", json!({}));
    assert!(p[0].run("edit.undo", &json!({})).is_err());
}

#[test]
fn undo_of_typing_is_one_step() {
    let mut srv = Server::new();
    let mut p = people(&doc(&[""]), 2);
    at(&mut p[0], 0, 0);
    for c in ["a", "b", "c"] {
        run(&mut p[0], "text.insert", json!({"text": c}));
    }
    srv.settle(&mut p);
    run(&mut p[0], "edit.undo", json!({}));
    srv.settle(&mut p);
    assert_same(&p);
    assert_eq!(text(&p[0]), "");
}

#[test]
fn comments_added_at_once_both_stay() {
    let mut srv = Server::new();
    let mut p = people(&doc(&["first", "second"]), 2);
    select(&mut p[0], 0, 0, 5);
    run(&mut p[0], "review.newComment", json!({"text": "from one"}));
    select(&mut p[1], 1, 0, 6);
    run(&mut p[1], "review.newComment", json!({"text": "from two"}));
    srv.settle(&mut p);
    assert_same(&p);
    assert_eq!(p[0].doc.comments.len(), 2);
    let ids: Vec<u32> = p[0].doc.comments.keys().copied().collect();
    assert_ne!(ids[0], ids[1]);
}

#[test]
fn tables_and_lists_are_shared() {
    let mut srv = Server::new();
    let mut p = people(&doc(&["intro", "end"]), 2);
    at(&mut p[0], 0, 5);
    run(&mut p[0], "insert.table", json!({"rows": 2, "cols": 2}));
    at(&mut p[1], 1, 0);
    run(&mut p[1], "para.bullets", json!({}));
    srv.settle(&mut p);
    assert_same(&p);
    assert!(p[1].doc.body.iter().any(|b| matches!(**b, Block::Table(_))));
}

#[test]
fn read_only_people_cant_edit() {
    let mut p = people(&doc(&["fixed"]), 1);
    p[0].collab_set_read_only(true);
    at(&mut p[0], 0, 0);
    assert!(p[0].run("text.insert", &json!({"text": "x"})).is_err());
    assert_eq!(text(&p[0]), "fixed");
    assert!(p[0].collab_outgoing().is_none());
}

#[test]
fn someone_else_typing_keeps_my_caret_in_place() {
    let mut srv = Server::new();
    let mut p = people(&doc(&["abcdef"]), 2);
    at(&mut p[0], 0, 4);
    at(&mut p[1], 0, 1);
    run(&mut p[1], "text.insert", json!({"text": "XYZ"}));
    srv.settle(&mut p);
    assert_eq!(p[0].sel.focus, Pos::body(0, 7));
}

#[test]
fn snapshots_round_trip() {
    let d = crate::sample::sample_document();
    let back = from_snapshot(&snapshot(&d).unwrap()).unwrap();
    assert_eq!(canonical(&d), canonical(&back));
    assert_eq!(d.media, back.media);
}

#[test]
fn joiner_from_a_snapshot_follows_the_log() {
    let mut srv = Server::new();
    let mut p = people(&doc(&["shared"]), 1);
    at(&mut p[0], 0, 6);
    run(&mut p[0], "text.insert", json!({"text": " text"}));
    srv.settle(&mut p);
    let (seq, snap) = p[0].collab_snapshot().unwrap().unwrap();
    let mut late = Session::new(Document::new());
    late.collab_load_snapshot(&snap).unwrap();
    late.collab_start(2, seq).unwrap();
    p.push(late);
    at(&mut p[1], 0, 0);
    run(&mut p[1], "text.insert", json!({"text": "> "}));
    srv.settle(&mut p);
    assert_same(&p);
    assert_eq!(text(&p[0]), "> shared text");
}

// --- Fuzz ------------------------------------------------------------------------------------

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
}

/// A random place in the body (a paragraph at the top level, at a character boundary).
fn random_pos(s: &Session, r: &mut Rng) -> Pos {
    let paras: Vec<(usize, usize)> = s.doc.body.iter().enumerate().filter_map(|(i, b)| b.as_para().map(|p| (i, p.text.chars().count()))).collect();
    let (i, n) = paras[r.below(paras.len())];
    let p = s.doc.body[i].as_para().unwrap();
    let k = r.below(n + 1);
    let off = p.text.char_indices().nth(k).map_or(p.text.len(), |(o, _)| o);
    Pos::new(StoryRef::Body, Path::top(i), off)
}

fn random_edit(s: &mut Session, r: &mut Rng) {
    let a = random_pos(s, r);
    let b = if r.below(3) == 0 { random_pos(s, r) } else { a.clone() };
    s.sel = Selection { anchor: a, focus: b };
    let words = ["a", "bc", "def ", "ζ", "👍", "x y"];
    let (id, v) = match r.below(24) {
        16 => ("insert.table", json!({"rows": 2, "cols": 2})),
        17 => ("review.newComment", json!({"text": "note"})),
        18 => ("para.style", json!({"style": "Heading1"})),
        19 => ("edit.cut", json!({})),
        20 => ("edit.paste", json!({})),
        21 => ("text.tab", json!({})),
        22 => ("para.numbering", json!({})),
        23 => ("review.trackChanges", json!({})),
        0..=4 => ("text.insert", json!({"text": words[r.below(words.len())]})),
        5 => ("text.newParagraph", json!({})),
        6 => ("text.backspace", json!({})),
        7 => ("text.delete", json!({})),
        8 => ("format.bold", json!({})),
        9 => ("format.italic", json!({})),
        10 => ("para.alignCenter", json!({})),
        11 => ("para.bullets", json!({})),
        12 => ("edit.undo", json!({})),
        13 => ("edit.redo", json!({})),
        14 => ("format.color", json!({"color": "C00000"})),
        _ => ("text.lineBreak", json!({})),
    };
    let _ = s.run(id, &v);
}

fn fuzz(seed: u64, steps: usize, n: u64) {
    let mut r = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
    let start = doc(&["The quick brown fox", "jumps over", "the lazy dog"]);
    let mut p = people(&start, n);
    let mut srv = Server::new();
    for _ in 0..steps {
        let who = r.below(p.len());
        let last = format!("{:?}", p[who].last_command);
        let kind = r.below(6);
        let kind_name = ["edit", "edit", "edit", "push", "deliver", "deliver"][kind];
        match kind {
            0..=2 => random_edit(&mut p[who], &mut r),
            3 => {
                srv.push(&mut p[who]);
            }
            _ => {
                for _ in 0..=r.below(3) {
                    srv.deliver_one(&mut p[who]);
                }
            }
        }
        if srv.check {
            for (i, s) in p.iter().enumerate() {
                let want = s.collab.as_ref().unwrap().replica.expected();
                assert!(
                    canonical(&s.doc) == canonical(&want),
                    "copy {} drifted after {kind_name} by copy {} (last command {last}): {}",
                    i + 1,
                    who + 1,
                    first_diff(&canonical(&want), &canonical(&s.doc))
                );
            }
        }
    }
    srv.settle(&mut p);
    for (i, s) in p.iter().enumerate() {
        assert!(
            canonical(&s.doc) == canonical(s.collab_confirmed().unwrap().1),
            "seed {seed}: copy {} shows something else than it has confirmed: {}",
            i + 1,
            serde_json::to_string(&capture(s.collab_confirmed().unwrap().1, &s.doc).unwrap()).unwrap()
        );
    }
    let first = canonical(&p[0].doc);
    for (i, s) in p.iter().enumerate().skip(1) {
        assert!(first == canonical(&s.doc), "seed {seed}: copy {} differs:\n{:?}\nvs\n{:?}", i + 1, text(&p[0]), text(s));
    }
}

#[test]
fn copies_converge_whatever_the_timing() {
    let seeds: u64 = std::env::var("WORDCRAFT_COLLAB_SEEDS").ok().and_then(|v| v.parse().ok()).unwrap_or(150);
    let steps: usize = std::env::var("WORDCRAFT_COLLAB_STEPS").ok().and_then(|v| v.parse().ok()).unwrap_or(120);
    if let Some(seed) = std::env::var("WORDCRAFT_COLLAB_SEED").ok().and_then(|v| v.parse().ok()) {
        return fuzz(seed, steps, 3);
    }
    for seed in 1..=seeds {
        fuzz(seed, steps, 3);
    }
}

// --- Deltas ----------------------------------------------------------------------------------

fn ins(s: &str) -> Comp {
    Comp::Insert { items: vec![Item::Text { s: s.into(), p: Default::default() }] }
}

#[test]
fn transform_satisfies_tp1_on_text() {
    use delta::{Builder, apply_segs};
    use seq::{Seg, blocks_from_segs, push_segs};
    let base = doc(&["abcdef", "ghij"]);
    let cases: Vec<Delta> = vec![
        Delta(vec![Comp::Retain { n: 2 }, ins("X")]),
        Delta(vec![Comp::Retain { n: 1 }, Comp::Delete { n: 3 }]),
        Delta(vec![Comp::Retain { n: 6 }, Comp::Delete { n: 1 }]),
        Delta(vec![Comp::Retain { n: 3 }, ins("Y"), Comp::Delete { n: 5 }]),
        Delta(vec![Comp::Retain { n: 2 }, Comp::Format { n: 3, p: [("bold".to_string(), json!(true))].into_iter().collect() }]),
        Delta(vec![Comp::Retain { n: 6 }, Comp::Para { p: [("p.align".to_string(), json!("center"))].into_iter().collect() }]),
        Delta(vec![Comp::Retain { n: 2 }, Comp::Format { n: 3, p: [("bold".to_string(), json!(false))].into_iter().collect() }]),
        Delta(vec![ins("Q"), Comp::Retain { n: 11 }, ins("Z")]),
    ];
    let segs = || {
        let mut v: Vec<Seg> = Vec::new();
        push_segs(&base.body, &mut v);
        v
    };
    let applied = |d: &[&Delta]| {
        let mut s = segs();
        for x in d {
            s = apply_segs(s, &x.0).unwrap();
        }
        let blocks = blocks_from_segs(s);
        serde_json::to_value(story::story_items(&blocks).unwrap()).unwrap()
    };
    for a in &cases {
        for b in &cases {
            let a2 = transform(a, b, true);
            let b2 = transform(b, a, false);
            assert_eq!(applied(&[b, &a2]), applied(&[a, &b2]), "a={a:?} b={b:?}");
        }
    }
    let _ = Builder::default();
}

#[test]
fn compose_is_applying_one_then_the_other() {
    use delta::apply_segs;
    use seq::{Seg, blocks_from_segs, push_segs};
    let base = doc(&["abcdef", "ghij"]);
    let a = Delta(vec![Comp::Retain { n: 2 }, ins("XY"), Comp::Delete { n: 2 }]);
    let b = Delta(vec![Comp::Retain { n: 3 }, Comp::Delete { n: 2 }, ins("Z")]);
    let mut s: Vec<Seg> = Vec::new();
    push_segs(&base.body, &mut s);
    let one = apply_segs(apply_segs(s.clone(), &a.0).unwrap(), &b.0).unwrap();
    let both = apply_segs(s, &compose(&a, &b).unwrap().0).unwrap();
    assert_eq!(blocks_from_segs(one), blocks_from_segs(both));
}

/// Where two JSON values first differ, with some context.
fn first_diff(a: &Value, b: &Value) -> String {
    let (a, b) = (a.to_string(), b.to_string());
    let at = a.bytes().zip(b.bytes()).take_while(|(x, y)| x == y).count();
    let from = at.saturating_sub(200);
    let cut = |s: &str| s.get(from..(at + 200).min(s.len())).unwrap_or("").to_string();
    format!("at {at}:\n{}\nvs\n{}", cut(&a), cut(&b))
}
