//! Reading and writing individual config keys.
//!
//! `toml_edit` rather than a serde round-trip, because the starter config is
//! mostly comments explaining the choices — rewriting the file through a
//! struct would throw all of that away on the first `config set`.
//!
//! These operate on the *file*. Effective values, including everything the
//! file doesn't mention, come from `inkling config` instead.

use crate::config::Config;
use std::path::Path;
use toml_edit::{Array, DocumentMut, Item, Value};

#[derive(Debug, thiserror::Error)]
pub enum EditError {
    #[error("reading {path}: {source}")]
    Read {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("{path} is not valid TOML: {source}")]
    Parse {
        path: String,
        #[source]
        source: toml_edit::TomlError,
    },
    #[error("writing {path}: {source}")]
    Write {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("no such key: {0}")]
    NoSuchKey(String),
    #[error("{key} is a section, not a value")]
    NotAValue { key: String },
    /// The edit was applied to a copy, found invalid, and discarded.
    #[error("that change would make the config invalid: {reason}")]
    WouldBeInvalid { reason: String },
}

pub fn load(path: &Path) -> Result<DocumentMut, EditError> {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        // Setting a key in a config that doesn't exist yet should just work.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(source) => {
            return Err(EditError::Read {
                path: path.display().to_string(),
                source,
            });
        }
    };
    raw.parse().map_err(|source| EditError::Parse {
        path: path.display().to_string(),
        source,
    })
}

/// Read one key, as written in the file.
pub fn get(document: &DocumentMut, key: &str) -> Result<String, EditError> {
    let mut item: &Item = document.as_item();
    for segment in key.split('.') {
        item = item
            .get(segment)
            .ok_or_else(|| EditError::NoSuchKey(key.to_string()))?;
    }
    match item {
        Item::Value(value) => Ok(render(value)),
        // A whole section has no single value to print.
        _ => Err(EditError::NotAValue {
            key: key.to_string(),
        }),
    }
}

/// Set a key, or remove it when `value` is empty.
///
/// Returns the TOML to write. The caller validates before committing it, so a
/// bad edit can't leave an unusable config behind.
pub fn set(document: &mut DocumentMut, key: &str, value: &str) -> Result<(), EditError> {
    let segments: Vec<&str> = key.split('.').collect();
    let (last, parents) = segments.split_last().expect("split never yields empty");

    if value.is_empty() {
        return remove(document, &segments);
    }

    let mut item: &mut Item = document.as_item_mut();
    for segment in parents {
        // Creating missing tables is the point: `config set status.merged.
        // canonical x` shouldn't require the section to exist first.
        let table = item
            .as_table_like_mut()
            .ok_or_else(|| EditError::NotAValue {
                key: (*segment).to_string(),
            })?;
        if table.get(segment).is_none() {
            table.insert(segment, toml_edit::table());
        }
        item = table
            .get_mut(segment)
            .expect("just inserted if it was missing");
    }

    let table = item
        .as_table_like_mut()
        .ok_or_else(|| EditError::NotAValue {
            key: key.to_string(),
        })?;

    // Replacing the Item drops the surrounding whitespace and comments with
    // it. Mutating the value in place, carrying its decor over, keeps the
    // explanation sitting above the line someone just changed.
    if let Some(Item::Value(existing)) = table.get_mut(last) {
        let mut updated = parse_value(value);
        *updated.decor_mut() = existing.decor().clone();
        *existing = updated;
    } else {
        table.insert(last, Item::Value(parse_value(value)));
    }
    Ok(())
}

fn remove(document: &mut DocumentMut, segments: &[&str]) -> Result<(), EditError> {
    let (last, parents) = segments.split_last().expect("never empty");
    let mut item: &mut Item = document.as_item_mut();
    for segment in parents {
        item = match item.get_mut(segment) {
            Some(next) => next,
            // Removing something already absent is the desired end state.
            None => return Ok(()),
        };
    }
    if let Some(table) = item.as_table_like_mut() {
        table.remove(last);
    }
    Ok(())
}

/// Guess the TOML type from how the value is written, which is what someone
/// typing at a shell expects. Durations stay strings ("20m"), so anything
/// non-numeric and non-boolean lands correctly by default.
fn parse_value(raw: &str) -> Value {
    let trimmed = raw.trim();
    if let Ok(b) = trimmed.parse::<bool>() {
        return Value::from(b);
    }
    if let Ok(n) = trimmed.parse::<i64>() {
        return Value::from(n);
    }
    // A comma or brackets means a list — the shape `channels` and `aliases`
    // both want, and the only ones a user is likely to set.
    let bracketed = trimmed.starts_with('[') && trimmed.ends_with(']');
    if bracketed || trimmed.contains(',') {
        let inner = if bracketed {
            &trimmed[1..trimmed.len() - 1]
        } else {
            trimmed
        };
        let mut array = Array::new();
        for element in inner.split(',') {
            let element = element.trim().trim_matches(['"', '\'']);
            if !element.is_empty() {
                array.push(element);
            }
        }
        return Value::Array(array);
    }
    Value::from(trimmed.trim_matches(['"', '\'']))
}

