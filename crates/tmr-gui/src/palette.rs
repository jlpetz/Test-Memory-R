//! The colour palette (TODO 7, the owner's note 1: colours are a setting): built-in defaults per
//! light/dark, overridable from `colours.json` next to tmr.exe. The file is read once, when the
//! window opens; a missing file means the defaults, and a bad key or value is reported on the
//! console and that one colour keeps its default.
//!
//! `colours.json` holds a `"dark"` and/or a `"light"` object, each mapping a key below to a colour
//! string, `"#RRGGBB"` or `"#AARRGGBB"` (alpha first, as XAML writes it). Keys starting with `_`
//! are ignored, so the file can carry notes. Any key left out keeps its default.
//! `crates/tmr-gui/colours.example.json` has every key at its default (a test keeps it so): copy it
//! next to tmr.exe as `colours.json` and edit it.
//!
//! | key               | used for                                                             |
//! |-------------------|----------------------------------------------------------------------|
//! | `text_primary`    | most text (the title, table cells, error lines, card titles, CPU)    |
//! | `text_secondary`  | the info line, table headings, empty-state lines, chart axis labels  |
//! | `error`           | error bullets, a non-zero error count, the chart's error marks       |
//! | `critical`        | the "Failed" badge fill                                              |
//! | `success`         | the "Running" and "Passed" badge fill                                |
//! | `neutral`         | the other badges' fill: Ready, Stopping, Interrupted, Not run        |
//! | `on_status`       | badge text                                                           |
//! | `accent`          | the chart's throughput line                                          |
//! | `chart_grid`      | the chart's gridlines, the header divider                            |
//! | `card_background` | card fill                                                            |
//! | `card_stroke`     | card hairline border, the progress track's border                    |
//! | `stripe`          | alternate table rows                                                 |
//! | `progress_track`  | the progress bar's empty part                                        |
//! | `progress_fill`   | the progress bar's filled part                                       |
//! | `progress_text`   | the percentage drawn inside the progress bar                         |
//!
//! Defaults: text and cards are WinUI's Fluent values (TextFillColorPrimary etc.), so dark mode
//! keeps WinUI's bright white primary text (the owner's pick; egui's grey is `#8C8C8C` in dark,
//! `#505050` in light, for anyone who wants it). `error` is egui 0.37's `error_fg_color`
//! (`#FF0000` in both schemes), the darker, richer red the owner chose over WinUI's critical red.
//! The progress bar takes egui's colours (track `extreme_bg_color`, fill `selection.bg_fill`,
//! text `selection.stroke`).

use std::path::PathBuf;

use windows_reactor::{Brush, Color};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Palette {
    pub text_primary: Color,
    pub text_secondary: Color,
    pub error: Color,
    pub critical: Color,
    pub success: Color,
    pub neutral: Color,
    pub on_status: Color,
    pub accent: Color,
    pub chart_grid: Color,
    pub card_background: Color,
    pub card_stroke: Color,
    pub stripe: Color,
    pub progress_track: Color,
    pub progress_fill: Color,
    pub progress_text: Color,
}

/// Picks one colour field out of a palette.
type Field = fn(&mut Palette) -> &mut Color;

/// Every key, in file order, with the field it sets.
const KEYS: [(&str, Field); 15] = [
    ("text_primary", |p| &mut p.text_primary),
    ("text_secondary", |p| &mut p.text_secondary),
    ("error", |p| &mut p.error),
    ("critical", |p| &mut p.critical),
    ("success", |p| &mut p.success),
    ("neutral", |p| &mut p.neutral),
    ("on_status", |p| &mut p.on_status),
    ("accent", |p| &mut p.accent),
    ("chart_grid", |p| &mut p.chart_grid),
    ("card_background", |p| &mut p.card_background),
    ("card_stroke", |p| &mut p.card_stroke),
    ("stripe", |p| &mut p.stripe),
    ("progress_track", |p| &mut p.progress_track),
    ("progress_fill", |p| &mut p.progress_fill),
    ("progress_text", |p| &mut p.progress_text),
];

const fn rgb(r: u8, g: u8, b: u8) -> Color {
    Color::rgb(r, g, b)
}

const fn argb(a: u8, r: u8, g: u8, b: u8) -> Color {
    Color::argb(a, r, g, b)
}

