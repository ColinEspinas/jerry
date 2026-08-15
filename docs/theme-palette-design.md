# Jerry's syntax palette

Design rationale for the ~270 syntax-highlighting color tokens in `crates/app/src/theme.rs`. For
the theme *file format* and how to author/import/generate a whole theme (not just syntax colors),
see [`themes.md`](./themes.md) instead — that's the user-facing guide; this is the design record
for why the syntax tier specifically looks the way it does.

The full specification of Jerry Dark's syntax palette: every colour in OKLCH, its role, and its
measured WCAG contrast against the editor background.

Jerry Dark is the *source* palette. The five other bundled themes are generated from it
programmatically (`crates/app/src/settings/builtin_themes.rs`); they are never hand-edited.
Regenerate with:

```
JERRY_REGENERATE_THEMES=1 cargo test -p app --lib builtin_themes -- --nocapture
```

---

## 1. The design, in one paragraph

**Ten accent hues, all at one OKLCH lightness (L 0.732) and one chroma (C 0.105), differing only in
hue.** Every semantic category a reader actually distinguishes gets its own — keywords, calls,
definition sites, types, constants, strings, attributes, properties, locals, parameters. The only
things left at plain foreground are the neutrals; punctuation sits deliberately *below* plain
foreground; comments are held to the full body-text contrast floor. The bracket-pair ring is a
further set of six hues placed on the *punctuation* lightness band, so a bracket can never shout
louder than a string.

The colour → meaning mapping is meant to be recitable from memory:

| hue | hex | OKLCH H | means |
|---|---|---|---|
| red-rose | `#e28c93` | 15 | parameter bindings |
| orange | `#de946b` | 50 | constants, numbers, booleans, `self` |
| gold | `#c7a356` | 85 | types, JSX tags, markdown headings |
| green | `#98b46a` | 126 | strings |
| teal | `#4bbeb1` | 185 | attributes / decorators |
| cyan-blue | `#51b7d8` | 222 | property / member access |
| blue | `#74ade8` | 250 | function and method **calls**, links, the caret |
| violet-blue | `#a19fe8` | 285 | function/method **definition sites** |
| purple | `#c194d6` | 315 | keywords, markdown emphasis |
| rose | `#da8db2` | 350 | ordinary variables |

Red at hue 25 is reserved for diagnostics, which are a different channel entirely (an underline and
a row tint, never a token foreground).

---

## 2. Where this palette came from, and what it walks back

### The tier is the original blue

`L 0.732 / C 0.105` is not an invention. It is **the OKLCH of this app's own original
`syntax.function` blue `#74ade8`** — the palette the maintainer says they preferred. Every accent
above is that colour rotated in hue only. `syntax.function` therefore reproduces the original value
bit-exactly, `syntax.string` lands ΔE 2 from the original `#9dbb6f` (below a just-noticeable
difference), and the largest drift anywhere is ΔE 16 on `syntax.keyword`, which stays unmistakably
the same purple.

Two hues moved further, for a stated reason. `constant` went from H 67 to H 50, because at one
shared lightness it was only 18° from `type`'s gold and the two would have collapsed into each
other — Solarized separates its own yellow and orange by about the same 40° this now uses.
`function.definition` is new: H 285, `function`'s blue rotated 35°, so a declaration reads as a
*distinguished call* rather than as an unrelated concept.

### What is being walked back, and why

An earlier revision of this branch held `syntax.keyword`, `syntax.function` and
`syntax.function_method` at **exactly** `syntax.text`'s grey, and `syntax.type` at a cyan
`#5ec4c4` rather than the original gold. The argument was tonsky's — "I don't highlight variables or function calls
— they are everywhere" — applied at the use-site/binding-site line.

The maintainer built the app, looked at it, and rejected it: *"I don't like the new colors at all. I
preferred the old colors but I think they were not used correctly."*

That is the verdict of the person who reads this screen all day, and it is not a lone opinion. The
two references that settled it:

