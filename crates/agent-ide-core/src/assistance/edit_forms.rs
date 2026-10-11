//! The address-shaped `ide.edit` forms of `symbol-read-edit-v1` section 3.1, rewritten into the
//! one-file `changes` form before validation so a single engine applies them.
//!
//! Rewritten here (whole-file `content`, `files`, `patch` and `rename` are not touched):
//!
//! | request | becomes |
//! |---|---|
//! | `{symbol, old, new}` (E3) | `{path: <symbol's file>, changes: [{old, new, within: symbol}]}` |
//! | `{path, old, new}` and the `file_path`/`old_string`/`new_string` aliases (E4) | `{path, changes: [{old, new}]}` |
//! | `{path, add_import}` / `{path, remove_import}` (E8) | `{path, changes: [{add_import}]}` / `{... remove_import}` |
//! | `{symbol, content \| op}` (E1, E2, E5, E6, windows and regions included) | `{path: <symbol's file>, changes: [{symbol, content \| op, where}]}` |
//! | `{path, lines, content}` | `{path, changes: [{lines, content}]}` |
//!
//! A `changes` entry that carries both `symbol` and `old` is the E3 entry: its `symbol` becomes
//! `within`. A request that mixes a rewritten form with a field of another form keeps that field,
//! so the `changes` validation refuses the mix exactly as it refuses any other. One engine
//! therefore applies every form and every success reply lists the changed lines by address.

use serde_json::{Map, Value, json};

/// Top-level aliases of the Claude-native edit shape and the field each stands for.
const ALIASES: [(&str, &str); 3] = [
    ("file_path", "path"),
    ("old_string", "old"),
    ("new_string", "new"),
];

/// Rewrites the address-shaped single-edit forms of `parameters` into `{path, changes}` and the
/// `symbol`+`old` entries of an existing `changes` array into `within` entries.
///
/// `parameters` is the object of an `ide.edit` call that has not been validated yet; a value that
/// is not an object, or a request none of the forms apply to, is left unchanged.
pub(super) fn normalize(parameters: &mut Value) {
    let Some(object) = parameters.as_object_mut() else {
        return;
    };
    for (alias, field) in ALIASES {
        if !object.contains_key(field)
            && let Some(value) = object.remove(alias)
        {
            object.insert(field.to_owned(), value);
        }
    }
    normalize_entries(object);
    if object.contains_key("changes")
        || object.contains_key("files")
        || object.contains_key("patch")
    {
        return;
    }
    if object.contains_key("old") || object.contains_key("new") {
        wrap_old_text(object);
    } else if object.contains_key("add_import") || object.contains_key("remove_import") {
        wrap_import(object);
    } else if object.contains_key("symbol")
        && !object.contains_key("lines")
        && matches!(
            object.get("op").and_then(Value::as_str),
            None | Some("replace" | "insert" | "delete")
        )
    {
        wrap_symbol(object);
    } else if object.contains_key("lines")
        && !object.contains_key("symbol")
        && matches!(
            object.get("op").and_then(Value::as_str),
            None | Some("replace")
        )
    {
        wrap_lines(object);
    }
}

/// `{path, lines, content}` becomes one `{lines, content}` entry.
fn wrap_lines(object: &mut Map<String, Value>) {
    let mut entry = Map::new();
    for field in ["lines", "content"] {
        if let Some(value) = object.remove(field) {
            entry.insert(field.to_owned(), value);
        }
    }
    object.remove("op");
    object.insert("changes".to_owned(), json!([Value::Object(entry)]));
}

/// Turns each `{symbol, old, new}` entry of `changes` into `{old, new, within: symbol}`.
fn normalize_entries(object: &mut Map<String, Value>) {
    let Some(entries) = object.get_mut("changes").and_then(Value::as_array_mut) else {
        return;
    };
    for entry in entries {
        let Some(entry) = entry.as_object_mut() else {
            continue;
        };
        if entry.contains_key("old")
            && !entry.contains_key("within")
            && let Some(symbol) = entry.remove("symbol")
        {
            entry.insert("within".to_owned(), symbol);
        }
    }
}

/// The file part of `symbol` (`src/a.rs` of `src/a.rs#Foo`), when it has one.
fn file_of(symbol: &str) -> Option<String> {
    if let Some((file, _)) = symbol.split_once('#') {
        return Some(file.to_owned()).filter(|file| !file.is_empty());
    }
    // A relative `path:line` location (what grep prints) names its file too; it addresses the
    // unit or region holding the line, resolved against the file's map by the worker.
    match crate::lang::address::Address::parse(symbol) {
        Ok(crate::lang::address::Parsed::Location { file, .. }) if !file.is_absolute() => {
            Some(file.display().to_string())
        }
        _ => None,
    }
}

