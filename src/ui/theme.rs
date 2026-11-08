//! Semantic theme tokens, bundled themes, user themes, and color modes.

use std::path::Path;

use ratatui::style::{Color, Modifier, Style};
use serde::Deserialize;

use crate::config::ColorMode;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Bg,
    Fg,
    Muted,
    Border,
    Accent,
    AccentDim,
    Success,
    Warning,
    Error,
    Tool,
    ToolRunning,
    ToolOk,
    ToolError,
    Reasoning,
    Code,
    CodeBg,
    DiffAdd,
    DiffDel,
    DiffHunk,
    User,
    Assistant,
    System,
    Selection,
    Cursor,
}

#[derive(Debug, Clone, Copy)]
pub struct ThemeColors {
    pub bg: Color,
    pub fg: Color,
    pub muted: Color,
    pub border: Color,
    pub accent: Color,
    pub accent_dim: Color,
    pub success: Color,
    pub warning: Color,
    pub error: Color,
    pub tool: Color,
    pub tool_running: Color,
    pub tool_ok: Color,
    pub tool_error: Color,
    pub reasoning: Color,
    pub code: Color,
    pub code_bg: Color,
    pub diff_add: Color,
    pub diff_del: Color,
    pub diff_hunk: Color,
    pub user: Color,
    pub assistant: Color,
    pub system: Color,
    pub selection: Color,
    pub cursor: Color,
}

