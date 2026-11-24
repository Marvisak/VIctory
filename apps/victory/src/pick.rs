//! The game picker: find the title's executable when the command line has
//! none.
//!
//! `victory` with no arguments, and `victory run` without an executable,
//! open the normal Windows folder dialog so the game can be located with the
//! mouse. We then look inside what was chosen for the title's executable (the
//! one its package manifest names, or one importing DLLs only the console's
//! platform has) and run that. The folder is remembered, so the dialog opens
//! there next time.

use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use victory_core::Config;

/// The executable the picker found. Being chosen in a window means the user
/// wants to play, so the title runs instead of only being bound.
pub fn choose_exe(cfg: &Config) -> Result<PathBuf> {
    let start = remembered(cfg)
        .or_else(|| cfg.game_dir.clone())
        .filter(|p| p.is_dir());
    println!("VIctory: no game on the command line, opening the picker.");
    let Some(dir) = folder_dialog(start.as_deref())? else {
        anyhow::bail!("the picker was cancelled");
    };
    remember(cfg, &dir);
    println!("game folder    {}", dir.display());

    let candidates = find_candidates(&dir);
    if candidates.is_empty() {
        println!("no console title executable found there; choose the title's .exe yourself.");
        return file_dialog(&dir)?.context("no executable chosen");
    }
    if let [only] = candidates.as_slice() {
        println!("title exe      {}", only.path.display());
        return Ok(only.path.clone());
    }
    println!("more than one title executable there:");
    for (n, c) in candidates.iter().enumerate() {
        let why = if c.declared {
            "the folder's MicrosoftGame.config names it"
        } else {
            "it imports console DLLs"
        };
        println!("  {}) {}  ({why})", n + 1, c.path.display());
    }
    print!("which one? [1]: ");
    io::stdout().flush().ok();
    let mut answer = String::new();
    io::stdin().lock().read_line(&mut answer).ok();
    let n = answer
        .trim()
        .parse::<usize>()
        .ok()
        .filter(|n| *n > 0 && *n <= candidates.len())
        .unwrap_or(1);
    Ok(candidates[n - 1].path.clone())
}

/// A title executable found in the folder the user chose.
struct Candidate {
    path: PathBuf,
    /// Console-only DLLs it imports: why we think this is the title.
    console_dlls: usize,
    /// The folder's `MicrosoftGame.config` names this one.
    declared: bool,
}

/// Executables in `dir` and one level below that look like the title's,
/// best first.
fn find_candidates(dir: &Path) -> Vec<Candidate> {
    let declared = declared_exe(dir);
    let mut found: Vec<Candidate> = walkdir::WalkDir::new(dir)
        .max_depth(2)
        .follow_links(false)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file())
        .filter(|e| {
            e.path()
                .extension()
                .is_some_and(|x| x.eq_ignore_ascii_case("exe"))
        })
        // The title's executable is large; a few kilobytes means a launcher
        // and a gigabyte packed data with the wrong suffix.
        .filter(|e| {
            let len = e.metadata().map(|m| m.len()).unwrap_or(0);
            (64 << 10..=1 << 30).contains(&len)
        })
        .map(|e| Candidate {
            console_dlls: console_dlls(e.path()),
            declared: declared.as_deref().is_some_and(|d| same_path(d, e.path())),
            path: e.path().to_path_buf(),
        })
        .filter(|c| c.console_dlls > 0 || c.declared)
        .collect();
    found.sort_by(|a, b| {
        b.declared
            .cmp(&a.declared)
            .then(b.console_dlls.cmp(&a.console_dlls))
            .then(a.path.cmp(&b.path))
    });
    found
}

/// The executable a package manifest points at, if the folder has one.
fn declared_exe(dir: &Path) -> Option<PathBuf> {
    let text = std::fs::read_to_string(dir.join("MicrosoftGame.config")).ok()?;
    let at = text.find("Executable=\"")? + "Executable=\"".len();
    let name = text[at..].split('"').next()?;
    let path = dir.join(name);
    path.is_file().then_some(path)
}

/// The same file under the separators and letter case Windows allows a
/// manifest to use.
fn same_path(a: &Path, b: &Path) -> bool {
    let plain = |p: &Path| p.to_string_lossy().replace('/', "\\").to_ascii_lowercase();
    plain(a) == plain(b)
}

