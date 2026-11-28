//! The window's settings (TODO 109): `window.json` in the `settings\` folder of TMR's home (beside tmr.exe, or `TMR_HOME`), beside
//! `tmr-cfg.json`, which is the engine's record of this machine and its calibrations (`tmr::settings_file`). What a user sets, and what
//! the window keeps of its last session:
//!
//! | key             | what it is                                                                     | default   |
//! |-----------------|--------------------------------------------------------------------------------|-----------|
//! | `chart`         | the throughput chart shows; its ▾ hides it, leaving its title line             | `true`    |
//! | `theme`         | `"system"`, `"light"` or `"dark"`                                              | `system`  |
//! | `start_on_open` | the run starts as the window opens, as `--start` does                          | `false`   |
//! | `keep_serials`  | the DIMMs' serials, the system's UUID and fingerprints in the run's output, log | `false`   |
//! |                 | and results, as `--serials` keeps them                                         |           |
//! | `notify`        | a sound at the first error, and when the run ends                                | `false`   |
//! | `language`      | the window's language: `en`, `it`, `ru`, `zh`, `ja`, `es`                         | `en`      |
//! | `parameters`    | the parameters box's text when tmr.exe is given none: the last run's           | none      |
//! | `size`          | the window's size, `[width, height]`, as it last closed                         | 860 × 960 |
//! | `position`      | its frame's top-left, `[x, y]` in screen pixels, as it last closed             | Windows'  |
//! | `maximized`     | it closed maximized and opens so, restoring to `size` and `position`           | `false`   |
//! | `colours`       | the palette over its defaults, a `"dark"` and a `"light"` object (`palette.rs`) | none      |
//!
//! `position` and `maximized` are put back only onto the screens there now: mostly off them, the window opens where Windows puts it
//! (`placement.rs`). The window writes `chart` as it changes, `parameters` at Start, `size`, `position` and `maximized` as it closes (and
//! with any other save), and its Settings dialog's (`start_on_open`, `keep_serials`, `notify`, `theme`) as they change, keeping the rest of the file
//! (the colours, any notes) as it was, but for `serials`, which windows before `keep_serials` wrote (on, their default), dropped unread.
//! Keys starting with `_` are notes. A missing file is the defaults; a bad key or value goes in the errors list and keeps its default. A
//! save is written whole, then as the file's last good copy, `window.json.bak` (`tmr::settings_file::save`), so a PC that goes down
//! mid-save keeps the last one. A file there that can't be read or isn't JSON takes that copy when it reads, which the errors list says,
//! and is kept as `window.json.unreadable`; with no copy that reads, it is the defaults, and the window doesn't write over it (the user's
//! to fix). `crates/tmr-gui/window.example.json` has every key at its default, a test keeping it so: copy it into TMR's `settings\` as
//! `window.json` and edit it.

use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::lang::Language;
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

    /// Its place in `NAMES`. The dialog's words are `lang`'s, in this order.
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
    /// Display state of the protected per-account switch. Only its own toggle writes the switch.
    pub allow_admin: bool,
    /// The run's output, log and results keep the DIMMs' serials, the system's UUID and the machine's fingerprints: the window runs with
    /// `--serials`. Off by default, as a run without `--serials` leaves them out (the owner, 2026-11-23)
    pub keep_serials: bool,
    /// A sound at the first error of a run, and when the run ends. Off: a long soak stays quiet.
    pub notify: bool,
    /// The window's language. English unless the user picks another. Not Windows' language: a test must not change with the machine.
    pub language: Language,
    pub parameters: Option<String>,
    pub size: Option<(f64, f64)>,
    /// Its frame's top-left in screen pixels, and whether it was maximized, as it last closed (`placement.rs`)
    pub position: Option<(i32, i32)>,
    pub maximized: bool,
    /// The file as it was read, so a save keeps what the window doesn't own: the colours, notes
    file: Map<String, Value>,
    /// The file can't be read or isn't JSON, and wasn't kept aside: the window doesn't write over it
    unreadable: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            chart: true,
            theme: Theme::System,
            start_on_open: false,
            allow_admin: false,
            keep_serials: false,
            notify: false,
            language: Language::En,
            parameters: None,
            size: None,
            position: None,
            maximized: false,
            file: Map::new(),
            unreadable: false,
        }
    }
}

