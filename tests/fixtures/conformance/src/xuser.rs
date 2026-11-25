//! The signed-in user.
//!
//! Public reference: reference/system/xuser: XUserAddOptions (None 0,
//! AddDefaultUserSilently 1); a known default user added silently again "will
//! return that same user"; XUserCompare "Returns 0 if the handles are for the
//! same user"; XUserGetGamertag components (Classic 0, Modern 1,
//! ModernSuffix 2, UniqueModern 3) with sizes including the NUL of 16, 97,
//! 15 and 101 bytes, E_INSUFFICIENT_BUFFER for a short buffer, E_INVALIDARG
//! for an invalid component; XUserFindUserByLocalId: no such user is
//! HRESULT_FROM_WIN32(ERROR_NOT_FOUND); XUserState (SignedIn 0); XUserAgeGroup
//! (Unknown 0 to Adult 3).
//!
//! The area's ids and method slots are VIctory's (`crates/victory-hle/src/
//! modules/xuser.rs`).

use std::ffi::c_void;

use crate::rt::{Guid, Hresult, Object, S_OK, area};
use crate::tap::Tap;
use crate::xasync::{Block, wait};

const PROVIDER: Guid = Guid(
    0x01ac_d177,
    0x91f9,
    0x4763,
    [0xa3, 0x8e, 0xcc, 0xbb, 0x55, 0xce, 0x32, 0xe0],
);
const IID: Guid = Guid(
    0xeb9b_f948,
    0x18dc,
    0x4d82,
    [0xbb, 0xcc, 0x40, 0xe0, 0xa8, 0x09, 0xc4, 0xc0],
);
const GAMERTAG_IID: Guid = Guid(
    0xcef4_fac0,
    0x7676,
    0x4a94,
    [0xa1, 0x19, 0x4c, 0x43, 0xf9, 0xeb, 0x5b, 0x74],
);

const ADD_DEFAULT_USER_SILENTLY: u32 = 1;
/// E_INSUFFICIENT_BUFFER, HRESULT_FROM_WIN32(ERROR_INSUFFICIENT_BUFFER).
const E_INSUFFICIENT_BUFFER: Hresult = 0x8007_007A_u32 as i32;
const E_INVALIDARG: Hresult = 0x8007_0057_u32 as i32;
/// HRESULT_FROM_WIN32(ERROR_NOT_FOUND).
const NOT_FOUND: Hresult = 0x8007_0490_u32 as i32;
/// Gamertag components and their sizes including the NUL.
const COMPONENTS: [(u32, usize, &str); 4] = [
    (0, 16, "Classic"),
    (1, 97, "Modern"),
    (2, 15, "ModernSuffix"),
    (3, 101, "UniqueModern"),
];

type User = *mut c_void;

struct Users(Object);

impl Users {
    fn this(&self) -> *mut c_void {
        self.0.this()
    }
    fn add(&self, options: u32) -> Result<User, Hresult> {
        let mut block = Block::on_process_queue();
        // SAFETY: (all methods) the slot's signature in VIctory's table.
        let begin: extern "system" fn(*mut c_void, u32, *mut Block) -> Hresult =
            unsafe { self.0.method(0x38) };
        let hr = begin(self.this(), options, &mut block);
        if hr != S_OK {
            return Err(hr);
        }
        wait(&mut block);
        let mut user = std::ptr::null_mut();
        let result: extern "system" fn(*mut c_void, *mut Block, *mut User) -> Hresult =
            unsafe { self.0.method(0x40) };
        match result(self.this(), &mut block, &mut user) {
            S_OK => Ok(user),
            hr => Err(hr),
        }
    }
    fn duplicate(&self, user: User) -> Result<User, Hresult> {
        let mut out = std::ptr::null_mut();
        let f: extern "system" fn(*mut c_void, User, *mut User) -> Hresult =
            unsafe { self.0.method(0x18) };
        match f(self.this(), user, &mut out) {
            S_OK => Ok(out),
            hr => Err(hr),
        }
    }
    fn close(&self, user: User) {
        let f: extern "system" fn(*mut c_void, User) = unsafe { self.0.method(0x20) };
        f(self.this(), user)
    }
    fn compare(&self, a: User, b: User) -> i32 {
        let f: extern "system" fn(*mut c_void, User, User) -> i32 = unsafe { self.0.method(0x28) };
        f(self.this(), a, b)
    }
    fn u64_of(&self, offset: usize, user: User) -> (Hresult, u64) {
        let mut out = 0;
        let f: extern "system" fn(*mut c_void, User, *mut u64) -> Hresult =
            unsafe { self.0.method(offset) };
        (f(self.this(), user, &mut out), out)
    }
    fn u32_of(&self, offset: usize, user: User) -> (Hresult, u32) {
        let mut out = 0;
        let f: extern "system" fn(*mut c_void, User, *mut u32) -> Hresult =
            unsafe { self.0.method(offset) };
        (f(self.this(), user, &mut out), out)
    }
    fn find(&self, offset: usize, id: u64) -> Result<User, Hresult> {
        let mut out = std::ptr::null_mut();
        let f: extern "system" fn(*mut c_void, u64, *mut User) -> Hresult =
            unsafe { self.0.method(offset) };
        match f(self.this(), id, &mut out) {
            S_OK => Ok(out),
            hr => Err(hr),
        }
    }
    fn privilege(&self, user: User, privilege: u32) -> (Hresult, bool, u32) {
        let (mut has, mut reason) = (false, 0xffff_ffff);
        let f: extern "system" fn(*mut c_void, User, u32, u32, *mut bool, *mut u32) -> Hresult =
            unsafe { self.0.method(0xa0) };
        let hr = f(self.this(), user, 0, privilege, &mut has, &mut reason);
        (hr, has, reason)
    }
}