- **[Solarized](https://ethanschoonover.com/solarized)** is not a minimal palette. It is **8 base
  monotones plus 8 genuinely saturated accents** (`yellow #b58900`, `orange #cb4b16`, `red #dc322f`,
  `magenta #d33682`, `violet #6c71c4`, `blue #268bd2`, `cyan #2aa198`, `green #859900`). What is
  disciplined about it is the *construction* — symmetric CIELAB lightness relationships, so light
  and dark modes keep an identical perceived contrast structure rather than being naive inversions —
  and the clear split between which tokens draw from base tones and which from accents. Restraint in
  hue *count* was never the point.
- **Asenov, Hilliges & Muller, "The Effect of Richer Visualizations on Code Comprehension"
  (CHI'16)** is frequently mis-cited as a caution against colour. It is the opposite. It is a
  controlled 33-participant study comparing three levels of visual richness in Java presentation,
  and it found that richer visual differentiation cut time-to-answer on structural comprehension
  questions by **21–75% with no loss of correctness** — contrary to what the participants themselves
  expected going in. Its literal recommendation to tool designers is to *"boost the syntax
  highlighting capabilities of their tools in two ways: (i) use a wider variety of colors by default
  and (ii) enable the highlighting of more constructs."* Its one real caution is that the *most*
  maximal condition — icons replacing keywords, background tinting on many expressions — felt
  subjectively overwhelming even though it did not hurt objective performance; participants' top
  subjective preference was the **middle** option, not the sparse one. That caution is about
  non-colour visual noise, not about hue count.

So the maintainer's instinct and the literature agree, and the restraint pass is substantially
reversed. **This is not a repudiation of the construction methodology.** The OKLCH tier, the
enforced contrast floors, and the definition-site query work are all kept — the last of those now
buys a real distinction between a declaration and a call rather than between "coloured" and "not
coloured".

### Solarized's discipline, applied honestly

Solarized's real method is symmetric perceptual-lightness relationships. The equivalent here is
stronger, because OKLCH's lightness is more perceptually uniform than CIELAB's: every accent shares
one `L` and one `C` and differs *only* in hue, so no accent can shout louder than another, and
`enforce_syntax_contrast_floors` re-establishes the same relationship against **each derived
theme's own background** rather than trusting the transform. Two tests pin it —
`every_accent_shares_one_lightness_and_one_chroma` and `every_accent_hue_family_stays_a_real_hue_apart`.

### Variables and properties

`syntax.variable` is a warm rose, `syntax.property` a cool cyan-blue. That warm/cool split is what
makes an `a.b.c` chain legible at a glance, and it answers Motlin's strongest case — a bare
`count++` where you cannot tell a field from a local without scrolling.

### The bug hiding underneath, which is still the most important lesson here

The first attempt at giving variables colour changed `syntax.variable` and **nothing happened on
screen.**

`tree-sitter-rust`'s bundled query has no blanket `(identifier) @variable` rule. Its only
`@variable`-family pattern is `(parameter (identifier) @variable.parameter)`. Every other grammar
here has one — `tree-sitter-python` at line 3, `-javascript` at line 4, `-go` at line 26, `-c` at
line 1. Rust was the sole outlier, so a plain Rust local classified as unstyled `Text`, and
`syntax.variable` was **literally unreachable in this app's primary language.**

No contrast or ΔE calculation could have caught this. What caught it was taking a screenshot after
the change, diffing it against the one before, and finding **zero changed pixels** in the code area.
`RUST_VARIABLE_PREFIX` is the fix; `a_plain_rust_local_is_a_real_variable_not_unclassified_text`
pins it. It caused one real regression — the blanket rule claimed the identifiers inside
`#[cfg(all(test, unix))]` — also found by pixel diff and fixed by `RUST_ATTRIBUTE_SUPPLEMENT`.

---

## 2b. The same class of bug, found again in four more places

Giving every semantic role a *distinct* colour makes a misclassification visible for the first time.
Asking, for each newly-coloured token, "which grammars can actually emit this?" turned up five more
real gaps of exactly the `RUST_VARIABLE_PREFIX` kind. Every one was verified by executing the app's
real composed query against the real grammar, and every node kind was checked against that grammar's
own `src/node-types.json`.

| gap | effect before | fix |
|---|---|---|
| **TS/JS shorthand properties had no capture at all.** `shorthand_property_identifier(_pattern)` appears once in `tree-sitter-javascript`'s query, guarded by an all-caps `@constant` predicate. The blanket `(identifier) @variable` does not reach them — different node kinds. | Every `const { data, error } = useQuery()` and every `return { id, name }` rendered as plain foreground. By blast radius, the largest gap in this round. | `TYPESCRIPT_IDENTIFIER_SUPPLEMENT`, with a `#not-match?` guard so an all-caps shorthand still reaches JavaScript's own earlier `@constant` rule |
| **Rust `@constant` is dead upstream.** `tree-sitter-rust-0.24.2/queries/highlights.scm:10-11` reads `(#match? @constant "^[A-Z][A-Z\d_]+$'")` — a stray apostrophe after the `$`, confirmed byte-for-byte with `od -c`. The rule can never fire, and the later `@constructor` heuristic then claims the identifier. | Every Rust `const`/`static` rendered as a **type**. Measured directly in the previous committed screenshot: `WRITE_TIMEOUT` was `#5ec4c4`, the old `syntax.type` cyan — and this repo's own screenshot README described it as "amber", which was simply wrong. | `RUST_CONSTANT_SUPPLEMENT` |
| **`variable.parameter` unreachable in Python and Go.** Neither bundled query contains a single `@variable.parameter`; every parameter fell through to the blanket `(identifier) @variable`. | The parameter hue existed but no Python or Go file could ever show it. | `PYTHON_PARAMETER_SUPPLEMENT`, `GO_CLASSIFICATION_SUPPLEMENT` |
| **TypeScript misses three parameter shapes.** Its rules match a *direct* `identifier` child of `required_parameter`/`optional_parameter` only. | `items.map(x => x + 1)` — how most callbacks are written — plus rest parameters and destructured parameters all read as ordinary locals. | `TYPESCRIPT_IDENTIFIER_SUPPLEMENT` |
| **Go: no `@constant`, and composite-literal keys are not `@property`.** Go has no all-caps convention, so upstream never wrote the heuristic; and `(field_identifier) @property` covers `v.Name` but not `User{Name: "x"}`, whose key is a plain `identifier`. | `const MaxRetries = 3` read as a local; struct-literal field names read as locals while the identical Rust and TypeScript constructs read as properties. | `GO_CLASSIFICATION_SUPPLEMENT`, keyed on the real `const_spec` node rather than on casing |

Two known divergences were checked and deliberately **not** changed:

- `tree-sitter-rust` emits no `@number` at all; Rust numeric literals arrive as `@constant.builtin`.
  Since `syntax.number`, `syntax.constant` and `syntax.constant_builtin` all resolve to the same
  orange, this is currently invisible. Recorded so that a future palette that splits them knows it
  has to add the query rule first.
- Inside a Rust macro body, `println!("{}", s.field)` gives `field` as a variable rather than a
  property, because a macro body parses as opaque tokens and `field_identifier` does not exist
  there. This is structural and not fixable by any capture rule.

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
  found. It is now **4.88:1**.
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
against `SEED_REFERENCE_ACCENT`. That constant moved to `#88b4ed` for one revision, because the
restraint palette had made `syntax.function` a near-neutral grey whose hue was numerically
meaningless — rotating a whole palette against it would have produced garbage from something that
still looked plausible. `syntax.function` is a genuinely chromatic blue again, so the constant
points back at its own `#74ade8`. `the_seed_reference_accent_is_a_genuinely_chromatic_colour` guards
that class of breakage in either direction.

---

## 5. The bracket-pair depth ring

Six hues chosen by `nesting depth % 6`, both halves of a pair alike. A bracket with no matching
partner keeps the de-emphasized punctuation tone instead, so malformed or mid-edit code degrades
visibly-but-quietly rather than lying about structure.

The ring sits at **L 0.560 / C 0.090 — exactly `punctuation_bracket`'s own lightness** — with its
six hues at the canonical even split, **0, 60, 120, 180, 240, 300**.

That is a change from the previous `L 0.700 / C 0.080` at hues hidden in the gaps between the
accents, and the reason is arithmetic rather than taste. With **ten** semantic hue families on the
wheel there is no longer a set of six gaps wide enough to hide in: held at the old tier, `bracket_5`
measured ΔE 9.9 from `syntax.function`. So the ring stopped buying its separation from hue and
started buying it from lightness — which turns out to be a much better trade, and one that also
says something true. A bracket *is* punctuation; colouring it should tell you the nesting depth
without promoting it above the code it encloses.

Measured across every bundled theme (CIE-Lab ΔE; ~2.3 is a just-noticeable difference):

| comparison | worst case | previous ring |
|---|---|---|
| cyclically adjacent depths (`n` vs `n+1`) | 20.8 (`Moss`) | 20.7 |
| any ring colour vs plain text | 24.0 (`Ember`) | 16.7 |
| any ring colour vs the de-emphasized bracket tone | 17.6 (`Moss`) | 26.5 |
| any ring colour vs any semantic accent | **12.6** (`Ember`) | 11.7 |
| the same, in Jerry Dark alone | **20.8** | 11.3 |
| contrast against the editor background | ≥ 3:1, all themes | ≥ 3:1 |

The ring-vs-accent floors in `theme.rs` were **raised** accordingly — 8 → 12 across all themes, and
11 → 18 in Jerry Dark. A palette that grew from eight semantic hue families to ten ended up with a
*more* clearly separated bracket ring, not a less one.

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
| `syntax.keyword` | `#c194d6` | 0.732 | 0.105 | 315 | 7.75:1 |
| `syntax.function` | `#74ade8` | 0.732 | 0.105 | 250 | 8.13:1 |
| `syntax.function_method` | `#74ade8` | 0.732 | 0.105 | 250 | 8.13:1 |
| `syntax.function_definition` | `#a19fe8` | 0.732 | 0.105 | 285 | 7.90:1 |
| `syntax.type` | `#c7a356` | 0.732 | 0.105 | 85 | 8.05:1 |
| `syntax.type_builtin` | `#c7a356` | 0.732 | 0.105 | 85 | 8.05:1 |
| `syntax.constant` | `#de946b` | 0.732 | 0.105 | 49 | 7.82:1 |
| `syntax.constant_builtin` | `#de946b` | 0.732 | 0.105 | 49 | 7.82:1 |
| `syntax.string` | `#98b46a` | 0.731 | 0.104 | 126 | 8.30:1 |
| `syntax.string_escape` | `#bddb8e` | 0.851 | 0.106 | 126 | 12.52:1 |
| `syntax.number` | `#de946b` | 0.732 | 0.105 | 49 | 7.82:1 |
| `syntax.comment` | `#7a818a` | 0.600 | 0.016 | 255 | 4.88:1 |
| `syntax.comment_doc` | `#8c939c` | 0.660 | 0.016 | 255 | 6.19:1 |
| `syntax.variable` | `#da8db2` | 0.732 | 0.104 | 350 | 7.71:1 |
| `syntax.variable_parameter` | `#e28c93` | 0.731 | 0.105 | 15 | 7.69:1 |
| `syntax.variable_builtin` | `#de946b` | 0.732 | 0.105 | 49 | 7.82:1 |
| `syntax.property` | `#51b7d8` | 0.732 | 0.105 | 222 | 8.34:1 |
| `syntax.operator` | `#6f757e` | 0.560 | 0.016 | 258 | 4.13:1 |
| `syntax.punctuation_bracket` | `#6f757e` | 0.560 | 0.016 | 258 | 4.13:1 |
| `syntax.punctuation_delimiter` | `#6f757e` | 0.560 | 0.016 | 258 | 4.13:1 |
| `syntax.bracket_1` | `#9f5d72` | 0.559 | 0.090 | 360 | 3.92:1 |
| `syntax.bracket_2` | `#9b673b` | 0.560 | 0.090 | 60 | 4.02:1 |
| `syntax.bracket_3` | `#6e7c3c` | 0.560 | 0.090 | 120 | 4.22:1 |
| `syntax.bracket_4` | `#268676` | 0.561 | 0.091 | 179 | 4.35:1 |
| `syntax.bracket_5` | `#3d7ba4` | 0.560 | 0.090 | 240 | 4.18:1 |
| `syntax.bracket_6` | `#7d68a2` | 0.561 | 0.091 | 300 | 4.00:1 |
| `syntax.tag` | `#c7a356` | 0.732 | 0.105 | 85 | 8.05:1 |
| `syntax.attribute` | `#4bbeb1` | 0.733 | 0.105 | 185 | 8.50:1 |
| `syntax.embedded` | `#acb2bc` | 0.762 | 0.016 | 261 | 9.01:1 |
| `syntax.heading` | `#c7a356` | 0.732 | 0.105 | 85 | 8.05:1 |
| `syntax.link` | `#74ade8` | 0.732 | 0.105 | 250 | 8.13:1 |
| `syntax.strong` | `#d3dae4` | 0.886 | 0.016 | 257 | 13.64:1 |
| `syntax.emphasis` | `#c194d6` | 0.732 | 0.105 | 315 | 7.75:1 |
| `syntax.caret` | `#4d97de` | 0.660 | 0.130 | 250 | 6.22:1 |
| `syntax.error_underline` | `#dc655f` | 0.651 | 0.151 | 25 | 5.55:1 |
| `syntax.hover_underline` | `#5a84af` | 0.600 | 0.081 | 250 | 4.90:1 |
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
| `keyword` | Keyword | **purple.** Held at plain foreground for one revision; see §2 |
| `function` | Function | a **call site** — blue, the original palette's own `#74ade8` |
| `function.method` | FunctionMethod | a method call; the same blue |
| `function.definition` | FunctionDefinition | **binding site** — violet-blue, the call blue rotated 35°. Added by per-language supplement rules for Rust/Python/JS/TS/Go/C; no bundled grammar query distinguishes this from a call |
| *(none — `token_tree` contents)* | Variable | inside a macro body the grammar parses only opaque tokens, so a call there is indistinguishable from any other identifier and reads as a variable. Honest rather than a gap |
| `type`, `constructor` | Type | gold. Rust routes its all-caps constants here too unless `RUST_CONSTANT_SUPPLEMENT` intervenes — see §2b |
| `type.builtin` | TypeBuiltin | `i32`/`void` are still types. Go has no `@type.builtin` at all, so `int`/`string` read as user types there — invisible only because this shares `Type`'s value |
| `tag` | Tag | a JSX element names a type |
| `constant`, `boolean` | Constant / ConstantBuiltin | orange. Reachable in Rust only via `RUST_CONSTANT_SUPPLEMENT` and in Go only via `GO_CLASSIFICATION_SUPPLEMENT` — see §2b |
| `constant.builtin`, `number` | ConstantBuiltin / Number | same family, same orange. Rust emits no `@number` at all |
| `string`, `string.special` | String | green |
| `escape`, `string.escape` | StringEscape | the same green, lifted in lightness. `string.escape` is live: both markdown grammars emit `(backslash_escape) @string.escape` |
| `string.special.key` | Property | a JSON key is a property name, not a string value |
| `comment` | Comment | readable grey at 4.88:1 |
| `comment.documentation` | CommentDoc | a `///` comment reads brighter than a `//` one |
| `variable`, `label` | Variable | **rose.** Rust needs `RUST_VARIABLE_PREFIX` to emit this at all (§2); TypeScript needs `TYPESCRIPT_IDENTIFIER_SUPPLEMENT` for shorthand properties (§2b) |
| `variable.parameter` | VariableParameter | **binding site** — red-rose, adjacent to `variable`'s own rose. Unreachable in Python and Go before `PYTHON_PARAMETER_SUPPLEMENT` / `GO_CLASSIFICATION_SUPPLEMENT` — see §2b |
| `variable.builtin` | VariableBuiltin | `self`/`this`; orange, like a literal |
| `property` | Property | **cyan-blue** — cool against the warm locals, so an `a.b.c` chain reads at a glance. Go's composite-literal keys need `GO_CLASSIFICATION_SUPPLEMENT` |
| `operator`, `punctuation.special` | Operator | de-emphasized |
| `punctuation.bracket` | PunctuationBracket | de-emphasized; the ring's fallback for an unmatched bracket |
| `punctuation.delimiter`, `delimiter` | PunctuationDelimiter | de-emphasized |
| `attribute` | Attribute | teal — its own hue, restored from the original palette. `RUST_ATTRIBUTE_SUPPLEMENT` re-asserts it on the identifiers *inside* an attribute, which the blanket variable rule would otherwise claim |
| `embedded` | Embedded | the one bucket still at plain foreground; rarely visible, inner tokens win by nesting |
| `text.title` | Heading | gold; shares the type hue, unambiguous in context |
| `text.uri`, `text.reference` | Link | blue |
| `text.strong` | Strong | brighter neutral — no per-run font weight exists yet |
| `text.emphasis` | Emphasis | purple; shares the keyword hue — a Markdown file has no keywords to collide with |
| `text.literal` | String | a fence's info string |
| `none` | *(unregistered, deliberately)* | leaves no parent highlight open across an injected range — registering it breaks fence injection |

Sharing a hue between two buckets is deliberate and follows the "unambiguous in context" rule: a
Markdown file has no types for `heading`'s gold to collide with, and no keywords for `emphasis`'s
purple.

---

## 8. References

- [ethanschoonover.com/solarized](https://ethanschoonover.com/solarized) — symmetric perceptual lightness design, **and eight genuinely saturated accents**. The discipline is in the construction, not in a small hue count
- Asenov, Hilliges & Muller, [*The Effect of Richer Visualizations on Code Comprehension*](https://doi.org/10.1145/2858036.2858372) (CHI'16) — a controlled 33-participant study. Richer colour cut structural-comprehension answer times by 21–75% with no loss of correctness; the paper's own recommendation is to "use a wider variety of colors by default" and "enable the highlighting of more constructs"
- [tonsky.me/blog/syntax-highlighting](https://tonsky.me/blog/syntax-highlighting/) — minimal-palette philosophy; the use-site/binding-site line. Followed for one revision of this branch and then walked back — see §2
- [motlin.medium.com — how to pick colors for a syntax highlighting theme](https://motlin.medium.com/how-to-pick-colors-for-a-syntax-highlighting-theme-96d3e06c19dc) — "spending contrast"
- [neovim.io/doc/user/treesitter.html](https://neovim.io/doc/user/treesitter.html) and [docs.helix-editor.com/themes.html](https://docs.helix-editor.com/themes.html) — standard capture taxonomy
- [huetone.ardov.me](https://huetone.ardov.me) — OKLCH palette building with contrast checks