/// How many DLLs this program imports that our runtime answers for and the
/// host does not offer: the shape of an Xbox Series title, and zero for a
/// Windows program.
fn console_dlls(path: &Path) -> usize {
    let Ok(bytes) = std::fs::read(path) else {
        return 0;
    };
    let Ok(info) = victory_pe::ImageInfo::parse(&bytes) else {
        return 0;
    };
    if info.characteristics & victory_pe::layout::file_flags::DLL != 0 {
        return 0;
    }
    let host = victory_hle::HostPolicy::default();
    let mut dlls: Vec<String> = info
        .imports
        .iter()
        .chain(&info.delay_imports)
        .map(|i| i.dll.clone())
        .filter(|dll| console_only(&host, dll))
        .collect();
    dlls.sort_unstable();
    dlls.dedup();
    dlls.len()
}

/// A DLL only the console's platform answers for: our runtime implements it
/// and the host does not offer it. A few of those names exist on Windows too
/// (`gameinput.dll`), which can only ever make a file a candidate to ask
/// about, never a title we run by itself without asking.
fn console_only(host: &victory_hle::HostPolicy, dll: &str) -> bool {
    !host.allows(dll) && victory_hle::registry().iter().any(|m| m.serves(dll))
}

/// The folder chosen last time, kept in the cache directory: losing it only
/// means the dialog opens wherever Windows likes.
fn remembered(cfg: &Config) -> Option<PathBuf> {
    let text = std::fs::read_to_string(last_file(cfg).ok()?).ok()?;
    let dir = PathBuf::from(text.trim());
    dir.is_dir().then_some(dir)
}

fn remember(cfg: &Config, dir: &Path) {
    if let Ok(file) = last_file(cfg) {
        std::fs::write(file, dir.as_os_str().as_encoded_bytes()).ok();
    }
}

fn last_file(cfg: &Config) -> Result<PathBuf> {
    Ok(cfg.cache_subdir("picker")?.join("last-folder.txt"))
}

/// The folder dialog, starting in `start` if it still exists. `None` if the
/// user cancelled.
#[cfg(windows)]
fn folder_dialog(start: Option<&Path>) -> Result<Option<PathBuf>> {
    let _com = Com::start();
    // SAFETY: COM is initialised for this thread by `_com` and released when
    // it is dropped; every call below checks its result.
    unsafe {
        let dialog = new_dialog()?;
        dialog
            .SetOptions(FOS_PICKFOLDERS | FOS_PATHMUSTEXIST | FOS_FORCEFILESYSTEM | FOS_NOCHANGEDIR)
            .ok();
        set_title(&dialog, "Choose the folder that holds the game");
        start_in(&dialog, start);
        if !shown(&dialog)? {
            return Ok(None);
        }
        chosen(&dialog)
    }
}

/// The file dialog filtered to executables, starting in `dir`. `None` if the
/// user cancelled.
#[cfg(windows)]
fn file_dialog(dir: &Path) -> Result<Option<PathBuf>> {
    let _com = Com::start();
    // SAFETY: as in `folder_dialog`; the filter strings outlive `SetFileTypes`.
    unsafe {
        let dialog = new_dialog()?;
        dialog
            .SetOptions(
                FOS_FILEMUSTEXIST | FOS_PATHMUSTEXIST | FOS_FORCEFILESYSTEM | FOS_NOCHANGEDIR,
            )
            .ok();
        let names = [wide("Executables (*.exe)"), wide("All files")];
        let specs = [wide("*.exe"), wide("*.*")];
        let filters = [
            COMDLG_FILTERSPEC {
                pszName: PCWSTR(names[0].as_ptr()),
                pszSpec: PCWSTR(specs[0].as_ptr()),
            },
            COMDLG_FILTERSPEC {
                pszName: PCWSTR(names[1].as_ptr()),
                pszSpec: PCWSTR(specs[1].as_ptr()),
            },
        ];
        dialog.SetFileTypes(&filters).ok();
        set_title(&dialog, "Choose the game's executable");
        start_in(&dialog, Some(dir));
        if !shown(&dialog)? {
            return Ok(None);
        }
        chosen(&dialog)
    }
}

#[cfg(not(windows))]
fn folder_dialog(_: Option<&Path>) -> Result<Option<PathBuf>> {
    anyhow::bail!(
        "picking a game in a window needs Windows; give the title's executable on the command line"
    )
}

