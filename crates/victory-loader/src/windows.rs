//! Loading a prepared image as a real Windows module and binding it.

use std::collections::HashSet;
use std::ffi::{CString, c_void};
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicU64;

use anyhow::{Context, Result, bail};
use victory_pe::{ImageInfo, ImportSymbol};
use windows_sys::Win32::Foundation::{FreeLibrary, HMODULE};
use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryExW};
use windows_sys::Win32::System::Memory::{
    MEMORY_BASIC_INFORMATION, PAGE_READWRITE, VirtualProtect, VirtualQuery,
};

use crate::thunks::{ImportSite, ThunkBlock, UnimplementedPolicy};
use crate::{
    DLL_PROCESS_ATTACH, DLL_PROCESS_DETACH, ImportResolver, PreparedImage, Provenance, Resolution,
    prepare,
};

/// A title image mapped into this process. A clone names the same mapping
/// (no reference of its own): unload one copy only.
#[derive(Debug, Clone)]
pub struct LoadedImage {
    module: HMODULE,
    path: PathBuf,
    info: ImageInfo,
}

// SAFETY: HMODULE is a process-wide handle (the base address); using it from
// any thread is fine.
unsafe impl Send for LoadedImage {}
// SAFETY: as above; `LoadedImage` has no interior mutability.
unsafe impl Sync for LoadedImage {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MissingImport {
    pub dll: String,
    pub symbol: String,
}

/// Outcome of [`LoadedImage::bind_imports`].
#[derive(Debug, Default, Clone)]
pub struct BindReport {
    pub hle: usize,
    pub host: usize,
    pub title: usize,
    pub missing: Vec<MissingImport>,
}

/// Writes the prepared copy into `dir` and maps it with the Windows loader.
///
/// `dir` receives derived game data, so it must be outside the repository.
pub fn load(prepared: &PreparedImage, dir: &Path) -> Result<LoadedImage> {
    victory_core::ensure_outside_repo(dir)?;
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let path = dir.join(prepared.file_name());
    write_if_changed(&path, &prepared.bytes)?;

    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    // SAFETY: `wide` is a NUL-terminated path. The image has no entry point
    // and no imports, so loading it runs no title code.
    let module = unsafe { LoadLibraryExW(wide.as_ptr(), std::ptr::null_mut(), 0) };
    if module.is_null() {
        bail!(
            "LoadLibraryExW({}) failed: {}{}",
            path.display(),
            std::io::Error::last_os_error(),
            if prepared.info.has_relocations {
                ""
            } else {
                " (image has no relocations; its preferred base may be taken)"
            }
        );
    }
    let loaded = LoadedImage {
        module,
        path,
        info: prepared.info.clone(),
    };
    // A DLL indexes its TLS properly; only an exe assumes index 0.
    if !loaded.info.is_dll() {
        // SAFETY: just mapped, and no title code has run.
        unsafe { crate::tls::redirect(loaded.base(), &loaded.info) }?;
    }
    tracing::info!(
        base = format_args!("{:#x}", loaded.base()),
        preferred = format_args!("{:#x}", loaded.info.image_base),
        path = %loaded.path.display(),
        "image mapped"
    );
    crate::threads::mapped(&loaded);
    Ok(loaded)
}

/// Maps the DLLs the title ships in `title_dir` that `main` imports,
/// directly or through each other, in initialisation order (dependencies
/// first). Only DLL names `wanted` accepts are considered, so DLLs the host
/// or our runtime provide stay with them. Imports are not bound.
pub fn load_title_dlls(
    main: &ImageInfo,
    title_dir: &Path,
    cache_dir: &Path,
    wanted: &dyn Fn(&str) -> bool,
) -> Result<Vec<(String, LoadedImage)>> {
    let mut loaded = Vec::new();
    let dirs = (title_dir, cache_dir);
    map_imports(main, dirs, wanted, &mut HashSet::new(), &mut loaded)?;
    Ok(loaded)
}

/// Maps the DLL `name` from `title_dir` like [`load_title_dlls`] does, with
/// the title DLLs it imports (dependencies first, `name` last); empty if
/// `wanted` refuses it or the folder has no such file. For DLLs a title
/// loads itself; `wanted` must refuse those already mapped.
pub fn load_title_dll(
    name: &str,
    title_dir: &Path,
    cache_dir: &Path,
    wanted: &dyn Fn(&str) -> bool,
) -> Result<Vec<(String, LoadedImage)>> {
    let mut loaded = Vec::new();
    let dirs = (title_dir, cache_dir);
    map_dll(name, dirs, wanted, &mut HashSet::new(), &mut loaded)?;
    Ok(loaded)
}

fn map_imports(
    info: &ImageInfo,
    dirs: (&Path, &Path),
    wanted: &dyn Fn(&str) -> bool,
    seen: &mut HashSet<String>,
    loaded: &mut Vec<(String, LoadedImage)>,
) -> Result<()> {
    for module in info.imports.iter().chain(&info.delay_imports) {
        map_dll(&module.dll, dirs, wanted, seen, loaded)?;
    }
    Ok(())
}

fn map_dll(
    name: &str,
    dirs: (&Path, &Path),
    wanted: &dyn Fn(&str) -> bool,
    seen: &mut HashSet<String>,
    loaded: &mut Vec<(String, LoadedImage)>,
) -> Result<()> {
    let name = name.to_ascii_lowercase();
    let path = dirs.0.join(&name);
    if !seen.insert(name.clone()) || !wanted(&name) || !path.is_file() {
        return Ok(());
    }
    let original = std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
    let prepared = prepare(&original).with_context(|| format!("preparing {}", path.display()))?;
    let image = load(&prepared, dirs.1).with_context(|| format!("loading {}", path.display()))?;
    map_imports(&image.info, dirs, wanted, seen, loaded)?;
    loaded.push((name, image));
    Ok(())
}

/// Resolves through `inner`, then through the exports of the title's DLLs.
pub struct WithTitleDlls<'a> {
    pub inner: &'a dyn ImportResolver,
    pub dlls: &'a [(String, LoadedImage)],
}

