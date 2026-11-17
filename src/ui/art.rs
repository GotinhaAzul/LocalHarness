//! The welcome art: Skoll and Hati closing in on the eclipse.
//!
//! The picture is drawn on a pixel grid and packed two pixels per cell with
//! half blocks (`▀`), so it needs no image protocol. Colors pass through the
//! theme's color mode, and `NO_COLOR` keeps the silhouette.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use super::theme::{Role, Theme, convert};

/// A wolf at full gallop, facing right. `a` is the lit coat, `b` the shaded
/// underside and legs, `e` the eye.
const WOLF: [&str; 16] = [
    "..........................a..a....",
    ".........................aa.aa....",
    "........................aaaaaa....",
    "......................aaaaeaaaa...",
    "a...................aaaaaaaaaaaaa.",
    "aa.................aaaaaaaaaaaaaaa",
    "aaa.............aaaaaaaaaaa..bbb..",
    "aaaa.....aaaaaaaaaaaaaaaaaaa......",
    "aaaaaaaaaaaaaaaaaaaaaaaaaaaa......",
    ".bbbaaaaaaaaaaaaaaaaaaaaaaaa......",
    "...bbbbaaaaaaaaaaaaaaaaaaaab......",
    ".....bbbbbbb.bbbbbbbbbbbbbb.......",
    ".....bbbbb.........bbbbbbb........",
    "...bbbb...........bbb..bbbb.......",
    ".bbbb............bbb.....bbbb.....",
    "bbb.............bb.........bbbb...",
];

const WOLF_WIDTH: usize = 34;
const DISC: usize = 16;
/// How far each snout reaches into the corona.
const OVERLAP: usize = 3;
/// Width of the full picture in cells.
pub const WIDE: usize = WOLF_WIDTH * 2 + DISC - OVERLAP * 2;

type Rgb = (u8, u8, u8);

const SKOLL: (Rgb, Rgb) = ((255, 216, 128), (236, 124, 66));
const HATI: (Rgb, Rgb) = ((232, 238, 250), (118, 138, 192));
const CORONA: (Rgb, Rgb) = ((255, 196, 104), (176, 196, 236));
const CORE: Rgb = (12, 10, 16);
const EYE: Rgb = (22, 19, 26);
const FLARE: Rgb = (255, 252, 232);

fn lerp(a: Rgb, b: Rgb, t: f32) -> Rgb {
    let mix = |x: u8, y: u8| (f32::from(x) + (f32::from(y) - f32::from(x)) * t).round() as u8;
    (mix(a.0, b.0), mix(a.1, b.1), mix(a.2, b.2))
}

fn shade(c: Rgb, k: f32) -> Rgb {
    let f = |v: u8| (f32::from(v) * k).round() as u8;
    (f(c.0), f(c.1), f(c.2))
}

type Grid = Vec<Vec<Option<Rgb>>>;

fn wolf((top, bottom): (Rgb, Rgb), mirror: bool) -> Grid {
    let rows = WOLF.len();
    WOLF.iter()
        .enumerate()
        .map(|(y, row)| {
            let lit = lerp(top, bottom, y as f32 / (rows - 1) as f32);
            let mut cells: Vec<Option<Rgb>> = row
                .bytes()
                .map(|ch| match ch {
                    b'a' => Some(lit),
                    b'b' => Some(shade(lit, 0.72)),
                    b'e' => Some(EYE),
                    _ => None,
                })
                .collect();
            if mirror {
                cells.reverse();
            }
            cells
        })
        .collect()
}

/// The eclipse at totality: a dark core, a corona that warms from Hati's
/// silver to Skoll's gold, and the diamond-ring flare.
fn disc() -> Grid {
    let center = (DISC as f32 - 1.0) / 2.0;
    let mut grid: Grid = (0..DISC)
        .map(|y| {
            (0..DISC)
                .map(|x| {
                    let d = (x as f32 - center).hypot(y as f32 - center);
                    let ring = lerp(CORONA.0, CORONA.1, x as f32 / (DISC - 1) as f32);
                    if d <= center - 2.3 {
                        Some(CORE)
                    } else if d <= center - 0.6 {
                        Some(ring)
                    } else if d <= center + 0.4 {
                        Some(shade(ring, 0.55))
                    } else {
                        None
                    }
                })
                .collect()
        })
        .collect();
    grid[2][12] = Some(FLARE);
    grid[1][13] = Some(FLARE);
    grid
}

