//! Writing a setting back to `victory.toml` without disturbing the rest:
//! comments, order and other values stay exactly as the user wrote them.

use std::path::Path;

use anyhow::{Context, Result};

use super::choice::{Choice, ConfigKey};

/// `doc` with `[section] key = 'value'` set. An existing line for the key
/// is rewritten in place (its indent and trailing comment kept); otherwise
/// the line goes right under the section header, or a new section is
/// appended. `value` must be a plain name (see [`super::validate`]).
pub fn with_value(doc: &str, at: ConfigKey, value: &str) -> String {
    let newline = if doc.contains("\r\n") { "\r\n" } else { "\n" };
    let mut lines: Vec<String> = doc.lines().map(str::to_owned).collect();
    let assignment = format!("{} = '{value}'", at.key);

    let Some(header) = lines.iter().position(|l| section_of(l) == Some(at.section)) else {
        if lines.last().is_some_and(|l| !l.trim().is_empty()) {
            lines.push(String::new());
        }
        lines.push(format!("[{}]", at.section));
        lines.push(assignment);
        return join(&lines, newline);
    };
    let end = lines[header + 1..]
        .iter()
        .position(|l| section_of(l).is_some())
        .map_or(lines.len(), |i| header + 1 + i);
    match (header + 1..end).find(|&i| key_of(&lines[i]) == Some(at.key)) {
        Some(i) => {
            let line = &lines[i];
            let indent = &line[..line.len() - line.trim_start().len()];
            let comment = trailing_comment(line).map_or(String::new(), |c| format!(" {c}"));
            lines[i] = format!("{indent}{assignment}{comment}");
        }
        None => lines.insert(header + 1, assignment),
    }
    join(&lines, newline)
}

/// Stores `value` in the config file at `path` (created if missing),
/// replacing the file in one rename so a crash leaves the old or the new
/// file, never half of one.
pub fn save<C: Choice>(path: &Path, value: C) -> Result<()> {
    let name = value
        .toml_name()
        .with_context(|| format!("{value:?} is not among the options of `{}`", C::CONFIG.key))?;
    let old = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let new = with_value(&old, C::CONFIG, name);
    check_edit(&old, &new, C::CONFIG, name)
        .with_context(|| format!("not changing {}: the edit would not be clean", path.display()))?;
    let temp = path.with_extension(format!("toml.{}.tmp", std::process::id()));
    std::fs::write(&temp, new).with_context(|| format!("writing {}", temp.display()))?;
    std::fs::rename(&temp, path)
        .inspect_err(|_| {
            let _ = std::fs::remove_file(&temp);
        })
        .with_context(|| format!("replacing {}", path.display()))
}

/// The line edit is only trusted if, read as TOML, `new` is `old` with just
/// `[section] key` set to `value`. Inline tables, dotted or quoted keys and
/// the like are refused rather than mangled.
fn check_edit(old: &str, new: &str, at: ConfigKey, value: &str) -> Result<()> {
    let mut old: toml::Table = toml::from_str(old).context("the config file is not valid TOML")?;
    let mut new: toml::Table = toml::from_str(new).context("the edited file is not valid TOML")?;
    let got = new.get(at.section).and_then(|s| s.get(at.key)).and_then(|v| v.as_str());
    anyhow::ensure!(got == Some(value), "`{}.{}` did not come out as `{value}`", at.section, at.key);
    for table in [&mut old, &mut new] {
        if let Some(section) = table.get_mut(at.section).and_then(|s| s.as_table_mut()) {
            section.remove(at.key);
            if section.is_empty() {
                table.remove(at.section);
            }
        }
    }
    anyhow::ensure!(old == new, "other values would change");
    Ok(())
}

/// The section a header line opens (`[graphics]` → `graphics`).
fn section_of(line: &str) -> Option<&str> {
    let code = strip_comment(line).trim();
    code.strip_prefix('[')?.strip_suffix(']').map(str::trim)
}

/// The key a `key = value` line sets (not for comments or headers).
fn key_of(line: &str) -> Option<&str> {
    let code = strip_comment(line);
    let (key, _) = code.split_once('=')?;
    let key = key.trim();
    (!key.is_empty() && !key.starts_with('[')).then_some(key)
}

/// `line` without a trailing `# comment` (a `#` inside quotes is not one).
fn strip_comment(line: &str) -> &str {
    comment_start(line).map_or(line, |i| &line[..i])
}

fn trailing_comment(line: &str) -> Option<&str> {
    comment_start(line).map(|i| line[i..].trim_end())
}

fn comment_start(line: &str) -> Option<usize> {
    let mut quote = None;
    for (i, c) in line.char_indices() {
        match (quote, c) {
            (None, '#') => return Some(i),
            (None, '\'' | '"') => quote = Some(c),
            (Some(q), c) if c == q => quote = None,
            _ => {}
        }
    }
    None
}

