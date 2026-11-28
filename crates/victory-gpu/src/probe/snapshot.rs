//! The probe's snapshot: what is drawn into between one present and the
//! next, read back at that next present.
//!
//! While recording, the hooks keep each target's host texture alive and note
//! which pipeline wrote it last. The snapshot's own lock is never held
//! across another lock or a host call: draws are recorded on several of the
//! title's threads, which hold locks of the layer's own. Reading back is
//! done at the present, outside any recording.

use std::cell::RefCell;
use std::collections::HashMap;
use std::fmt::Write;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex};

use windows::Win32::Graphics::Direct3D12::*;

use super::pixels::{Health, health, kind, pick, preview, texel, texel_size};
use super::{config, coverage, folder, json_str, write_coverage, write_whole};
use crate::resource::HostTexture;

/// At most this many textures are noted.
const MOST: usize = 512;

struct Noted<T> {
    texture: T,
    uav: bool,
    /// The last pipeline that wrote it, and the number of that draw.
    producer: usize,
    draw: u64,
}

/// What the hooks note while a snapshot records.
struct Recorded<T> {
    /// Resources in first-use order.
    order: Vec<usize>,
    noted: HashMap<usize, Noted<T>>,
    ran: u64,
    missed: u64,
}

impl<T> Recorded<T> {
    fn new() -> Self {
        Recorded {
            order: Vec::new(),
            noted: HashMap::new(),
            ran: 0,
            missed: 0,
        }
    }

    /// Whether `resource` is noted already (marked `uav` if so asked) or
    /// there is no room for it.
    fn known(&mut self, resource: usize, uav: bool) -> bool {
        match self.noted.get_mut(&resource) {
            Some(n) => {
                n.uav |= uav;
                true
            }
            None => self.noted.len() >= MOST,
        }
    }

    fn add(&mut self, resource: usize, uav: bool, texture: T) {
        if !self.known(resource, uav) {
            self.order.push(resource);
            self.noted.insert(
                resource,
                Noted {
                    texture,
                    uav,
                    producer: 0,
                    draw: 0,
                },
            );
        }
    }

    /// A draw or dispatch of `pipeline`; one that ran wrote `written`.
    fn drawn(&mut self, pipeline: usize, ran: bool, written: impl IntoIterator<Item = usize>) {
        if !ran {
            self.missed += 1;
            return;
        }
        self.ran += 1;
        for resource in written {
            if let Some(n) = self.noted.get_mut(&resource) {
                (n.producer, n.draw) = (pipeline, self.ran);
            }
        }
    }
}

static RECORDING: AtomicBool = AtomicBool::new(false);
static NOTES: Mutex<Option<Recorded<Arc<HostTexture>>>> = Mutex::new(None);

thread_local! {
    /// Textures this thread bound for unordered access since its last draw
    /// or dispatch: written by the next one.
    static PENDING: RefCell<Vec<usize>> = const { RefCell::new(Vec::new()) };
}

fn notes() -> std::sync::MutexGuard<'static, Option<Recorded<Arc<HostTexture>>>> {
    NOTES.lock().unwrap_or_else(|e| e.into_inner())
}

pub(super) fn recording() -> bool {
    RECORDING.load(Relaxed)
}

pub(super) fn begin() {
    *notes() = Some(Recorded::new());
    RECORDING.store(true, Relaxed);
    tracing::info!("probe: snapshot recording until the next present");
}

/// A render target or depth (`uav` false) or a texture bound for unordered
/// access. Nothing outside a snapshot.
#[inline]
pub(crate) fn note_target(resource: crate::object::This, uav: bool) {
    if recording() && !resource.is_null() {
        note(resource, uav);
    }
}

#[cold]
fn note(resource: crate::object::This, uav: bool) {
    let key = resource as usize;
    if uav {
        PENDING.with_borrow_mut(|p| {
            if p.len() < 64 {
                p.push(key);
            }
        });
    }
    if notes().as_mut().is_none_or(|n| n.known(key, uav)) {
        return;
    }
    // Looked up with the notes' lock let go. Buffers have no host texture.
    let Some(texture) = crate::resource::host_copy(resource) else {
        return;
    };
    if let Some(n) = notes().as_mut() {
        n.add(key, uav, texture);
    }
}

