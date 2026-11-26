//! `prebuild` (`cargo prebuild`): the README's warm-up step without its
//! paths. The files the title's shaders are in are found in the game
//! folder (its shader archive, its executable and the DLLs beside it), and
//! the shader cache and pipeline library are made where `run` reads them:
//! both go through the same configuration (`victory.toml` in the folder it
//! is started from, or `VICTORY_CONFIG`), which is shown first.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// Makes the shader cache, and with `pipelines` the pipeline library, for
/// the title in `game_dir` (default: the configured game folder).
pub(crate) fn run(game_dir: Option<PathBuf>, jobs: Option<usize>, pipelines: bool) -> Result<()> {
    let cfg = victory_core::Config::load()?;
    match &cfg.source {
        Some(file) => println!("config         {}", file.display()),
        None => println!(
            "config         none in {} (defaults and VICTORY_* variables only): run this \
             where you run the title, or set VICTORY_CONFIG, so that both use one cache",
            std::env::current_dir().unwrap_or_default().display()
        ),
    }
    let game_dir = match game_dir {
        Some(dir) => dir,
        None => cfg.require_game_dir()?.to_owned(),
    };
    let files = title_files(&game_dir)?;
    println!("shader cache   {}", cfg.cache_subdir("shaders")?.display());
    if pipelines {
        println!(
            "pipelines      {}",
            cfg.cache_subdir("pipelines")?.display()
        );
    }
    for file in &files {
        println!("reads          {}", file.display());
    }
    crate::translate::warm(files, jobs, pipelines)
}

/// The files of the title in `game_dir` that hold its shaders: its
/// executable, the first its manifest (`MicrosoftGame.config`, read as
/// `run` reads it) lists that is a `.exe` inside the folder, else the
/// folder's only `.exe`; then, from the executable's own folder (where
/// `run` takes the title's folder to be): its shader archive (`shaders.rpf`,
/// which holds the shader pack the pipelines come from) and the DLLs there.
/// Every DLL there, not only those the executable imports: the title loads
/// some at run time (`LoadLibrary`, `victory_hle::runtime::load_title_dll`),
/// which no import table names, such as the FidelityFX upscaler its loader
/// DLL opens. A DLL with no shaders costs one read.
pub(crate) fn title_files(game_dir: &Path) -> Result<Vec<PathBuf>> {
    anyhow::ensure!(game_dir.is_dir(), "{} is not a folder", game_dir.display());
    let manifest =
        std::fs::read_to_string(game_dir.join("MicrosoftGame.config")).unwrap_or_default();
    let listed = victory_hle::runtime::manifest_executables(&manifest)
        .iter()
        .filter_map(|name| inside(game_dir, name))
        .find(|path| path.is_file());
    let exe = match listed {
        Some(exe) => Some(exe),
        None => {
            let mut exes = files_in(game_dir)?.into_iter().filter(|p| is(p, "exe"));
            match (exes.next(), exes.next()) {
                (Some(exe), None) => Some(exe),
                _ => None,
            }
        }
    };
    let exe = exe.with_context(|| {
        format!(
            "no title executable in {}: its MicrosoftGame.config lists none that is \
                 there, and the folder has no single .exe",
            game_dir.display()
        )
    })?;
    let folder = exe.parent().unwrap_or(game_dir).to_owned();
    let archive = folder.join("shaders.rpf");
    Ok(archive
        .is_file()
        .then_some(archive)
        .into_iter()
        .chain([exe])
        .chain(files_in(&folder)?.into_iter().filter(|p| is(p, "dll")))
        .collect())
}

/// `name` (from the manifest) inside `dir`, if it is a relative path of a
/// `.exe` there: no root, drive, empty, `..` or `.` parts (checked on the
/// text, as [`Path::components`] drops an inner `.`), and the file it names
/// is in `dir` once links and junctions on the way are followed.
fn inside(dir: &Path, name: &str) -> Option<PathBuf> {
    let parts_plain = name
        .split(['/', '\\'])
        .all(|part| !matches!(part, "" | "." | ".."));
    let path = Path::new(name);
    let plain = parts_plain
        && path
            .components()
            .all(|c| matches!(c, std::path::Component::Normal(_)));
    if !plain || !is(path, "exe") {
        return None;
    }
    let path = dir.join(path);
    let real = path.canonicalize().ok()?;
    real.starts_with(dir.canonicalize().ok()?).then_some(path)
}

/// Whether `path` has extension `ext` (any case).
fn is(path: &Path, ext: &str) -> bool {
    path.extension()
        .is_some_and(|e| e.eq_ignore_ascii_case(ext))
}