/// The serials' setting as windows before `keep_serials` wrote it, on by default
const RETIRED_SERIALS: &str = "serials";

/// `window.json`, in the `settings\` of TMR's home, one still in the old place taken into it first (`tmr::settings_file::path`)
fn path() -> PathBuf {
    tmr::settings_file::path(&tmr::home::window_settings_name())
}

/// `text` as window.json's object, or why it isn't one
fn object(text: &str) -> Result<Map<String, Value>, String> {
    match serde_json::from_str::<Value>(text) {
        Ok(Value::Object(file)) => Ok(file),
        Ok(_) => Err("not a JSON object".to_string()),
        Err(e) => Err(e.to_string()),
    }
}

/// The settings file at `path` as an object: none when it isn't there, or why it can't be read or isn't one
fn read(path: &Path) -> Result<Option<Map<String, Value>>, String> {
    match tmr::home::read_settings(path).and_then(|b| String::from_utf8(b).map_err(std::io::Error::other)) {
        Ok(text) => object(&text).map(Some),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

/// A screen coordinate: a whole number of pixels that fits Win32's
fn pixel(value: &Value) -> Option<i32> {
    value.as_i64().and_then(|value| i32::try_from(value).ok())
}

impl Settings {
    /// `window.json`'s settings and colours, over the defaults, and what it gets wrong, for the window's errors list: it opens before
    /// Start sets up its logger, and maybe with no console
    pub fn load() -> (Self, Palettes, Vec<String>) {
        Self::load_from(&path())
    }

    /// The settings file at `path`'s, over the defaults, and what it gets wrong. One there that can't be read or isn't JSON takes its
    /// last good copy (`tmr::settings_file::backup_of`) when that reads, said first, and is kept aside, the window writing over it only
    /// once it is (`tmr::settings_file::keep_aside`); with no copy that reads, the defaults, and the window doesn't write over it.
    fn load_from(path: &Path) -> (Self, Palettes, Vec<String>) {
        let mut problems = Vec::new();
        let why = match read(path) {
            Ok(Some(file)) => {
                let (settings, palettes) = Self::over_defaults(file, &mut problems);
                return (settings, palettes, problems);
            }
            Ok(None) => return (Self::default(), Palettes::DEFAULT, problems),
            Err(why) => why,
        };
        let named = |path: &Path| path.file_name().map_or_else(|| path.display().to_string(), |name| name.display().to_string());
        let backup = tmr::settings_file::backup_of(path);
        let copy = named(&backup);
        let file = match read(&backup) {
            Ok(Some(file)) => file,
            not_taken => {
                let no_good =
                    not_taken.err().map_or_else(String::new, |bad| format!("; its last good copy, {copy}, is no good either ({bad})"));
                problems.push(format!("{why}: the defaults, and the window won't write over it{no_good}"));
                return (Settings { unreadable: true, ..Self::default() }, Palettes::DEFAULT, problems);
            }
        };
        let kept = tmr::settings_file::keep_aside(path);
        problems.push(match &kept {
            Ok(kept) => format!("{why}: its last good copy, {copy}, is taken; it is kept as {}", named(kept)),
            Err(e) => {
                format!("{why}: its last good copy, {copy}, is taken; it couldn't be kept aside ({e}), so the window won't write over it")
            }
        });
        let (settings, palettes) = Self::over_defaults(file, &mut problems);
        (Settings { unreadable: kept.is_err(), ..settings }, palettes, problems)
    }

    /// `text`'s settings and colours over the defaults, what it gets wrong into `problems`; why when it isn't a JSON object
    #[cfg(test)]
    fn parse(text: &str, problems: &mut Vec<String>) -> Result<(Self, Palettes), String> {
        object(text).map(|file| Self::over_defaults(file, problems))
    }

    /// `file`'s settings and colours over the defaults; what it gets wrong into `problems`, each keeping its default
    fn over_defaults(mut file: Map<String, Value>, problems: &mut Vec<String>) -> (Self, Palettes) {
        let mut settings = Self::default();
        // The setting before `keep_serials`, which every window.json has had (`true`, its default, as the window wrote it): not read, so a
        // file written then doesn't keep the serials, and gone from the next save
        file.remove(RETIRED_SERIALS);
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
                "keep_serials" => match value.as_bool() {
                    Some(keep) => settings.keep_serials = keep,
                    None => problems.push(wrong("true or false")),
                },
                "notify" => match value.as_bool() {
                    Some(notify) => settings.notify = notify,
                    None => problems.push(wrong("true or false")),
                },
                "language" => match value.as_str().and_then(Language::parse) {
                    Some(language) => settings.language = language,
                    None => problems.push(wrong("\"en\", \"it\", \"ru\", \"zh\", \"ja\" or \"es\"")),
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
                // Whole pixels, either side of the primary screen's top-left
                "position" => match value.as_array().map(|at| at.iter().map(pixel).collect::<Vec<_>>()).as_deref() {
                    Some([Some(x), Some(y)]) => settings.position = Some((*x, *y)),
                    _ => problems.push(wrong("[x, y], in whole pixels")),
                },
                "maximized" => match value.as_bool() {
                    Some(maximized) => settings.maximized = maximized,
                    None => problems.push(wrong("true or false")),
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
        file.insert("keep_serials".to_string(), Value::Bool(self.keep_serials));
        file.insert("notify".to_string(), Value::Bool(self.notify));
        file.insert("language".to_string(), Value::String(self.language.code().to_string()));
        file.insert("parameters".to_string(), self.parameters.clone().map_or(Value::Null, Value::String));
        if let Some((width, height)) = self.size {
            file.insert("size".to_string(), serde_json::json!([width.round(), height.round()]));
        }
        if let Some((x, y)) = self.position {
            file.insert("position".to_string(), serde_json::json!([x, y]));
        }
        file.insert("maximized".to_string(), Value::Bool(self.maximized));
        let mut text = serde_json::to_string_pretty(&Value::Object(file)).unwrap_or_default();
        text.push('\n');
        text
    }

    /// Writes `window.json`, whole, then its last good copy (`tmr::settings_file::save`: in place where `settings\` takes no new file),
    /// unless the file there couldn't be read or isn't JSON and wasn't kept aside (the user's to fix); why it couldn't, otherwise
    pub fn save(&self) -> Result<(), String> {
        self.save_to(&path())
    }

    /// `save`, to the settings file at `path`
    fn save_to(&self, path: &Path) -> Result<(), String> {
        if self.unreadable {
            return Ok(());
        }
        tmr::settings_file::save(path, self.text().as_bytes()).map(drop)
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
             \"Copy this file into the settings folder in TMR's home (beside tmr.exe, or TMR_HOME) as window.json and edit it.\",\n    \
             \"It is read when the window opens. Keys starting with _ are notes.\",\n    \
             \"chart: the throughput chart shows. theme: system, light or dark.\",\n    \
             \"start_on_open: the run starts as the window opens, as --start does.\",\n    \
             \"keep_serials: the DIMMs' serials, the system's UUID and fingerprints in the run's output, log and results, as --serials \
             keeps them; false, the default, leaves them out.\",\n    \
             \"notify: a sound at the first error, and when the run ends. Off by default.\",\n    \
             \"language: the window's language, en, it, ru, zh, ja or es. The log and the results stay English.\",\n    \
             \"parameters: the parameters box's text when tmr.exe is given none; the window keeps the last run's.\",\n    \
             \"size: [width, height], as it last closed.\",\n    \
             \"position: [x, y], its top-left in screen pixels, as it last closed; off the screens now, Windows places it.\",\n    \
             \"maximized: it closed maximized, and opens so; size and position are what it restores to.\",\n    \
             \"colours: #RRGGBB or #AARRGGBB (alpha first). Leave a key out to keep its default.\"\n  ],\n  \
             \"chart\": true,\n  \"theme\": \"system\",\n  \"start_on_open\": false,\n  \"keep_serials\": false,\n  \
             \"notify\": false,\n  \"language\": \"en\",\n  \"parameters\": null,\n  \
             \"maximized\": false,\n  \"colours\": {colours}\n}}\n"
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
        let (settings, palettes) = Settings::parse(&text, &mut problems).expect("an object");
        assert_eq!(
            (settings.chart, settings.theme, settings.start_on_open, settings.language, settings.parameters),
            (true, Theme::System, false, Language::En, None)
        );
        assert_eq!((settings.position, settings.maximized), (None, false));
        assert_eq!((palettes.dark, palettes.light, problems.len()), (Palettes::DEFAULT.dark, Palettes::DEFAULT.light, 0), "{problems:?}");
    }

    /// The file's settings over the defaults, a bad one keeping its default and saying so; a save keeps what it doesn't own, the colours
    /// and the notes; a file that isn't a JSON object says why
    #[test]
    fn a_settings_file_over_the_defaults_and_saved_with_what_it_had() {
        let mut problems = Vec::new();
        let text = r##"{"_note": "mine", "chart": false, "theme": "dark", "start_on_open": "yes", "size": [1600, 1000.4],
                        "parameters": "memory=50%", "colours": {"dark": {"accent": "#FF0000"}}, "zoom": 2}"##;
        let (mut settings, palettes) = Settings::parse(text, &mut problems).expect("an object");
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

        assert_eq!(Settings::parse("{", &mut Vec::new()).map(drop), Err("EOF while parsing an object at line 1 column 1".to_string()));
        assert_eq!(Settings::parse("[]", &mut Vec::new()).map(drop), Err("not a JSON object".to_string()));
    }

    /// A window.json damaged (a crash while it was written in place, an edit gone wrong) takes its last good copy, the version the last
    /// save wrote, which the errors list says first, and is kept aside, so the window writes over it, and the copy with it. One not there
    /// is the defaults, the copy left alone; with no copy that reads, the defaults, said, and the window doesn't write over it.
    #[test]
    fn a_damaged_settings_file_takes_its_last_good_copy() {
        let dir = tmr::test_support::test_home().join("gui-last-good");
        let path = dir.join(tmr::settings_file::FOLDER).join("window.json");
        let (copy, kept) = (tmr::settings_file::backup_of(&path), path.with_file_name("window.json.unreadable"));
        let saved = Settings { chart: false, theme: Theme::Dark, parameters: Some("plan=stability".to_string()), ..Settings::default() };
        saved.save_to(&path).expect("saved, settings\\ made");
        assert_eq!(std::fs::read(&copy).expect("the copy"), std::fs::read(&path).expect("the file"), "the version just written");

        std::fs::write(&path, b"{\n  \"chart\": fal").expect("damaged");
        let (restored, _, problems) = Settings::load_from(&path);
        assert_eq!((restored.chart, restored.theme, restored.parameters.as_deref()), (false, Theme::Dark, Some("plan=stability")));
        assert_eq!(problems, [
            "EOF while parsing a value at line 2 column 14: its last good copy, window.json.bak, is taken; it is kept as \
             window.json.unreadable"
        ]);
        assert_eq!(std::fs::read(&kept).expect("kept aside"), b"{\n  \"chart\": fal");
        Settings { chart: true, ..restored }.save_to(&path).expect("written over, as it is kept");
        let (again, _, problems) = Settings::load_from(&path);
        assert!(again.chart && problems.is_empty(), "{problems:?}");
        assert_eq!(std::fs::read(&copy).expect("the copy"), std::fs::read(&path).expect("the file"));

        std::fs::remove_file(&path).expect("deleted");
        let (fresh, _, problems) = Settings::load_from(&path);
        assert!(fresh == Settings::default() && problems.is_empty() && copy.exists(), "not there: the defaults, the copy left alone");

        std::fs::write(&path, b"[]").expect("not an object");
        std::fs::write(&copy, b"{").expect("nor the copy");
        let (neither, _, problems) = Settings::load_from(&path);
        assert!(neither.unreadable && neither.chart, "the defaults");
        assert_eq!(problems, [
            "not a JSON object: the defaults, and the window won't write over it; its last good copy, window.json.bak, is no good \
             either (EOF while parsing an object at line 1 column 1)"
        ]);
        neither.save_to(&path).expect("nothing to write");
        assert_eq!(std::fs::read(&path).expect("the file"), b"[]", "not written over");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Where the window closed: its position in whole pixels, either side of the primary screen's top-left, and maximized or not; a bad
    /// one keeps the default, Windows' place, and a save writes both, the position only once there is one
    #[test]
    fn where_the_window_closed_read_and_written() {
        let mut problems = Vec::new();
        let (mut settings, _) =
            Settings::parse(r#"{"position": [-1500, 40], "maximized": true, "chart": false}"#, &mut problems).expect("an object");
        assert_eq!((settings.position, settings.maximized, problems.len()), (Some((-1500, 40)), true, 0), "{problems:?}");
        settings.position = Some((2600, -8));
        let saved: Value = serde_json::from_str(&settings.text()).expect("JSON");
        assert_eq!(
            (&saved["position"], &saved["maximized"], &saved["chart"]),
            (&serde_json::json!([2600, -8]), &Value::from(true), &Value::from(false))
        );

        for bad in
            [r#"{"position": [10.5, 20]}"#, r#"{"position": [10]}"#, r#"{"position": [1, 2, 3]}"#, r#"{"position": [4294967296, 0]}"#]
        {
            let mut problems = Vec::new();
            let (settings, _) = Settings::parse(bad, &mut problems).expect("an object");
            assert_eq!(settings.position, None, "{bad}");
            assert!(problems.len() == 1 && problems[0].ends_with("is not [x, y], in whole pixels"), "{bad}: {problems:?}");
        }
        let mut problems = Vec::new();
        let (settings, _) = Settings::parse(r#"{"maximized": "yes"}"#, &mut problems).expect("an object");
        assert_eq!((settings.maximized, &problems[..]), (false, &["maximized = \"yes\" is not true or false".to_string()][..]));

        let saved: Value = serde_json::from_str(&Settings::default().text()).expect("JSON");
        assert_eq!((saved.get("position"), &saved["maximized"]), (None, &Value::from(false)));
    }

    /// The serials are left out unless `keep_serials` (the owner, 2026-11-23). `serials`, which a window before it wrote to every
    /// window.json as `true`, its default then, isn't read, so such a file doesn't keep them, and the next save drops it; it is no problem
    /// to show
    #[test]
    fn the_serials_are_kept_only_when_asked_and_the_old_key_is_dropped() {
        let mut problems = Vec::new();
        let (settings, _) = Settings::parse(r#"{"serials": true, "chart": false}"#, &mut problems).expect("an object");
        assert_eq!((settings.keep_serials, settings.chart, problems.len()), (false, false, 0), "{problems:?}");
        let saved: Value = serde_json::from_str(&settings.text()).expect("JSON");
        assert_eq!((saved.get("serials"), &saved["keep_serials"]), (None, &Value::from(false)));
        let (settings, _) = Settings::parse(r#"{"keep_serials": true}"#, &mut problems).expect("an object");
        assert!(settings.keep_serials && problems.is_empty(), "{problems:?}");
        assert!(!Settings::default().keep_serials);
    }
}
