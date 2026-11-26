//! Game saves: containers, blobs, updates.
//!
//! Public reference: reference/system/xgamesave and the game-save overview
//! (_content/gc/system/overviews/game-save/game-saves): one update writes at
//! most 16 MB (E_GS_UPDATE_TOO_BIG, 0x80830005); an update is all or
//! nothing; container names outside the allowed characters are
//! E_GS_INVALID_CONTAINER_NAME (0x80830001); XGameSaveBlob is the blob's
//! info (name, size) and a pointer to its data.
//!
//! The area's ids and method slots are VIctory's (`crates/victory-hle/src/
//! modules/xgamesave.rs`).

use std::ffi::{CStr, c_char, c_void};

use crate::rt::{Guid, Hresult, Object, S_OK, area};
use crate::tap::Tap;
use crate::xasync::{Block, wait};

const PROVIDER: Guid = Guid(
    0x704c_3f58,
    0xe629,
    0x4cc2,
    [0xb1, 0x97, 0x30, 0x51, 0x1b, 0x99, 0x6f, 0xe2],
);

const E_GS_INVALID_CONTAINER_NAME: Hresult = 0x8083_0001_u32 as i32;
const E_GS_UPDATE_TOO_BIG: Hresult = 0x8083_0005_u32 as i32;

type Handle = *mut c_void;

/// XGameSaveBlob.
#[repr(C)]
struct Blob {
    name: *const c_char,
    size: u32,
    data: *const u8,
}

struct Saves(Object);

impl Saves {
    fn this(&self) -> *mut c_void {
        self.0.this()
    }
    fn provider(&self) -> Result<Handle, Hresult> {
        let mut block = Block::on_process_queue();
        // SAFETY: (all methods) the slot's signature in VIctory's table.
        let begin: extern "system" fn(
            *mut c_void,
            *const c_void,
            *const c_char,
            bool,
            *mut Block,
        ) -> Hresult = unsafe { self.0.method(0x20) };
        let hr = begin(
            self.this(),
            std::ptr::null(),
            c"conformance-scid".as_ptr(),
            false,
            &mut block,
        );
        if hr != S_OK {
            return Err(hr);
        }
        let hr = wait(&mut block);
        if hr != S_OK {
            return Err(hr);
        }
        let mut provider = std::ptr::null_mut();
        let result: extern "system" fn(*mut c_void, *mut Block, *mut Handle) -> Hresult =
            unsafe { self.0.method(0x28) };
        match result(self.this(), &mut block, &mut provider) {
            S_OK => Ok(provider),
            hr => Err(hr),
        }
    }
    fn close_provider(&self, p: Handle) {
        let f: extern "system" fn(*mut c_void, Handle) = unsafe { self.0.method(0x30) };
        f(self.this(), p)
    }
    fn container(&self, p: Handle, name: &CStr) -> Result<Handle, Hresult> {
        let mut out = std::ptr::null_mut();
        let f: extern "system" fn(*mut c_void, Handle, *const c_char, *mut Handle) -> Hresult =
            unsafe { self.0.method(0x80) };
        match f(self.this(), p, name.as_ptr(), &mut out) {
            S_OK => Ok(out),
            hr => Err(hr),
        }
    }
    fn close_container(&self, c: Handle) {
        let f: extern "system" fn(*mut c_void, Handle) = unsafe { self.0.method(0x88) };
        f(self.this(), c)
    }
    /// Writes `blobs` in one update and submits it; the first failure.
    fn update(&self, c: Handle, blobs: &[(&CStr, &[u8])]) -> Hresult {
        let mut u = std::ptr::null_mut();
        let create: extern "system" fn(*mut c_void, Handle, *const c_char, *mut Handle) -> Hresult =
            unsafe { self.0.method(0xb8) };
        let hr = create(self.this(), c, c"Conformance".as_ptr(), &mut u);
        if hr != S_OK {
            return hr;
        }
        let write: extern "system" fn(
            *mut c_void,
            Handle,
            *const c_char,
            *const u8,
            usize,
        ) -> Hresult = unsafe { self.0.method(0xc8) };
        let close: extern "system" fn(*mut c_void, Handle) = unsafe { self.0.method(0xc0) };
        for (name, data) in blobs {
            let hr = write(self.this(), u, name.as_ptr(), data.as_ptr(), data.len());
            if hr != S_OK {
                close(self.this(), u);
                return hr;
            }
        }
        let mut block = Block::on_process_queue();
        let submit: extern "system" fn(*mut c_void, Handle, *mut Block) -> Hresult =
            unsafe { self.0.method(0xe0) };
        let result: extern "system" fn(*mut c_void, *mut Block) -> Hresult =
            unsafe { self.0.method(0xe8) };
        let mut hr = submit(self.this(), u, &mut block);
        if hr == S_OK {
            wait(&mut block);
            hr = result(self.this(), &mut block);
        }
        close(self.this(), u);
        hr
    }
    /// Reads `names`; each blob's bytes, or the failure.
    fn read(&self, c: Handle, names: &[&CStr]) -> Result<Vec<Vec<u8>>, Hresult> {
        let pointers: Vec<*const c_char> = names.iter().map(|n| n.as_ptr()).collect();
        let mut count = names.len() as u32;
        let mut buffer = vec![0u64; (1 << 20) / 8];
        let f: extern "system" fn(
            *mut c_void,
            Handle,
            *const *const c_char,
            *mut u32,
            usize,
            *mut u8,
        ) -> Hresult = unsafe { self.0.method(0xa0) };
        let hr = f(
            self.this(),
            c,
            pointers.as_ptr(),
            &mut count,
            buffer.len() * 8,
            buffer.as_mut_ptr().cast(),
        );
        if hr != S_OK {
            return Err(hr);
        }
        let blobs = buffer.as_ptr().cast::<Blob>();
        Ok((0..count as usize)
            .map(|i| {
                // SAFETY: the call wrote `count` blobs whose data lies in the buffer.
                unsafe {
                    let b = &*blobs.add(i);
                    std::slice::from_raw_parts(b.data, b.size as usize).to_vec()
                }
            })
            .collect())
    }
}

