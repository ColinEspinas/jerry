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

Six accent hues, all at one OKLCH lightness (**L 0.760**) and one chroma (**C 0.095**), differing
only in hue. Everything a source file is *made of* — variables, property accesses, function calls,
keywords — renders at plain foreground. Punctuation sits deliberately below plain foreground.
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

---

## 2. Why variables and function calls are not coloured

This was the one genuinely contested decision, and it was resolved from the references rather than
by preference.

**tonsky, [*I am sorry, but everyone is getting syntax highlighting wrong*](https://tonsky.me/blog/syntax-highlighting/)**
states the position directly:

> I don't highlight variables or function calls — they are everywhere, your code is probably 75%
> variable names and function calls.

But the more useful part of his argument is *where he draws the line*, which is not at "identifiers"
but at **use sites versus binding sites**:

> Notice that we've kept variable declarations. These are not as ubiquitous and help you quickly
> answer a common question: where does this thing come from?

**Motlin, [*How to pick colors for a syntax highlighting theme*](https://motlin.medium.com/how-to-pick-colors-for-a-syntax-highlighting-theme-96d3e06c19dc)**
is often read as arguing the opposite. He does not. His claim is *relational* — "Keywords,
parameters, locals, and fields should use distinct colors", i.e. *if* you colour these, they must
differ from each other. He never asserts that a variable reference deserves colour against plain
foreground. His own stated principle points the other way:

> Contrast is a scarce resource which we only spend to resolve ambiguity.

> Escape sequences don't get confused with anything else, because they're inside a string.

That second quote is the general rule that **context can substitute for contrast**, and it is
exactly what makes a plain use site affordable once its declaration nearby is distinguished.

So the synthesis both authors' stated principles endorse: **use sites plain, binding sites
coloured.**

Three consequences:

- `syntax.function` and `syntax.function_method` (call sites) are plain foreground;
  `syntax.function_definition` gets the blue accent. This distinction did not previously exist and
  is not expressible from any bundled grammar query — no grammar here distinguishes a definition
  from a call — so `code_view.rs` grew real per-language definition-site query rules to make it
  possible.
- `syntax.variable_parameter` keeps a real accent, and this is tonsky's rule rather than an
  exception to it: every grammar here captures `@variable.parameter` at the *parameter declaration*
  (`tree-sitter-rust`'s `(parameter (identifier) @variable.parameter)`), never at a use inside the
  body. It is already a binding site — and it resolves precisely the local-versus-parameter
  ambiguity Motlin names.
- `syntax.property` is plain foreground. This is where the two authors genuinely disagree: Motlin's
  strongest case is a bare `count++` where you cannot tell a field from a local without scrolling.
  tonsky simply prices that answer as unaffordable at 75% of the screen. This palette takes tonsky's
  side, on the grounds that a member access is already marked by the `.` in front of it — context
  doing the work contrast would otherwise have to.

`syntax.keyword` is plain foreground for the same frequency reason ("Don't highlight language
keywords").

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

`L`/`C`/`H` are OKLCH. Contrast is WCAG 2.x against `surface.center` `#131518`.

| token | hex | L | C | H | contrast |
|---|---|---|---|---|---|
| `syntax.text` | `#acb2bc` | 0.762 | 0.016 | 261 | 8.58:1 |
| `syntax.keyword` | `#acb2bc` | 0.762 | 0.016 | 261 | 8.58:1 |
| `syntax.function` | `#acb2bc` | 0.762 | 0.016 | 261 | 8.58:1 |
| `syntax.function_method` | `#acb2bc` | 0.762 | 0.016 | 261 | 8.58:1 |
| `syntax.function_definition` | `#88b4ed` | 0.760 | 0.095 | 255 | 8.53:1 |
| `syntax.type` | `#5ec4c4` | 0.760 | 0.095 | 195 | 8.86:1 |
| `syntax.type_builtin` | `#5ec4c4` | 0.760 | 0.095 | 195 | 8.86:1 |
| `syntax.constant` | `#d8a76d` | 0.761 | 0.095 | 70 | 8.42:1 |
| `syntax.constant_builtin` | `#d8a76d` | 0.761 | 0.095 | 70 | 8.42:1 |
| `syntax.string` | `#8bc18c` | 0.759 | 0.094 | 145 | 8.80:1 |
| `syntax.string_escape` | `#a8e0a9` | 0.854 | 0.095 | 145 | 12.11:1 |
| `syntax.number` | `#d8a76d` | 0.761 | 0.095 | 70 | 8.42:1 |
| `syntax.comment` | `#7a818a` | 0.600 | 0.016 | 255 | 4.65:1 |
| `syntax.comment_doc` | `#8c939c` | 0.660 | 0.016 | 255 | 5.90:1 |
| `syntax.variable` | `#acb2bc` | 0.762 | 0.016 | 261 | 8.58:1 |
| `syntax.variable_parameter` | `#cc9ed7` | 0.761 | 0.094 | 320 | 8.22:1 |
| `syntax.variable_builtin` | `#d8a76d` | 0.761 | 0.095 | 70 | 8.42:1 |
| `syntax.property` | `#acb2bc` | 0.762 | 0.016 | 261 | 8.58:1 |
| `syntax.operator` | `#6f757e` | 0.560 | 0.016 | 258 | 3.94:1 |
| `syntax.punctuation_bracket` | `#6f757e` | 0.560 | 0.016 | 258 | 3.94:1 |
| `syntax.punctuation_delimiter` | `#6f757e` | 0.560 | 0.016 | 258 | 3.94:1 |
| `syntax.bracket_1` | `#c58aa5` | 0.700 | 0.080 | 350 | 6.58:1 |
| `syntax.bracket_2` | `#c88f73` | 0.699 | 0.080 | 47 | 6.65:1 |
| `syntax.bracket_3` | `#a4a267` | 0.700 | 0.079 | 107 | 6.92:1 |
| `syntax.bracket_4` | `#69af97` | 0.701 | 0.080 | 170 | 7.12:1 |
| `syntax.bracket_5` | `#64a9c4` | 0.699 | 0.080 | 225 | 6.98:1 |
| `syntax.bracket_6` | `#9b97ce` | 0.700 | 0.080 | 287 | 6.72:1 |
| `syntax.tag` | `#5ec4c4` | 0.760 | 0.095 | 195 | 8.86:1 |
| `syntax.attribute` | `#d8a76d` | 0.761 | 0.095 | 70 | 8.42:1 |
| `syntax.embedded` | `#acb2bc` | 0.762 | 0.016 | 261 | 8.58:1 |
| `syntax.heading` | `#5ec4c4` | 0.760 | 0.095 | 195 | 8.86:1 |
| `syntax.link` | `#88b4ed` | 0.760 | 0.095 | 255 | 8.53:1 |
| `syntax.strong` | `#d3dae4` | 0.886 | 0.016 | 257 | 12.99:1 |
| `syntax.emphasis` | `#cc9ed7` | 0.761 | 0.094 | 320 | 8.22:1 |
| `syntax.caret` | `#5894e0` | 0.660 | 0.130 | 255 | 5.86:1 |
| `syntax.error_underline` | `#dc655f` | 0.651 | 0.151 | 25 | 5.29:1 |
| `syntax.hover_underline` | `#5f82b0` | 0.599 | 0.081 | 256 | 4.62:1 |
| `syntax.diagnostic_row_bg` | `#191416` | 0.198 | 0.009 | 352 | 1.00:1 |
| `syntax.diagnostic_inline_message` | `#b6706b` | 0.619 | 0.090 | 24 | 4.81:1 |
| `syntax.diagnostic_card_message` | `#f07f77` | 0.721 | 0.140 | 25 | 6.97:1 |

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
| `variable`, `label` | Variable | plain foreground |
| `variable.parameter` | VariableParameter | **binding site** — magenta |
| `variable.builtin` | VariableBuiltin | `self`/`this`; amber, like a literal |
| `property` | Property | plain foreground — the `.` already marks it |
| `operator`, `punctuation.special` | Operator | de-emphasized |
| `punctuation.bracket` | PunctuationBracket | de-emphasized; the ring's fallback for an unmatched bracket |
| `punctuation.delimiter`, `delimiter` | PunctuationDelimiter | de-emphasized |
| `attribute` | Attribute | amber — a fixed marker, like a constant |
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
