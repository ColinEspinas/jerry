# Screenshots

Real captures of the running app, taken with per-window `XGetImage` against the app's own X11
toplevel. See `BUILD-LOG.md`'s screenshot correction for the recipe and for what does *not* work
(root-window capture returns black; `weston_screenshooter` is permission-denied; GPUI's
`render_to_image` has no Linux implementation).

| file | theme | what it shows |
|---|---|---|
| `jerry-dark-rust.png` | Jerry Dark | A real Rust file. Function **definition** name in blue, method calls (`.arg`, `.stdin`, `.spawn`, `.expect`) and locals (`child`, `descendants`, `pid`) at plain foreground, types (`Command`, `Stdio`, `Duration`, `Signal`) in cyan, strings in green, numbers in amber, keywords (`let`, `mut`, `fn`, `use`, `for`) at plain foreground, brackets on the depth ring. |
| `jerry-dark-rust-doc-comments.png` | Jerry Dark | The comment-readability fix. A screenful of `///` doc comments at the brighter `comment_doc` tone, plus screaming-case constants in amber and `usize`/`i64`/`u32` in cyan. |
| `paper-chrome.png` | Paper | The light theme's chrome. **No editor tab is open in this one** — it does not show syntax colours. |

## What is missing, and why

There is no scripted way to open a *chosen* file in the editor: the app takes a repo path as
`argv[1]`, not a file path, and `xdotool type --window` mangles characters. The captures above were
obtained by driving the file tree with mouse events, which works but is not reliable enough to
automate per-fixture.

So there are no per-fixture screenshots and no Paper-with-code capture. The substitute is
`fixture_corpus_tests` in `crates/app/src/code_surface/code_view.rs`, which dumps for every fixture
exactly which bucket each byte lands in and exactly what colour and contrast ratio that resolves to
— the thing a screenshot would be inspected *for*. Run it with `--nocapture`.
