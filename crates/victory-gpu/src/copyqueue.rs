use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC;
use windows::core::Interface;

pub(crate) fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| crate::switch::on("VICTORY_COPY_QUEUE"))
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FenceOp {
    Wait(u8, u64),
    Execute,
    Signal(u8, u64),
}

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn valid_order(ops: &[FenceOp]) -> bool {
    let mut seen = [0u64; 4];
    for op in ops {
        match *op {
            FenceOp::Wait(id, value) => {
                if usize::from(id) >= seen.len() || seen[usize::from(id)] < value {
                    return false;
                }
            }
            FenceOp::Signal(id, value) => {
                if usize::from(id) >= seen.len() || value <= seen[usize::from(id)] {
                    return false;
                }
                seen[usize::from(id)] = value;
            }
            FenceOp::Execute => {}
        }
    }
    true
}

pub(crate) struct CopyLane {
    queue: ID3D12CommandQueue,
    fence: ID3D12Fence,
    zeros: ID3D12Resource,
    allocator: ID3D12CommandAllocator,
    list: ID3D12GraphicsCommandList,
    sent: AtomicU64,
    pending: Mutex<bool>,
}

// SAFETY: D3D12 objects are free-threaded and every field is synchronized
// by `pending` or atomics.
unsafe impl Send for CopyLane {}
// SAFETY: as above.
unsafe impl Sync for CopyLane {}

impl CopyLane {
    pub(crate) fn new(device: &ID3D12Device) -> windows::core::Result<Self> {
        // SAFETY: plain D3D12 creation with valid descriptions.
        unsafe {
            let queue: ID3D12CommandQueue = device.CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC {
                Type: D3D12_COMMAND_LIST_TYPE_COPY,
                ..Default::default()
            })?;
            let fence = device.CreateFence(0, D3D12_FENCE_FLAG_NONE)?;
            let allocator = device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_COPY)?;
            let list: ID3D12GraphicsCommandList =
                device.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_COPY, &allocator, None)?;
            list.Close()?;
            let heap = D3D12_HEAP_PROPERTIES {
                Type: D3D12_HEAP_TYPE_UPLOAD,
                ..Default::default()
            };
            let desc = D3D12_RESOURCE_DESC {
                Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
                Width: 4,
                Height: 1,
                DepthOrArraySize: 1,
                MipLevels: 1,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
                ..Default::default()
            };
            let mut zeros: Option<ID3D12Resource> = None;
            device.CreateCommittedResource(
                &heap,
                D3D12_HEAP_FLAG_NONE,
                &desc,
                D3D12_RESOURCE_STATE_GENERIC_READ,
                None,
                &mut zeros,
            )?;
            let zeros = zeros.ok_or_else(windows::core::Error::empty)?;
            let mut data = std::ptr::null_mut();
            zeros.Map(0, None, Some(&mut data))?;
            data.cast::<u32>().write(0);
            zeros.Unmap(0, None);
            Ok(Self {
                queue,
                fence,
                zeros,
                allocator,
                list,
                sent: AtomicU64::new(0),
                pending: Mutex::new(false),
            })
        }
    }

    pub(crate) fn fence(&self) -> &ID3D12Fence {
        &self.fence
    }

    pub(crate) fn stage_zero(&self, dest: &ID3D12Resource, bytes: u64) -> windows::core::Result<()> {
        let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        // SAFETY: the fence, allocator and list are ours; the fence wait
        // makes the allocator idle before it is reset.
        unsafe {
            let sent = self.sent.load(Ordering::Relaxed);
            while self.fence.GetCompletedValue() < sent {
                std::thread::yield_now();
            }
            if !*pending {
                self.allocator.Reset()?;
                self.list.Reset(&self.allocator, None)?;
                *pending = true;
            }
            self.list.CopyBufferRegion(dest, 0, &self.zeros, 0, bytes);
        }
        Ok(())
    }

    pub(crate) fn flush(&self, after: &[(&ID3D12Fence, u64)]) -> windows::core::Result<Option<u64>> {
        let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        if !*pending {
            return Ok(None);
        }
        // SAFETY: an open list of ours, fences of the same device and a
        // monotonically increasing value on our own fence.
        unsafe {
            self.list.Close()?;
            let lists = [Some(self.list.cast::<ID3D12CommandList>()?)];
            for &(fence, value) in after {
                if value != 0 {
                    self.queue.Wait(fence, value)?;
                }
            }
            self.queue.ExecuteCommandLists(&lists);
            let job = self.sent.fetch_add(1, Ordering::Relaxed) + 1;
            self.queue.Signal(&self.fence, job)?;
            *pending = false;
            Ok(Some(job))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{FenceOp, valid_order};

    #[test]
    fn a_copy_job_waits_only_for_values_already_signalled() {
        let mut ops = [
            FenceOp::Signal(0, 1),
            FenceOp::Wait(0, 1),
            FenceOp::Execute,
            FenceOp::Signal(1, 1),
        ];
        assert!(valid_order(&ops));
        ops[1] = FenceOp::Wait(0, 2);
        assert!(!valid_order(&ops));
    }

    #[test]
    fn the_direct_queue_waits_for_a_copy_job_already_sent() {
        assert!(valid_order(&[
            FenceOp::Signal(1, 7),
            FenceOp::Wait(1, 7),
            FenceOp::Execute,
        ]));
        assert!(!valid_order(&[
            FenceOp::Wait(1, 7),
            FenceOp::Signal(1, 7),
            FenceOp::Execute,
        ]));
    }

    #[test]
    fn signals_are_monotonic() {
        assert!(!valid_order(&[FenceOp::Signal(0, 2), FenceOp::Signal(0, 2)]));
        assert!(!valid_order(&[FenceOp::Signal(0, 2), FenceOp::Signal(0, 1)]));
        assert!(valid_order(&[FenceOp::Signal(0, 1), FenceOp::Signal(0, 2)]));
    }
}
