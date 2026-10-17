# Real-time co-editing: design

Status: engine done (phase 1), 2026-10-17. Fork-specific (WordLogic, Aegean Logic). Not upstream
behaviour. Sibling of GridLogic's `docs/co-editing.md`: same server, same log protocol, a
different merge for text.

Goal: several people edit one document at once, as in Word for the web. Each person sees the
others' edits within about a second, everyone ends with the same document, and Undo takes back
only your own edits. First host: Nextcloud (`../wordcraft-nextcloud`), through the web build's
host bridge.

## 1. What an edit is

Every edit is a command, and `Session::run` is the single entry point (`crates/engine/src/session.rs`).
In a shared document `collab::run_shared` wraps the outermost command: it diffs the document
before and after and shares the difference as an `Op`. Every command (ribbon, keys, paste, agents
through MCP) is shared without code of its own.

An `Op` (`crates/engine/src/collab/mod.rs`) has:

| Part | What | Merge |
|---|---|---|
| `stories` | For each changed story (body, header, footer, note, comment text, text box), a `Delta` over its flat sequence | Operational transform, character by character |
| `added` / `removed` | Parts made (with their content) and removed | Ids can't collide (below); an edit of a removed part goes |
| `fields` | Everything else as changed keys: settings and page setup by setting, styles by id, lists by id, comments by id, sources, custom properties, pictures by media key, part settings | The later change of a key wins; different keys both stay |
| `revs` | The tracked-change revisions the payloads name, by value | Each copy numbers revisions its own way |

### The flat sequence (`collab/seq.rs`)

A story is one sequence of units: each character (an inline object's U+FFFC included), each
paragraph mark (carrying the paragraph's formatting, its mark's formatting and any section
settings), and each table (one unit). This is the model Quill uses, and it is why splitting and
joining paragraphs merge well:

- **Enter** inserts a mark; **Backspace at the start of a paragraph** deletes one. Someone typing
  in that paragraph at the same moment keeps their text where they typed it, in whichever
  paragraph it ends up (`tests::enter_while_someone_types_further_on`,
  `tests::joining_paragraphs_keeps_the_other_persons_typing`).
- **Formatting** is a shallow patch of the keys that changed (`bold`, `color`, `p.align`,
  `m.size`…). One person bolding and another italicising the same words both stay; two people
  setting the same key, the later one wins.

### The delta (`collab/delta.rs`)

Steps: keep `n`, insert items, delete `n`, patch the character formatting of `n`, patch a
paragraph mark, replace an inline object, replace a table. With `transform` (TP1, deterministic
tie-breaking by log order), `compose` (typing between two round trips goes as one op) and
`invert` (undo). Payloads are JSON, so model fields added upstream travel without changes here.

### Diffing (`collab/story.rs`)

Blocks an edit didn't touch are the same `Arc`s before and after (`Arc::make_mut`), so the diff
skips them, aligns the untouched ones (longest increasing run), and diffs only the gaps, unit by
unit (common start and end, formatting changes on what's kept). Only the blocks an op touches are
taken apart and rebuilt when it's applied.

## 2. Ordering and convergence (`collab/replica.rs`)

Server-ordered, client-transformed, one op in flight per client (the model GridLogic uses; the
Nextcloud sequencer is shared code):

- Each op carries `base`, the last log position its author had applied. Log op `n` is rebased
  over entries `base+1 .. n-1` by every copy alike, so convergence only needs a deterministic
  `transform`.
- Local edits are optimistic. A copy keeps `confirmed` (the document at its last log position),
  its op in flight and the edits since. Someone else's op is transformed past the pending edits
  and applied straight to what's shown; when this person's own op comes back after such a
  shortcut, what's shown is rebuilt from `confirmed` + pending.
- The caret and selection stay where they were through others' edits (`Delta::map_pos`); text
  someone types at your caret goes after it.
- A copy missing log history must rejoin rather than guess.

**Proof:** `collab::tests::copies_converge_whatever_the_timing`: random 3-person sessions (typing,
Enter, Backspace/Delete across paragraphs, bold/italic/colour, alignment, bullets, numbering,
headings, tables, comments, cut and paste, track changes, undo/redo) with random delivery timing;
every copy ends with the same document and shows exactly what it has confirmed. In tests, every
captured op is also checked to reproduce its edit exactly. Soak with
`WORDCRAFT_COLLAB_SEEDS`/`WORDCRAFT_COLLAB_STEPS`; replay one with `WORDCRAFT_COLLAB_SEED`, which
also checks every copy after every step and every log position across copies.

## 3. Ids that can't collide

Parts, comments and lists get ids "one past the largest", and pictures get `imageN` keys; two
people adding one at the same moment would pick the same. While a shared command runs, the doc
crate's `IdSpace` (`wordcraft_doc::with_id_space`) gives each person their own range: `0x1000_0000
+ (client mod 2048) << 16`, 65,536 ids each, still below 2³¹ for Word's signed ids. Media keys get
a `c<range>-` prefix. (`tests::comments_added_at_once_both_stay`.)

## 4. Undo

Snapshot undo would take back other people's work, so in a shared document Undo and Redo are each
person's own (`Replica::undo`): an edit's inverse goes on their stack, is rebased over everything
that happens afterwards (where someone changed the same thing later, their change stays) and is
shared like any edit. Typing is one undo step, as without co-editing. `Session::can_undo`,
`undo_history` and the Undo list follow the shared stack.

## 5. Joining later: snapshots

A `.docx` written as a checkpoint doesn't read back with the same part, comment and list numbers,
so a checkpoint keeps beside the file an exact **state snapshot** of the confirmed document
(`collab::snapshot`, JSON with pictures as base64). Someone joining at that checkpoint opens the
snapshot (`Session::collab_load_snapshot`) and follows the log from its position. A fresh session
(log position 0) opens the file itself: everyone reads the same bytes the same way.

## 6. Session API

`collab_start(client, seq)`, `collab_load_snapshot`, `collab_outgoing` → `Outgoing`,
`collab_receive(Incoming)`, `collab_refused` (the server refused the op in flight for good),
`collab_snapshot` / `collab_confirmed` (checkpoints), `collab_set_read_only` (edits are taken back
with a message), `collab_presence`, `collab_stop`. An op over 8 MB (`MAX_SHARED_OP_BYTES`) is taken
back with a message. Opening another document in the same window ends sharing.

## 7. Accepted limits

- **Tables merge whole.** Two people editing the same table at the same moment: the later change to
  that table wins (edits elsewhere are unaffected). Cell-by-cell merging is the next step.
- **An object changed twice at once** (the same picture resized by two people): the later wins.
- **Revisions by value.** Two tracked changes with the same author, kind and timestamp (to the
  second) are one revision on other copies.
- Fields (page numbers, dates) and layout are computed by each copy.

## 8. Next

| Phase | Scope |
|---|---|
| 2. Transport | Bridge messages in `apps/wordcraft-web` (`wordcraft:collab*`, as in `../wordcraft-nextcloud` README § Protocol); checkpoint writer with snapshot |
| 3. Presence | Others' carets and selections with names and colours; who's editing |
| 4. Sharing and desktop | Share dialog through Nextcloud; Nextcloud in the desktop app's File view; Open in Desktop App (`wordlogic://`) |
| 5. Hardening | Cell-by-cell table merging; large-op caps tuned; performance on very long documents |