impl ImportResolver for WithTitleDlls<'_> {
    fn resolve(&self, dll: &str, symbol: &ImportSymbol) -> Resolution {
        let found = self.inner.resolve(dll, symbol);
        if found != Resolution::Missing {
            return found;
        }
        self.dlls
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(dll))
            .and_then(|(_, image)| image.export_symbol(symbol))
            .map_or(Resolution::Missing, |address| Resolution::Found {
                address,
                provenance: Provenance::Title,
            })
    }
}

fn write_if_changed(path: &Path, bytes: &[u8]) -> Result<()> {
    crate::publish::write_if_changed(path, bytes)
        .with_context(|| format!("publishing {}", path.display()))
}

/// Protects and restores whole runs of IAT slots at once: one unprotect and
/// one restore per run of pages with one protection, instead of two per
/// slot. Import tables can sit in sections with different protections
/// (delay-load tables in `.didat`), so each page's protection is asked once
/// and runs are split where it changes.
///
/// Returns the runs it made (`first page`, `last page + 1`, protection).
fn write_slots(base: usize, slots: &[(u32, usize)]) -> Result<Vec<(usize, usize, u32)>> {
    const PAGE: usize = 4096;
    // The slots by the page they sit in.
    let mut by_page: std::collections::BTreeMap<usize, Vec<(u32, usize)>> = Default::default();
    for &(rva, address) in slots {
        by_page
            .entry((base + rva as usize) / PAGE)
            .or_default()
            .push((rva, address));
    }
    // Each page's current protection, asked once.
    let mut pages: Vec<(usize, u32)> = Vec::with_capacity(by_page.len());
    for &page in by_page.keys() {
        // SAFETY: plain data filled in by the query.
        let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: a valid out pointer of the right size; the image is mapped.
        if unsafe {
            VirtualQuery(
                (page * PAGE) as *const c_void,
                &mut info,
                size_of_val(&info),
            )
        } == 0
        {
            bail!(
                "VirtualQuery(IAT page {page:#x}) failed: {}",
                std::io::Error::last_os_error()
            );
        }
        pages.push((page, info.Protect));
    }
    // Merge consecutive pages with the same protection into runs.
    let mut runs: Vec<(usize, usize, u32)> = Vec::new(); // (first page, last page + 1, protect)
    for (page, protect) in pages {
        match runs.last_mut() {
            Some(run) if run.2 == protect && run.1 == page => run.1 = page + 1,
            _ => runs.push((page, page + 1, protect)),
        }
    }
    for &(first, end, protect) in &runs {
        let (at, len) = (first * PAGE, (end - first) * PAGE);
        // SAFETY: the parser only yields slots inside the image.
        if unsafe { VirtualProtect(at as *const c_void, len, PAGE_READWRITE, &mut 0) } == 0 {
            bail!(
                "VirtualProtect(IAT run {at:#x}..{:#x}) failed: {}",
                at + len,
                std::io::Error::last_os_error()
            );
        }
        for (&page, slots) in by_page.range(first..end) {
            for &(rva, address) in slots {
                let slot = (base + rva as usize) as *mut u64;
                debug_assert!(page * PAGE <= slot as usize && (slot as usize) < (page + 1) * PAGE);
                // SAFETY: inside the image, in the run just made writable,
                // 8-byte aligned by the PE format.
                unsafe { slot.write_volatile(address as u64) };
            }
        }
        let mut ignored = 0;
        // SAFETY: restoring the protection the run had (uniform by construction).
        if unsafe { VirtualProtect(at as *const c_void, len, protect, &mut ignored) } == 0 {
            // The slots are written; the run stays PAGE_READWRITE. Say so
            // instead of losing it: a load that leaves image pages writable
            // must not do it silently. Not fatal, because an image page
            // mapped from a file can report a protection from the
            // PAGE_WRITECOPY family that VirtualProtect cannot set back, and
            // failing the load over it would keep the title from booting.
            let error = std::io::Error::last_os_error();
            tracing::warn!(
                run = format_args!("{at:#x}..{:#x}", at + len),
                protect = format_args!("{protect:#x}"),
                %error,
                "restoring an IAT run's protection failed; the run stays PAGE_READWRITE"
            );
        }
    }
    Ok(runs)
}