/// XUserGetGamertag through the gamertag interface.
fn gamertag(o: &Object, user: User, component: u32, size: usize) -> (Hresult, String, usize) {
    let mut buffer = vec![0u8; size];
    let mut used = 0;
    // SAFETY: the slot's signature in VIctory's table.
    let f: extern "system" fn(*mut c_void, User, u32, usize, *mut u8, *mut usize) -> Hresult =
        unsafe { o.method(0x18) };
    let hr = f(
        o.this(),
        user,
        component,
        size,
        buffer.as_mut_ptr(),
        &mut used,
    );
    let end = buffer.iter().position(|&b| b == 0).unwrap_or(0);
    (
        hr,
        String::from_utf8_lossy(&buffer[..end]).into_owned(),
        used,
    )
}

pub fn run(t: &mut Tap) {
    let users = match area(PROVIDER, IID) {
        Ok(o) => Users(o),
        Err(hr) => {
            t.check(false, "xuser: the user area is served", || {
                format!("QueryApiImpl {hr:#010x}")
            });
            return;
        }
    };
    let first = match users.add(ADD_DEFAULT_USER_SILENTLY) {
        Ok(u) => u,
        Err(hr) => {
            t.check(
                false,
                "xuser: XUserAddAsync adds the default user silently",
                || format!("{hr:#010x}"),
            );
            return;
        }
    };
    t.check(
        !first.is_null(),
        "xuser: XUserAddAsync adds the default user silently",
        || "null".into(),
    );
    let second = users.add(ADD_DEFAULT_USER_SILENTLY);
    t.check(
        second.is_ok_and(|s| users.compare(first, s) == 0),
        "xuser: adding the default user silently again returns the same user",
        || format!("{second:?}"),
    );
    if let Ok(s) = second {
        users.close(s);
    }
    match users.duplicate(first) {
        Ok(d) => {
            t.eq(
                users.compare(first, d),
                0,
                "xuser: XUserCompare is 0 for a duplicated handle",
            );
            users.close(d);
        }
        Err(hr) => t.check(false, "xuser: XUserDuplicateHandle succeeds", || {
            format!("{hr:#010x}")
        }),
    }

    let (hr, local) = users.u64_of(0x48, first);
    t.eq(hr, S_OK, "xuser: XUserGetLocalId succeeds");
    match users.find(0x50, local) {
        Ok(found) => {
            t.eq(
                users.compare(first, found),
                0,
                "xuser: XUserFindUserByLocalId finds the user by its local id",
            );
            users.close(found);
        }
        Err(hr) => t.check(
            false,
            "xuser: XUserFindUserByLocalId finds the user by its local id",
            || format!("{hr:#010x}"),
        ),
    }
    t.eq(
        users.find(0x50, local ^ 0x5a5a_0000).err(),
        Some(NOT_FOUND),
        "xuser: XUserFindUserByLocalId of no user is HRESULT_FROM_WIN32(ERROR_NOT_FOUND)",
    );
    let (hr, xuid) = users.u64_of(0x58, first);
    t.check(
        hr == S_OK && xuid != 0,
        "xuser: XUserGetId gives a user id",
        || format!("{hr:#010x} {xuid:#x}"),
    );
    match users.find(0x60, xuid) {
        Ok(found) => {
            t.eq(
                users.compare(first, found),
                0,
                "xuser: XUserFindUserById finds the user by its id",
            );
            users.close(found);
        }
        Err(hr) => t.check(
            false,
            "xuser: XUserFindUserById finds the user by its id",
            || format!("{hr:#010x}"),
        ),
    }
    t.eq(
        users.u32_of(0x70, first),
        (S_OK, 0),
        "xuser: the added user's state is SignedIn",
    );
    let (hr, age) = users.u32_of(0x98, first);
    t.check(
        hr == S_OK && age <= 3,
        "xuser: XUserGetAgeGroup is one of Unknown, Child, Teen, Adult",
        || format!("{hr:#010x} {age}"),
    );
    // XUserPrivilege Multiplayer (254). Offline, either
    // answer is allowed; the call must succeed and fill both outputs.
    let (hr, has, reason) = users.privilege(first, 254);
    t.check(
        hr == S_OK && (has || reason != 0xffff_ffff),
        "xuser: XUserCheckPrivilege answers with a privilege or a deny reason",
        || format!("{hr:#010x} {has} {reason:#x}"),
    );

    match area(PROVIDER, GAMERTAG_IID) {
        Ok(o) => {
            for (component, max, name) in COMPONENTS {
                let (hr, text, used) = gamertag(&o, first, component, max);
                t.check(
                    hr == S_OK && used <= max && text.len() < max,
                    &format!("xuser: the {name} gamertag fits its documented {max} bytes"),
                    || format!("{hr:#010x} {text:?} used {used}"),
                );
            }
            let (hr, text, _) = gamertag(&o, first, 1, 97);
            if hr == S_OK && !text.is_empty() {
                let (hr, _, _) = gamertag(&o, first, 1, text.len());
                t.eq(
                    hr,
                    E_INSUFFICIENT_BUFFER,
                    "xuser: a gamertag buffer without room for the NUL is E_INSUFFICIENT_BUFFER",
                );
            }
            let (hr, _, _) = gamertag(&o, first, 7, 128);
            t.eq(
                hr,
                E_INVALIDARG,
                "xuser: an invalid gamertag component is E_INVALIDARG",
            );
        }
        Err(hr) => t.check(false, "xuser: the gamertag interface is served", || {
            format!("QueryApiImpl {hr:#010x}")
        }),
    }
    users.close(first);
}
