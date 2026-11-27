//! Debugging aid for the settings menu (`VICTORY_MENU_TRACE=1`): each save
//! of the title's settings container logs which values changed and the
//! stack it was saved from, so a menu's Apply can be traced to its handler.
//!
//! The container holds a header and an indented JSON text, one value per
//! line, keyed by hashes. Only that layout is read here; anything else is
//! left alone.

use std::collections::BTreeMap;

/// The container the title keeps its settings in.
const CONTAINER: &str = "SettingsProfileSettings";
/// Changes logged per save, at most.
const MAX_CHANGES: usize = 64;
/// Stack frames logged per save, at most.
const MAX_FRAMES: usize = 24;

/// Whether saves to `container` are traced: tracing is on and it is the
/// settings container.
pub(crate) fn wanted(container: &str) -> bool {
    container == CONTAINER && victory_gpu::switch::on("VICTORY_MENU_TRACE")
}

/// Logs what a save of blob `new` (replacing `old`, if any) in `container`
/// changes, if [`wanted`].
pub(crate) fn on_save(container: &str, old: Option<&[u8]>, new: &[u8]) {
    if !wanted(container) {
        return;
    }
    let text = |b: &[u8]| String::from_utf8_lossy(b).into_owned();
    let before = old.map(text).map_or_else(Vec::new, |t| flatten(&t));
    let after = flatten(&text(new));
    let changes = changed(&before, &after);
    tracing::info!(
        target: "menu_trace",
        values = after.len(),
        changed = changes.len(),
        first = old.is_none(),
        "settings saved"
    );
    for (path, was, now) in changes.iter().take(MAX_CHANGES) {
        tracing::info!(target: "menu_trace", %path, ?was, ?now, "setting changed");
    }
    let stack = victory_loader::stack_here(MAX_FRAMES);
    tracing::info!(target: "menu_trace", stack = %stack.join(" < "), "settings saved from");
}

/// The values of an indented JSON text as (`a/b/key`, value) pairs, in
/// order. Empty unless the text, from its first `{`, opens and closes one
/// object with one key or brace per line.
pub(crate) fn flatten(text: &str) -> Vec<(String, String)> {
    let Some(start) = text.find('{') else {
        return Vec::new();
    };
    let mut lines = text[start..].lines().map(str::trim);
    if lines.next() != Some("{") {
        return Vec::new();
    }
    let (mut path, mut out) = (Vec::<String>::new(), Vec::new());
    for line in lines {
        let line = line.trim_end_matches(',');
        if line.is_empty() {
            continue;
        }
        if line == "}" {
            if path.pop().is_none() {
                return out; // the outer object closed
            }
            continue;
        }
        let Some((key, value)) = key_value(line) else {
            return Vec::new();
        };
        if value == "{" {
            path.push(key.to_owned());
        } else {
            let mut full = path.join("/");
            if !full.is_empty() {
                full.push('/');
            }
            full.push_str(key);
            out.push((full, value.to_owned()));
        }
    }
    Vec::new() // the outer object never closed
}

/// `"key": value` split, or `None`.
fn key_value(line: &str) -> Option<(&str, &str)> {
    let rest = line.strip_prefix('"')?;
    let (key, rest) = rest.split_once('"')?;
    let value = rest.trim_start().strip_prefix(':')?.trim();
    (!value.is_empty()).then_some((key, value))
}

/// The paths whose value differs between `old` and `new`: (path, old value,
/// new value), by path.
pub(crate) fn changed(
    old: &[(String, String)],
    new: &[(String, String)],
) -> Vec<(String, Option<String>, Option<String>)> {
    let old: BTreeMap<&str, &str> = old.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let new: BTreeMap<&str, &str> = new.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let mut paths: Vec<&str> = old.keys().chain(new.keys()).copied().collect();
    paths.sort_unstable();
    paths.dedup();
    paths
        .into_iter()
        .filter(|p| old.get(p) != new.get(p))
        .map(|p| {
            let get = |m: &BTreeMap<&str, &str>| m.get(p).map(|v| (*v).to_owned());
            (p.to_owned(), get(&old), get(&new))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pairs(list: &[(&str, &str)]) -> Vec<(String, String)> {
        list.iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    #[test]
    fn nested_keys_flatten_to_paths() {
        let text = "X\n{\n\t\"$0xa\": {\n\t\t\"$0xb\": 6,\n\t\t\"$0xc\": true\n\t}\n}";
        assert_eq!(
            flatten(text),
            pairs(&[("$0xa/$0xb", "6"), ("$0xa/$0xc", "true")])
        );
    }

    #[test]
    fn only_changed_values_are_reported() {
        let old = pairs(&[("$0xa/$0xb", "6"), ("$0xa/$0xc", "true")]);
        let new = pairs(&[
            ("$0xa/$0xb", "7"),
            ("$0xa/$0xc", "true"),
            ("$0xa/$0xd", "1"),
        ]);
        assert_eq!(
            changed(&old, &new),
            [
                (
                    "$0xa/$0xb".to_owned(),
                    Some("6".to_owned()),
                    Some("7".to_owned())
                ),
                ("$0xa/$0xd".to_owned(), None, Some("1".to_owned())),
            ]
        );
    }

    #[test]
    fn garbage_flattens_to_nothing() {
        assert!(flatten("\u{0}\u{1}no braces").is_empty());
        assert!(flatten("{ \"a\": {").is_empty());
        assert!(flatten("{\n\t\"a\": {\n\t\t\"b\": 1\n").is_empty()); // cut short
        assert!(flatten("{\n\t[1, 2]\n}").is_empty());
    }

    #[test]
    fn other_containers_are_not_read() {
        // Returns before reading anything: no panic on bytes that are not text.
        on_save("Player", None, &[0xff, 0xfe, 0x00]);
    }
}