impl LoadedImage {
    pub fn base(&self) -> usize {
        self.module as usize
    }

    /// Parsed from the original image (imports, entry point, TLS callbacks).
    pub fn info(&self) -> &ImageInfo {
        &self.info
    }

    /// Address of an export of the image, if it has one by that name.
    pub fn export(&self, name: &str) -> Option<usize> {
        let name = CString::new(name).ok()?;
        // SAFETY: valid module handle and NUL-terminated name.
        unsafe { GetProcAddress(self.module, name.as_ptr().cast()) }
            .map(|f| f as *const () as usize)
    }

    /// Address of an export, by name or ordinal.
    pub fn export_symbol(&self, symbol: &ImportSymbol) -> Option<usize> {
        match symbol {
            ImportSymbol::Name { name, .. } => self.export(name),
            // SAFETY: MAKEINTRESOURCE-style ordinal lookup on a valid module.
            ImportSymbol::Ordinal(o) => unsafe { GetProcAddress(self.module, *o as usize as _) }
                .map(|f| f as *const () as usize),
        }
    }

    /// Fills every IAT slot of the image.
    ///
    /// Each import goes to `resolver`; whatever it cannot resolve gets an
    /// unimplemented-import stub that behaves according to `policy`.
    pub fn bind_imports(
        &self,
        resolver: &dyn ImportResolver,
        policy: UnimplementedPolicy,
    ) -> Result<BindReport> {
        let mut report = BindReport::default();
        let mut slots: Vec<(u32, usize)> = Vec::with_capacity(self.info.import_count());
        let mut unresolved: Vec<(u32, &str, &ImportSymbol)> = Vec::new();

        // Delay-load slots are bound eagerly too, so the title's own
        // delay-load helper (which would ask the host) never runs.
        for module in self.info.imports.iter().chain(&self.info.delay_imports) {
            for entry in &module.entries {
                match resolver.resolve(&module.dll, &entry.symbol) {
                    Resolution::Found {
                        address,
                        provenance,
                    } => {
                        match provenance {
                            Provenance::Hle => report.hle += 1,
                            Provenance::Host => report.host += 1,
                            Provenance::Title => report.title += 1,
                        }
                        slots.push((entry.iat_slot_rva, address));
                    }
                    Resolution::Missing => {
                        unresolved.push((entry.iat_slot_rva, &module.dll, &entry.symbol));
                    }
                }
            }
        }

        if !unresolved.is_empty() {
            let mut block = ThunkBlock::new(unresolved.len())?;
            for (slot, dll, symbol) in unresolved {
                let site: &'static ImportSite = Box::leak(Box::new(ImportSite {
                    dll: dll.to_owned(),
                    symbol: symbol.to_string(),
                    policy,
                    calls: AtomicU64::new(0),
                }));
                slots.push((slot, block.emit(site)?));
                report.missing.push(MissingImport {
                    dll: site.dll.clone(),
                    symbol: site.symbol.clone(),
                });
            }
            block.seal()?;
        }

        self.write_slots(&slots)?;
        tracing::info!(
            hle = report.hle,
            host = report.host,
            missing = report.missing.len(),
            "imports bound"
        );
        Ok(report)
    }

    fn write_slots(&self, slots: &[(u32, usize)]) -> Result<()> {
        // The runs are protected, written and restored by the free function;
        // only the tests look at their boundaries.
        write_slots(self.base(), slots).map(drop)
    }

    /// Calls the image's TLS callbacks with `reason` (e.g. `DLL_PROCESS_ATTACH`).
    ///
    /// # Safety
    /// Runs title code. Imports must be bound first.
    pub unsafe fn run_tls_callbacks(&self, reason: u32) {
        // SAFETY: forwarded from the caller.
        unsafe { self.tls_callbacks(reason, std::ptr::null_mut()) }
    }

    /// # Safety
    /// See [`Self::run_tls_callbacks`].
    unsafe fn tls_callbacks(&self, reason: u32, reserved: *mut c_void) {
        let Some(tls) = &self.info.tls else { return };
        for &rva in &tls.callback_rvas {
            type TlsCallback = unsafe extern "system" fn(*mut c_void, u32, *mut c_void);
            // SAFETY: the RVA came from the image's own TLS directory.
            let callback: TlsCallback = unsafe { std::mem::transmute(self.base() + rva as usize) };
            // SAFETY: the caller guarantees imports are bound.
            unsafe { callback(self.module, reason, reserved) };
        }
    }

    /// Initialises a DLL as the Windows loader would: its TLS callbacks,
    /// then `DllMain(DLL_PROCESS_ATTACH)`. From then on it hears of every
    /// thread start and end (see `threads`).
    ///
    /// # Safety
    /// Runs title code. Imports must be bound first, and the DLLs this one
    /// depends on attached.
    pub unsafe fn attach(&self) -> Result<()> {
        // SAFETY: forwarded from the caller.
        if unsafe { self.notify(DLL_PROCESS_ATTACH, true, std::ptr::null_mut()) } == Some(0) {
            bail!("DllMain returned FALSE");
        }
        crate::threads::attached(self.base());
        Ok(())
    }

    /// `DLL_PROCESS_DETACH` as `FreeLibrary` sends it (TLS callbacks, then
    /// `DllMain`); the image hears of no more threads.
    ///
    /// # Safety
    /// Runs title code. The image must have been attached (or its attach
    /// failed), and no other thread may be using it.
    pub unsafe fn detach(&self) {
        crate::threads::detached(self.base());
        // SAFETY: forwarded from the caller.
        unsafe { self.notify(DLL_PROCESS_DETACH, true, std::ptr::null_mut()) };
    }

    /// Runs the image's TLS callbacks with `reason`, then, if `dll_main` and
    /// the image is a DLL with an entry point, its `DllMain`, whose result
    /// is returned. `reserved` is the entry points' third argument.
    ///
    /// # Safety
    /// Runs title code. Imports must be bound first.
    pub(crate) unsafe fn notify(
        &self,
        reason: u32,
        dll_main: bool,
        reserved: *mut c_void,
    ) -> Option<i32> {
        // SAFETY: forwarded from the caller.
        unsafe { self.tls_callbacks(reason, reserved) };
        if !dll_main || !self.info.is_dll() || self.info.entry_rva == 0 {
            return None;
        }
        type DllMain = unsafe extern "system" fn(*mut c_void, u32, *mut c_void) -> i32;
        // SAFETY: the entry RVA came from the original image header.
        let main: DllMain =
            unsafe { std::mem::transmute(self.base() + self.info.entry_rva as usize) };
        // SAFETY: forwarded from the caller.
        Some(unsafe { main(self.module, reason, reserved) })
    }

    /// Starts the title: on a new thread with the stack size from the image
    /// header, attaches `dlls` in order (see [`Self::attach`]), runs the
    /// image's TLS callbacks (`DLL_PROCESS_ATTACH`), then its original entry
    /// point. Returns the thread so the caller can join it.
    ///
    /// # Safety
    /// Runs title code. Imports of the image and of `dlls` must be bound.
    pub unsafe fn spawn_main(
        self: Arc<Self>,
        dlls: Vec<(String, LoadedImage)>,
    ) -> Result<std::thread::JoinHandle<Result<u32>>> {
        if self.info.entry_rva == 0 {
            bail!("image has no entry point");
        }
        let stack = usize::try_from(self.info.stack_reserve)
            .unwrap_or(usize::MAX)
            .max(1 << 20);
        std::thread::Builder::new()
            .name("title-main".into())
            .stack_size(stack)
            .spawn(move || {
                for (name, dll) in &dlls {
                    // SAFETY: forwarded from the caller of `spawn_main`.
                    unsafe { dll.attach() }.with_context(|| format!("attaching {name}"))?;
                }
                // SAFETY: forwarded from the caller of `spawn_main`.
                unsafe { self.run_tls_callbacks(DLL_PROCESS_ATTACH) };
                crate::threads::attached(self.base());
                type Entry = unsafe extern "system" fn(*mut c_void) -> u32;
                // SAFETY: the entry RVA came from the original image header.
                let entry: Entry =
                    unsafe { std::mem::transmute(self.base() + self.info.entry_rva as usize) };
                // SAFETY: forwarded from the caller of `spawn_main`.
                Ok(unsafe { entry(std::ptr::null_mut()) })
            })
            .context("spawning title main thread")
    }

    /// Unmaps the image.
    ///
    /// # Safety
    /// No title code may be running or run later, and no pointer into the
    /// image may be used afterwards.
    pub unsafe fn unload(self) {
        crate::threads::unmapped(self.base());
        // SAFETY: guaranteed by the caller.
        unsafe { FreeLibrary(self.module) };
    }
}

