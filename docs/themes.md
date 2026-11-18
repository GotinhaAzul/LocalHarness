# Themes

Themes are TOML files with semantic tokens. Bundled: `default` (dusk, also
called `totality`), `hati` (cool night), `skoll` (warm light), `classic` (the
pre-0.3 default), `catppuccin-mocha`, `gruvbox-dark`, `high-contrast`, and
`light`. `omarchy` is accepted as an alias for `default`, so older configs keep
working.

Two tokens carry the interface's two voices: `sun` (Skoll) colors the model,
its reply marker and its streaming phases; `moon` (Hati) colors the harness,
its tool rows, turn summaries, and tool phases. Missing tokens fall back to the
default palette. User themes
live in `~/.config/totality/themes/<name>.toml`.

```toml
name = "mine"

[colors]
bg = "#1b1b1f"
fg = "#e4e4e7"
muted = "#71717a"
border = "#3f3f46"
accent = "#7aa2f7"
accent_dim = "#3d59a1"
success = "#9ece6a"
warning = "#e0af68"
error = "#f7768e"
tool = "#bb9af7"
tool_running = "#7aa2f7"
tool_ok = "#9ece6a"
tool_error = "#f7768e"
reasoning = "#565f89"
code = "#c0caf5"
code_bg = "#24283b"
diff_add = "#9ece6a"
diff_del = "#f7768e"
diff_hunk = "#7aa2f7"
user = "#7aa2f7"
assistant = "#e4e4e7"
system = "#71717a"
selection = "#283457"
cursor = "#c0caf5"
sun = "#f2a65a"
moon = "#a9b8d8"
```

Values: `#rgb`, `#rrggbb`, ANSI names (`red`, `bright_blue`, ...),
`indexed:0..255`, and `default`.

`color_mode = "auto" | "truecolor" | "256" | "16"` controls conversion. Auto
detects `COLORTERM` and `TERM`; `NO_COLOR` disables color. `/theme` opens a
picker that previews each theme as you move (Esc restores the previous one);
moving through the preview saves nothing. `/theme <name>` and a confirmed pick
apply the theme and write `[ui] theme` to the global config at once; `/theme save`
re-writes the current theme and reports the path. A failed write leaves the
theme applied for this session and shows a warning.
Status is never conveyed by color alone: labels and glyphs accompany every
color.
