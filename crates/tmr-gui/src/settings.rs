//! The window's settings (TODO 109): `window.json` in TMR's home (beside tmr.exe, or `TMR_HOME`), beside `tmr-cfg.json`, which is the
//! engine's record of this machine and its calibrations. What a user sets, and what the window keeps of its last session:
//!
//! | key             | what it is                                                                     | default   |
//! |-----------------|--------------------------------------------------------------------------------|-----------|
//! | `chart`         | the throughput chart shows; its ▾ hides it, leaving its title line             | `true`    |
//! | `theme`         | `"system"`, `"light"` or `"dark"`                                              | `system`  |
//! | `start_on_open` | the run starts as the window opens, as `--start` does                          | `false`   |
//! | `serials`       | the DIMMs' serials and the system's UUID in the run's output, log and results   | `true`    |
//! | `parameters`    | the parameters box's text when tmr.exe is given none: the last run's           | none      |
//! | `size`          | the window's size, `[width, height]`, as it last closed                         | 860 × 960 |
//! | `colours`       | the palette over its defaults, a `"dark"` and a `"light"` object (`palette.rs`) | none      |
//!
//! The window writes `chart` as it changes, `parameters` at Start, `size` as it closes, and its Settings dialog's (`start_on_open`,
//! `serials`, `theme`) as they change, keeping the rest of the file (the colours, any
//! notes) as it was. Keys starting with `_` are notes. A missing file is the defaults; a bad key or value goes in the errors list and keeps
//! its default; a file that isn't JSON says so, and the window doesn't write over it. `crates/tmr-gui/window.example.json` has every key
//! at its default, a test keeping it so: copy it into TMR's folder as `window.json` and edit it.

use std::path::PathBuf;

use serde_json::{Map, Value};

use crate::palette::Palettes;

/// The window's theme: Windows', or always light or dark
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Theme {
    System,
    Light,
    Dark,
}

impl Theme {
    const NAMES: [(&str, Theme); 3] = [("system", Theme::System), ("light", Theme::Light), ("dark", Theme::Dark)];

    /// The Settings dialog's choices, in the order of `NAMES`
    pub const LABELS: [&str; 3] = ["As Windows", "Light", "Dark"];

    /// Its place in `NAMES` and `LABELS`
    pub fn index(self) -> usize {
        Self::NAMES.iter().position(|(_, theme)| *theme == self).unwrap_or(0)
    }

    pub fn at(index: usize) -> Self {
        Self::NAMES.get(index).map_or(Theme::System, |(_, theme)| *theme)
    }

    fn name(self) -> &'static str {
        Self::NAMES.iter().find(|(_, theme)| *theme == self).map_or("system", |(name, _)| name)
    }
}

/// The settings, and the file they came from
#[derive(Clone, Debug, PartialEq)]
pub struct Settings {
    pub chart: bool,
    pub theme: Theme,
    pub start_on_open: bool,
    /// The run's output, log and results keep the DIMMs' serials and the system's UUID; off, the window runs with `--no-serials`
    pub serials: bool,
    pub parameters: Option<String>,
    pub size: Option<(f64, f64)>,
    /// The file as it was read, so a save keeps what the window doesn't own: the colours, notes
    file: Map<String, Value>,
    /// The file isn't JSON: the window doesn't write over it
    unreadable: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            chart: true,
            theme: Theme::System,
            start_on_open: false,
            serials: true,
            parameters: None,
            size: None,
            file: Map::new(),
            unreadable: false,
        }
    }
}

/// `window.json`, in TMR's home
fn path() -> PathBuf {
    tmr::home::dir().join("window.json")
}