#[cfg(test)]
mod slots_tests {
    use super::*;
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READONLY, PAGE_READWRITE, VirtualAlloc,
        VirtualFree,
    };

    /// 500 slots spread over three pages whose middle page is read-only:
    /// every slot is written, the run splits at each protection change
    /// (three runs, not one), and each page gets its own protection back.
    #[test]
    fn slot_runs_span_three_pages_with_a_read_only_middle_page() {
        const PAGE: usize = 4096;
        // Four pages: the slots use the first three, so a run that ran past
        // the last slot's page would show up too.
        const PAGES: usize = 4;
        // SAFETY: a fresh allocation, freed by the test.
        let base = unsafe {
            VirtualAlloc(
                std::ptr::null(),
                PAGES * PAGE,
                MEM_RESERVE | MEM_COMMIT,
                PAGE_READWRITE,
            )
        } as usize;
        assert!(base != 0);
        // The middle page is read-only: the runs must split on both sides of
        // it instead of merging into one read-write run.
        let mut ignored = 0;
        // SAFETY: our allocation.
        let read_only = unsafe {
            VirtualProtect(
                (base + PAGE) as *const c_void,
                PAGE,
                PAGE_READONLY,
                &mut ignored,
            )
        };
        assert_ne!(
            read_only,
            0,
            "make the middle page read-only: {}",
            std::io::Error::last_os_error()
        );
        // 167 slots per page, 8 bytes apart: no slot straddles a page, and
        // 3 * 167 = 501 covers the 500 asked for.
        const PER_PAGE: u32 = 167;
        let slots: Vec<(u32, usize)> = (0..500u32)
            .map(|i| {
                (
                    (i / PER_PAGE) * PAGE as u32 + (i % PER_PAGE) * 8,
                    0x1000 + i as usize,
                )
            })
            .collect();
        let pages: Vec<usize> = slots
            .iter()
            .map(|&(rva, _)| (base + rva as usize) / PAGE)
            .collect();
        assert_eq!(
            (pages[0], pages[499]),
            (base / PAGE, base / PAGE + 2),
            "the slots span three pages"
        );
        let runs = write_slots(base, &slots).unwrap();
        let first = base / PAGE;
        assert_eq!(
            runs,
            vec![
                (first, first + 1, PAGE_READWRITE),
                (first + 1, first + 2, PAGE_READONLY),
                (first + 2, first + 3, PAGE_READWRITE),
            ],
            "one run per protection, split at the read-only middle page"
        );
        for &(rva, address) in &slots {
            // SAFETY: inside our allocation.
            assert_eq!(
                // SAFETY: inside our allocation.
                unsafe { ((base + rva as usize) as *const u64).read_volatile() },
                address as u64,
                "slot at rva {rva:#x}"
            );
        }
        for (index, want) in [
            (0usize, PAGE_READWRITE),
            (1, PAGE_READONLY),
            (2, PAGE_READWRITE),
        ] {
            // SAFETY: plain data filled in by the query.
            let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
            // SAFETY: valid out pointer.
            unsafe {
                VirtualQuery(
                    (base + index * PAGE) as *const c_void,
                    &mut info,
                    size_of_val(&info),
                )
            };
            assert_eq!(info.Protect, want, "page {index} keeps its own protection");
        }
        // SAFETY: freeing our allocation.
        unsafe { VirtualFree(base as *mut _, 0, MEM_RELEASE) };
    }

    /// Cost: 5,000 slots in one read-write region — batched vs per-slot.
    #[test]
    fn batched_slots_cost_less_than_two_protects_per_slot() {
        const PAGE: usize = 4096;
        // SAFETY: a fresh allocation, freed by the test.
        let base = unsafe {
            VirtualAlloc(
                std::ptr::null(),
                16 * PAGE,
                MEM_RESERVE | MEM_COMMIT,
                PAGE_READWRITE,
            )
        } as usize;
        assert!(base != 0);
        let mut ignored = 0;
        let many: Vec<(u32, usize)> = (0..5000u32).map(|i| (i * 8, i as usize)).collect();
        let t0 = std::time::Instant::now();
        write_slots(base, &many).unwrap();
        let batched = t0.elapsed();
        let t0 = std::time::Instant::now();
        for &(rva, address) in &many {
            let slot = (base + rva as usize) as *mut u64;
            let mut old = 0;
            // SAFETY: our allocation.
            unsafe { VirtualProtect(slot as *const c_void, 8, PAGE_READWRITE, &mut old) };
            // SAFETY: writable now.
            unsafe { slot.write_volatile(address as u64) };
            // SAFETY: restoring.
            unsafe { VirtualProtect(slot as *const c_void, 8, old, &mut ignored) };
        }
        let per_slot = t0.elapsed();
        eprintln!("5000 IAT slots: batched {batched:?}, per-slot {per_slot:?}");
        // SAFETY: freeing our allocation.
        unsafe { VirtualFree(base as *mut _, 0, MEM_RELEASE) };
    }
}
