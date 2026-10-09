use std::collections::BTreeMap;
use std::sync::OnceLock;

include!(concat!(env!("OUT_DIR"), "/locales.rs"));
const ENGLISH: &str = include_str!("../locales/en.toml");

#[derive(Debug)]
pub struct Locale {
    strings: BTreeMap<String, String>,
}

impl Locale {
    pub fn load(english: &str, translation: Option<&str>) -> Result<Self, toml::de::Error> {
        let mut strings: BTreeMap<String, String> = toml::from_str(english)?;
        if let Some(translation) = translation {
            let selected: BTreeMap<String, String> = toml::from_str(translation)?;
            for (key, value) in selected {
                if strings.contains_key(&key) {
                    strings.insert(key, value);
                }
            }
        }
        Ok(Self { strings })
    }

    pub fn get<'a>(&'a self, key: &'a str) -> &'a str {
        self.strings
            .get(key)
            .map(String::as_str)
            .unwrap_or_else(|| {
                tracing::error!("Missing English locale key: {key}");
                key
            })
    }
}

pub fn is_supported(language: &str) -> bool {
    BUNDLED_CATALOGS
        .iter()
        .any(|(identifier, _)| *identifier == language)
}

fn bundled(language: &str) -> Result<&'static Locale, &'static toml::de::Error> {
    static LOCALES: OnceLock<BTreeMap<&str, Result<Locale, toml::de::Error>>> = OnceLock::new();
    let locales = LOCALES.get_or_init(|| {
        BUNDLED_CATALOGS
            .iter()
            .map(|(identifier, contents)| {
                let translation = (*identifier != "en").then_some(*contents);
                let result = Locale::load(ENGLISH, translation);
                if *identifier != "en" {
                    if let Err(error) = &result {
                        tracing::warn!(
                            "Bundled {identifier} locale is invalid; using English: {error}"
                        );
                    }
                }
                (*identifier, result)
            })
            .collect()
    });
    let english = locales["en"].as_ref()?;
    Ok(locales
        .get(language)
        .and_then(|locale| locale.as_ref().ok())
        .unwrap_or(english))
}

pub fn languages_json() -> String {
    let languages = BUNDLED_CATALOGS.iter().map(|(identifier, _)| {
        serde_json::json!({"identifier": identifier, "name": text(identifier, "language.name")})
    }).collect::<Vec<_>>();
    serde_json::to_string(&languages).unwrap()
}

pub fn text(language: &str, key: &str) -> String {
    match bundled(language) {
        Ok(locale) => locale.get(key).to_string(),
        Err(error) => {
            tracing::error!("Bundled English locale is invalid: {error}");
            key.to_string()
        }
    }
}

pub fn format(language: &str, key: &str, values: &[(&str, &str)]) -> String {
    substitute(&text(language, key), values)
}

fn substitute(template: &str, values: &[(&str, &str)]) -> String {
    let mut result = String::new();
    let mut rest = template;
    while let Some(start) = rest.find('{') {
        result.push_str(&rest[..start]);
        let Some(end) = rest[start..].find('}') else {
            result.push_str(&rest[start..]);
            return result;
        };
        let name = &rest[start + 1..start + end];
        if let Some((_, value)) = values.iter().find(|(key, _)| *key == name) {
            result.push_str(value);
        } else {
            result.push_str(&rest[start..=start + end]);
        }
        rest = &rest[start + end + 1..];
    }
    result.push_str(rest);
    result
}

