# Screenshots

Real captures of the running app, taken with per-window `XGetImage` against the app's own X11
toplevel. See `BUILD-LOG.md`'s screenshot correction for the recipe and for what does *not* work
(root-window capture returns black; `weston_screenshooter` is permission-denied; GPUI's
`render_to_image` has no Linux implementation).

| file | theme | what it shows |
|---|---|---|
| `jerry-dark-rust.png` | Jerry Dark | A real Rust file (`crates/lsp-core/src/proc.rs`, lines 136-170) under the **final** palette. Keywords (`mod`, `use`, `let`, `mut`, `fn`, `for`) in **purple**, method calls (`.arg`, `.stdin`, `.spawn`, `.expect`) and `collect_descendant_pids` in **blue**, the function *definition* name in **violet-blue**, types (`Command`, `Stdio`, `Duration`, `Signal`) in **gold**, locals (`child`, `descendants`, `sh_pid`, `pid`) in **rose**, strings in green, `200`/`SIGKILL` in orange, `#[cfg(all(test, unix))]` in **teal**, brackets on the depth ring at the punctuation lightness. |
| `jerry-dark-rust-doc-comments.png` | Jerry Dark | **Predates the final palette** — kept only as the "before" side of the §2b evidence below. A screenful of `///` doc comments at the brighter `comment_doc` tone. Its screaming-case constants render in `syntax.type`'s cyan, which is the misclassification `RUST_CONSTANT_SUPPLEMENT` fixes; this README used to describe them as "amber", which was simply wrong. |
| `jerry-dark-markdown.png` | Jerry Dark | Markdown prose under the **final** palette — this file's own `THEME.md`, so it doubles as a check that the spec and the render agree. Headings in **gold**, inline code in green, bold at the brighter neutral. The fenced block visible is **untagged**, so it shows no language injection — per-fence injection is covered by `fixture_corpus_tests`. |
| `paper-chrome.png` | Paper | The light theme's chrome. **No editor tab is open in this one** — it does not show syntax colours. |
| `completions-popup-top.png` | Jerry Dark | The Completions popup (GitHub issue #185) against a **real, live rust-analyzer** response — `Ctrl+Space` at `s.` where `s: String`, so the list is every real method on `String`, well over a hundred items. Twelve rows visible (`MAX_VISIBLE_COMPLETION_ROWS`), the overlay scrollbar's thumb parked at the top, real signatures in the right-hand detail column of each row. |
| `completions-popup-scrolled.png` | Jerry Dark | The same popup after 30 real `Down` keystrokes. A completely different set of rows, `replace_range` selected on the bottom row, and the thumb moved down the track — item 30 was **permanently unreachable** before this fix, which hard-capped rendering at 12 items with no scroll mechanism at all. |
| `completions-unfiltered.png` | Jerry Dark | The Completions popup (GitHub issue #189) against a **real, live rust-analyzer**, right after `Ctrl+Space` at `v.` where `v: Vec<u8>` — nothing typed past the trigger point yet, so it shows the server's own broad candidate set in the server's own order: `reverse`, `clone(as Clone)`, `sort_unstable`, `insert`, `trim_ascii_start`, `utf8_chunks`, … |
| `completions-filtered.png` | Jerry Dark | The **same popup after typing three real characters**, `res`. Every row now genuinely matches: `resize_with`, `reserve_exact`, `reserve`, `resize`, the two `try_reserve*` rows, and the fuzzy (non-contiguous) `reverse`/`iter().rev()` matches. `clone`, `sort_unstable`, `insert`, `utf8_chunks` are gone. The status bar reads `1 servers`, i.e. a real server answered — these are not mock items. |

## The completions captures

Driven the same way as everything else here (`python-xlib` XTEST clicks into the file tree, then
per-window `XGetImage`), against a throwaway cargo project rather than this repo — a small crate is
what makes rust-analyzer reach a real, answering state in seconds instead of minutes. The
`Ctrl+Space` force-invoke (`CompletionsInvoke`) is what makes this scriptable at all: it needs three
key events, where reproducing the same popup by *typing* a prefix would run into the `xdotool
type`/XTEST character-mangling limitation described below.

The `completions-popup-top`/`-scrolled` pair was captured against a two-file project; the status
bar in those uncropped frames reads `1 servers · 3 errors`, the real server answering with real
diagnostics for the deliberately incomplete `s.` — not mock items.

The `completions-unfiltered`/`-filtered` pair was captured separately, against a one-file project,
using single XTEST key events for the three characters typed after the trigger (which do work,
unlike the `xdotool type` mangling described below). **What this pair does and does not prove.** It
is real, direct evidence that the popup visibly narrows as the user types, which is exactly the
symptom issue #189 reported. It is *not* on its own proof that the narrowing is **client-side**: the
50ms debounced `textDocument/completion` re-request also fires in that window, and a capture cannot
be timed between the keystroke and the round trip. That half is carried by the real tests
(`typing_past_the_trigger_point_narrows_the_real_completions_list` and its two siblings in
`crate::code_surface::editing::editing_tests`), which run with **no LSP client at all** and never
advance the debounce clock — so only `AdeApp::refilter_completions` can explain the narrowing they
observe. Both frames also carry a mild double-drawn-text artifact from `XGetImage` grabbing a
partially-updated GL surface; it is a capture artifact, not how the app renders on screen.

## Verified by pixel diff, not by eye

`jerry-dark-rust.png` has now been replaced twice, each time by a capture of the *same file at the
same scroll position*, and each time the diff was the evidence rather than the eye.

**Round 2 (the final palette).** Against the previous capture, over the code column only:
**21,097 pixels changed**. The dominant transitions are exactly the intended ones:

| from | to | pixels | meaning |
|---|---|---|---|
| `#acb2bc` | `#74ade8` | 315 | plain foreground → **function/method call blue** |
| `#acb2bc` | `#c194d6` | 136 | plain foreground → **keyword purple** |
| `#de99be` | `#da8db2` | 231 | old rose → new rose (locals) |
| `#8bc18c` | `#98b46a` | 176 | old string green → new |
| `#88b4ed` | `#a19fe8` | 129 | old definition blue → violet-blue |
| `#5ec4c4` | `#c7a356` | 85 | type cyan → **type gold** |
| `#d8a76d` | `#4bbeb1` | 74 | attribute amber → **attribute teal** |

The first two rows are the whole point: 451 pixels of text that was literally indistinguishable
from prose now carry a real hue.

**Round 1 (identifiers).** Against a capture taken before variables were given colour: **3297
pixels changed**, dominated by `#acb2bc -> #de99be` (231 instances), with **zero**
`#d8a76d -> #de99be` (the attribute regression, 65 instances in an intermediate build, now fixed).

That first diff is also what caught the underlying bug: an earlier attempt at the same palette
change produced **zero** changed pixels, which is how `tree-sitter-rust`'s missing blanket
`(identifier) @variable` rule was found. See `THEME.md` §2.

Sampling the *committed* screenshots directly is also how `THEME.md` §2b's Rust-constant
misclassification was confirmed rather than inferred: `WRITE_TIMEOUT` in
`jerry-dark-rust-doc-comments.png` measures `#5ec4c4`, which was `syntax.type`, not
`syntax.constant`.

## What is missing, and why

There is no scripted way to open a *chosen* file in the editor: the app takes a repo path as
`argv[1]`, not a file path, and `xdotool type --window` mangles characters. The captures above were
obtained by driving the file tree with mouse events, which works but is not reliable enough to
automate per-fixture.

The captures here were driven with `python-xlib` XTEST clicks (`warp_pointer` + `fake_input`) into
the window's own root-relative origin, then per-window `XGetImage`. That works for the file tree and
the scroll wheel; it is not reliable enough for arbitrary navigation. Repeated attempts to
re-capture `client.rs` at line 181 landed on the diagnostics list the file view appends below the
end of a file (rust-analyzer emits ~1000 `inactive-code` diagnostics for that file) and could not be
driven back out, which is why the `jerry-dark-rust-doc-comments.png` row above is still the old
image.

So there are no per-fixture screenshots and no Paper-with-code capture. The substitute is
`fixture_corpus_tests` in `crates/app/src/code_surface/code_view.rs`, which dumps for every fixture
exactly which bucket each byte lands in and exactly what colour and contrast ratio that resolves to
— the thing a screenshot would be inspected *for*. Run it with `--nocapture`.
