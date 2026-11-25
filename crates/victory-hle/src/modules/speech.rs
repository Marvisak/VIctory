//! Accessibility: speech synthesis for the title's narration. Streams are
//! rendered by the host's SAPI voice when there is one, and every stream is
//! a valid WAVE file, so narration is silent exactly when the host has no
//! voice (or no SAPI) to speak with.
// The title hands us the words (slot 0x68) or SSML (slot 0xa8) and plays the
// stream back. SAPI through COM renders those into a 16 kHz mono 16-bit WAV;
// without a voice the stream is the same silence as before.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::ffi::{c_char, c_void};
use std::sync::Mutex;

use super::xgameruntime::{
    Guid, HRESULT, Interface, Methods, Object, S_OK, add_ref, missing, put, text,
};

/// Closed captions, high contrast, speech to text and synthesis.
pub const PROVIDER: Guid = Guid::new(
    0x3e24_1977,
    0x6237,
    0x41e9,
    [0x85, 0x59, 0x10, 0x2c, 0x2d, 0x99, 0x83, 0xf1],
);
/// Unchanged since 2020.
pub const IID: Guid = Guid::new(
    0xd722_b373,
    0x8c4d,
    0x4692,
    [0x9e, 0x51, 0xd6, 0xad, 0x9b, 0x37, 0xaa, 0x7d],
);

pub static OBJECT: Object = Object::new("accessibility", &VTABLE);
pub const INTERFACES: &[Interface] = &[(PROVIDER, IID, &OBJECT)];

/// A WAVE file of 16 kHz mono 16-bit PCM with no samples: RIFF size 36,
/// a 16-byte format chunk (PCM, 1 channel, 16,000 Hz, 32,000 bytes/s,
/// block 2, 16 bits), an empty data chunk.
const SILENCE: [u8; 44] =
    *b"RIFF\x24\0\0\0WAVEfmt \x10\0\0\0\x01\0\x01\0\x80\x3e\0\0\0\x7d\0\0\x02\0\x10\0data\0\0\0\0";

/// Synthesizers hold nothing, so their handles are a fixed address and
/// closing them is free.
static SYNTHESIZER: u8 = 0;

/// One WAV file per stream the title created. Streams are handed out as
/// odd pointer values encoding a slot in this registry, so a stream handle
/// from the title is never dereferenced as memory; slots only ever grow.
/// Nothing frees them (close is a no-op, as on the console's side the
/// stream dies with the synthesizer): a session leaks one narration line
/// at a time, bounded by what the title speaks.
static STREAMS: Mutex<Vec<Vec<u8>>> = Mutex::new(Vec::new());

type This = *const Object;

/// No voice is installed, so the callback never runs.
extern "system" fn enumerate_voices(
    _: This,
    _context: *mut c_void,
    _callback: *const c_void,
) -> HRESULT {
    S_OK
}
extern "system" fn create(_: This, out: *mut *const u8) -> HRESULT {
    put(out, &raw const SYNTHESIZER)
}
extern "system" fn close(_: This, _: *const u8) -> HRESULT {
    S_OK
}
extern "system" fn set_default_voice(_: This, _: *const u8) -> HRESULT {
    S_OK
}
extern "system" fn set_custom_voice(_: This, _: *const u8, _voice: *const c_char) -> HRESULT {
    S_OK
}
extern "system" fn create_stream(
    _: This,
    _: *const u8,
    words: *const c_char,
    out: *mut *const u8,
) -> HRESULT {
    let words = text(words).unwrap_or_default();
    tracing::debug!(text = words, "speech");
    put(out, stream_handle(&words))
}
extern "system" fn stream_size(_: This, stream: *const u8, out: *mut usize) -> HRESULT {
    put(out, stream_bytes(stream).map_or(0, |wav| wav.len()))
}
extern "system" fn stream_data(
    _: This,
    stream: *const u8,
    size: usize,
    buffer: *mut u8,
    used: *mut usize,
) -> HRESULT {
    let Some(wav) = stream_bytes(stream) else {
        return super::xasync::E_NOT_SUFFICIENT_BUFFER;
    };
    if buffer.is_null() || size < wav.len() {
        return super::xasync::E_NOT_SUFFICIENT_BUFFER;
    }
    // SAFETY: the buffer holds `size` bytes, checked above.
    unsafe { buffer.copy_from_nonoverlapping(wav.as_ptr(), wav.len()) };
    if !used.is_null() {
        // SAFETY: checked non-null.
        unsafe { used.write(wav.len()) };
    }
    S_OK
}