impl Palette {
    pub const DARK: Self = Self {
        text_primary: rgb(0xFF, 0xFF, 0xFF),          // TextFillColorPrimary
        text_secondary: argb(0xC5, 0xFF, 0xFF, 0xFF), // TextFillColorSecondary
        error: rgb(0xFF, 0x00, 0x00),                 // egui error_fg_color
        critical: rgb(0xFF, 0x99, 0xA4),              // SystemFillColorCritical
        success: rgb(0x6C, 0xCB, 0x5F),               // SystemFillColorSuccess
        neutral: rgb(0x9D, 0x9D, 0x9D),
        on_status: rgb(0x00, 0x00, 0x00), // TextOnAccentFillColorPrimary
        accent: rgb(0x39, 0x87, 0xE5),
        chart_grid: rgb(0x3D, 0x3D, 0x3D),
        card_background: argb(0x0D, 0xFF, 0xFF, 0xFF), // CardBackgroundFillColorDefault
        card_stroke: argb(0x19, 0x00, 0x00, 0x00),     // CardStrokeColorDefault
        stripe: argb(0x0F, 0xFF, 0xFF, 0xFF),          // SubtleFillColorSecondary
        progress_track: rgb(0x0A, 0x0A, 0x0A),         // egui extreme_bg_color
        progress_fill: rgb(0x00, 0x5C, 0x80),          // egui selection.bg_fill
        progress_text: rgb(0xC0, 0xDE, 0xFF),          // egui selection.stroke
    };

    pub const LIGHT: Self = Self {
        text_primary: argb(0xE4, 0x00, 0x00, 0x00),
        text_secondary: argb(0x9E, 0x00, 0x00, 0x00),
        error: rgb(0xFF, 0x00, 0x00),
        critical: rgb(0xC4, 0x2B, 0x1C),
        success: rgb(0x0F, 0x7B, 0x0F),
        neutral: rgb(0x6E, 0x6E, 0x6E),
        on_status: rgb(0xFF, 0xFF, 0xFF),
        accent: rgb(0x2A, 0x78, 0xD6),
        chart_grid: rgb(0xE3, 0xE3, 0xE3),
        card_background: argb(0xB3, 0xFF, 0xFF, 0xFF),
        card_stroke: argb(0x0F, 0x00, 0x00, 0x00),
        stripe: argb(0x09, 0x00, 0x00, 0x00),
        progress_track: rgb(0xFF, 0xFF, 0xFF),
        progress_fill: rgb(0x90, 0xD1, 0xFF),
        progress_text: rgb(0x00, 0x53, 0x7D),
    };
}

/// Shorthand for a solid brush from a palette colour.
pub fn brush(color: Color) -> Brush {
    Brush::Solid(color)
}

#[derive(Clone, Copy, Debug)]
pub struct Palettes {
    pub light: Palette,
    pub dark: Palette,
}

impl Palettes {
    pub fn get(&self, dark: bool) -> &Palette {
        if dark { &self.dark } else { &self.light }
    }

    /// The defaults, with `colours.json` next to the exe applied over them if it exists. What it
    /// gets wrong goes to the console: the window opens before the session sets up its logger.
    pub fn load() -> Self {
        let mut problems = Vec::new();
        let palettes = beside_exe("colours.json").map_or(Self::DEFAULT, |path| match std::fs::read_to_string(&path) {
            Ok(text) => Self::parse(&text, &mut problems),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Self::DEFAULT,
            Err(e) => {
                problems.push(format!("{}: {e}", path.display()));
                Self::DEFAULT
            }
        });
        for problem in problems {
            eprintln!("colours.json: {problem}");
        }
        palettes
    }

    const DEFAULT: Self = Self { light: Palette::LIGHT, dark: Palette::DARK };

    /// `text`'s colours over the defaults; what it gets wrong into `problems`, each keeping its
    /// default
    fn parse(text: &str, problems: &mut Vec<String>) -> Self {
        let mut palettes = Self::DEFAULT;
        let root: serde_json::Value = match serde_json::from_str(text) {
            Ok(root) => root,
            Err(e) => {
                problems.push(format!("{e}; using the default colours"));
                return palettes;
            }
        };
        let Some(root) = root.as_object() else {
            problems.push("not a JSON object; using the default colours".to_string());
            return palettes;
        };
        for (scheme, value) in root {
            let palette = match scheme.as_str() {
                "dark" => &mut palettes.dark,
                "light" => &mut palettes.light,
                s if s.starts_with('_') => continue,
                s => {
                    problems.push(format!("unknown scheme {s:?} (dark or light)"));
                    continue;
                }
            };
            let Some(entries) = value.as_object() else {
                problems.push(format!("{scheme:?} is not an object"));
                continue;
            };
            for (key, value) in entries {
                if key.starts_with('_') {
                    continue;
                }
                let Some((_, field)) = KEYS.iter().find(|(k, _)| k == key) else {
                    problems.push(format!("{scheme}.{key} is not a colour key"));
                    continue;
                };
                match value.as_str().and_then(parse_colour) {
                    Some(colour) => *field(palette) = colour,
                    None => problems.push(format!("{scheme}.{key} = {value} is not #RRGGBB or #AARRGGBB")),
                }
            }
        }
        palettes
    }
}

fn beside_exe(name: &str) -> Option<PathBuf> {
    Some(std::env::current_exe().ok()?.parent()?.join(name))
}

