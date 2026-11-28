//! A profile-specific, read-only replacement for a loose controls diagram.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::windows::fs::OpenOptionsExt;
use std::path::{Component, Path, PathBuf};
use std::sync::OnceLock;

use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ;

struct Diagram {
    package_path: PathBuf,
    profiles: PathBuf,
}

static DIAGRAM: OnceLock<Diagram> = OnceLock::new();

/// Configure one package-relative file and the root of its profile directories.
pub fn use_controller_diagram(file: &Path, profiles: PathBuf) -> io::Result<()> {
    DIAGRAM.set(Diagram::new(file, profiles)?).map_err(|_| {
        io::Error::new(
            io::ErrorKind::AlreadyExists,
            "controller diagram already configured",
        )
    })
}

pub(crate) fn controller_diagram(name: &str) -> Option<File> {
    let diagram = DIAGRAM.get()?;
    diagram.open(name, super::super::gameinput::active_glyph_profile)
}

impl Diagram {
    fn new(file: &Path, profiles: PathBuf) -> io::Result<Self> {
        if file.file_name().is_none()
            || !file.components().all(|part| match part {
                Component::Normal(name) => !name.as_encoded_bytes().contains(&b':'),
                Component::CurDir => true,
                _ => false,
            })
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "VICTORY_GLYPH_FILE must be a package-relative file path without a drive or '..'",
            ));
        }
        Ok(Self {
            package_path: Path::new(r"G:\").join(file),
            // The guest can change the process's current directory after startup.
            profiles: std::path::absolute(profiles)?,
        })
    }

    fn open(&self, name: &str, profile: impl FnOnce() -> &'static str) -> Option<File> {
        let name = name.strip_prefix(r"\\?\").unwrap_or(name);
        let mut actual = Path::new(name).components();
        for wanted in self.package_path.components() {
            if !actual
                .next()?
                .as_os_str()
                .eq_ignore_ascii_case(wanted.as_os_str())
            {
                return None;
            }
        }
        if actual.next().is_some() {
            return None;
        }
        let profile = profile();
        let file_name = self.package_path.file_name()?;
        let candidate = self.profiles.join(profile).join(file_name);
        match OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ)
            .open(&candidate)
        {
            Ok(file) => {
                tracing::info!(profile, path = %candidate.display(), "opened controller diagram");
                Some(file)
            }
            Err(error) => {
                if error.kind() != std::io::ErrorKind::NotFound {
                    tracing::warn!(profile, %error, "using package controller diagram");
                }
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    #[test]
    fn relative_profiles_are_resolved_at_configuration() {
        let diagram = Diagram::new(Path::new("ui/controls.bin"), "packs".into()).unwrap();
        assert_eq!(
            diagram.profiles,
            std::env::current_dir().unwrap().join("packs")
        );
    }

    #[test]
    fn only_accepts_a_file_relative_to_the_package() {
        for path in [
            "",
            ".",
            "C:/controls.bin",
            "G:controls.bin",
            "/controls.bin",
            "//server/share/file",
            "../controls.bin",
            "ui/../controls.bin",
            "ui/a:stream",
        ] {
            assert!(
                Diagram::new(Path::new(path), PathBuf::new()).is_err(),
                "{path}"
            );
        }
    }

    #[test]
    fn each_open_uses_its_own_profile_and_preserves_existing_handles() {
        let dir = tempfile::tempdir().unwrap();
        let diagram = Diagram::new(Path::new("./ui/controls.bin"), dir.path().to_owned()).unwrap();
        for profile in ["playstation", "nintendo"] {
            std::fs::create_dir(dir.path().join(profile)).unwrap();
            std::fs::write(dir.path().join(profile).join("controls.bin"), profile).unwrap();
        }
        let mut first = diagram
            .open(r"g:\.\UI\Controls.bin", || "playstation")
            .unwrap();
        let mut second = diagram
            .open(r"\\?\G:\ui\controls.bin", || "nintendo")
            .unwrap();
        assert!(diagram.open("G:/ui/controls.bin", || "steam").is_none());
        for name in [
            "T:/ui/controls.bin",
            "G:/other/ui/controls.bin",
            "G:/data.rpf",
        ] {
            assert!(diagram.open(name, || "playstation").is_none());
        }
        let mut contents = String::new();
        second.read_to_string(&mut contents).unwrap();
        assert_eq!(contents, "nintendo");
        contents.clear();
        first.read_to_string(&mut contents).unwrap();
        assert_eq!(contents, "playstation");
    }
}
