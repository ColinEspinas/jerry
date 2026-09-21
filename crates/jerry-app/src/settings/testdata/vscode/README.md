# Real VSCode theme fixtures

Verbatim, unmodified copies of real, shipped VSCode themes, used by
`crate::settings::vscode_theme`'s import tests. They are checked in deliberately: importing a
*real* theme file exercises paths a hand-written fixture does not — JSONC comments, tab
indentation, `include` chains, and colour keys we would not have thought to invent. Two real
bugs (Dark+ failing to convert at all, Dark Modern being wrongly rejected as unreadable) were
found by testing against these rather than against synthetic JSON.

| File | Source | Licence |
| --- | --- | --- |
| `dark_vs.json`, `dark_plus.json`, `dark_modern.json`, `light_vs.json`, `light_plus.json`, `light_modern.json` | [microsoft/vscode](https://github.com/microsoft/vscode) `extensions/theme-defaults/themes/` | MIT |
| `monokai.json` | [microsoft/vscode](https://github.com/microsoft/vscode) `extensions/theme-monokai/themes/` | MIT |
| `solarized_dark.json` | [microsoft/vscode](https://github.com/microsoft/vscode) `extensions/theme-solarized-dark/themes/` | MIT |
| `onedark.json` | [Binaryify/OneDark-Pro](https://github.com/Binaryify/OneDark-Pro) `themes/OneDark-Pro.json` | MIT |

Refresh them by re-downloading from those paths; they are inputs to tests only and are never
shipped in the application binary.
