//! The panel's translation catalogs (omarchy/plugin/i18n) against the QML that reads them: `en.json` lists exactly the
//! strings the panel translates, so a string added without a catalog entry, or left behind after its last use, fails
//! here; and every other catalog only translates what `en.json` has, with the same placeholders.
use regex::Regex;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// Read by the installer for the app launcher entry (src/desktop/install.rs), not by the QML.
const LAUNCHER_KEYS: [&str; 2] = ["desktop.name", "desktop.comment"];
/// Captions that are meant to be the same in every language: the brand name, and the `code:` prefix the engine looks for, and
/// example values that have to be typed as shown.
const UNTRANSLATED: [&str; 5] =
    ["Recall", "code: ", "heic, csv", "/home/you/projects", "/home/you/Documents\\n/home/you/Pictures"];

fn plugin() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("omarchy/plugin")
}

fn qml() -> Vec<(String, String)> {
    let mut files: Vec<_> = std::fs::read_dir(plugin())
        .unwrap()
        .flatten()
        .filter(|entry| entry.path().extension().is_some_and(|e| e == "qml"))
        .map(|entry| (entry.file_name().to_string_lossy().into_owned(), std::fs::read_to_string(entry.path()).unwrap()))
        .collect();
    files.sort();
    assert!(!files.is_empty(), "no QML in {}", plugin().display());
    files
}

fn catalog(name: &str) -> BTreeMap<String, Value> {
    let path = plugin().join("i18n").join(name);
    serde_json::from_slice(&std::fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display())))
        .unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// The `%1`, `%2` holes of a string, or of every form of a plural entry.
fn placeholders(value: &Value) -> BTreeSet<String> {
    let hole = Regex::new(r"%\d").unwrap();
    let texts: Vec<&str> = match value {
        Value::Object(forms) => forms.values().filter_map(Value::as_str).collect(),
        other => other.as_str().into_iter().collect(),
    };
    texts.iter().flat_map(|text| hole.find_iter(text).map(|m| m.as_str().to_string())).collect()
}

/// Every key the QML asks for, with whether it is asked for as a plural (`tn`).
fn used() -> BTreeMap<String, bool> {
    let call = Regex::new(r#"strings\.(tn?)\(\s*"([^"]+)""#).unwrap();
    let any_call = Regex::new(r"strings\.tn?\(").unwrap();
    let mut keys = BTreeMap::new();
    for (name, text) in qml() {
        // A key built at run time could not be checked against the catalog, so every call names its key outright.
        assert_eq!(call.find_iter(&text).count(), any_call.find_iter(&text).count(), "{name}: a strings.t() call without a literal key");
        for found in call.captures_iter(&text) {
            let plural = &found[1] == "tn";
            let earlier = keys.insert(found[2].to_string(), plural);
            assert!(earlier.is_none_or(|was| was == plural), "{name}: {} is read both as a plural and as a plain string", &found[2]);
        }
    }
    keys
}

#[test]
fn english_lists_exactly_the_strings_the_panel_uses() {
    let english = catalog("en.json");
    let used = used();
    let listed: BTreeSet<&str> = english.keys().map(String::as_str).collect();
    let wanted: BTreeSet<&str> = used.keys().map(String::as_str).chain(LAUNCHER_KEYS).collect();
    let missing: Vec<_> = wanted.difference(&listed).collect();
    let unused: Vec<_> = listed.difference(&wanted).collect();
    assert!(missing.is_empty(), "used by the panel but not in en.json: {missing:?}");
    assert!(unused.is_empty(), "in en.json but no longer used: {unused:?}");
    for (key, value) in &english {
        if used.get(key) == Some(&true) {
            // English has two plural forms; a count goes through them rather than through a glued "N RESULTS".
            let forms = value.as_object().unwrap_or_else(|| panic!("{key} is counted, so it needs plural forms"));
            assert!(forms.contains_key("one") && forms.contains_key("other"), "{key} needs `one` and `other`");
            assert!(forms.values().all(|form| form.as_str().is_some_and(|text| text.contains("%1"))), "{key}: each form shows the count as %1");
        } else {
            assert!(value.as_str().is_some_and(|text| !text.trim().is_empty()), "{key} must be a non-empty string");
        }
    }
}

#[test]
fn other_catalogs_translate_only_what_english_has() {
    let english = catalog("en.json");
    let code = Regex::new(r"^[a-z]{2,3}(-[A-Z]{2})?\.json$").unwrap();
    for entry in std::fs::read_dir(plugin().join("i18n")).unwrap().flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        // Catalogs are named by Omarchy language code (da.json, pt-BR.json); anything else is never read.
        assert!(code.is_match(&name), "{name} is not an Omarchy language code");
        if name == "en.json" {
            continue;
        }
        for (key, value) in catalog(&name) {
            let source = english.get(&key).unwrap_or_else(|| panic!("{name}: {key} is not in en.json"));
            assert_eq!(placeholders(&value), placeholders(source), "{name}: {key} has other placeholders than English");
            match source {
                Value::Object(_) => assert!(value.get("other").is_some_and(Value::is_string), "{name}: {key} needs an `other` form"),
                _ => assert!(value.is_string(), "{name}: {key} must be a string"),
            }
        }
    }
}

#[test]
fn no_caption_is_written_straight_into_the_qml() {
    // What a person reads comes from the catalog. A quoted caption with a word in it is either on the short list of
    // things that are the same in every language, or it was forgotten.
    let caption = Regex::new(r#"\b(text|placeholderText|hint|label|tooltipText|message)\s*:\s*"([^"]*)""#).unwrap();
    let word = Regex::new(r"[A-Za-z]{2,}").unwrap();
    for (name, text) in qml() {
        for found in caption.captures_iter(&text) {
            let literal = &found[2];
            assert!(!word.is_match(literal) || UNTRANSLATED.contains(&literal), "{name}: \"{literal}\" is not translatable; read it with strings.t()");
        }
    }
}