pub(super) fn drawn(pipeline: usize, ran: bool, targets: &[usize], depth: usize) {
    if !recording() {
        return;
    }
    let uavs = PENDING.with_borrow_mut(std::mem::take);
    if let Some(n) = notes().as_mut() {
        n.drawn(
            pipeline,
            ran,
            targets.iter().copied().chain([depth]).chain(uavs),
        );
    }
}

/// Subresource 0 of `texture`, which is in `state`, as RGBA floats, with the
/// format it was decoded as.
fn read(
    host: &crate::host::Host,
    texture: &ID3D12Resource,
    state: u32,
) -> Option<(u32, u32, u32, Vec<[f32; 4]>)> {
    // SAFETY: a plain query of a live texture.
    let desc = unsafe { texture.GetDesc() };
    let list = host.new_list().ok()?;
    let state = D3D12_RESOURCE_STATES(crate::host::legal_state(texture, state) as i32);
    let copy = D3D12_RESOURCE_STATE_COPY_SOURCE;
    // SAFETY: an open list of ours and a live texture, put back in the state
    // it was in.
    let read = unsafe {
        if state != copy {
            list.list
                .ResourceBarrier(&[crate::host::transition(texture, state, copy)]);
        }
        let read = crate::capture::read_back(&host.device, &list.list, texture);
        if state != copy {
            list.list
                .ResourceBarrier(&[crate::host::transition(texture, copy, state)]);
        }
        read
    };
    list.close();
    host.execute(&[&list]);
    let (buffer, footprint) = read?;
    let f = footprint.Footprint;
    // A 24-bit depth plane copies out as 32-bit words.
    let format = if matches!(desc.Format.0, 44..=47) {
        desc.Format.0
    } else {
        f.Format.0
    } as u32;
    let size = texel_size(format)?;
    let (w, h, pitch) = (f.Width as usize, f.Height as usize, f.RowPitch as usize);
    let mut data = std::ptr::null_mut();
    // SAFETY: the copy finished (every submission is waited for).
    unsafe { buffer.Map(0, None, Some(&mut data)) }.ok()?;
    // SAFETY: the mapping spans the footprint's rows; the last may be short
    // of the pitch.
    let bytes =
        unsafe { std::slice::from_raw_parts(data.cast::<u8>(), pitch * (h - 1) + w * size) };
    let mut values = Vec::with_capacity(w * h);
    for y in 0..h {
        for x in 0..w {
            values.push(texel(format, &bytes[y * pitch + x * size..][..size]).unwrap_or_default());
        }
    }
    // SAFETY: mapped above.
    unsafe { buffer.Unmap(0, None) };
    Some((f.Width, f.Height, format, values))
}

/// One target's results.
struct Target {
    index: usize,
    size: (u32, u32),
    format: u32,
    uav: bool,
    producer: String,
    draw: u64,
    health: Health,
    at: Option<((u32, u32), [f32; 4])>,
    png: String,
}

impl Target {
    fn line(&self) -> String {
        let h = &self.health;
        let at = self
            .at
            .map_or_else(String::new, |((x, y), v)| format!("; at {x},{y}: {v:?}"));
        format!(
            "{}: {}x{} format {} {} by {} (draw {}): black {:.2} %, NaN {:.3} %, Inf {:.3} %, magenta {:.3} %, luma mean {:.4}, range {} to {}{at}",
            self.png,
            self.size.0,
            self.size.1,
            self.format,
            if self.uav { "uav" } else { "target" },
            self.producer,
            self.draw,
            h.percent(h.black),
            h.percent(h.nan),
            h.percent(h.inf),
            h.percent(h.magenta),
            h.luma_mean,
            h.low,
            h.high,
        )
    }