impl Settings {
    /// `window.json`'s settings and colours, over the defaults, and what it gets wrong, for the window's errors list: it opens before
    /// Start sets up its logger, and maybe with no console
    pub fn load() -> (Self, Palettes, Vec<String>) {
        let mut problems = Vec::new();
        let (settings, palettes) = match std::fs::read_to_string(path()) {
            Ok(text) => Self::parse(&text, &mut problems),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (Self::default(), Palettes::DEFAULT),
            Err(e) => {
                problems.push(format!("{}: {e}", path().display()));
                (Settings { unreadable: true, ..Self::default() }, Palettes::DEFAULT)
            }
        };
        (settings, palettes, problems)
    }

    /// `text`'s settings and colours over the defaults; what it gets wrong into `problems`, each keeping its default
    fn parse(text: &str, problems: &mut Vec<String>) -> (Self, Palettes) {
        let mut settings = Self::default();
        let file = match serde_json::from_str::<Value>(text) {
            Ok(Value::Object(file)) => file,
            Ok(_) => {
                problems.push("not a JSON object: the defaults, and the window won't write over it".to_string());
                return (Settings { unreadable: true, ..settings }, Palettes::DEFAULT);
            }
            Err(e) => {
                problems.push(format!("{e}: the defaults, and the window won't write over it"));
                return (Settings { unreadable: true, ..settings }, Palettes::DEFAULT);
            }
        };
        let mut palettes = Palettes::DEFAULT;
        for (key, value) in &file {
            let wrong = |what: &str| format!("{key} = {value} is not {what}");
            match key.as_str() {
                key if key.starts_with('_') => {}
                "chart" => match value.as_bool() {
                    Some(chart) => settings.chart = chart,
                    None => problems.push(wrong("true or false")),
                },
                "start_on_open" => match value.as_bool() {
                    Some(start) => settings.start_on_open = start,
                    None => problems.push(wrong("true or false")),
                },
                "serials" => match value.as_bool() {
                    Some(serials) => settings.serials = serials,
                    None => problems.push(wrong("true or false")),
                },
                "theme" => match Theme::NAMES.iter().find(|(name, _)| value.as_str() == Some(name)) {
                    Some((_, theme)) => settings.theme = *theme,
                    None => problems.push(wrong("\"system\", \"light\" or \"dark\"")),
                },
                "parameters" => match value {
                    Value::Null => settings.parameters = None,
                    Value::String(text) => settings.parameters = Some(text.clone()),
                    _ => problems.push(wrong("a string")),
                },
                "size" => match value.as_array().map(|size| size.iter().map(Value::as_f64).collect::<Vec<_>>()).as_deref() {
                    Some([Some(width), Some(height)]) if *width > 0.0 && *height > 0.0 => settings.size = Some((*width, *height)),
                    _ => problems.push(wrong("[width, height]")),
                },
                "colours" => palettes = Palettes::parse(value, problems),
                key => problems.push(format!("{key} is not a setting")),
            }
        }
        (Settings { file, ..settings }, palettes)
    }

    /// The file as the window writes it: what it read, its settings as they are now
    fn text(&self) -> String {
        let mut file = self.file.clone();
        file.insert("chart".to_string(), Value::Bool(self.chart));
        file.insert("theme".to_string(), Value::String(self.theme.name().to_string()));
        file.insert("start_on_open".to_string(), Value::Bool(self.start_on_open));
        file.insert("serials".to_string(), Value::Bool(self.serials));
        file.insert("parameters".to_string(), self.parameters.clone().map_or(Value::Null, Value::String));
        if let Some((width, height)) = self.size {
            file.insert("size".to_string(), serde_json::json!([width.round(), height.round()]));
        }
        let mut text = serde_json::to_string_pretty(&Value::Object(file)).unwrap_or_default();
        text.push('\n');
        text
    }