pub fn page_json(language: &str) -> String {
    match bundled(language) {
        Ok(locale) => serde_json::to_string(&locale.strings).unwrap_or_else(|_| "{}".to_string()),
        Err(_) => "{}".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check_catalog(
        identifier: &str,
        contents: &str,
        english: &BTreeMap<String, String>,
    ) -> Result<(), String> {
        let file = format!("{identifier}.toml");
        let identifiers = regex::Regex::new(r"^[A-Za-z]{2,8}(?:-[A-Za-z0-9]{1,8})*$").unwrap();
        if !identifiers.is_match(identifier) {
            return Err(format!(
                "{file}: invalid language identifier {identifier:?}"
            ));
        }
        let catalog: BTreeMap<String, String> =
            toml::from_str(contents).map_err(|error| format!("{file}: {error}"))?;
        if catalog
            .get("language.name")
            .is_none_or(|name| name.trim().is_empty())
        {
            return Err(format!("{file}: missing or empty language.name"));
        }
        for key in catalog.keys() {
            if !english.contains_key(key) {
                return Err(format!("{file}: orphan key {key}"));
            }
        }
        let placeholders = regex::Regex::new(r"\{([A-Za-z_][A-Za-z_0-9]*)\}").unwrap();
        let names = |text: &str| {
            let mut names = placeholders
                .captures_iter(text)
                .map(|capture| capture[1].to_string())
                .collect::<Vec<_>>();
            names.sort();
            names
        };
        for (key, value) in english {
            let translated = catalog
                .get(key)
                .ok_or_else(|| format!("{file}: missing key {key}"))?;
            if names(value) != names(translated) {
                return Err(format!("{file}: placeholder mismatch for {key}"));
            }
        }
        Ok(())
    }

    #[test]
    fn bundled_catalogs_are_complete_with_matching_placeholders() {
        let english: BTreeMap<String, String> =
            toml::from_str(ENGLISH).expect("en.toml must contain a flat string table");
        let mut files = std::fs::read_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/locales"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.is_file()
                    && path
                        .extension()
                        .is_some_and(|extension| extension == "toml")
            })
            .map(|path| path.file_stem().unwrap().to_str().unwrap().to_string())
            .collect::<Vec<_>>();
        files.sort();
        assert_eq!(
            files,
            BUNDLED_CATALOGS
                .iter()
                .map(|(identifier, _)| identifier.to_string())
                .collect::<Vec<_>>(),
            "Every locales/*.toml file must be embedded"
        );
        let languages: serde_json::Value = serde_json::from_str(&languages_json()).unwrap();
        assert_eq!(languages.as_array().unwrap().len(), BUNDLED_CATALOGS.len());
        for (index, (identifier, contents)) in BUNDLED_CATALOGS.iter().enumerate() {
            check_catalog(identifier, contents, &english).unwrap_or_else(|error| panic!("{error}"));
            let catalog: BTreeMap<String, String> = toml::from_str(contents).unwrap();
            let resolved: BTreeMap<String, String> =
                serde_json::from_str(&page_json(identifier)).unwrap();
            assert_eq!(resolved, catalog, "{identifier}.toml: bundled lookup");
            assert_eq!(languages[index]["identifier"], *identifier);
            assert_eq!(
                languages[index]["name"], catalog["language.name"],
                "{identifier}.toml: language.name"
            );
        }
    }

    #[test]
    fn english_hotkey_names_match_the_action_catalog() {
        let english: BTreeMap<String, String> = toml::from_str(ENGLISH).unwrap();
        let names = english
            .into_iter()
            .filter(|(key, _)| key.starts_with("hotkeys.command."))
            .collect::<BTreeMap<_, _>>();
        let expected = leopardwm_ipc::hotkeys::hotkey_catalog()
            .into_iter()
            .map(|action| (format!("hotkeys.command.{}", action.id), action.label))
            .collect::<BTreeMap<_, _>>();
        for (key, label) in &expected {
            assert_eq!(names.get(key), Some(label), "en.toml: {key}");
        }
        for key in names.keys() {
            assert!(expected.contains_key(key), "en.toml: orphan key {key}");
        }
    }

    #[test]
    fn catalog_guard_names_defective_files_and_keys() {
        let english = BTreeMap::from([
            ("language.name".to_string(), "English".to_string()),
            ("message".to_string(), "{name} {name}".to_string()),
        ]);
        for (identifier, contents, diagnostic) in [
            ("fr", "not TOML", "expected"),
            ("fr", "\"language.name\" = \"Français\"", "message"),
            ("fr", "message = \"{name} {name}\"", "language.name"),
            ("fr", "\"language.name\" = \"\"", "language.name"),
            (
                "fr",
                "\"language.name\" = \"Français\"\nmessage = \"{name}\"",
                "message",
            ),
            (
                "fr",
                "\"language.name\" = \"Français\"\nmessage = \"{name} {name}\"\norphan = \"value\"",
                "orphan",
            ),
            ("bad_id", "", "identifier"),
        ] {
            let error = check_catalog(identifier, contents, &english).unwrap_err();
            assert!(error.contains(&format!("{identifier}.toml")), "{error}");
            assert!(error.contains(diagnostic), "{error}");
        }
    }

    #[test]
    fn loader_rejects_invalid_and_duplicate_keys() {
        for invalid in [
            "not TOML",
            "\"key\" = \"one\"\n\"key\" = \"two\"",
            "key = 3",
            "[nested]\nkey = \"value\"",
        ] {
            assert!(Locale::load(invalid, None).is_err());
            assert!(Locale::load("key = \"English\"", Some(invalid)).is_err());
        }
    }

    #[test]
    fn missing_translation_and_unsupported_language_use_english() {
        let locale = Locale::load(
            "one = \"English\"\ntwo = \"Fallback\"",
            Some("one = \"翻译\""),
        )
        .unwrap();
        assert_eq!(locale.get("one"), "翻译");
        assert_eq!(locale.get("two"), "Fallback");
        assert_eq!(page_json("unsupported"), page_json("en"));
    }

    #[test]
    fn substitution_is_single_pass_and_preserves_unknown_placeholders() {
        assert_eq!(
            substitute(
                "{name}: {count} {unknown}",
                &[("name", "{count}"), ("count", "2")]
            ),
            "{count}: 2 {unknown}"
        );
    }

    fn missing_keys(source: &str, english: &BTreeMap<String, String>) -> Vec<String> {
        let references = regex::Regex::new(
            r#"["']((?:settings|tray|notification|dialog)\.(?:[a-z_0-9-]+\.)*[a-z_0-9-]+)["']"#,
        )
        .unwrap();
        let attributes = regex::Regex::new(r#"data-i18n(?:-[a-z-]+)?="([^"]+)""#).unwrap();
        references
            .captures_iter(source)
            .map(|capture| capture[1].to_string())
            .chain(
                attributes
                    .captures_iter(source)
                    .filter(|capture| !capture[1].contains("' + "))
                    .map(|capture| capture[1].to_string()),
            )
            .filter(|key| !english.contains_key(key))
            .collect()
    }

    #[test]
    fn referenced_keys_exist_in_english() {
        let english: BTreeMap<String, String> = toml::from_str(ENGLISH).unwrap();
        for source in [
            include_str!("settings/html.rs"),
            include_str!("tray.rs"),
            include_str!("event_handler.rs"),
            include_str!("main.rs"),
            include_str!("settings/win32.rs"),
        ] {
            let missing = missing_keys(source, &english);
            assert!(missing.is_empty(), "Unknown locale keys: {missing:?}");
        }
    }

    #[test]
    fn attribute_key_guard_rejects_invalid_generated_markup_key() {
        let english: BTreeMap<String, String> = toml::from_str(ENGLISH).unwrap();
        let source = include_str!("settings/html.rs");
        let defective = source.replace(
            "data-i18n=\"settings.text.workspace_index\"",
            "data-i18n=\"settings.text.workspace_index!\"",
        );
        assert_ne!(source, defective);
        assert!(missing_keys(&defective, &english)
            .contains(&"settings.text.workspace_index!".to_string()));
    }
}
