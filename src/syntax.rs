//! Syntect-based syntax highlighting, lazily initialized once per process.

use ratatui::style::Color;
use std::path::Path;
use std::sync::OnceLock;
use syntect::easy::HighlightLines;
use syntect::highlighting::{Style as SynStyle, ThemeSet};
use syntect::parsing::SyntaxSet;
use syntect::util::LinesWithEndings;

#[derive(Clone)]
pub struct HlSpan {
    pub fg: Color,
    pub text: String,
}

static SYNTAX_SET: OnceLock<SyntaxSet> = OnceLock::new();
static THEME_SET: OnceLock<ThemeSet> = OnceLock::new();

fn syntax_set() -> &'static SyntaxSet {
    // two-face ships ~200 syntaxes (bat's set), much richer than syntect's
    // built-in 75. Critical for TypeScript/TSX/TOML/Swift/Kotlin/etc.
    SYNTAX_SET.get_or_init(two_face::syntax::extra_newlines)
}

fn theme_set() -> &'static ThemeSet {
    THEME_SET.get_or_init(ThemeSet::load_defaults)
}

pub struct Highlighted {
    pub lines: Vec<Vec<HlSpan>>,
    pub language: String,
}

/// Highlight `content` as the file at `path`. Returns one Vec of spans per
/// line plus the detected language name. On any failure (unknown extension,
/// parse error) falls back to returning the lines as a single uncolored span.
pub fn highlight(content: &str, path: &str) -> Highlighted {
    if content.is_empty() {
        return Highlighted { lines: Vec::new(), language: String::new() };
    }

    let ps = syntax_set();
    let ts = theme_set();
    // Use Solarized (dark): its palette is built on the 8 distinct ANSI
    // accent colors, so nearest-ANSI mapping below produces clean named
    // colors that the user's terminal palette will paint.
    let theme = ts
        .themes
        .get("Solarized (dark)")
        .or_else(|| ts.themes.get("base16-ocean.dark"))
        .or_else(|| ts.themes.values().next())
        .expect("syntect ships with default themes");

    let extension = Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");
    let syntax = ps
        .find_syntax_by_extension(extension)
        .or_else(|| ps.find_syntax_by_first_line(content.lines().next().unwrap_or("")))
        .unwrap_or_else(|| ps.find_syntax_plain_text());
    let language = syntax.name.clone();

    let mut h = HighlightLines::new(syntax, theme);
    let mut out: Vec<Vec<HlSpan>> = Vec::new();
    for line in LinesWithEndings::from(content) {
        let spans = match h.highlight_line(line, ps) {
            Ok(ranges) => ranges
                .into_iter()
                .map(|(style, segment)| HlSpan {
                    fg: to_ratatui_color(style),
                    text: segment.trim_end_matches('\n').to_string(),
                })
                .filter(|s| !s.text.is_empty())
                .collect(),
            Err(_) => vec![HlSpan {
                fg: Color::Reset,
                text: line.trim_end_matches('\n').to_string(),
            }],
        };
        out.push(spans);
    }
    Highlighted { lines: out, language }
}

/// Map syntect's RGB foreground to the nearest of the 16 standard ANSI
/// colors, returned as a ratatui named `Color`. The user's terminal then
/// paints these using whatever palette they have configured — which is the
/// whole point: the highlighting blends with their terminal theme.
fn to_ratatui_color(style: SynStyle) -> Color {
    nearest_ansi(style.foreground.r, style.foreground.g, style.foreground.b)
}

fn nearest_ansi(r: u8, g: u8, b: u8) -> Color {
    // VGA-standard ANSI 16. The user's terminal palette overrides these
    // values; we just need each named slot to be a sensible bucket.
    const PALETTE: &[(Color, u8, u8, u8)] = &[
        (Color::Black, 0, 0, 0),
        (Color::Red, 170, 0, 0),
        (Color::Green, 0, 170, 0),
        (Color::Yellow, 170, 85, 0),
        (Color::Blue, 0, 0, 170),
        (Color::Magenta, 170, 0, 170),
        (Color::Cyan, 0, 170, 170),
        (Color::Gray, 170, 170, 170),
        (Color::DarkGray, 85, 85, 85),
        (Color::LightRed, 255, 85, 85),
        (Color::LightGreen, 85, 255, 85),
        (Color::LightYellow, 255, 255, 85),
        (Color::LightBlue, 85, 85, 255),
        (Color::LightMagenta, 255, 85, 255),
        (Color::LightCyan, 85, 255, 255),
        (Color::White, 255, 255, 255),
    ];
    let mut best = Color::Reset;
    let mut best_d = i64::MAX;
    for (c, pr, pg, pb) in PALETTE {
        let dr = r as i64 - *pr as i64;
        let dg = g as i64 - *pg as i64;
        let db = b as i64 - *pb as i64;
        let d = dr * dr + dg * dg + db * db;
        if d < best_d {
            best_d = d;
            best = *c;
        }
    }
    best
}
