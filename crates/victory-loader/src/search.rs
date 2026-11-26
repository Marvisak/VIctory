//! Where a title's own DLLs are looked for.

use std::path::{Path, PathBuf};

/// The file `name` (a DLL name, matched ignoring ASCII case by the file
/// system on Windows) in the first of the title folders `dirs` that has it.
/// The folders are in search order: a DLL present in two of them is taken
/// from the earlier one.
pub fn find_title_dll(dirs: &[&Path], name: &str) -> Option<PathBuf> {
    let name = name.to_ascii_lowercase();
    dirs.iter()
        .map(|dir| dir.join(&name))
        .find(|path| path.is_file())
}

#[cfg(test)]
mod tests {
    use super::find_title_dll;

    #[test]
    fn the_first_folder_that_has_the_file_wins() {
        let exe = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        std::fs::write(exe.path().join("only-exe.dll"), b"exe").unwrap();
        std::fs::write(data.path().join("only-data.dll"), b"data").unwrap();
        std::fs::write(exe.path().join("both.dll"), b"exe").unwrap();
        std::fs::write(data.path().join("both.dll"), b"data").unwrap();
        let dirs = [exe.path(), data.path()];

        assert_eq!(
            find_title_dll(&dirs, "only-exe.dll"),
            Some(exe.path().join("only-exe.dll"))
        );
        assert_eq!(
            find_title_dll(&dirs, "ONLY-DATA.dll"),
            Some(data.path().join("only-data.dll"))
        );
        assert_eq!(
            find_title_dll(&dirs, "both.dll"),
            Some(exe.path().join("both.dll"))
        );
        assert_eq!(find_title_dll(&dirs, "absent.dll"), None);
        assert_eq!(find_title_dll(&[], "both.dll"), None);
    }
}
