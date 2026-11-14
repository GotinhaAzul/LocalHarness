# Themes

Themes are TOML files with semantic tokens. Bundled: `default`,
`catppuccin-mocha`, `gruvbox-dark`, `high-contrast`, `light`. `omarchy` is
accepted as an alias for `default`, so older configs keep working. User themes
live in `~/.config/localharness/themes/<name>.toml`.

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
```

Values: `#rgb`, `#rrggbb`, ANSI names (`red`, `bright_blue`, ...),
`indexed:0..255`, and `default`.

`color_mode = "auto" | "truecolor" | "256" | "16"` controls conversion. Auto
detects `COLORTERM` and `TERM`; `NO_COLOR` disables color. `/theme` opens a
picker that previews each theme as you move (Esc restores the previous one);
`/theme <name>` applies for the session; `/theme save` writes `[ui] theme` to
the global config.
Status is never conveyed by color alone: labels and glyphs accompany every
color.