/// E3/E4: `old`/`new` (and `symbol` as the scope) become one `changes` entry.
fn wrap_old_text(object: &mut Map<String, Value>) {
    let mut entry = Map::new();
    for field in ["old", "new"] {
        if let Some(value) = object.remove(field) {
            entry.insert(field.to_owned(), value);
        }
    }
    if let Some(Value::String(symbol)) = object.get("symbol").cloned()
        && let Some(file) = file_of(&symbol)
    {
        object.remove("symbol");
        entry.insert("within".to_owned(), Value::String(symbol));
        object.entry("path").or_insert(Value::String(file));
    }
    object.insert("changes".to_owned(), json!([Value::Object(entry)]));
}

/// E8: `add_import`/`remove_import` become one `changes` entry.
fn wrap_import(object: &mut Map<String, Value>) {
    let mut entry = Map::new();
    for field in ["add_import", "remove_import"] {
        if let Some(value) = object.remove(field) {
            entry.insert(field.to_owned(), value);
        }
    }
    object.insert("changes".to_owned(), json!([Value::Object(entry)]));
}

/// E1/E2/E5/E6 on a window or region address become one `changes` entry carrying `symbol`,
/// `op`, `where` and `content`.
fn wrap_symbol(object: &mut Map<String, Value>) {
    let symbol = object.get("symbol").cloned().unwrap_or(Value::Null);
    let Some(file) = symbol.as_str().and_then(file_of) else {
        return;
    };
    // A `path` naming another file is the two-targets mix; the legacy validation refuses it.
    if object
        .get("path")
        .is_some_and(|path| path.as_str() != Some(file.as_str()))
    {
        return;
    }
    let mut entry = Map::new();
    for field in ["symbol", "op", "where", "content"] {
        if let Some(value) = object.remove(field) {
            entry.insert(field.to_owned(), value);
        }
    }
    object.entry("path").or_insert(Value::String(file));
    object.insert("changes".to_owned(), json!([Value::Object(entry)]));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The normalized form of `request`.
    fn normalized(mut request: Value) -> Value {
        normalize(&mut request);
        request
    }

    /// E3 and E4 (with the Claude-native aliases) become one `old` entry; E3 scopes it.
    #[test]
    fn old_text_forms_become_one_entry() {
        assert_eq!(
            normalized(json!({"symbol":"src/a.rs#Foo/bar","old":"x","new":"y"})),
            json!({"path":"src/a.rs","changes":[{"old":"x","new":"y","within":"src/a.rs#Foo/bar"}]})
        );
        assert_eq!(
            normalized(json!({"file_path":"src/a.rs","old_string":"x","new_string":"y"})),
            json!({"path":"src/a.rs","changes":[{"old":"x","new":"y"}]})
        );
        assert_eq!(
            normalized(json!({"path":"a.rs","changes":[{"symbol":"a.rs#F","old":"x","new":"y"}]})),
            json!({"path":"a.rs","changes":[{"within":"a.rs#F","old":"x","new":"y"}]})
        );
    }

    /// E8 becomes one import entry.
    #[test]
    fn imports_become_one_entry() {
        assert_eq!(
            normalized(json!({"path":"a.rs","add_import":"use std::fmt;"})),
            json!({"path":"a.rs","changes":[{"add_import":"use std::fmt;"}]})
        );
        assert_eq!(
            normalized(json!({"path":"a.py","remove_import":"import os"})),
            json!({"path":"a.py","changes":[{"remove_import":"import os"}]})
        );
    }

    /// Symbol and line forms are rewritten; a rename and whole-file content are not.
    #[test]
    fn symbol_and_line_forms_are_rewritten() {
        assert_eq!(
            normalized(json!({"symbol":"a.rs#F","op":"delete"})),
            json!({"path":"a.rs","changes":[{"symbol":"a.rs#F","op":"delete"}]})
        );
        // A relative `path:line` location names its file and stays the entry's address.
        assert_eq!(
            normalized(json!({"symbol":"src/a.rs:9","content":"z"})),
            json!({"path":"src/a.rs","changes":[{"symbol":"src/a.rs:9","content":"z"}]})
        );
        assert_eq!(
            normalized(json!({"path":"a.rs","lines":"1-2","source_ref":"s","content":"z"})),
            json!({"path":"a.rs","source_ref":"s","changes":[{"lines":"1-2","content":"z"}]})
        );
        assert_eq!(
            normalized(json!({"symbol":"a.rs#F:2-3","content":"z"})),
            json!({"path":"a.rs","changes":[{"symbol":"a.rs#F:2-3","content":"z"}]})
        );
        assert_eq!(
            normalized(
                json!({"symbol":"a.rs#@imports","op":"insert","where":"after","content":"z"})
            ),
            json!({"path":"a.rs","changes":[{"symbol":"a.rs#@imports","op":"insert","where":"after","content":"z"}]})
        );
        for untouched in [
            json!({"symbol":"a.rs#F:2-3","op":"rename","new_name":"g"}),
            json!({"path":"a.rs","content":"z"}),
            json!({"symbol":"a.rs#F","path":"b.rs","lines":"1-2","content":"z"}),
        ] {
            assert_eq!(normalized(untouched.clone()), untouched);
        }
    }
}
