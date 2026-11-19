//! Reading and writing individual config keys.
//!
//! `toml_edit` rather than a serde round-trip, because the starter config is
//! mostly comments explaining the choices — rewriting the file through a
//! struct would throw all of that away on the first `config set`.
//!
//! These operate on the *file*. Effective values, including everything the
//! file doesn't mention, come from `inkling config` instead.

use crate::config::{Config, RETIRED};
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
    #[error("{key} is retired and has no effect; `inkling config set {key} \"\"` removes it")]
    Retired { key: String },
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

/// Whether `key` is, or is inside, one `Config` ignores on load. Setting one
/// would write a knob nothing turns.
pub fn retired(key: &str) -> bool {
    RETIRED.iter().any(|r| {
        key.strip_prefix(r)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with('.'))
    })
}

/// The retired keys this file still holds.
pub fn retired_present(document: &DocumentMut) -> Vec<&'static str> {
    RETIRED
        .iter()
        .copied()
        .filter(|key| lookup(document, key).is_some())
        .collect()
}

fn lookup<'a>(document: &'a DocumentMut, key: &str) -> Option<&'a Item> {
    key.split('.')
        .try_fold(document.as_item(), |item, segment| item.get(segment))
}

/// Read one key, as written in the file.
pub fn get(document: &DocumentMut, key: &str) -> Result<String, EditError> {
    let item = lookup(document, key).ok_or_else(|| EditError::NoSuchKey(key.to_string()))?;
    if retired(key) {
        return Err(EditError::Retired {
            key: key.to_string(),
        });
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

    // Removing one is still allowed: that's how a dead key leaves the file.
    if value.is_empty() {
        remove(document, key);
        return Ok(());
    }
    if retired(key) {
        return Err(EditError::Retired {
            key: key.to_string(),
        });
    }

    let mut item: &mut Item = document.as_item_mut();
    for segment in parents {
        // Creating missing tables is the point: `config set slack.mcp.tools.
        // read_thread x` shouldn't require the section to exist first.
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

/// Remove a key, and any table that leaves empty. Returns whether it was
/// there; when it wasn't, the document is untouched.
pub fn remove(document: &mut DocumentMut, key: &str) -> bool {
    let leading = leading_comment(&document.to_string());
    let segments: Vec<&str> = key.split('.').collect();
    let removed = remove_in(document.as_table_mut(), &segments);

    // A file's opening comment is decor on whatever came first; when that
    // goes, the comment would go with it.
    let text = document.to_string();
    if removed && !text.starts_with(&leading) {
        *document = format!("{leading}{text}")
            .parse()
            .expect("comments before valid TOML are valid TOML");
    }
    removed
}

/// Walks with `TableLike::get_mut`: `Item::get_mut` inserts what it doesn't
/// find.
fn remove_in(table: &mut dyn toml_edit::TableLike, path: &[&str]) -> bool {
    let (first, rest) = path.split_first().expect("never empty");
    if rest.is_empty() {
        return table.remove(first).is_some();
    }
    let Some(inner) = table.get_mut(first).and_then(Item::as_table_like_mut) else {
        return false;
    };
    let removed = remove_in(inner, rest);
    if removed && inner.is_empty() {
        table.remove(first);
    }
    removed
}

/// The comment lines a file opens with, or nothing if it opens otherwise.
fn leading_comment(text: &str) -> String {
    let lines: String = text
        .split_inclusive('\n')
        .take_while(|line| line.trim().is_empty() || line.trim_start().starts_with('#'))
        .collect();
    match lines.contains('#') {
        true => lines,
        false => String::new(),
    }
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
    // A comma or brackets means a list — the shape `github.ignore_checks`
    // wants.
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
/// the tool out of service should fail at the edit, not later in a hook.
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
[github]
# Where the token comes from.
token_command = "gh auth token"
ignore_checks = ["merge-lock"]

[serve]
idle_after = "5m"
"##;

    fn doc() -> DocumentMut {
        SAMPLE.parse().unwrap()
    }

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("inkling-edit-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn reads_scalars_and_lists() {
        let d = doc();
        assert_eq!(get(&d, "github.token_command").unwrap(), "gh auth token");
        assert_eq!(get(&d, "github.ignore_checks").unwrap(), "merge-lock");
    }

    #[test]
    fn an_unknown_key_says_so() {
        assert!(matches!(
            get(&doc(), "github.nope"),
            Err(EditError::NoSuchKey(_))
        ));
    }

    #[test]
    fn a_section_is_not_a_value() {
        assert!(matches!(
            get(&doc(), "github"),
            Err(EditError::NotAValue { .. })
        ));
    }

    #[test]
    fn setting_preserves_comments() {
        // The starter config is mostly comments; losing them on the first
        // edit would be worse than not having the command.
        let mut d = doc();
        set(&mut d, "github.token_command", "op read x").unwrap();
        let out = d.to_string();
        assert!(out.contains("# Inkling config"));
        assert!(out.contains("# Where the token comes from."));
        assert!(out.contains(r#"token_command = "op read x""#), "{out}");
    }

    #[test]
    fn values_take_the_type_they_look_like() {
        let mut d = doc();
        set(&mut d, "flag", "false").unwrap();
        set(&mut d, "count", "500").unwrap();
        set(&mut d, "serve.interval", "20s").unwrap();

        let out = d.to_string();
        assert!(out.contains("flag = false"), "{out}");
        assert!(out.contains("count = 500"), "{out}");
        // Durations must stay strings or they won't deserialize.
        assert!(out.contains(r#"interval = "20s""#), "{out}");
    }

    #[test]
    fn a_comma_separated_value_becomes_a_list() {
        let mut d = doc();
        set(&mut d, "github.ignore_checks", "merge-lock, labels").unwrap();
        assert_eq!(
            get(&d, "github.ignore_checks").unwrap(),
            "merge-lock, labels"
        );

        set(&mut d, "github.ignore_checks", r#"["a", "b"]"#).unwrap();
        assert_eq!(get(&d, "github.ignore_checks").unwrap(), "a, b");
    }

    #[test]
    fn setting_an_empty_value_removes_the_key() {
        let mut d = doc();
        set(&mut d, "github.ignore_checks", "").unwrap();
        assert!(matches!(
            get(&d, "github.ignore_checks"),
            Err(EditError::NoSuchKey(_))
        ));
        // The section and its neighbours survive.
        assert_eq!(get(&d, "github.token_command").unwrap(), "gh auth token");
    }

    #[test]
    fn removing_something_absent_is_not_an_error() {
        // Nor a change: walking to it must not create the tables on the way.
        let mut d = doc();
        assert!(!remove(&mut d, "github.never_set"));
        assert!(!remove(&mut d, "nothing.here.at.all"));
        assert!(!remove(&mut d, "serve.idle_after.deeper"));
        assert_eq!(d.to_string(), SAMPLE);

        // The cleanup the 0.4.0 changelog tells people to run.
        let raw = "# lead\n[slack]\nuser_id = \"U1\"\n";
        let mut d: DocumentMut = raw.parse().unwrap();
        assert!(!remove(&mut d, "slack.mcp.tools.search"));
        assert_eq!(d.to_string(), raw);
    }

    #[test]
    fn removing_the_last_key_drops_its_table_but_not_the_files_leading_comment() {
        let mut d = doc();
        assert!(remove(&mut d, "serve.idle_after"));
        assert!(!d.to_string().contains("[serve]"), "{d}");

        let mut d = doc();
        assert!(remove(&mut d, "github.token_command"));
        assert!(remove(&mut d, "github.ignore_checks"));
        let out = d.to_string();
        assert!(!out.contains("[github]"), "{out}");
        assert!(out.starts_with("# Inkling config\n"), "{out}");
        assert_eq!(get(&d, "serve.idle_after").unwrap(), "5m");

        // Up through every table the removal emptied.
        let mut d: DocumentMut = "[slack]\nuser_id = \"U1\"\n[slack.mcp.tools]\nsearch = \"x\"\n"
            .parse()
            .unwrap();
        assert!(remove(&mut d, "slack.mcp.tools.search"));
        assert_eq!(d.to_string(), "[slack]\nuser_id = \"U1\"\n");
    }

    #[test]
    fn missing_sections_are_created() {
        let mut d = doc();
        set(&mut d, "slack.mcp.tools.read_thread", "read").unwrap();
        assert_eq!(get(&d, "slack.mcp.tools.read_thread").unwrap(), "read");
    }

    #[test]
    fn an_edit_that_breaks_the_config_is_refused() {
        // A second host — caught by the config's own validation rather than
        // duplicated here.
        let mut d = doc();
        set(&mut d, "github.hosts", "github.com, ghe.example.com").unwrap();

        let dir = scratch("invalid");
        let path = dir.join("config.toml");

        let error = save(&path, &d).unwrap_err();
        assert!(matches!(error, EditError::WouldBeInvalid { .. }), "{error}");
        assert!(!path.exists(), "nothing should have been written");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_retired_key_is_refused_but_can_be_removed() {
        let mut d: DocumentMut = "[window]\noverlap = \"5m\"\n[slack]\nchannels = [\"#eng\"]\n"
            .parse()
            .unwrap();
        for key in [
            "window.overlap",
            "tracking.max_tracking_age",
            "filters.only_my_messages",
            "status.merged.canonical",
            "run.dry_run",
            "slack.personal",
            "slack.channels",
            "slack.mcp.tools.search",
        ] {
            let error = set(&mut d, key, "1").unwrap_err();
            assert!(matches!(error, EditError::Retired { .. }), "{key}: {error}");
            assert!(error.to_string().contains(key), "{error}");
        }
        // A name that only starts like a retired one is not retired.
        let mut other = DocumentMut::new();
        set(&mut other, "slack.mcp.tools.search_channels", "find").unwrap();
        set(&mut other, "statusbar.x", "1").unwrap();

        // A config already holding them still saves, and removing them works.
        set(&mut d, "serve.interval", "30s").unwrap();
        set(&mut d, "window.overlap", "").unwrap();
        assert!(matches!(
            get(&d, "window.overlap"),
            Err(EditError::NoSuchKey(_))
        ));
        assert!(matches!(
            get(&d, "slack.channels"),
            Err(EditError::Retired { .. })
        ));
        let dir = scratch("retired");
        let path = dir.join("config.toml");
        save(&path, &d).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn retired_keys_in_the_file_are_named() {
        let d: DocumentMut = "[status.merged]\nx = 1\n[slack]\nuser_id = \"U1\"\nchannels = []\n"
            .parse()
            .unwrap();
        assert_eq!(retired_present(&d), ["status", "slack.channels"]);
        assert!(retired_present(&doc()).is_empty());
    }
}