#[cfg(not(windows))]
fn file_dialog(_: &Path) -> Result<Option<PathBuf>> {
    anyhow::bail!(
        "picking a game in a window needs Windows; give the title's executable on the command line"
    )
}

#[cfg(windows)]
mod win {
    use std::path::{Path, PathBuf};

    use anyhow::{Context, Result};
    pub use windows::Win32::System::Com::CoUninitialize;
    use windows::Win32::System::Com::IBindCtx;
    pub use windows::Win32::System::Com::{
        CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoCreateInstance,
        CoInitializeEx, CoTaskMemFree,
    };
    pub use windows::Win32::UI::Shell::Common::COMDLG_FILTERSPEC;
    pub use windows::Win32::UI::Shell::{
        FOS_FILEMUSTEXIST, FOS_FORCEFILESYSTEM, FOS_NOCHANGEDIR, FOS_PATHMUSTEXIST,
        FOS_PICKFOLDERS, FileOpenDialog, IFileOpenDialog, IShellItem, SHCreateItemFromParsingName,
        SIGDN_FILESYSPATH,
    };
    pub use windows::core::PCWSTR;

    /// `HRESULT_FROM_WIN32(ERROR_CANCELLED)`: what the dialog reports when
    /// the user closes it without choosing anything.
    const CANCELLED: i32 = 0x8007_04C7_u32 as i32;

    /// COM for the calling thread, released when the dialog is gone. The
    /// count is ours alone, so `CoUninitialize` is balanced.
    pub struct Com(bool);

    impl Com {
        pub fn start() -> Self {
            // SAFETY: initialising COM for this thread only, paired with
            // `CoUninitialize` in `Drop`.
            let started =
                unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) }
                    .is_ok();
            Self(started)
        }
    }

    impl Drop for Com {
        fn drop(&mut self) {
            if self.0 {
                // SAFETY: matches the successful `CoInitializeEx`.
                unsafe { CoUninitialize() };
            }
        }
    }

    /// The uninitialised dialog shell object.
    pub fn new_dialog() -> Result<IFileOpenDialog> {
        // SAFETY: COM is initialised by the caller's `Com`.
        unsafe {
            CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER)
                .context("creating the Windows file dialog")
        }
    }

    pub fn set_title(dialog: &IFileOpenDialog, title: &str) {
        let title = wide(title);
        // SAFETY: `title` outlives the call.
        unsafe { dialog.SetTitle(PCWSTR(title.as_ptr())) }.ok();
    }

    /// Opens the dialog in `dir`; a folder that has since vanished simply
    /// leaves it where Windows last was.
    pub fn start_in(dialog: &IFileOpenDialog, dir: Option<&Path>) {
        let Some(dir) = dir else { return };
        let path = wide(&dir.display().to_string());
        // SAFETY: `path` outlives the call; `None` means no bind context.
        let item: Result<IShellItem, _> =
            unsafe { SHCreateItemFromParsingName(PCWSTR(path.as_ptr()), None::<&IBindCtx>) };
        if let Ok(item) = item {
            // SAFETY: `item` is a live interface pointer.
            unsafe { dialog.SetFolder(&item) }.ok();
        }
    }

    /// Shows the dialog; `false` if the user cancelled.
    pub fn shown(dialog: &IFileOpenDialog) -> Result<bool> {
        // SAFETY: `dialog` is a live interface pointer.
        match unsafe { dialog.Show(None) } {
            Ok(()) => Ok(true),
            Err(e) if e.code().0 == CANCELLED => Ok(false),
            Err(e) => Err(e).context("showing the dialog"),
        }
    }

    /// The path the user chose, which the dialog requires in both modes.
    pub fn chosen(dialog: &IFileOpenDialog) -> Result<Option<PathBuf>> {
        // SAFETY: `Show` returned without cancelling, so a result exists.
        unsafe {
            let item = dialog.GetResult().context("reading the choice")?;
            let name = item
                .GetDisplayName(SIGDN_FILESYSPATH)
                .context("reading the chosen path")?;
            // SAFETY: Windows wrote a NUL-terminated wide string here, which
            // we copy before freeing.
            let text = name.to_string().context("the path is not valid text")?;
            CoTaskMemFree(Some(name.as_ptr() as *const core::ffi::c_void));
            Ok(Some(PathBuf::from(text)))
        }
    }

    pub fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(std::iter::once(0)).collect()
    }
}

