//! The title's screenshots: the frame a present showed, carried back from
//! the back buffer as 8-bit BGRA or RGBA rows on request (`display` slot
//! 0x28) and saved as a full-size PNG. The console saves screenshots on
//! its own storage; here the file goes to the capture folder
//! (`VICTORY_CAPTURE`) or `./screenshots`, and the path lands in the log.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;

/// A presented frame, read back as 8-bit rows (`bgra`: byte order).
#[derive(Clone)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    /// The byte distance between rows, which the GPU rounds up.
    pub pitch: usize,
    pub bgra: bool,
    pub rows: Vec<u8>,
}

/// How long [`take`] waits for the next present to carry a frame back.
const WAIT: Duration = Duration::from_secs(2);

static PENDING: AtomicBool = AtomicBool::new(false);
/// The host presented something at least once: screenshots wait for a
/// frame only then, else there is nothing to ask the GPU to read back.
static PRESENTED: AtomicBool = AtomicBool::new(false);
static LAST: Mutex<Option<Frame>> = Mutex::new(None);

/// A screenshot was asked for: the next present carries it. An older
/// frame must not pass for it.
pub(crate) fn request() {
    *LAST.lock().unwrap_or_else(|e| e.into_inner()) = None;
    PENDING.store(true, Relaxed);
}

/// Whether a screenshot is still due.
pub(crate) fn pending() -> bool {
    PENDING.load(Relaxed)
}

/// The host showed a frame: present cycles exist.
pub(crate) fn presenter_seen() {
    PRESENTED.store(true, Relaxed);
}

/// Whether the host has presented (screenshots wait for a frame only
/// then).
pub fn presenting() -> bool {
    PRESENTED.load(Relaxed)
}

/// The frame of the last screenshot hook, if there was one since the
/// current request.
pub fn last_frame() -> Option<Frame> {
    LAST.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// `request` was answered with the frame the hook read back at present
/// time (every submission is waited for, so the copy has run): keep it
/// for [`take`].
pub(crate) fn fill(buffer: &ID3D12Resource, footprint: &D3D12_PLACED_SUBRESOURCE_FOOTPRINT) {
    PENDING.store(false, Relaxed);
    let f = footprint.Footprint;
    let bgra = match f.Format {
        DXGI_FORMAT_R8G8B8A8_UNORM | DXGI_FORMAT_R8G8B8A8_UNORM_SRGB => false,
        DXGI_FORMAT_B8G8R8A8_UNORM | DXGI_FORMAT_B8G8R8A8_UNORM_SRGB => true,
        other => {
            tracing::warn!(format = other.0, "screenshot not taken: not 8-bit color");
            return;
        }
    };
    let mut data = std::ptr::null_mut();
    // SAFETY: the copy finished (every submission is waited for); the map
    // spans the footprint's rows.
    unsafe {
        if buffer.Map(0, None, Some(&mut data)).is_err() {
            return;
        }
        let len = f.RowPitch as usize * f.Height as usize;
        let rows = std::slice::from_raw_parts(data.cast::<u8>(), len).to_vec();
        buffer.Unmap(0, None);
        *LAST.lock().unwrap_or_else(|e| e.into_inner()) = Some(Frame {
            width: f.Width,
            height: f.Height,
            pitch: f.RowPitch as usize,
            bgra,
            rows,
        });
    }
}

/// A screenshot, as the title's capture asks: the next present carries the
/// frame back. `None` when the host has not presented, or a present did
/// not land within [`WAIT`].
pub fn take() -> Option<PathBuf> {
    if !presenting() {
        tracing::info!("screenshot not taken: the host has not presented");
        return None;
    }
    request();
    let deadline = Instant::now() + WAIT;
    loop {
        if let Some(frame) = last_frame() {
            return save(&frame);
        }
        if Instant::now() >= deadline {
            tracing::info!("screenshot not taken: no frame arrived in time");
            return None;
        }
        std::thread::sleep(Duration::from_millis(16));
    }
}

/// Saves `frame` at full size; the path is the answer to report.
fn save(frame: &Frame) -> Option<PathBuf> {
    let folder = std::env::var_os("VICTORY_CAPTURE")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default().join("screenshots"));
    let _ = std::fs::create_dir_all(&folder);
    let at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default();
    let path = folder.join(format!("screenshot-{at}.png"));
    let png = png(frame.width, frame.height, frame.pitch, frame.bgra, &frame.rows);
    match std::fs::write(&path, png) {
        Ok(()) => {
            tracing::info!(path = %path.display(), "screenshot saved");
            Some(path)
        }
        Err(e) => {
            tracing::warn!(error = %e, path = %path.display(), "screenshot not saved");
            None
        }
    }
}