fn blit(canvas: &mut Grid, image: &Grid, x0: usize) {
    for (y, row) in image.iter().enumerate() {
        for (x, cell) in row.iter().enumerate() {
            if cell.is_some() {
                canvas[y][x0 + x] = *cell;
            }
        }
    }
}

fn canvas(wide: bool) -> Grid {
    if !wide {
        return disc();
    }
    let mut canvas: Grid = vec![vec![None; WIDE]; WOLF.len()];
    blit(&mut canvas, &disc(), WOLF_WIDTH - OVERLAP);
    blit(&mut canvas, &wolf(SKOLL, false), 0);
    blit(
        &mut canvas,
        &wolf(HATI, true),
        WOLF_WIDTH + DISC - OVERLAP * 2,
    );
    canvas
}

/// The picture for `width` columns: both wolves when they fit, the eclipse
/// alone when they do not, nothing on very narrow terminals. Lines are
/// centered in `width`.
pub fn welcome(width: usize, theme: &Theme) -> Vec<Line<'static>> {
    let wide = width >= WIDE;
    if !wide && width < DISC + 2 {
        return Vec::new();
    }
    let grid = canvas(wide);
    let art_width = grid[0].len();
    let pad = " ".repeat((width - art_width) / 2);
    let color = |rgb: Rgb| {
        if theme.no_color {
            Color::Reset
        } else {
            convert(Color::Rgb(rgb.0, rgb.1, rgb.2), theme.color_mode)
        }
    };
    grid.chunks(2)
        .map(|pair| {
            let mut spans = vec![Span::raw(pad.clone())];
            for x in 0..art_width {
                let top = pair[0][x];
                let bottom = pair.get(1).and_then(|row| row[x]);
                spans.push(match (top, bottom) {
                    (None, None) => Span::raw(" "),
                    (Some(t), None) => Span::styled("▀", Style::default().fg(color(t))),
                    (None, Some(b)) => Span::styled("▄", Style::default().fg(color(b))),
                    (Some(t), Some(b)) if t == b || theme.no_color => {
                        Span::styled("█", Style::default().fg(color(t)))
                    }
                    (Some(t), Some(b)) => {
                        Span::styled("▀", Style::default().fg(color(t)).bg(color(b)))
                    }
                });
            }
            Line::from(spans)
        })
        .collect()
}

/// `name` letter-spaced, fading from the sun to the moon.
pub fn wordmark(name: &str, theme: &Theme) -> Vec<Span<'static>> {
    let letters: Vec<char> = name.to_uppercase().chars().collect();
    let last = letters.len().saturating_sub(1).max(1) as f32;
    letters
        .iter()
        .enumerate()
        .map(|(index, letter)| {
            let t = index as f32 / last;
            let fg = match (theme.color(Role::Sun), theme.color(Role::Moon)) {
                (Color::Rgb(r1, g1, b1), Color::Rgb(r2, g2, b2)) => {
                    let (r, g, b) = lerp((r1, g1, b1), (r2, g2, b2), t);
                    Color::Rgb(r, g, b)
                }
                (sun, moon) => {
                    if t < 0.5 {
                        sun
                    } else {
                        moon
                    }
                }
            };
            let text = if index + 1 < letters.len() {
                format!("{letter} ")
            } else {
                letter.to_string()
            };
            Span::styled(text, Style::default().fg(fg).add_modifier(Modifier::BOLD))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn width(line: &Line<'_>) -> usize {
        line.spans
            .iter()
            .map(|span| span.content.chars().count())
            .sum()
    }

    #[test]
    fn wolves_fit_eighty_columns() {
        const { assert!(WIDE <= 80) };
        assert!(WOLF.iter().all(|row| row.len() == WOLF_WIDTH));
        let theme = Theme::bundled("default");
        let lines = welcome(80, &theme);
        assert_eq!(lines.len(), WOLF.len() / 2);
        assert!(lines.iter().all(|line| width(line) <= 80));
    }

    #[test]
    fn narrow_terminals_get_the_eclipse_then_nothing() {
        let theme = Theme::bundled("default");
        let lines = welcome(40, &theme);
        assert_eq!(lines.len(), DISC / 2);
        assert!(lines.iter().all(|line| width(line) <= 40));
        assert!(welcome(10, &theme).is_empty());
    }

    #[test]
    fn wordmark_spaces_letters() {
        let theme = Theme::bundled("default");
        let text: String = wordmark("totality", &theme)
            .iter()
            .map(|span| span.content.to_string())
            .collect();
        assert_eq!(text, "T O T A L I T Y");
    }
}