    /// Writes `window.json`, unless the file there isn't JSON (the user's to fix); why it couldn't, otherwise
    pub fn save(&self) -> Result<(), String> {
        if self.unreadable {
            return Ok(());
        }
        std::fs::write(path(), self.text()).map_err(|e| format!("{}: {e}", path().display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The example file: every setting at its default, and the colours' every key at its default, with what each is
    fn example_text() -> String {
        let colours = crate::palette::example_colours();
        format!(
            "{{\n  \"_about\": [\n    \
             \"Copy this file into TMR's folder (beside tmr.exe, or TMR_HOME) as window.json and edit it.\",\n    \
             \"It is read when the window opens. Keys starting with _ are notes.\",\n    \
             \"chart: the throughput chart shows. theme: system, light or dark.\",\n    \
             \"start_on_open: the run starts as the window opens, as --start does.\",\n    \
             \"serials: the DIMMs' serials and the system's UUID in the run's output, log and results; false runs with --no-serials.\",\n    \
             \"parameters: the parameters box's text when tmr.exe is given none; the window keeps the last run's.\",\n    \
             \"size: [width, height], as it last closed.\",\n    \
             \"colours: #RRGGBB or #AARRGGBB (alpha first). Leave a key out to keep its default.\"\n  ],\n  \
             \"chart\": true,\n  \"theme\": \"system\",\n  \"start_on_open\": false,\n  \"serials\": true,\n  \"parameters\": null,\n  \
             \"colours\": {colours}\n}}\n"
        )
    }

    /// `window.example.json` is every key at its default; `TMR_BLESS=1 cargo test -p test-memory-r-gui` rewrites it, as it does the
    /// console's goldens
    #[test]
    fn the_example_file_has_every_key_at_its_default() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("window.example.json");
        let text = example_text();
        if std::env::var_os("TMR_BLESS").is_some_and(|v| v == "1") {
            std::fs::write(&path, &text).expect("the example file");
        }
        let saved = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}; `TMR_BLESS=1` writes it", path.display()));
        assert_eq!(
            saved.replace("\r\n", "\n"),
            text,
            "{} is stale: `TMR_BLESS=1 cargo test -p test-memory-r-gui` rewrites it",
            path.display()
        );
        let mut problems = Vec::new();
        let (settings, palettes) = Settings::parse(&text, &mut problems);
        assert_eq!((settings.chart, settings.theme, settings.start_on_open, settings.parameters), (true, Theme::System, false, None));
        assert_eq!((palettes.dark, palettes.light, problems.len()), (Palettes::DEFAULT.dark, Palettes::DEFAULT.light, 0), "{problems:?}");
    }

    /// The file's settings over the defaults, a bad one keeping its default and saying so; a save keeps what it doesn't own, the colours
    /// and the notes, and a file that isn't JSON is the defaults and isn't written over
    #[test]
    fn a_settings_file_over_the_defaults_and_saved_with_what_it_had() {
        let mut problems = Vec::new();
        let text = r##"{"_note": "mine", "chart": false, "theme": "dark", "start_on_open": "yes", "size": [1600, 1000.4],
                        "parameters": "memory=50%", "colours": {"dark": {"accent": "#FF0000"}}, "zoom": 2}"##;
        let (mut settings, palettes) = Settings::parse(text, &mut problems);
        assert_eq!((settings.chart, settings.theme, settings.start_on_open), (false, Theme::Dark, false));
        assert_eq!((settings.size, settings.parameters.as_deref()), (Some((1600.0, 1000.4)), Some("memory=50%")));
        assert_eq!(palettes.dark.accent, windows_reactor::Color::rgb(0xFF, 0, 0));
        assert_eq!(problems, ["start_on_open = \"yes\" is not true or false", "zoom is not a setting"]);

        settings.chart = true;
        let saved: Value = serde_json::from_str(&settings.text()).expect("JSON");
        assert_eq!(
            (&saved["_note"], &saved["chart"], &saved["size"], &saved["colours"]["dark"]["accent"]),
            (&Value::from("mine"), &Value::from(true), &serde_json::json!([1600.0, 1000.0]), &Value::from("#FF0000"))
        );

        let mut problems = Vec::new();
        let (broken, _) = Settings::parse("{", &mut problems);
        assert!(broken.unreadable && problems.len() == 1 && broken.save().is_ok(), "{problems:?}");
    }
}