    fn json(&self) -> String {
        let h = &self.health;
        let number = |v: f64| {
            if v.is_finite() {
                format!("{v}")
            } else {
                "null".to_owned()
            }
        };
        let at = self.at.map_or_else(
            || "null".to_owned(),
            |((x, y), v)| {
                let rgba = v.map(|c| number(f64::from(c))).join(",");
                format!("{{\"x\":{x},\"y\":{y},\"rgba\":[{rgba}]}}")
            },
        );
        format!(
            "{{\"index\":{},\"width\":{},\"height\":{},\"format\":{},\"uav\":{},\"producer\":{},\"draw\":{},\"black\":{},\"nan\":{},\"inf\":{},\"magenta\":{},\"luma_mean\":{},\"low\":{},\"high\":{},\"at\":{at},\"png\":{}}}",
            self.index,
            self.size.0,
            self.size.1,
            self.format,
            self.uav,
            json_str(&self.producer),
            self.draw,
            h.black,
            h.nan,
            h.inf,
            h.magenta,
            number(h.luma_mean),
            number(f64::from(h.low)),
            number(f64::from(h.high)),
            json_str(&self.png),
        )
    }
}

/// At the present after [`begin`]: the picked targets read back, measured and
/// written to `snap-<n>`, with the coverage so far.
pub(super) fn finish() {
    RECORDING.store(false, Relaxed);
    let Some(recorded) = notes().take() else {
        return;
    };
    let started = std::time::Instant::now();
    static SNAPSHOTS: AtomicU64 = AtomicU64::new(0);
    let n = SNAPSHOTS.fetch_add(1, Relaxed) + 1;
    let Some(host) = crate::host::host() else {
        return;
    };
    let Some(dir) = folder().map(|d| d.join(format!("snap-{n:02}"))) else {
        return;
    };
    if let Err(e) = std::fs::create_dir_all(&dir) {
        tracing::warn!(error = %e, "probe: no folder for the snapshot");
        return;
    }
    // Two-dimensional, one sample.
    let candidates: Vec<(usize, (u32, u32))> = recorded
        .order
        .iter()
        .filter_map(|&resource| {
            // SAFETY: a live texture (the notes hold it).
            let d = unsafe { recorded.noted[&resource].texture.texture.GetDesc() };
            (d.Dimension == D3D12_RESOURCE_DIMENSION_TEXTURE2D && d.SampleDesc.Count == 1)
                .then_some((resource, (d.Width as u32, d.Height)))
        })
        .collect();
    let sizes: Vec<(u32, u32)> = candidates.iter().map(|c| c.1).collect();
    let (chosen, reference) = pick(&sizes, config().max_rt);
    let mut targets = Vec::new();
    for (index, &k) in chosen.iter().enumerate() {
        let noted = &recorded.noted[&candidates[k].0];
        let state = noted.texture.state.load(Relaxed);
        let Some((w, h, format, values)) = read(host, &noted.texture.texture, state) else {
            continue;
        };
        let kind = kind(format);
        let health = health(&values, kind == super::pixels::Kind::Single);
        let at = config().xy.zip(reference).map(|((x, y), (rw, rh))| {
            let scale = |v: u32, to: u32, from: u32| {
                ((u64::from(v) * u64::from(to) / u64::from(from.max(1))) as u32).min(to - 1)
            };
            let (x, y) = (scale(x, w, rw), scale(y, h, rh));
            ((x, y), values[y as usize * w as usize + x as usize])
        });
        let (pw, ph, rows) = preview(&values, (w, h), kind, &health, 960);
        let png = format!("target-{index:02}.png");
        write_whole(&dir.join(&png), &crate::capture::png_rgb(pw, ph, &rows));
        targets.push(Target {
            index,
            size: (w, h),
            format,
            uav: noted.uav,
            producer: coverage::name_of(noted.producer).unwrap_or_else(|| "unknown".to_owned()),
            draw: noted.draw,
            health,
            at,
            png,
        });
    }
    let memory = crate::host::video_memory().map(|m| (m[0].0, m[0].1));
    let mut text = format!(
        "VICTORY_PROBE snapshot {n}\ndraws and dispatches recorded: {} ran, {} did not; textures noted: {}, read back: {}\n",
        recorded.ran,
        recorded.missed,
        recorded.order.len(),
        targets.len()
    );
    if let Some((used, budget)) = memory {
        let _ = writeln!(text, "video memory: {used:.0} of {budget:.0} MB");
    }
    let _ = writeln!(text, "NaN and infinite texels are cyan in the previews.\n");
    for t in &targets {
        let _ = writeln!(text, "{}", t.line());
    }
    let json = format!(
        "{{\"snapshot\":{n},\"ran\":{},\"missed\":{},\"noted\":{},\"targets\":[\n{}\n]}}\n",
        recorded.ran,
        recorded.missed,
        recorded.order.len(),
        targets
            .iter()
            .map(Target::json)
            .collect::<Vec<_>>()
            .join(",\n")
    );
    write_whole(&dir.join("snapshot.txt"), text.as_bytes());
    write_whole(&dir.join("snapshot.json"), json.as_bytes());
    write_coverage();
    tracing::info!(
        dir = %dir.display(),
        targets = targets.len(),
        ms = started.elapsed().as_millis() as u64,
        "probe: snapshot written"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notes_keep_first_use_order_and_the_last_writer() {
        let mut r = Recorded::new();
        r.add(0x10, false, ());
        r.add(0x20, false, ());
        assert!(r.known(0x10, true));
        assert!(!r.known(0x30, false));
        assert_eq!(r.order, [0x10, 0x20]);
        assert!(r.noted[&0x10].uav && !r.noted[&0x20].uav);
        r.drawn(0xa, true, [0x10, 0x30]);
        r.drawn(0xb, false, [0x20]);
        r.drawn(0xc, true, [0x20]);
        assert_eq!((r.ran, r.missed), (2, 1));
        assert_eq!((r.noted[&0x10].producer, r.noted[&0x10].draw), (0xa, 1));
        assert_eq!((r.noted[&0x20].producer, r.noted[&0x20].draw), (0xc, 2));
        for i in 0..MOST + 10 {
            r.add(0x1000 + i, false, ());
        }
        assert_eq!(r.noted.len(), MOST);
    }

    /// The read-back itself, on whatever adapter the host has.
    #[test]
    fn a_cleared_target_reads_back_as_its_colour() {
        let Some(host) = crate::host::host() else {
            return;
        };
        let heap = D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_DEFAULT,
            ..Default::default()
        };
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
            Width: 65,
            Height: 3,
            DepthOrArraySize: 1,
            MipLevels: 1,
            Format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_R8G8B8A8_UNORM,
            SampleDesc: windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Flags: D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET,
            ..Default::default()
        };
        let state = D3D12_RESOURCE_STATE_RENDER_TARGET;
        let mut texture: Option<ID3D12Resource> = None;
        // SAFETY: plain creations; an open list; the view heap outlives the
        // list's run.
        let (texture, _views) = unsafe {
            host.device
                .CreateCommittedResource(
                    &heap,
                    D3D12_HEAP_FLAG_NONE,
                    &desc,
                    state,
                    None,
                    &mut texture,
                )
                .expect("a texture");
            let texture = texture.expect("made");
            let views: ID3D12DescriptorHeap = host
                .device
                .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                    Type: D3D12_DESCRIPTOR_HEAP_TYPE_RTV,
                    NumDescriptors: 1,
                    ..Default::default()
                })
                .expect("a view heap");
            let view = views.GetCPUDescriptorHandleForHeapStart();
            host.device.CreateRenderTargetView(&texture, None, view);
            let list = host.new_list().expect("a list");
            list.list
                .ClearRenderTargetView(view, &[1.0, 0.0, 0.2, 1.0], None);
            list.close();
            host.execute(&[&list]);
            (texture, views)
        };
        let (w, h, format, values) = read(host, &texture, state.0 as u32).expect("read back");
        assert_eq!((w, h, format), (65, 3, 28));
        assert_eq!(values.len(), 65 * 3);
        assert!(
            values.iter().all(|&v| v == [1.0, 0.0, 0.2, 1.0]),
            "{:?}",
            &values[..2]
        );
    }
}