#[cfg(windows)]
use win::*;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_manifest_names_its_executable() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("MicrosoftGame.config"),
            r#"<Game><Applications><Application Id="main" Executable="sub\gta6.exe" \
               EntryPoint="Windows.FullTrustApplication" /></Applications></Game>"#,
        )
        .unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub").join("gta6.exe"), b"pe").unwrap();
        std::fs::write(dir.path().join("other.exe"), b"pe").unwrap();
        assert_eq!(
            declared_exe(dir.path()),
            Some(dir.path().join("sub").join("gta6.exe"))
        );
    }

    #[test]
    fn nothing_is_declared_without_a_manifest_or_a_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(declared_exe(dir.path()), None);
        std::fs::write(
            dir.path().join("MicrosoftGame.config"),
            r#"<Game><Application Executable="gone.exe" /></Game>"#,
        )
        .unwrap();
        assert_eq!(declared_exe(dir.path()), None);
    }

    #[test]
    fn small_files_and_other_suffixes_are_no_candidates() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("readme.txt"), b"x").unwrap();
        std::fs::write(dir.path().join("small.exe"), vec![b'x'; 70 << 10]).unwrap();
        assert!(find_candidates(dir.path()).is_empty());
    }

    #[test]
    fn a_manifests_executable_is_a_candidate() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("gta6.exe");
        std::fs::write(
            dir.path().join("MicrosoftGame.config"),
            r#"<Game><Application Executable="gta6.exe" /></Game>"#,
        )
        .unwrap();
        // Not a PE image, so only the manifest points at it.
        std::fs::write(&exe, vec![b'x'; 70 << 10]).unwrap();
        let found = find_candidates(dir.path());
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].path, exe);
        assert!(found[0].declared);
    }

    #[test]
    fn a_manifest_may_spell_its_path_any_way_windows_allows() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(
            dir.path().join("sub").join("GTA6.EXE"),
            vec![b'x'; 70 << 10],
        )
        .unwrap();
        std::fs::write(
            dir.path().join("MicrosoftGame.config"),
            r#"<Game><Application Executable="sub\gta6.exe" /></Game>"#,
        )
        .unwrap();
        let found = find_candidates(dir.path());
        assert_eq!(found.len(), 1);
        assert!(found[0].declared, "the manifest names the file we found");
    }

    #[test]
    fn a_real_windows_program_is_no_title() {
        // Not a made-up image: the imports of a system program are host
        // DLLs through and through, so it gives no evidence of a title.
        let Some(windir) = std::env::var_os("SystemRoot").map(PathBuf::from) else {
            return;
        };
        let mut checked = 0;
        for name in ["notepad.exe", "mspaint.exe", "explorer.exe"] {
            let path = windir.join(name);
            if !path.is_file() {
                continue;
            }
            checked += 1;
            assert_eq!(console_dlls(&path), 0, "{name} is no title");
        }
        assert!(checked > 0, "no system executable to check");
    }

    #[test]
    fn only_the_consoles_dlls_count_as_evidence() {
        let host = victory_hle::HostPolicy::default();
        // The host's own, or Windows' own that we do not answer for.
        for dll in [
            "kernel32.dll",
            "advapi32.dll",
            "ws2_32.dll",
            "mfplat.dll",
            "xaudio2_9.dll",
            "user32.dll",
            "shell32.dll",
            "comdlg32.dll",
            "api-ms-win-core-file-l1-1-0.dll",
        ] {
            assert!(!console_only(&host, dll), "{dll} is a Windows DLL");
        }
        // What only the console's platform (or our runtime) answers for.
        // `gameinput.dll` does exist on Windows 10 and 11; it may make a file
        // worth asking about, which is all a candidate is.
        for dll in [
            "xgameruntime.dll",
            "xmem.dll",
            "xg_xs.dll",
            "d3d12_xs.dll",
            "xcrdapi.dll",
            "chad.dll",
            "gameinput.dll",
        ] {
            assert!(console_only(&host, dll), "{dll} is console-only");
        }
        // The names in an image are the title's own, in any case it likes.
        assert!(console_only(&host, "XGameRuntime.dll"));
    }
}