/// `#RRGGBB` (opaque) or `#AARRGGBB`.
fn parse_colour(text: &str) -> Option<Color> {
    let hex = text.trim().strip_prefix('#')?;
    if !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let byte = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).ok();
    match hex.len() {
        6 => Some(Color::rgb(byte(0)?, byte(2)?, byte(4)?)),
        8 => Some(Color::argb(byte(0)?, byte(2)?, byte(4)?, byte(6)?)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn format_colour(c: Color) -> String {
        if c.a == 0xFF {
            format!("#{:02X}{:02X}{:02X}", c.r, c.g, c.b)
        } else {
            format!("#{:02X}{:02X}{:02X}{:02X}", c.a, c.r, c.g, c.b)
        }
    }

    /// What each key colours, for the example file (same order as `KEYS`).
    const ABOUT: [&str; 15] = [
        "most text: title, table cells, error lines, card titles, CPU name",
        "info line, table headings, empty-state lines, chart axis labels",
        "error bullets, a non-zero error count, the chart's error marks",
        "the 'Failed' badge fill",
        "the 'Running' and 'Passed' badge fill",
        "the other badges' fill: 'Ready', 'Stopping', 'Interrupted', 'Not run'",
        "badge text",
        "the chart's throughput line",
        "the chart's gridlines, the header divider",
        "card fill",
        "card border, progress track border",
        "alternate table rows",
        "progress bar, empty part",
        "progress bar, filled part",
        "the percentage inside the progress bar",
    ];

    /// The example file: every key at its default, and what it colours
    fn example_text() -> String {
        let mut out = String::from("{\n");
        out.push_str(
            "  \"_about\": [\n    \
             \"Copy this file next to tmr.exe as colours.json and edit it; it is read when the window opens.\",\n    \
             \"Colours are #RRGGBB or #AARRGGBB (alpha first). Leave a key out to keep its default; keys starting with _ are ignored.\",\n    \
             \"egui's grey text instead of WinUI's white: dark.text_primary #8C8C8C, light.text_primary #505050.\"\n  ],\n",
        );
        out.push_str("  \"_keys\": {\n");
        for (i, ((key, _), about)) in KEYS.iter().zip(ABOUT).enumerate() {
            let comma = if i + 1 < KEYS.len() { "," } else { "" };
            out.push_str(&format!("    \"{key}\": \"{about}\"{comma}\n"));
        }
        out.push_str("  },\n");
        for (name, palette, last) in [("dark", Palette::DARK, false), ("light", Palette::LIGHT, true)] {
            let mut palette = palette;
            out.push_str(&format!("  \"{name}\": {{\n"));
            for (i, (key, field)) in KEYS.iter().enumerate() {
                let comma = if i + 1 < KEYS.len() { "," } else { "" };
                let value = format_colour(*field(&mut palette));
                out.push_str(&format!("    \"{key}\": \"{value}\"{comma}\n"));
            }
            out.push_str(if last { "  }\n" } else { "  },\n" });
        }
        out.push_str("}\n");
        out
    }

    /// `colours.example.json` is every key at its default; `TMR_BLESS=1 cargo test -p tmr-gui`
    /// rewrites it, as it does the console's goldens
    #[test]
    fn the_example_file_has_every_key_at_its_default() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("colours.example.json");
        let text = example_text();
        if std::env::var_os("TMR_BLESS").is_some_and(|v| v == "1") {
            std::fs::write(&path, &text).unwrap();
        }
        let saved = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}; `TMR_BLESS=1` writes it", path.display()));
        assert_eq!(saved.replace("\r\n", "\n"), text, "{} is stale: `TMR_BLESS=1 cargo test -p tmr-gui` rewrites it", path.display());
        let mut problems = Vec::new();
        let parsed = Palettes::parse(&text, &mut problems);
        assert_eq!((parsed.dark, parsed.light, problems.len()), (Palette::DARK, Palette::LIGHT, 0), "{problems:?}");
    }

    /// A file's colours replace the defaults key by key; a bad one keeps its default and is reported
    #[test]
    fn a_colours_file_overrides_key_by_key() {
        let mut problems = Vec::new();
        let parsed = Palettes::parse(r##"{"_note": 1, "dark": {"text_primary": "#8C8C8C", "error": "#80FF0000", "accent": "blue",
                                          "nope": "#000000"}, "dim": {}}"##, &mut problems);
        assert_eq!(parsed.dark.text_primary, Color::rgb(0x8C, 0x8C, 0x8C));
        assert_eq!(parsed.dark.error, Color::argb(0x80, 0xFF, 0, 0));
        assert_eq!((parsed.dark.accent, parsed.light), (Palette::DARK.accent, Palette::LIGHT));
        assert_eq!(problems, ["dark.accent = \"blue\" is not #RRGGBB or #AARRGGBB", "dark.nope is not a colour key",
                              "unknown scheme \"dim\" (dark or light)"]);
        let mut problems = Vec::new();
        assert_eq!(Palettes::parse("[", &mut problems).dark, Palette::DARK);
        assert_eq!(problems.len(), 1);
    }
}