impl ThemeColors {
    pub fn get(&self, role: Role) -> Color {
        match role {
            Role::Bg => self.bg,
            Role::Fg => self.fg,
            Role::Muted => self.muted,
            Role::Border => self.border,
            Role::Accent => self.accent,
            Role::AccentDim => self.accent_dim,
            Role::Success => self.success,
            Role::Warning => self.warning,
            Role::Error => self.error,
            Role::Tool => self.tool,
            Role::ToolRunning => self.tool_running,
            Role::ToolOk => self.tool_ok,
            Role::ToolError => self.tool_error,
            Role::Reasoning => self.reasoning,
            Role::Code => self.code,
            Role::CodeBg => self.code_bg,
            Role::DiffAdd => self.diff_add,
            Role::DiffDel => self.diff_del,
            Role::DiffHunk => self.diff_hunk,
            Role::User => self.user,
            Role::Assistant => self.assistant,
            Role::System => self.system,
            Role::Selection => self.selection,
            Role::Cursor => self.cursor,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Theme {
    pub name: String,
    pub colors: ThemeColors,
    pub color_mode: ColorMode,
    pub no_color: bool,
}

impl Theme {
    pub fn names() -> &'static [&'static str] {
        &[
            "omarchy",
            "catppuccin-mocha",
            "gruvbox-dark",
            "high-contrast",
            "light",
        ]
    }

    /// Load a theme by name: user themes first, then bundled, then omarchy.
    pub fn load(name: &str, theme_dir: &Path, color_mode: ColorMode, no_color: bool) -> Theme {
        if name == "light" {
            // "light" can be a bundled name too; user files win.
        }
        let user = theme_dir.join(format!("{name}.toml"));
        let mut theme = if user.is_file() {
            match std::fs::read_to_string(&user)
                .map_err(|err| err.to_string())
                .and_then(|raw| parse_theme(&raw, color_mode))
            {
                Ok(theme) => theme,
                Err(err) => {
                    tracing::warn!(%err, theme = name, "cannot load user theme; using bundled");
                    Self::bundled(name)
                }
            }
        } else {
            Self::bundled(name)
        };
        theme.color_mode = color_mode;
        theme.no_color = no_color;
        theme.apply_mode();
        theme
    }

    pub fn bundled(name: &str) -> Theme {
        let colors = match name {
            "catppuccin-mocha" => catppuccin(),
            "gruvbox-dark" => gruvbox(),
            "high-contrast" => high_contrast(),
            "light" => light(),
            _ => omarchy(),
        };
        Theme {
            name: if name.is_empty() {
                "omarchy".into()
            } else {
                name.to_string()
            },
            colors,
            color_mode: ColorMode::Auto,
            no_color: false,
        }
    }

    fn apply_mode(&mut self) {
        if self.no_color {
            for role in ALL_ROLES {
                set_color(&mut self.colors, *role, Color::Reset);
            }
            return;
        }
        for role in ALL_ROLES {
            let current = self.colors.get(*role);
            set_color(&mut self.colors, *role, convert(current, self.color_mode));
        }
    }

    pub fn color(&self, role: Role) -> Color {
        self.colors.get(role)
    }

    pub fn style(&self, role: Role) -> Style {
        Style::default().fg(self.color(role))
    }

    pub fn bold(&self, role: Role) -> Style {
        self.style(role).add_modifier(Modifier::BOLD)
    }
}

const ALL_ROLES: &[Role] = &[
    Role::Bg,
    Role::Fg,
    Role::Muted,
    Role::Border,
    Role::Accent,
    Role::AccentDim,
    Role::Success,
    Role::Warning,
    Role::Error,
    Role::Tool,
    Role::ToolRunning,
    Role::ToolOk,
    Role::ToolError,
    Role::Reasoning,
    Role::Code,
    Role::CodeBg,
    Role::DiffAdd,
    Role::DiffDel,
    Role::DiffHunk,
    Role::User,
    Role::Assistant,
    Role::System,
    Role::Selection,
    Role::Cursor,
];

fn set_color(colors: &mut ThemeColors, role: Role, color: Color) {
    match role {
        Role::Bg => colors.bg = color,
        Role::Fg => colors.fg = color,
        Role::Muted => colors.muted = color,
        Role::Border => colors.border = color,
        Role::Accent => colors.accent = color,
        Role::AccentDim => colors.accent_dim = color,
        Role::Success => colors.success = color,
        Role::Warning => colors.warning = color,
        Role::Error => colors.error = color,
        Role::Tool => colors.tool = color,
        Role::ToolRunning => colors.tool_running = color,
        Role::ToolOk => colors.tool_ok = color,
        Role::ToolError => colors.tool_error = color,
        Role::Reasoning => colors.reasoning = color,
        Role::Code => colors.code = color,
        Role::CodeBg => colors.code_bg = color,
        Role::DiffAdd => colors.diff_add = color,
        Role::DiffDel => colors.diff_del = color,
        Role::DiffHunk => colors.diff_hunk = color,
        Role::User => colors.user = color,
        Role::Assistant => colors.assistant = color,
        Role::System => colors.system = color,
        Role::Selection => colors.selection = color,
        Role::Cursor => colors.cursor = color,
    }
}

/// Convert a color for the terminal's capability.
pub fn convert(color: Color, mode: ColorMode) -> Color {
    match mode {
        ColorMode::Truecolor | ColorMode::Auto => color,
        ColorMode::Ansi256 => match color {
            Color::Rgb(r, g, b) => Color::Indexed(rgb_to_256(r, g, b)),
            other => other,
        },
        ColorMode::Ansi16 => match color {
            Color::Rgb(r, g, b) => rgb_to_ansi16(r, g, b),
            Color::Indexed(index) => indexed_to_ansi16(index),
            other => other,
        },
    }
}

fn rgb_to_256(r: u8, g: u8, b: u8) -> u8 {
    if r == g && g == b {
        if r < 8 {
            return 16;
        }
        if r > 248 {
            return 231;
        }
        return 232 + ((u16::from(r) - 8) * 24 / 247) as u8;
    }
    let to_cube = |value: u8| -> u8 {
        if value < 48 {
            0
        } else if value < 115 {
            1
        } else {
            ((u16::from(value) - 35) / 40) as u8
        }
    };
    16 + 36 * to_cube(r) + 6 * to_cube(g) + to_cube(b)
}

fn rgb_to_ansi16(r: u8, g: u8, b: u8) -> Color {
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    if max < 40 {
        return Color::Black;
    }
    let delta = u16::from(max) - u16::from(min);
    if delta < 24 {
        return if max > 210 {
            Color::White
        } else if max > 130 {
            Color::Gray
        } else {
            Color::DarkGray
        };
    }
    let bright = max > 165;
    let max_is_r = max == r;
    let max_is_g = max == g;
    let (r, g, b) = (f32::from(r), f32::from(g), f32::from(b));
    let delta_f = f32::from(delta);
    let hue = if max_is_r {
        60.0 * (((g - b) / delta_f) % 6.0)
    } else if max_is_g {
        60.0 * (((b - r) / delta_f) + 2.0)
    } else {
        60.0 * (((r - g) / delta_f) + 4.0)
    };
    let hue = if hue < 0.0 { hue + 360.0 } else { hue };
    let base = match hue {
        h if h < 25.0 || h >= 335.0 => Color::Red,
        h if h < 70.0 => Color::Yellow,
        h if h < 155.0 => Color::Green,
        h if h < 195.0 => Color::Cyan,
        h if h < 265.0 => Color::Blue,
        _ => Color::Magenta,
    };
    if bright {
        match base {
            Color::Red => Color::LightRed,
            Color::Yellow => Color::LightYellow,
            Color::Green => Color::LightGreen,
            Color::Cyan => Color::LightCyan,
            Color::Blue => Color::LightBlue,
            Color::Magenta => Color::LightMagenta,
            other => other,
        }
    } else {
        base
    }
}

fn indexed_to_ansi16(index: u8) -> Color {
    match index {
        0 => Color::Black,
        1 => Color::Red,
        2 => Color::Green,
        3 => Color::Yellow,
        4 => Color::Blue,
        5 => Color::Magenta,
        6 => Color::Cyan,
        7 => Color::Gray,
        8 => Color::DarkGray,
        9 => Color::LightRed,
        10 => Color::LightGreen,
        11 => Color::LightYellow,
        12 => Color::LightBlue,
        13 => Color::LightMagenta,
        14 => Color::LightCyan,
        15 => Color::White,
        other => {
            let level = other % 16;
            indexed_to_ansi16(level)
        }
    }
}

#[derive(Debug, Deserialize, Default)]
struct ThemeFile {
    name: Option<String>,
    #[serde(default)]
    colors: std::collections::HashMap<String, String>,
}

pub fn parse_theme(raw: &str, color_mode: ColorMode) -> Result<Theme, String> {
    let file: ThemeFile = toml::from_str(raw).map_err(|err| err.to_string())?;
    let mut theme = Theme {
        name: file.name.unwrap_or_else(|| "custom".into()),
        colors: omarchy(),
        color_mode,
        no_color: false,
    };
    for (key, value) in file.colors {
        let role = role_from_key(&key).ok_or_else(|| format!("unknown theme token {key:?}"))?;
        let color = parse_color(&value).map_err(|err| format!("invalid color for {key}: {err}"))?;
        set_color(&mut theme.colors, role, color);
    }
    Ok(theme)
}

fn role_from_key(key: &str) -> Option<Role> {
    Some(match key {
        "bg" => Role::Bg,
        "fg" => Role::Fg,
        "muted" => Role::Muted,
        "border" => Role::Border,
        "accent" => Role::Accent,
        "accent_dim" => Role::AccentDim,
        "success" => Role::Success,
        "warning" => Role::Warning,
        "error" => Role::Error,
        "tool" => Role::Tool,
        "tool_running" => Role::ToolRunning,
        "tool_ok" => Role::ToolOk,
        "tool_error" => Role::ToolError,
        "reasoning" => Role::Reasoning,
        "code" => Role::Code,
        "code_bg" => Role::CodeBg,
        "diff_add" => Role::DiffAdd,
        "diff_del" => Role::DiffDel,
        "diff_hunk" => Role::DiffHunk,
        "user" => Role::User,
        "assistant" => Role::Assistant,
        "system" => Role::System,
        "selection" => Role::Selection,
        "cursor" => Role::Cursor,
        _ => return None,
    })
}

pub fn parse_color(value: &str) -> Result<Color, String> {
    let value = value.trim();
    if value.eq_ignore_ascii_case("default") {
        return Ok(Color::Reset);
    }
    if let Some(index) = value.strip_prefix("indexed:") {
        let index: u8 = index
            .trim()
            .parse()
            .map_err(|_| format!("indexed color must be 0..255, got {value:?}"))?;
        return Ok(Color::Indexed(index));
    }
    if let Some(hex) = value.strip_prefix('#') {
        return match hex.len() {
            3 => {
                let mut chars = hex.chars();
                let r = chars.next().unwrap().to_digit(16).ok_or("bad hex")? as u8 * 17;
                let g = chars.next().unwrap().to_digit(16).ok_or("bad hex")? as u8 * 17;
                let b = chars.next().unwrap().to_digit(16).ok_or("bad hex")? as u8 * 17;
                Ok(Color::Rgb(r, g, b))
            }
            6 => {
                let r = u8::from_str_radix(&hex[0..2], 16).map_err(|_| "bad hex")?;
                let g = u8::from_str_radix(&hex[2..4], 16).map_err(|_| "bad hex")?;
                let b = u8::from_str_radix(&hex[4..6], 16).map_err(|_| "bad hex")?;
                Ok(Color::Rgb(r, g, b))
            }
            _ => Err(format!("expected #rgb or #rrggbb, got {value:?}")),
        };
    }
    let named = match value.to_ascii_lowercase().as_str() {
        "black" => Color::Black,
        "red" => Color::Red,
        "green" => Color::Green,
        "yellow" => Color::Yellow,
        "blue" => Color::Blue,
        "magenta" | "purple" => Color::Magenta,
        "cyan" => Color::Cyan,
        "gray" | "grey" => Color::Gray,
        "darkgray" | "dark_gray" => Color::DarkGray,
        "lightred" | "bright_red" => Color::LightRed,
        "lightgreen" | "bright_green" => Color::LightGreen,
        "lightyellow" | "bright_yellow" => Color::LightYellow,
        "lightblue" | "bright_blue" => Color::LightBlue,
        "lightmagenta" | "bright_magenta" => Color::LightMagenta,
        "lightcyan" | "bright_cyan" => Color::LightCyan,
        "white" => Color::White,
        _ => {
            return Err(format!(
                "unknown color {value:?}; use #rrggbb, an ANSI name, indexed:N, or default"
            ));
        }
    };
    Ok(named)
}

/// Detect the color mode from the environment.
pub fn detect_color_mode() -> ColorMode {
    if std::env::var_os("NO_COLOR").is_some() {
        return ColorMode::Ansi16;
    }
    let colorterm = std::env::var("COLORTERM").unwrap_or_default();
    if colorterm.contains("truecolor") || colorterm.contains("24bit") {
        return ColorMode::Truecolor;
    }
    let term = std::env::var("TERM").unwrap_or_default();
    if term.contains("256") {
        return ColorMode::Ansi256;
    }
    ColorMode::Ansi16
}

fn omarchy() -> ThemeColors {
    ThemeColors {
        bg: Color::Rgb(27, 27, 31),
        fg: Color::Rgb(228, 228, 231),
        muted: Color::Rgb(113, 113, 122),
        border: Color::Rgb(63, 63, 70),
        accent: Color::Rgb(122, 162, 247),
        accent_dim: Color::Rgb(61, 89, 161),
        success: Color::Rgb(158, 206, 106),
        warning: Color::Rgb(224, 175, 104),
        error: Color::Rgb(247, 118, 142),
        tool: Color::Rgb(187, 154, 247),
        tool_running: Color::Rgb(122, 162, 247),
        tool_ok: Color::Rgb(158, 206, 106),
        tool_error: Color::Rgb(247, 118, 142),
        reasoning: Color::Rgb(86, 95, 137),
        code: Color::Rgb(192, 202, 245),
        code_bg: Color::Rgb(36, 40, 59),
        diff_add: Color::Rgb(158, 206, 106),
        diff_del: Color::Rgb(247, 118, 142),
        diff_hunk: Color::Rgb(122, 162, 247),
        user: Color::Rgb(122, 162, 247),
        assistant: Color::Rgb(228, 228, 231),
        system: Color::Rgb(113, 113, 122),
        selection: Color::Rgb(40, 52, 87),
        cursor: Color::Rgb(192, 202, 245),
    }
}

fn catppuccin() -> ThemeColors {
    ThemeColors {
        bg: Color::Rgb(30, 30, 46),
        fg: Color::Rgb(205, 214, 244),
        muted: Color::Rgb(127, 132, 156),
        border: Color::Rgb(69, 71, 90),
        accent: Color::Rgb(137, 180, 250),
        accent_dim: Color::Rgb(88, 91, 112),
        success: Color::Rgb(166, 227, 161),
        warning: Color::Rgb(249, 226, 175),
        error: Color::Rgb(243, 139, 168),
        tool: Color::Rgb(203, 166, 247),
        tool_running: Color::Rgb(137, 180, 250),
        tool_ok: Color::Rgb(166, 227, 161),
        tool_error: Color::Rgb(243, 139, 168),
        reasoning: Color::Rgb(108, 112, 134),
        code: Color::Rgb(205, 214, 244),
        code_bg: Color::Rgb(24, 24, 37),
        diff_add: Color::Rgb(166, 227, 161),
        diff_del: Color::Rgb(243, 139, 168),
        diff_hunk: Color::Rgb(137, 180, 250),
        user: Color::Rgb(137, 180, 250),
        assistant: Color::Rgb(205, 214, 244),
        system: Color::Rgb(127, 132, 156),
        selection: Color::Rgb(49, 50, 68),
        cursor: Color::Rgb(205, 214, 244),
    }
}

fn gruvbox() -> ThemeColors {
    ThemeColors {
        bg: Color::Rgb(40, 40, 40),
        fg: Color::Rgb(235, 219, 178),
        muted: Color::Rgb(146, 131, 116),
        border: Color::Rgb(80, 73, 69),
        accent: Color::Rgb(131, 165, 152),
        accent_dim: Color::Rgb(69, 133, 136),
        success: Color::Rgb(184, 187, 38),
        warning: Color::Rgb(250, 189, 47),
        error: Color::Rgb(251, 73, 52),
        tool: Color::Rgb(211, 134, 155),
        tool_running: Color::Rgb(131, 165, 152),
        tool_ok: Color::Rgb(184, 187, 38),
        tool_error: Color::Rgb(251, 73, 52),
        reasoning: Color::Rgb(146, 131, 116),
        code: Color::Rgb(235, 219, 178),
        code_bg: Color::Rgb(29, 32, 33),
        diff_add: Color::Rgb(184, 187, 38),
        diff_del: Color::Rgb(251, 73, 52),
        diff_hunk: Color::Rgb(131, 165, 152),
        user: Color::Rgb(131, 165, 152),
        assistant: Color::Rgb(235, 219, 178),
        system: Color::Rgb(146, 131, 116),
        selection: Color::Rgb(60, 56, 54),
        cursor: Color::Rgb(235, 219, 178),
    }
}

fn high_contrast() -> ThemeColors {
    ThemeColors {
        bg: Color::Black,
        fg: Color::White,
        muted: Color::Gray,
        border: Color::White,
        accent: Color::LightCyan,
        accent_dim: Color::Cyan,
        success: Color::LightGreen,
        warning: Color::LightYellow,
        error: Color::LightRed,
        tool: Color::LightMagenta,
        tool_running: Color::LightCyan,
        tool_ok: Color::LightGreen,
        tool_error: Color::LightRed,
        reasoning: Color::Gray,
        code: Color::White,
        code_bg: Color::Black,
        diff_add: Color::LightGreen,
        diff_del: Color::LightRed,
        diff_hunk: Color::LightCyan,
        user: Color::LightCyan,
        assistant: Color::White,
        system: Color::Gray,
        selection: Color::Blue,
        cursor: Color::White,
    }
}

fn light() -> ThemeColors {
    ThemeColors {
        bg: Color::Rgb(250, 250, 250),
        fg: Color::Rgb(30, 30, 30),
        muted: Color::Rgb(100, 100, 100),
        border: Color::Rgb(200, 200, 200),
        accent: Color::Rgb(30, 90, 180),
        accent_dim: Color::Rgb(120, 150, 200),
        success: Color::Rgb(30, 130, 60),
        warning: Color::Rgb(170, 110, 0),
        error: Color::Rgb(190, 40, 50),
        tool: Color::Rgb(110, 60, 160),
        tool_running: Color::Rgb(30, 90, 180),
        tool_ok: Color::Rgb(30, 130, 60),
        tool_error: Color::Rgb(190, 40, 50),
        reasoning: Color::Rgb(120, 120, 120),
        code: Color::Rgb(30, 30, 30),
        code_bg: Color::Rgb(240, 240, 240),
        diff_add: Color::Rgb(30, 130, 60),
        diff_del: Color::Rgb(190, 40, 50),
        diff_hunk: Color::Rgb(30, 90, 180),
        user: Color::Rgb(30, 90, 180),
        assistant: Color::Rgb(30, 30, 30),
        system: Color::Rgb(100, 100, 100),
        selection: Color::Rgb(220, 230, 245),
        cursor: Color::Rgb(30, 30, 30),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_hex_named_indexed_and_default() {
        assert_eq!(parse_color("#7aa2f7").unwrap(), Color::Rgb(122, 162, 247));
        assert_eq!(parse_color("#fff").unwrap(), Color::Rgb(255, 255, 255));
        assert_eq!(parse_color("bright_blue").unwrap(), Color::LightBlue);
        assert_eq!(parse_color("indexed:42").unwrap(), Color::Indexed(42));
        assert_eq!(parse_color("default").unwrap(), Color::Reset);
        assert!(parse_color("mauve").is_err());
    }

    #[test]
    fn theme_file_overrides_tokens() {
        let theme = parse_theme(
            "name = \"mine\"\n[colors]\naccent = \"#ff0000\"\n",
            ColorMode::Truecolor,
        )
        .unwrap();
        assert_eq!(theme.name, "mine");
        assert_eq!(theme.colors.accent, Color::Rgb(255, 0, 0));
    }

    #[test]
    fn all_bundled_themes_exist() {
        for name in Theme::names() {
            let theme = Theme::load(name, Path::new("/nonexistent"), ColorMode::Truecolor, false);
            assert_eq!(&theme.name, name);
        }
    }

    #[test]
    fn ansi16_conversion_keeps_readable_colors() {
        let converted = convert(Color::Rgb(122, 162, 247), ColorMode::Ansi16);
        assert!(matches!(converted, Color::LightBlue | Color::Blue));
    }
}