fn render(value: &Value) -> String {
    match value {
        Value::String(s) => s.value().clone(),
        Value::Array(array) => array.iter().map(render).collect::<Vec<_>>().join(", "),
        other => other.to_string().trim().to_string(),
    }
}

/// Write only if the result still parses as a config. An edit that would take
/// the tool out of service should fail at the edit, not at 3am from cron.
pub fn save(path: &Path, document: &DocumentMut) -> Result<(), EditError> {
    let text = document.to_string();
    Config::parse(&text, path).map_err(|e| EditError::WouldBeInvalid {
        reason: e.to_string(),
    })?;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|source| EditError::Write {
            path: path.display().to_string(),
            source,
        })?;
    }
    std::fs::write(path, text).map_err(|source| EditError::Write {
        path: path.display().to_string(),
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r##"# Inkling config
[slack]
# Find your own posts.
personal = true
channels = ["#eng"]

[status.merged]
canonical = "purple_heart"
aliases = ["merged", "shipit"]
"##;

    fn doc() -> DocumentMut {
        SAMPLE.parse().unwrap()
    }

    #[test]
    fn reads_scalars_and_lists() {
        let d = doc();
        assert_eq!(get(&d, "slack.personal").unwrap(), "true");
        assert_eq!(get(&d, "status.merged.canonical").unwrap(), "purple_heart");
        assert_eq!(get(&d, "status.merged.aliases").unwrap(), "merged, shipit");
    }

    #[test]
    fn an_unknown_key_says_so() {
        assert!(matches!(
            get(&doc(), "slack.nope"),
            Err(EditError::NoSuchKey(_))
        ));
    }

    #[test]
    fn a_section_is_not_a_value() {
        assert!(matches!(
            get(&doc(), "slack"),
            Err(EditError::NotAValue { .. })
        ));
    }

    #[test]
    fn setting_preserves_comments() {
        // The starter config is mostly comments; losing them on the first
        // edit would be worse than not having the command.
        let mut d = doc();
        set(&mut d, "slack.personal", "false").unwrap();
        let out = d.to_string();
        assert!(out.contains("# Inkling config"));
        assert!(out.contains("# Find your own posts."));
        assert!(out.contains("personal = false"));
    }

    #[test]
    fn values_take_the_type_they_look_like() {
        let mut d = doc();
        set(&mut d, "slack.personal", "false").unwrap();
        set(&mut d, "filters.max_message_chars", "500").unwrap();
        set(&mut d, "window.overlap", "20m").unwrap();

        let out = d.to_string();
        assert!(out.contains("personal = false"), "{out}");
        assert!(out.contains("max_message_chars = 500"), "{out}");
        // Durations must stay strings or they won't deserialize.
        assert!(out.contains(r#"overlap = "20m""#), "{out}");
    }

    #[test]
    fn a_comma_separated_value_becomes_a_list() {
        let mut d = doc();
        set(&mut d, "slack.channels", "#eng, #design").unwrap();
        assert_eq!(get(&d, "slack.channels").unwrap(), "#eng, #design");

        set(&mut d, "status.merged.aliases", r#"["a", "b"]"#).unwrap();
        assert_eq!(get(&d, "status.merged.aliases").unwrap(), "a, b");
    }

    #[test]
    fn setting_an_empty_value_removes_the_key() {
        let mut d = doc();
        set(&mut d, "slack.personal", "").unwrap();
        assert!(matches!(
            get(&d, "slack.personal"),
            Err(EditError::NoSuchKey(_))
        ));
        // The section and its neighbours survive.
        assert_eq!(get(&d, "slack.channels").unwrap(), "#eng");
    }

    #[test]
    fn removing_something_absent_is_not_an_error() {
        let mut d = doc();
        assert!(set(&mut d, "slack.never_set", "").is_ok());
        assert!(set(&mut d, "nothing.here.at.all", "").is_ok());
    }

    #[test]
    fn missing_sections_are_created() {
        let mut d = doc();
        set(&mut d, "status.approved.canonical", "eyes").unwrap();
        assert_eq!(get(&d, "status.approved.canonical").unwrap(), "eyes");
    }

    #[test]
    fn an_edit_that_breaks_the_config_is_refused() {
        // Same emoji for two states — caught by the config's own validation
        // rather than duplicated here.
        let mut d = doc();
        set(&mut d, "status.approved.canonical", "shipit").unwrap();

        let dir = std::env::temp_dir().join(format!("inkling-edit-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");

        let error = save(&path, &d).unwrap_err();
        assert!(matches!(error, EditError::WouldBeInvalid { .. }), "{error}");
        assert!(!path.exists(), "nothing should have been written");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
