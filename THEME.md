# THEME.md — Jerry's syntax palette

The full specification of Jerry Dark's syntax palette: every colour in OKLCH, its role, and its
measured WCAG contrast against the editor background (`surface.center`, `#131518`).

Jerry Dark is the *source* palette. The five other bundled themes are generated from it
programmatically (`crates/app/src/settings/builtin_themes.rs`); they are never hand-edited.
Regenerate with:

```
JERRY_REGENERATE_THEMES=1 cargo test -p app --lib builtin_themes -- --nocapture
```

---

## 1. The design, in one paragraph

Six accent hues plus two identifier tints, all at one OKLCH lightness (**L 0.760**) and one chroma
(**C 0.095**), differing only in hue. Function calls and keywords render at plain foreground;
ordinary identifiers are genuinely tinted. Punctuation sits deliberately below plain foreground.
Comments are held to the full body-text contrast floor, not a relaxed one. The bracket-pair ring is
a seventh set of six hues held **under** the accents in both lightness and chroma, so a bracket can
never shout louder than a string.

The colour → meaning mapping is meant to be recitable from memory:

| hue | OKLCH H | means |
|---|---|---|
| green | 145 | strings |
| amber | 70 | constants, numbers, booleans, `self`, attributes |
| blue | 255 | function/method **definition sites**, links |
| cyan | 195 | types, JSX tags, markdown headings |
| magenta | 320 | parameter bindings, markdown emphasis |
| red | 25 | diagnostics only |
| rose | 346 | ordinary variables *(identifier tint)* |
| cyan-blue | 225 | property / member access *(identifier tint)* |

---

## 2. Variables: coloured, after a reversal

This is the one genuinely contested decision, and the record of how it moved matters more than the
answer.

### What was tried first, and why it was reasonable

