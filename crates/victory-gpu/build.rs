//! Our compute shaders (`src/shaders/*.hlsl`, see `src/hlsl.rs`), compiled
//! into the build when it runs on Windows for Windows: the binary then needs
//! no HLSL compiler where it runs (Wine's cannot compile them), and a shader
//! that does not compile fails the build. A cross build leaves them to the
//! system's compiler at first use.

fn main() {
    println!("cargo:rustc-check-cfg=cfg(victory_hlsl_prebuilt)");
    println!("cargo:rerun-if-changed=src/shaders");
    #[cfg(all(windows, target_env = "msvc"))]
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let out = std::path::PathBuf::from(std::env::var_os("OUT_DIR").expect("set by cargo"));
        for entry in std::fs::read_dir("src/shaders").expect("src/shaders") {
            let path = entry.expect("a directory entry").path();
            if path.extension().is_none_or(|e| e != "hlsl") {
                continue;
            }
            let name = path
                .file_stem()
                .and_then(|n| n.to_str())
                .expect("a UTF-8 name");
            let source = std::fs::read(&path).expect("a readable shader");
            match fxc::compile(name, &source) {
                Ok(code) => std::fs::write(out.join(format!("{name}.dxbc")), code)
                    .expect("a writable OUT_DIR"),
                Err(e) => panic!("{}: {e}", path.display()),
            }
        }
        println!("cargo:rustc-cfg=victory_hlsl_prebuilt");
    }
}

/// The system's HLSL compiler (`d3dcompiler_47.dll`, part of Windows).
#[cfg(all(windows, target_env = "msvc"))]
mod fxc {
    use std::ffi::{c_char, c_void};

    /// An `ID3DBlob`.
    #[repr(C)]
    struct Blob {
        vtbl: *const BlobVtbl,
    }

    #[repr(C)]
    struct BlobVtbl {
        _query_interface: usize,
        _add_ref: usize,
        release: unsafe extern "system" fn(*mut Blob) -> u32,
        pointer: unsafe extern "system" fn(*mut Blob) -> *const u8,
        size: unsafe extern "system" fn(*mut Blob) -> usize,
    }

    #[link(name = "d3dcompiler_47", kind = "raw-dylib")]
    unsafe extern "system" {
        fn D3DCompile(
            source: *const c_void,
            size: usize,
            name: *const c_char,
            defines: *const c_void,
            include: *const c_void,
            entry: *const c_char,
            target: *const c_char,
            flags1: u32,
            flags2: u32,
            code: *mut *mut Blob,
            errors: *mut *mut Blob,
        ) -> i32;
    }

    /// `blob`'s bytes, releasing it; none for a null blob.
    ///
    /// # Safety
    ///
    /// `blob` is null or a blob the caller owns.
    unsafe fn take(blob: *mut Blob) -> Vec<u8> {
        if blob.is_null() {
            return Vec::new();
        }
        // SAFETY: a live blob (the caller's contract), released last.
        unsafe {
            let vtbl = &*(*blob).vtbl;
            let bytes =
                std::slice::from_raw_parts((vtbl.pointer)(blob), (vtbl.size)(blob)).to_vec();
            (vtbl.release)(blob);
            bytes
        }
    }

    /// `source` compiled as `src/hlsl.rs` compiles it (`main`, `cs_5_1`, no
    /// flags), or the compiler's messages.
    pub fn compile(name: &str, source: &[u8]) -> Result<Vec<u8>, String> {
        let name = format!("{name}\0");
        let (mut code, mut errors) = (std::ptr::null_mut(), std::ptr::null_mut());
        // SAFETY: the source and names live through the call; out pointers
        // ours.
        let hr = unsafe {
            D3DCompile(
                source.as_ptr().cast(),
                source.len(),
                name.as_ptr().cast(),
                std::ptr::null(),
                std::ptr::null(),
                c"main".as_ptr(),
                c"cs_5_1".as_ptr(),
                0,
                0,
                &mut code,
                &mut errors,
            )
        };
        // SAFETY: blobs the call made, ours, or null.
        let (code, errors) = unsafe { (take(code), take(errors)) };
        if hr < 0 || code.is_empty() {
            return Err(format!(
                "{:#010x}: {}",
                hr as u32,
                String::from_utf8_lossy(&errors)
            ));
        }
        Ok(code)
    }
}