/// A full-size PNG (8-bit RGB) of `rows`, drawn from a present.
pub fn png(width: u32, height: u32, pitch: usize, bgra: bool, rows: &[u8]) -> Vec<u8> {
    let (w, h) = (width.max(1) as usize, height.max(1) as usize);
    let mut raw = Vec::with_capacity((w * 3 + 1) * h);
    for y in 0..h {
        raw.push(0); // filter: none
        let row = &rows[y * pitch..];
        for x in 0..w {
            let p = &row[x * 4..][..3];
            if bgra {
                raw.extend([p[2], p[1], p[0]]);
            } else {
                raw.extend_from_slice(p);
            }
        }
    }
    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut header = Vec::new();
    header.extend((w as u32).to_be_bytes());
    header.extend((h as u32).to_be_bytes());
    header.extend([8, 2, 0, 0, 0]); // 8-bit RGB
    chunk(&mut out, b"IHDR", &header);
    chunk(&mut out, b"IDAT", &miniz_oxide::deflate::compress_to_vec_zlib(&raw, 3));
    chunk(&mut out, b"IEND", &[]);
    out
}

pub(crate) fn chunk(png: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    png.extend((data.len() as u32).to_be_bytes());
    let start = png.len();
    png.extend(kind);
    png.extend(data);
    let crc = crc32(&png[start..]);
    png.extend(crc.to_be_bytes());
}

pub(crate) fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &b in bytes {
        crc ^= u32::from(b);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb8_8320 & (crc & 1).wrapping_neg());
        }
    }
    !crc
}

/// Records a copy of `texture` (in the copy-source state) into a
/// readback buffer, so the frame survives to the CPU.
pub(crate) fn read_back(
    device: &ID3D12Device,
    list: &ID3D12GraphicsCommandList,
    texture: &ID3D12Resource,
) -> Option<(ID3D12Resource, D3D12_PLACED_SUBRESOURCE_FOOTPRINT)> {
    let mut footprint = D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default();
    let mut total = 0;
    // SAFETY: a live texture; outputs for one subresource.
    unsafe {
        let desc = texture.GetDesc();
        device.GetCopyableFootprints(
            &desc,
            0,
            1,
            0,
            Some(&mut footprint),
            None,
            None,
            Some(&mut total),
        );
    }
    let heap = D3D12_HEAP_PROPERTIES {
        Type: D3D12_HEAP_TYPE_READBACK,
        ..Default::default()
    };
    let desc = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
        Width: total,
        Height: 1,
        DepthOrArraySize: 1,
        MipLevels: 1,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
        ..Default::default()
    };
    let mut buffer: Option<ID3D12Resource> = None;
    // SAFETY: plain creation; an open list; locations naming live resources.
    unsafe {
        device
            .CreateCommittedResource(
                &heap,
                D3D12_HEAP_FLAG_NONE,
                &desc,
                D3D12_RESOURCE_STATE_COPY_DEST,
                None,
                &mut buffer,
            )
            .ok()?;
        let buffer = buffer?;
        let to = D3D12_TEXTURE_COPY_LOCATION {
            pResource: std::mem::transmute_copy(&buffer),
            Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                PlacedFootprint: footprint,
            },
        };
        let from = D3D12_TEXTURE_COPY_LOCATION {
            pResource: std::mem::transmute_copy(texture),
            Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                SubresourceIndex: 0,
            },
        };
        list.CopyTextureRegion(&to, 0, 0, 0, &from, None);
        Some((buffer, footprint))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A frame taken by a present comes back unchanged (byte order out).
    #[test]
    fn a_png_carries_the_frame() {
            // Two rows of a 2-wide BGRA frame, pitch 12 (8 used, 4 padded).
            let frame = Frame {
                width: 2,
                height: 2,
                pitch: 12,
                bgra: true,
                rows: vec![
                    0x11, 0x22, 0x33, 0xff, 0, 0, 0, 0, 0, 0, 0, 0, // row 0
                    0x44, 0x55, 0x66, 0xff, 0, 0, 0, 0, 0, 0, 0, 0, // row 1
                ],
            };
        let png = png(frame.width, frame.height, frame.pitch, frame.bgra, &frame.rows);
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
        // IHDR: length 8..12, kind 12..16, then width and height.
        assert_eq!(u32::from_be_bytes(png[8..12].try_into().unwrap()), 13);
        assert_eq!(&png[12..16], b"IHDR");
        assert_eq!(u32::from_be_bytes(png[16..20].try_into().unwrap()), 2);
        assert_eq!(u32::from_be_bytes(png[20..24].try_into().unwrap()), 2);
        // IDAT: length 33..37, kind 37..41, the zlib stream 41.., then IEND.
        assert_eq!(&png[37..41], b"IDAT");
        let len = u32::from_be_bytes(png[33..37].try_into().unwrap()) as usize;
        let zlib = miniz_oxide::inflate::decompress_to_vec_zlib(&png[41..41 + len])
            .expect("the IDAT zlib stream");
        // A filter byte per row, then the pixels as RGB.
        assert_eq!(
            zlib,
            [
                0, 0x33, 0x22, 0x11, 0, 0, 0, // row 0
                0, 0x66, 0x55, 0x44, 0, 0, 0, // row 1
            ]
        );
    }

    /// A request does not pass an older frame for the new one.
    #[test]
    fn a_request_clears_the_frame_before_it() {
        let frame = Frame {
            width: 1,
            height: 1,
            pitch: 4,
            bgra: false,
            rows: vec![0; 4],
        };
        *LAST.lock().unwrap() = Some(frame);
        assert!(last_frame().is_some());
        request();
        assert!(pending());
        assert!(last_frame().is_none());
    }
}