pub fn run(t: &mut Tap) {
    let saves = match area(PROVIDER, PROVIDER) {
        Ok(object) => Saves(object),
        Err(hr) => {
            t.check(false, "xgamesave: the game-save area is served", || {
                format!("QueryApiImpl {hr:#010x}")
            });
            return;
        }
    };
    let provider = match saves.provider() {
        Ok(p) => p,
        Err(hr) => {
            t.check(
                false,
                "xgamesave: XGameSaveInitializeProviderAsync succeeds",
                || format!("{hr:#010x}"),
            );
            return;
        }
    };
    t.check(
        true,
        "xgamesave: XGameSaveInitializeProviderAsync succeeds",
        String::new,
    );
    t.eq(
        saves.container(provider, c"bad name*").err(),
        Some(E_GS_INVALID_CONTAINER_NAME),
        "xgamesave: a container name with forbidden characters is E_GS_INVALID_CONTAINER_NAME",
    );
    let container = match saves.container(provider, c"conformance/slot_1") {
        Ok(c) => c,
        Err(hr) => {
            t.check(
                false,
                "xgamesave: a container with letters, digits, _ and / opens",
                || format!("{hr:#010x}"),
            );
            return;
        }
    };
    t.check(
        true,
        "xgamesave: a container with letters, digits, _ and / opens",
        String::new,
    );

    let a: Vec<u8> = (0..1024u32).map(|i| i as u8).collect();
    let b: Vec<u8> = (0..2048u32).map(|i| (i * 7) as u8).collect();
    t.eq(
        saves.update(container, &[(c"a", &a), (c"b", &b)]),
        S_OK,
        "xgamesave: an update of two blobs submits",
    );
    t.eq(
        saves.read(container, &[c"a", c"b"]).ok(),
        Some(vec![a.clone(), b.clone()]),
        "xgamesave: both blobs read back",
    );

    // Over 16 MB in one update: refused, and nothing of it lands.
    let big = vec![0x5au8; 9 << 20];
    let hr = saves.update(
        container,
        &[(c"a", &[1, 2, 3]), (c"big1", &big), (c"big2", &big)],
    );
    t.eq(
        hr,
        E_GS_UPDATE_TOO_BIG,
        "xgamesave: an update over 16 MB is E_GS_UPDATE_TOO_BIG",
    );
    t.eq(
        saves.read(container, &[c"a", c"b"]).ok(),
        Some(vec![a.clone(), b]),
        "xgamesave: a refused update changes nothing (all or nothing)",
    );
    t.check(
        saves.read(container, &[c"big1"]).is_err(),
        "xgamesave: ... and none of its blobs exist",
        || "big1 exists".into(),
    );
    // Exactly 16 MB is allowed.
    let limit = vec![0xa5u8; 16 << 20];
    t.eq(
        saves.update(container, &[(c"limit", &limit)]),
        S_OK,
        "xgamesave: an update of exactly 16 MB submits",
    );
    saves.close_container(container);

    // A second container handle sees what the first one committed.
    match saves.container(provider, c"conformance/slot_1") {
        Ok(again) => {
            t.eq(
                saves.read(again, &[c"a"]).ok(),
                Some(vec![a]),
                "xgamesave: a reopened container has its committed blobs",
            );
            saves.close_container(again);
        }
        Err(hr) => t.check(
            false,
            "xgamesave: a reopened container has its committed blobs",
            || format!("{hr:#010x}"),
        ),
    }
    saves.close_provider(provider);
}