fn join(lines: &[String], newline: &str) -> String {
    let mut text = lines.join(newline);
    text.push_str(newline);
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    const AT: ConfigKey = ConfigKey {
        section: "graphics",
        key: "upscaler",
    };

    #[test]
    fn rewrites_an_existing_value_and_keeps_everything_else() {
        let doc =
            "# my config\n[paths]\ngame_dir = 'D:/gta'\n\n[graphics]\nupscaler = 'title_default'\n";
        assert_eq!(
            with_value(doc, AT, "amd_fsr"),
            "# my config\n[paths]\ngame_dir = 'D:/gta'\n\n[graphics]\nupscaler = 'amd_fsr'\n"
        );
    }

    #[test]
    fn keeps_the_indent_and_the_comment_of_the_line() {
        let doc = "[graphics]\n  upscaler = 'title_default'   # chosen in the menu\n";
        assert_eq!(
            with_value(doc, AT, "amd_fsr"),
            "[graphics]\n  upscaler = 'amd_fsr' # chosen in the menu\n"
        );
    }

    #[test]
    fn adds_the_key_under_an_existing_section() {
        let doc = "[graphics]\nother = 1\n";
        assert_eq!(
            with_value(doc, AT, "amd_fsr"),
            "[graphics]\nupscaler = 'amd_fsr'\nother = 1\n"
        );
    }

    #[test]
    fn appends_a_missing_section() {
        let doc = "[paths]\ngame_dir = 'D:/gta'\n";
        assert_eq!(
            with_value(doc, AT, "amd_fsr"),
            "[paths]\ngame_dir = 'D:/gta'\n\n[graphics]\nupscaler = 'amd_fsr'\n"
        );
    }

    #[test]
    fn starts_an_empty_file() {
        assert_eq!(
            with_value("", AT, "amd_fsr"),
            "[graphics]\nupscaler = 'amd_fsr'\n"
        );
    }

    #[test]
    fn leaves_commented_out_lines_alone() {
        // As in victory.example.toml: the section is only a comment.
        let doc = "# [graphics]\n# upscaler = 'amd_fsr'\n";
        assert_eq!(
            with_value(doc, AT, "title_default"),
            "# [graphics]\n# upscaler = 'amd_fsr'\n\n[graphics]\nupscaler = 'title_default'\n"
        );
    }

    #[test]
    fn only_touches_the_key_in_its_own_section() {
        let doc = "[other]\nupscaler = 'x'\n[graphics]\nupscaler = 'title_default'\n";
        assert_eq!(
            with_value(doc, AT, "amd_fsr"),
            "[other]\nupscaler = 'x'\n[graphics]\nupscaler = 'amd_fsr'\n"
        );
    }

    #[test]
    fn a_hash_inside_quotes_is_not_a_comment() {
        let doc = "[paths]\ngame_dir = 'D:/#games'\n[graphics]\nupscaler = 'title_default'\n";
        let out = with_value(doc, AT, "amd_fsr");
        assert!(out.contains("game_dir = 'D:/#games'"), "{out}");
    }

    #[test]
    fn keeps_windows_line_endings() {
        let doc = "[graphics]\r\nupscaler = 'title_default'\r\n";
        assert_eq!(
            with_value(doc, AT, "amd_fsr"),
            "[graphics]\r\nupscaler = 'amd_fsr'\r\n"
        );
    }

    #[test]
    fn an_inline_table_is_refused_not_mangled() {
        let doc = "graphics = { upscaler = 'title_default' }\n";
        let new = with_value(doc, AT, "amd_fsr");
        assert!(check_edit(doc, &new, AT, "amd_fsr").is_err());
    }

    #[test]
    fn a_dotted_key_is_refused_not_mangled() {
        let doc = "graphics.upscaler = 'title_default'\n";
        let new = with_value(doc, AT, "amd_fsr");
        assert!(check_edit(doc, &new, AT, "amd_fsr").is_err());
    }

    #[test]
    fn a_plain_edit_passes_the_check() {
        let doc = "# c\n[graphics]\nupscaler = 'title_default' # x\n[paths]\ngame_dir = 'D:/g'\n";
        assert!(check_edit(doc, &with_value(doc, AT, "amd_fsr"), AT, "amd_fsr").is_ok());
        assert!(check_edit("", &with_value("", AT, "amd_fsr"), AT, "amd_fsr").is_ok());
    }

    #[test]
    fn save_leaves_a_refused_file_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("victory.toml");
        let doc = "graphics = { upscaler = 'title_default' }\n";
        std::fs::write(&path, doc).unwrap();
        assert!(save(&path, crate::config::GraphicsUpscaler::AmdFsr).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), doc);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1, "no temp file left");
    }

    #[test]
    fn the_result_still_parses_as_toml() {
        let doc =
            "# c\n[graphics] # gfx\nupscaler = 'title_default' # x\n[paths]\ngame_dir = 'D:/g'\n";
        let out = with_value(doc, AT, "amd_fsr");
        let table: toml::Table = toml::from_str(&out).expect("valid TOML");
        assert_eq!(table["graphics"]["upscaler"].as_str(), Some("amd_fsr"));
        assert_eq!(table["paths"]["game_dir"].as_str(), Some("D:/g"));
    }
}