/// The files directly in `dir`, sorted.
fn files_in(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .with_context(|| format!("reading {}", dir.display()))?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.is_file())
        .collect();
    files.sort();
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A temporary folder of empty files, removed when the test ends, also
    /// when it fails.
    struct Folder(PathBuf);

    impl Folder {
        fn new(name: &str, files: &[&str]) -> Folder {
            let dir = std::env::temp_dir()
                .join(format!("victory-title-files-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            for file in files {
                let path = dir.join(file);
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::write(path, b"").unwrap();
            }
            Folder(dir)
        }

        /// A manifest listing `names`, with a commented-out entry first.
        fn manifest(&self, names: &[&str]) {
            let entries: String = names
                .iter()
                .map(|n| format!(r#"<Executable Name="{n}" Id="Game"/>"#))
                .collect();
            let text = format!(
                r#"<Game><ExecutableList><!-- <Executable Name="Crash.exe"/> -->{entries}</ExecutableList></Game>"#
            );
            std::fs::write(self.0.join("MicrosoftGame.config"), text).unwrap();
        }

        /// The files `title_files` gives, relative to the folder.
        fn picked(&self) -> Result<Vec<String>> {
            Ok(title_files(&self.0)?
                .iter()
                .map(|f| {
                    let f = f.strip_prefix(&self.0).unwrap();
                    f.to_string_lossy().replace('\\', "/")
                })
                .collect())
        }
    }

    impl Drop for Folder {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A folder gives its archive, the executable its manifest lists (not
    /// one in a comment) and its DLLs, not those of a folder below; with no
    /// manifest, its one `.exe`; with two and no manifest, none.
    #[test]
    fn a_game_folder_gives_its_archive_executable_and_dlls() {
        let files = [
            "shaders.rpf",
            "Title.exe",
            "Crash.exe",
            "b.dll",
            "A.DLL",
            "data.bin",
            "sub/c.dll",
        ];
        let folder = Folder::new("plain", &files);
        folder.manifest(&["Title.exe"]);
        let want = ["shaders.rpf", "Title.exe", "A.DLL", "b.dll"];
        assert_eq!(folder.picked().unwrap(), want);
        std::fs::remove_file(folder.0.join("MicrosoftGame.config")).unwrap();
        assert!(folder.picked().is_err(), "two executables and no manifest");
        std::fs::remove_file(folder.0.join("Crash.exe")).unwrap();
        std::fs::remove_file(folder.0.join("shaders.rpf")).unwrap();
        assert_eq!(folder.picked().unwrap(), ["Title.exe", "A.DLL", "b.dll"]);
        std::fs::remove_dir_all(&folder.0).unwrap();
        assert!(folder.picked().is_err(), "no folder");
    }

    /// An executable the manifest lists in a folder below: the archive and
    /// the DLLs come from its folder, as `run` takes it to be the title's.
    #[test]
    fn a_nested_executable_brings_the_files_beside_it() {
        let files = ["top.dll", "bin/Title.exe", "bin/shaders.rpf", "bin/b.dll"];
        let folder = Folder::new("nested", &files);
        folder.manifest(&["bin/Title.exe"]);
        let want = ["bin/shaders.rpf", "bin/Title.exe", "bin/b.dll"];
        assert_eq!(folder.picked().unwrap(), want);
    }

    /// Manifest entries that leave the folder, are absolute or are not a
    /// `.exe` are passed over, also when such a file is there.
    #[test]
    fn manifest_entries_outside_the_folder_or_not_executables_are_passed_over() {
        let outside = Folder::new("outside", &["Other.exe"]);
        let folder = Folder::new("guarded", &["Title.exe", "notes.txt", "x.dll"]);
        let up = format!(
            "../{}/Other.exe",
            outside.0.file_name().unwrap().to_string_lossy()
        );
        let absolute = outside.0.join("Other.exe").to_string_lossy().into_owned();
        folder.manifest(&[&up, &absolute, "notes.txt", "./Title.exe"]);
        assert!(inside(&folder.0, &up).is_none() && inside(&folder.0, &absolute).is_none());
        assert!(inside(&folder.0, "notes.txt").is_none());
        assert!(inside(&folder.0, "./Title.exe").is_none());
        assert!(inside(&folder.0, "Title.exe").is_some());
        // None of them: the folder's only `.exe`.
        assert_eq!(folder.picked().unwrap(), ["Title.exe", "x.dll"]);
    }

    /// An inner `.` part, which [`Path::components`] drops, is passed over
    /// as the outer ones are, and so is an empty part.
    #[test]
    fn inner_dot_and_empty_parts_are_passed_over() {
        let folder = Folder::new("dots", &["bin/Title.exe"]);
        assert!(inside(&folder.0, "bin/Title.exe").is_some());
        assert!(inside(&folder.0, r"bin\Title.exe").is_some());
        for name in [
            "bin/./Title.exe",
            r"bin\.\Title.exe",
            "bin//Title.exe",
            "bin/Title.exe/",
        ] {
            assert!(inside(&folder.0, name).is_none(), "{name}");
        }
    }

    /// A manifest entry reached through a junction out of the folder is
    /// passed over, though the path has only plain parts.
    #[cfg(windows)]
    #[test]
    fn an_executable_through_a_junction_out_of_the_folder_is_passed_over() {
        let outside = Folder::new("beyond", &["Other.exe"]);
        let folder = Folder::new("junction", &["Title.exe"]);
        let link = folder.0.join("out");
        // A junction needs no privilege, unlike a symbolic link.
        let made = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&link)
            .arg(&outside.0)
            .output()
            .unwrap();
        assert!(made.status.success(), "{made:?}");
        assert!(link.join("Other.exe").is_file());
        folder.manifest(&["out/Other.exe"]);
        assert!(inside(&folder.0, "out/Other.exe").is_none());
        assert_eq!(folder.picked().unwrap(), ["Title.exe"]);
    }
}