/// Registers a stream's audio and returns its handle: slot `n` sits behind
/// the odd pointer value `(n << 1) | 1`.
fn stream_handle(words: &str) -> *const u8 {
    let audio = stream_audio(words);
    let mut streams = STREAMS.lock().unwrap();
    let slot = streams.len();
    streams.push(audio);
    ((slot << 1) | 1) as *const u8
}

/// The audible form of a line: the host's voice, or the same silence as
/// before when there is no voice (no SAPI, no Windows, or synthesis
/// failed for this line).
fn stream_audio(words: &str) -> Vec<u8> {
    #[cfg(windows)]
    {
        sapi::synthesize(words).unwrap_or_else(|| SILENCE.to_vec())
    }
    #[cfg(not(windows))]
    {
        let _ = words;
        SILENCE.to_vec()
    }
}

/// A copy of a registered stream's WAV bytes, or nothing when the title
/// passes back anything it was not handed.
fn stream_bytes(stream: *const u8) -> Option<Vec<u8>> {
    if (stream as usize) & 1 == 0 {
        return None;
    }
    let streams = STREAMS.lock().unwrap();
    streams.get((stream as usize) >> 1).cloned()
}

/// The SSML the title sends and the XML SAPI understands overlap, but not
/// fully: this is the spoken text alone, tags and entities out.
#[cfg(any(windows, test))]
fn spoken(words: &str) -> String {
    const ENTITIES: [(&str, &str); 5] =
        [("amp", "&"), ("lt", "<"), ("gt", ">"), ("quot", "\""), ("apos", "'")];
    let mut plain = String::new();
    let mut rest = words;
    while !rest.is_empty() {
        if let Some(tag) = rest.strip_prefix('<') {
            // A tag runs to its closing '>'; without one it is markup to
            // the end.
            rest = match tag.find('>') {
                Some(i) => &tag[i + 1..],
                None => "",
            };
        } else if let Some(after) = rest.strip_prefix('&') {
            let entity = after.split([';', '<', '&']).next().unwrap_or("");
            let (word, literal) = ENTITIES
                .iter()
                .find(|(name, _)| *name == entity)
                .map_or(("&", 0), |(name, value)| (*value, name.len()));
            plain.push_str(word);
            let taken = if literal > 0 && after.as_bytes().get(literal) == Some(&b';') {
                literal + 1
            } else {
                0
            };
            rest = &after[taken..];
        } else {
            // Plain text up to the next tag or entity.
            rest = match rest.find(['<', '&']) {
                Some(i) => {
                    plain.push_str(&rest[..i]);
                    &rest[i..]
                }
                None => {
                    plain.push_str(rest);
                    ""
                }
            };
        }
    }
    plain
}