The redesign initially held variables, property accesses, function calls **and** keywords at plain
foreground, following tonsky's [*I am sorry, but everyone is getting syntax highlighting
wrong*](https://tonsky.me/blog/syntax-highlighting/):

> I don't highlight variables or function calls — they are everywhere, your code is probably 75%
> variable names and function calls.

with the line drawn not at identifiers but at **use sites versus binding sites**:

> Notice that we've kept variable declarations. These are not as ubiquitous and help you quickly
> answer a common question: where does this thing come from?

[Motlin](https://motlin.medium.com/how-to-pick-colors-for-a-syntax-highlighting-theme-96d3e06c19dc)
does not actually contradict this. His claim is *relational* — "Keywords, parameters, locals, and
fields should use distinct colors", i.e. *if* you colour these they must differ from each other —
and his own principle ("Contrast is a scarce resource which we only spend to resolve ambiguity",
plus his escape-sequence argument that context can substitute for contrast) points the same way.

### Why it was reversed

The maintainer looked at the rendered result and rejected it — twice, independently, in the same
words both times: *"most of the text is just white."*

**The reasoning above was sound; the thing it produced was not what this editor's user wants to look
at.** That is a legitimate verdict from the person who reads this screen all day, and it is recorded
here as a reversal rather than smoothed into the original argument, because a future reader
comparing this palette against tonsky's essay deserves to know the difference was deliberate.

So: **`syntax.variable` and `syntax.property` carry real colour.** `syntax.function` /
`syntax.function_method` (call sites) and `syntax.keyword` stay at plain foreground — the complaint
was specifically about identifiers reading as an undifferentiated wall, not about calls.

### The bug hiding underneath, which is the real lesson

The first attempt at this reversal changed `syntax.variable` and **nothing happened on screen.**

`tree-sitter-rust`'s bundled query has no blanket `(identifier) @variable` rule. Its only
`@variable`-family pattern is `(parameter (identifier) @variable.parameter)`. Every other grammar
here has one — `tree-sitter-python` at line 3, `-javascript` at line 4, `-go` at line 26, `-c` at
line 1. Rust was the sole outlier, so a plain Rust local classified as unstyled `Text`, and
`syntax.variable` was **literally unreachable in this app's primary language.**

Two rounds of palette work aimed at that token could not have moved a single pixel of a Rust file.
The maintainer's complaint was correct both times, and both times it was diagnosed as a colour
problem when it was a *classification* problem.

No contrast or ΔE calculation could have caught this. What caught it was taking a screenshot after
the change, diffing it against the one before, and finding **zero changed pixels** in the code area.
`RUST_VARIABLE_PREFIX` is the fix; `a_plain_rust_local_is_a_real_variable_not_unclassified_text`
pins it.

That fix in turn caused one real regression — the blanket rule claimed the identifiers inside
`#[cfg(all(test, unix))]`, because `fold_highlight_events` resolves each byte to its *innermost*
open highlight and the bundled query only captures the enclosing `attribute_item`. Also found by
pixel diff (65 pixels moving the wrong way alongside 231 moving the right way) and fixed by
`RUST_ATTRIBUTE_SUPPLEMENT`.

### Where the two authors genuinely disagree

Motlin's strongest case is a bare `count++` where you cannot tell a field from a local without
scrolling. This palette now answers it: `syntax.property` is a cool cyan-blue against
`syntax.variable`'s warm rose — the warm/cool split that makes an `a.b.c` chain legible at a glance.
`syntax.variable_parameter` stays in the rose family, deeper and more saturated, saying "still a
variable, just a distinguished one".

---

## 3. Contrast floors

Two tiers, enforced by `theme::syntax_contrast_floor` and asserted on the real checked-in files:

| tier | floor | applies to |
|---|---|---|
| body text | **4.5:1** | every syntax foreground |
| de-emphasized | **3:1** | `operator`, `punctuation_bracket`, `punctuation_delimiter`, `bracket_1..6` |

The de-emphasized tier is not a relaxation for convenience — being quieter than the code is the
whole job of punctuation, and holding it to body-text contrast would defeat it.

Two floors that previously failed and now hold:

- `syntax.comment` was **3.03:1** — a real ghost-grey failure, the clearest legibility bug the audit
  found. It is now **4.65:1**.
- Across the five derived themes, `Ember` had 24 of 39 syntax tokens below 4.5:1 and `Slate` 22,
  some as low as 2.15:1. All six bundled themes now clear both tiers.

---

## 4. Deriving the other five themes

`derive_shift` solves an OKLCH transform from two themes' five preview swatches: lightness as a
linear fit through the two background swatches, hue as the circular mean of the three chromatic
swatches, chroma as their mean ratio.

**This used to be HSL, and that was measurably wrong.** HSL's `l` is not lightness — a hue rotation
at constant HSL saturation swings perceived chroma wildly, and a saturated blue and a saturated
yellow share an `l` while one is obviously darker. Derived through it, the palette lost most of its
contrast, and `Paper` collapsed `text.selected`, `text.primary` and `text.heading` into pure black.
Those three are now `#3b3e41` / `#414448` / `#484c4f` — genuinely distinct, pinned by a test rather
than assumed as a side effect of the port.

OKLCH alone was not sufficient. The lightness fit is solved from *background* swatches, so a theme
whose window and panel backgrounds sit close together compresses the whole range and pulls
foregrounds toward the background. `enforce_syntax_contrast_floors` fixes this at authoring time: it
pushes any offending colour away from the background **in lightness only**, holding hue and chroma,
binary-searching the smallest move that clears the floor. It measures the *quantized* colour,
because a theme file stores `#rrggbb` and an 8-bit rounding step is applied to whatever it produces.
It is one-directional — it only ever increases contrast — so a theme that already reads well is
returned untouched.

`shift_from_seed` (the Themes page's "Generate from colour") shares the same maths and rotates
against `SEED_REFERENCE_ACCENT`, which moved to the new blue `#88b4ed` in lockstep: under this
palette `syntax.function` is a near-neutral grey whose hue is numerically meaningless, so leaving
the constant pointed at it would have produced garbage from something that still looked plausible.

---

## 5. The bracket-pair depth ring

Six hues at one lightness (**L 0.700**) and one chroma (**C 0.080**), chosen by `nesting depth % 6`,
both halves of a pair alike. A bracket with no matching partner keeps the de-emphasized punctuation
tone instead, so malformed or mid-edit code degrades visibly-but-quietly rather than lying about
structure.

The ring sits under the accents on every axis: dimmer (0.700 vs 0.760) and less saturated (0.080 vs
0.095). Its hues sit at the *midpoints* between the accent hues, so no ring colour lands on top of a
semantic one.

Measured across every bundled theme (CIE-Lab ΔE; ~2.3 is a just-noticeable difference):

| comparison | worst case |
|---|---|
| cyclically adjacent depths (`n` vs `n+1`) | 20.7 |
| any ring colour vs plain text | 16.7 |
| any ring colour vs the de-emphasized bracket tone | 26.5 |
| any ring colour vs any semantic accent | 11.7 (`Paper`) |
| contrast against the editor background | ≥ 3:1, all themes |

These floors are lower than the ring this replaced was held to, and that is the design working
rather than a regression: ΔE is bought with chroma, and six hues at one deliberately low chroma have
a hard ceiling on how far apart they can be. The previous ring reached higher numbers by being the
most saturated thing in the palette — which a maintainer caught by looking at the real thing, and
which none of the ΔE-only checks could see.

**Precedence, stated explicitly.** The ring applies to bracket *tokens* only: it rewrites spans the
grammar itself classified `punctuation.bracket`, so a `{` inside a string or a comment is invisible
to it by construction rather than by a string-skipping heuristic. It never interacts with diagnostic
styling, because diagnostics are a different channel entirely — a row background tint
(`syntax.diagnostic_row_bg`) and an underline decoration (`syntax.error_underline`), never a text
foreground.

**`<` and `>` deliberately do not participate.** They really do arrive as `punctuation.bracket`
(Rust and TypeScript capture type-argument brackets that way; `tree-sitter-html` captures a tag's
own). Tracking them would make HTML actively wrong — `<div>` and `</div>` are two same-level pairs,
not one open/close pair, so a stack matcher would paint a whole document flat depth-0 while implying
structure that is not there. Generics therefore render as plain punctuation, which is a decision to
decline to guess rather than an omission.

**Injected regions pair independently.** Matching runs one separate stack per injected region, so a
`{` in one fenced code block can never pair with a `}` in the next, and an unbalanced fence cannot
shift the ring for the fences after it.

**Toggle.** `appearance.bracket_pair_colorization` (default on). When off, the pass genuinely does
not run and brackets render with the de-emphasized punctuation style.

---

## 6. Every syntax token

`L`/`C`/`H` are OKLCH. Contrast is WCAG 2.x against the **real** editor background, `surface.pty`
`#0d0f11` — which is what `code_surface::file_view` actually paints behind code.

Note a deliberate conservatism: `enforce_syntax_contrast_floors` measures against `surface.center`
`#131518` instead, which is *lighter*. Every ratio below is therefore better than the one the guard
enforced, and no floor can be violated by the difference. Worth correcting one day; harmless as it
stands, and recorded rather than quietly left as a discrepancy between the docs and the code.

| token | hex | L | C | H | contrast |
|---|---|---|---|---|---|
| `syntax.text` | `#acb2bc` | 0.762 | 0.016 | 261 | 9.01:1 |
| `syntax.keyword` | `#acb2bc` | 0.762 | 0.016 | 261 | 9.01:1 |
| `syntax.function` | `#acb2bc` | 0.762 | 0.016 | 261 | 9.01:1 |
| `syntax.function_method` | `#acb2bc` | 0.762 | 0.016 | 261 | 9.01:1 |
| `syntax.function_definition` | `#88b4ed` | 0.760 | 0.095 | 255 | 8.96:1 |
| `syntax.type` | `#5ec4c4` | 0.760 | 0.095 | 195 | 9.30:1 |
| `syntax.type_builtin` | `#5ec4c4` | 0.760 | 0.095 | 195 | 9.30:1 |
| `syntax.constant` | `#d8a76d` | 0.761 | 0.095 | 70 | 8.84:1 |
| `syntax.constant_builtin` | `#d8a76d` | 0.761 | 0.095 | 70 | 8.84:1 |
| `syntax.string` | `#8bc18c` | 0.759 | 0.094 | 145 | 9.24:1 |
| `syntax.string_escape` | `#a8e0a9` | 0.854 | 0.095 | 145 | 12.71:1 |
| `syntax.number` | `#d8a76d` | 0.761 | 0.095 | 70 | 8.84:1 |
| `syntax.comment` | `#7a818a` | 0.600 | 0.016 | 255 | 4.88:1 |
| `syntax.comment_doc` | `#8c939c` | 0.660 | 0.016 | 255 | 6.19:1 |
| `syntax.variable` | `#de99be` | 0.761 | 0.095 | 346 | 8.60:1 |
| `syntax.variable_parameter` | `#cc9ed7` | 0.761 | 0.094 | 320 | 8.63:1 |
| `syntax.variable_builtin` | `#d8a76d` | 0.761 | 0.095 | 70 | 8.84:1 |
| `syntax.property` | `#68bedf` | 0.760 | 0.096 | 225 | 9.16:1 |
| `syntax.operator` | `#6f757e` | 0.560 | 0.016 | 258 | 4.13:1 |
| `syntax.punctuation_bracket` | `#6f757e` | 0.560 | 0.016 | 258 | 4.13:1 |
| `syntax.punctuation_delimiter` | `#6f757e` | 0.560 | 0.016 | 258 | 4.13:1 |
| `syntax.bracket_1` | `#c88a9c` | 0.700 | 0.079 | 0 | 6.92:1 |
| `syntax.bracket_2` | `#c4936b` | 0.701 | 0.080 | 60 | 7.07:1 |
| `syntax.bracket_3` | `#98a66d` | 0.700 | 0.080 | 120 | 7.33:1 |
| `syntax.bracket_4` | `#62afa0` | 0.700 | 0.080 | 180 | 7.46:1 |
| `syntax.bracket_5` | `#6fa5cb` | 0.699 | 0.080 | 240 | 7.24:1 |
| `syntax.bracket_6` | `#a693c9` | 0.699 | 0.080 | 300 | 6.98:1 |
| `syntax.tag` | `#5ec4c4` | 0.760 | 0.095 | 195 | 9.30:1 |
| `syntax.attribute` | `#d8a76d` | 0.761 | 0.095 | 70 | 8.84:1 |
| `syntax.embedded` | `#acb2bc` | 0.762 | 0.016 | 261 | 9.01:1 |
| `syntax.heading` | `#5ec4c4` | 0.760 | 0.095 | 195 | 9.30:1 |
| `syntax.link` | `#88b4ed` | 0.760 | 0.095 | 255 | 8.96:1 |
| `syntax.strong` | `#d3dae4` | 0.886 | 0.016 | 257 | 13.64:1 |
| `syntax.emphasis` | `#cc9ed7` | 0.761 | 0.094 | 320 | 8.63:1 |
| `syntax.caret` | `#5894e0` | 0.660 | 0.130 | 255 | 6.15:1 |
| `syntax.error_underline` | `#dc655f` | 0.651 | 0.151 | 25 | 5.55:1 |
| `syntax.hover_underline` | `#5f82b0` | 0.599 | 0.081 | 256 | 4.85:1 |
| `syntax.diagnostic_row_bg` | `#191416` | 0.198 | 0.009 | 352 | 1.05:1 |
| `syntax.diagnostic_inline_message` | `#b6706b` | 0.619 | 0.090 | 24 | 5.05:1 |
| `syntax.diagnostic_card_message` | `#f07f77` | 0.721 | 0.140 | 25 | 7.32:1 |

`syntax.diagnostic_row_bg` is a background tint, not a foreground, so a contrast ratio against the
editor background is not meaningful for it and no floor is applied.

---

## 7. Capture → style mapping

Every tree-sitter capture the app recognizes, the bucket it resolves to, and why.

Resolution uses tree-sitter's own rule, which is **not** a prefix match: a recognized name matches
when every one of *its* dot-parts appears among the capture's dot-parts, and the match with the most
parts wins. That is the specific→general fallback chain — `function.method.foo` degrades to
`function.method`, then to `function` — enforced by the engine rather than by a second hand-rolled
lookup.

| capture | bucket | rationale |
|---|---|---|
| `keyword` | Keyword | plain foreground — too frequent to earn colour |
| `function` | Function | a **call site**; plain foreground |
| `function.method` | FunctionMethod | a method call; plain foreground |
| `function.definition` | FunctionDefinition | **binding site** — blue. Added by per-language supplement rules; no bundled grammar query distinguishes this from a call |
| *(none — `token_tree` contents)* | Variable | inside a macro body the grammar parses only opaque tokens, so a call there is indistinguishable from any other identifier and reads as a variable. Honest rather than a gap |
| `type`, `constructor` | Type | rare and anchoring — cyan |
| `type.builtin` | TypeBuiltin | `i32`/`void` are still types |
| `tag` | Tag | a JSX element names a type |
| `constant`, `boolean` | Constant / ConstantBuiltin | rare literals — amber |
| `constant.builtin`, `number` | ConstantBuiltin / Number | same family, amber |
| `string`, `string.special` | String | green |
| `escape`, `string.escape` | StringEscape | the same green, lifted in lightness. `string.escape` is live: both markdown grammars emit `(backslash_escape) @string.escape` |
| `string.special.key` | Property | a JSON key is a property name, not a string value |
| `comment` | Comment | readable grey at 4.65:1 |
| `comment.documentation` | CommentDoc | a `///` comment reads brighter than a `//` one |
| `variable`, `label` | Variable | **rose tint.** Rust needs `RUST_VARIABLE_PREFIX` to emit this at all — see §2 |
| `variable.parameter` | VariableParameter | **binding site** — magenta |
| `variable.builtin` | VariableBuiltin | `self`/`this`; amber, like a literal |
| `property` | Property | **cyan-blue tint** — cool against the warm locals, so an `a.b.c` chain reads at a glance |
| `operator`, `punctuation.special` | Operator | de-emphasized |
| `punctuation.bracket` | PunctuationBracket | de-emphasized; the ring's fallback for an unmatched bracket |
| `punctuation.delimiter`, `delimiter` | PunctuationDelimiter | de-emphasized |
| `attribute` | Attribute | amber — a fixed marker, like a constant. `RUST_ATTRIBUTE_SUPPLEMENT` re-asserts it on the identifiers *inside* an attribute, which the blanket variable rule would otherwise claim |
| `embedded` | Embedded | plain foreground; rarely visible, inner tokens win by nesting |
| `text.title` | Heading | cyan; shares the type hue, unambiguous in context |
| `text.uri`, `text.reference` | Link | blue |
| `text.strong` | Strong | brighter neutral — no per-run font weight exists yet |
| `text.emphasis` | Emphasis | magenta; shares the parameter hue, unambiguous in context |
| `text.literal` | String | a fence's info string |
| `none` | *(unregistered, deliberately)* | leaves no parent highlight open across an injected range — registering it breaks fence injection |

Sharing a hue between two buckets is deliberate and follows the "unambiguous in context" rule: a
markdown file has no types for `heading`'s cyan to collide with, and no parameters for `emphasis`'s
magenta.

---

## 8. References

- [tonsky.me/blog/syntax-highlighting](https://tonsky.me/blog/syntax-highlighting/) — minimal-palette philosophy; the use-site/binding-site line
- [ethanschoonover.com/solarized](https://ethanschoonover.com/solarized) — symmetric perceptual lightness design
- [motlin.medium.com — how to pick colors for a syntax highlighting theme](https://motlin.medium.com/how-to-pick-colors-for-a-syntax-highlighting-theme-96d3e06c19dc) — "spending contrast"
- [neovim.io/doc/user/treesitter.html](https://neovim.io/doc/user/treesitter.html) and [docs.helix-editor.com/themes.html](https://docs.helix-editor.com/themes.html) — standard capture taxonomy
- [huetone.ardov.me](https://huetone.ardov.me) — OKLCH palette building with contrast checks
