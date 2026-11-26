//! A title DLL that cannot be loaded is named in the error.
#![cfg(windows)]

#[test]
fn a_title_dll_that_does_not_prepare_is_named() {
    let title = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    std::fs::write(title.path().join("broken.dll"), b"MZ not a PE").unwrap();
    let wanted = |_: &str| true;
    let loaded =
        victory_loader::load_title_dll("broken.dll", &[title.path()], cache.path(), &wanted);
    let Err(error) = loaded else {
        panic!("a DLL that is not a PE loaded");
    };
    let text = format!("{error:#}");
    assert!(text.contains("broken.dll"), "{text}");
}

/// A title whose executable and data sit in different folders ships its DLLs
/// beside the executable: they are found there, ahead of the data folder.
#[test]
fn title_dlls_are_searched_in_folder_order() {
    let exe = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let wanted = |_: &str| true;
    let load = |dirs: &[&std::path::Path], name: &str| {
        victory_loader::load_title_dll(name, dirs, cache.path(), &wanted)
    };

    // Not a PE: finding the file is what shows, as the error that names it.
    std::fs::write(exe.path().join("beside-exe.dll"), b"MZ not a PE").unwrap();
    let dirs = [exe.path(), data.path()];
    let text = format!("{:#}", load(&dirs, "beside-exe.dll").unwrap_err());
    assert!(text.contains("beside-exe.dll"), "{text}");
    // The folder that has none of it, alone, finds nothing.
    assert!(load(&[data.path()], "beside-exe.dll").unwrap().is_empty());

    // In the later folder only: found there.
    std::fs::write(data.path().join("in-data.dll"), b"MZ not a PE").unwrap();
    let text = format!("{:#}", load(&dirs, "in-data.dll").unwrap_err());
    assert!(text.contains("in-data.dll"), "{text}");

    // In both: the executable's folder wins.
    std::fs::write(exe.path().join("both.dll"), b"MZ exe copy").unwrap();
    std::fs::write(data.path().join("both.dll"), b"MZ data copy").unwrap();
    let text = format!("{:#}", load(&dirs, "both.dll").unwrap_err());
    assert!(text.contains(&exe.path().display().to_string()), "{text}");
    assert!(!text.contains(&data.path().display().to_string()), "{text}");
}