/// The 44-byte WAVE header SILENCE carries, around 16 kHz mono 16-bit
/// `pcm`.
#[cfg(any(windows, test))]
fn wave(pcm: &[u8]) -> Vec<u8> {
    let len = u32::try_from(pcm.len()).unwrap_or(u32::MAX);
    let write = |out: &mut Vec<u8>, value: u32| out.extend_from_slice(&value.to_le_bytes());
    let mut wav = Vec::with_capacity(44 + pcm.len());
    wav.extend_from_slice(b"RIFF");
    write(&mut wav, 36 + len);
    wav.extend_from_slice(b"WAVEfmt ");
    write(&mut wav, 16);
    wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
    wav.extend_from_slice(&1u16.to_le_bytes()); // mono
    write(&mut wav, 16_000);
    write(&mut wav, 32_000);
    wav.extend_from_slice(&2u16.to_le_bytes());
    wav.extend_from_slice(&16u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    write(&mut wav, len);
    wav.extend_from_slice(pcm);
    wav
}

#[cfg(windows)]
mod sapi {
    use windows::core::{GUID, HSTRING, IUnknown, Interface, PCWSTR};
    use windows::Win32::Foundation::{HGLOBAL, RPC_E_CHANGED_MODE};
    use windows::Win32::Media::Audio::WAVEFORMATEX;
    use windows::Win32::Media::Speech::{
        ISpStream, ISpVoice, SPF_IS_NOT_XML, SPF_IS_XML, SpStream, SpVoice,
    };
    use windows::Win32::System::Com::{
        CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
        CoUninitialize,
    };
    use windows::Win32::System::Com::StructuredStorage::{
        CreateStreamOnHGlobal, GetHGlobalFromStream,
    };
    use windows::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};

    /// The stream format id for WAVEFORMATEX payloads (SPDFID_WaveFormatEx,
    /// sapi.h; not exported by the `windows` crate).
    const SPDFID_WAVEFORMATEX: GUID =
        GUID::from_u128(0xc31a_dbae_527f_4ff5_a230_f62b_b61f_f70c);

    /// The host's voice speaking `words`, as a complete WAV file, or
    /// nothing when there is no voice or SAPI refuses the line.
    pub fn synthesize(words: &str) -> Option<Vec<u8>> {
        if words.trim().is_empty() {
            return None;
        }
        // Apartments: the caller thread may already be COM-initialized in
        // another mode, which SAPI tolerates; anything else it cannot start
        // from means no synthesis.
        // SAFETY: no outstanding borrowed COM pointers.
        let apartment = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
        if apartment.is_err() && apartment != RPC_E_CHANGED_MODE {
            return None;
        }
        let spoken = speak_to_memory(words);
        if apartment.is_ok() {
            // SAFETY: pairing the successful CoInitializeEx above.
            unsafe { CoUninitialize() };
        }
        spoken
    }

    /// Renders `words` into an HGlobal-backed stream and copies the PCM
    /// back out, or nothing on any COM failure.
    fn speak_to_memory(words: &str) -> Option<Vec<u8>> {
        // SAFETY: all calls take no borrowed pointers out of scope; every
        // Result is checked before the next uses its predecessor.
        unsafe {
            let voice: ISpVoice =
                CoCreateInstance(&SpVoice, None, CLSCTX_INPROC_SERVER).ok()?;
            let sp_stream: ISpStream =
                CoCreateInstance(&SpStream, None, CLSCTX_INPROC_SERVER).ok()?;
            // An HGlobal of null allocates: SAPI writes into it and the
            // stream's own release (f_delete_on_release) frees it.
            let stream = CreateStreamOnHGlobal(HGLOBAL::default(), true).ok()?;
            let format = WAVEFORMATEX {
                wFormatTag: 1, // PCM
                nChannels: 1,
                nSamplesPerSec: 16_000,
                nAvgBytesPerSec: 32_000,
                nBlockAlign: 2,
                wBitsPerSample: 16,
                cbSize: 0,
            };
            sp_stream
                .SetBaseStream(&stream, &SPDFID_WAVEFORMATEX, &format)
                .ok()?;
            let output: IUnknown = sp_stream.cast().ok()?;
            voice.SetOutput(&output, false).ok()?;
            let wide = HSTRING::from(words);
            // SSML first (the title sends both); a voice that refuses it
            // gets the plain text instead, tags and entities out.
            if voice
                .Speak(PCWSTR(wide.as_ptr()), SPF_IS_XML.0 as u32, None)
                .is_err()
            {
                let plain = HSTRING::from(super::spoken(words));
                voice
                    .Speak(PCWSTR(plain.as_ptr()), SPF_IS_NOT_XML.0 as u32, None)
                    .ok()?;
            }
            let memory = GetHGlobalFromStream(&stream).ok()?;
            let source = GlobalLock(memory);
            if source.is_null() {
                return None;
            }
            let pcm = std::slice::from_raw_parts(source.cast::<u8>(), GlobalSize(memory)).to_vec();
            // SAFETY: locked just above; a return of false only means the
            // memory stays locked until the stream releases it, which is
            // this function's exit on the next line anyway.
            let _ = GlobalUnlock(memory);
            // The base stream receives bare PCM in the format set above;
            // the WAV header goes on here so the title plays it as-is.
            Some(super::wave(&pcm))
        }
    }
}

static VTABLE: Methods<22> = Methods([
    missing::<0> as *const (),
    add_ref as *const (),
    add_ref as *const (),
    missing::<3> as *const (),      // 0x18 closed captions
    missing::<4> as *const (),      // 0x20
    missing::<5> as *const (),      // 0x28 high contrast
    missing::<6> as *const (),      // 0x30 speech to text
    missing::<7> as *const (),      // 0x38
    enumerate_voices as *const (),  // 0x40
    create as *const (),            // 0x48
    close as *const (),             // 0x50
    set_default_voice as *const (), // 0x58
    set_custom_voice as *const (),  // 0x60
    create_stream as *const (),     // 0x68 from text
    close as *const (),             // 0x70 a stream
    stream_size as *const (),       // 0x78
    stream_data as *const (),       // 0x80
    missing::<17> as *const (),     // 0x88 speech to text
    missing::<18> as *const (),     // 0x90
    missing::<19> as *const (),     // 0x98
    missing::<20> as *const (),     // 0xa0
    create_stream as *const (),     // 0xa8 from SSML
]);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modules::xgameruntime::method;

    #[test]
    fn spoken_drops_tags_and_entities() {
        assert_eq!(spoken("plain words"), "plain words");
        assert_eq!(
            spoken("<voice name='en-US'>Hi &amp; bye</voice>"),
            "Hi & bye"
        );
        assert_eq!(
            spoken("a &lt; b &gt; c &quot;d&quot; &apos;e&apos; &unknown;"),
            "a < b > c \"d\" 'e' &unknown;"
        );
    }

    #[test]
    fn wave_files_carry_their_format() {
        let wav = wave(&[0x11, 0x22]);
        assert_eq!(
            (&wav[..4], &wav[8..16], &wav[36..40]),
            (&b"RIFF"[..], &b"WAVEfmt "[..], &b"data"[..])
        );
        assert_eq!(u32::from_le_bytes(wav[4..8].try_into().unwrap()), 38);
        assert_eq!(u32::from_le_bytes(wav[24..28].try_into().unwrap()), 16_000);
        assert_eq!(u32::from_le_bytes(wav[40..44].try_into().unwrap()), 2);
        assert_eq!(wav[44..], [0x11, 0x22]);
    }

    #[test]
    fn streams_are_valid_wave_files() {
        let this: This = &OBJECT;
        type Create = extern "system" fn(This, *mut *const u8) -> HRESULT;
        type Stream = extern "system" fn(This, *const u8, *const c_char, *mut *const u8) -> HRESULT;
        type Size = extern "system" fn(This, *const u8, *mut usize) -> HRESULT;
        type Data = extern "system" fn(This, *const u8, usize, *mut u8, *mut usize) -> HRESULT;
        let (mut synth, mut stream, mut size) = (std::ptr::null(), std::ptr::null(), 0);
        assert_eq!(method::<Create>(&OBJECT, 0x48)(this, &mut synth), S_OK);
        for words in [c"Hello".as_ptr(), c"<emphasis>Hi</emphasis>".as_ptr()] {
            let say = method::<Stream>(&OBJECT, 0x68);
            assert_eq!(say(this, synth, words, &mut stream), S_OK);
            assert_eq!(method::<Size>(&OBJECT, 0x78)(this, stream, &mut size), S_OK);
            let mut wav = vec![0u8; size];
            let mut used = 0;
            let data = method::<Data>(&OBJECT, 0x80);
            assert_eq!(data(this, stream, size, wav.as_mut_ptr(), &mut used), S_OK);
            assert_eq!(used, size);
            assert!(size >= 44);
            assert_eq!(
                (&wav[..4], &wav[8..16], &wav[36..40]),
                (&b"RIFF"[..], &b"WAVEfmt "[..], &b"data"[..])
            );
            assert_eq!(u32::from_le_bytes(wav[24..28].try_into().unwrap()), 16_000);
            assert_eq!(
                u32::from_le_bytes(wav[40..44].try_into().unwrap()) as usize + 44,
                used
            );
        }
    }

    #[test]
    fn a_stray_handle_never_returns_audio() {
        let this: This = &OBJECT;
        type Size = extern "system" fn(This, *const u8, *mut usize) -> HRESULT;
        type Data = extern "system" fn(This, *const u8, usize, *mut u8, *mut usize) -> HRESULT;
        let mut size = 1;
        let stray = [0xdead_beef as *const u8, std::ptr::null(), std::ptr::dangling()];
        for stream in stray {
            assert_eq!(method::<Size>(&OBJECT, 0x78)(this, stream, &mut size), S_OK);
            assert_eq!(size, 0);
            let mut used = 0;
            assert_eq!(
                method::<Data>(&OBJECT, 0x80)(this, stream, 1_024, [1u8; 1_024].as_mut_ptr(), &mut used),
                super::super::xasync::E_NOT_SUFFICIENT_BUFFER
            );
            assert_eq!(used, 0);
        }
    }
}
