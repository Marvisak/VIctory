//! Admission for host buffers used while running a title. The lock protects
//! counters only. Callers allocate, wait for work, and free objects outside it.

use std::sync::{Arc, Mutex, OnceLock};

static BUDGET: OnceLock<Arc<Budget>> = OnceLock::new();

/// Called at title startup, before starting either producer.
pub fn configure(bytes: u64) {
    if bytes != 0 {
        BUDGET.get_or_init(|| Budget::new(bytes));
    }
}

pub fn configured() -> Option<&'static Arc<Budget>> {
    BUDGET.get()
}

#[derive(Clone, Copy, Debug)]
pub enum Owner {
    DirectStorage,
    LooseShadow,
}

#[derive(Default, Debug, Clone, Copy)]
pub struct Usage {
    pub direct_storage: u64,
    pub loose_shadows: u64,
}

pub struct Budget {
    pub limit: u64,
    used: Mutex<Usage>,
}

impl Budget {
    pub fn new(limit: u64) -> Arc<Self> {
        Arc::new(Self {
            limit,
            used: Mutex::default(),
        })
    }

    pub fn usage(&self) -> Usage {
        *self.used.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Charge the entire operation before allocating anything. A refused
    /// caller gets the counts from the same admission decision.
    pub fn reserve(self: &Arc<Self>, owner: Owner, bytes: u64) -> Result<Reservation, Usage> {
        let mut used = self.used.lock().unwrap_or_else(|e| e.into_inner());
        if bytes > self.limit - used.loose_shadows - used.direct_storage {
            return Err(*used);
        }
        *used.bytes(owner) += bytes;
        Ok(Reservation {
            budget: self.clone(),
            owner,
            bytes,
        })
    }
}

impl Usage {
    fn bytes(&mut self, owner: Owner) -> &mut u64 {
        match owner {
            Owner::DirectStorage => &mut self.direct_storage,
            Owner::LooseShadow => &mut self.loose_shadows,
        }
    }
}

pub struct Reservation {
    budget: Arc<Budget>,
    owner: Owner,
    bytes: u64,
}

impl Drop for Reservation {
    fn drop(&mut self) {
        let mut used = self.budget.used.lock().unwrap_or_else(|e| e.into_inner());
        *used.bytes(self.owner) -= self.bytes;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Barrier;

    #[test]
    fn concurrent_producers_cannot_double_spend_remaining_capacity() {
        let budget = Budget::new(100);
        let pinned = budget.reserve(Owner::LooseShadow, 40).unwrap();
        let barrier = Barrier::new(8);
        std::thread::scope(|s| {
            let workers: Vec<_> = (0..8)
                .map(|_| {
                    s.spawn(|| {
                        barrier.wait();
                        let charge = budget.reserve(Owner::DirectStorage, 30);
                        // All attempts finish while the winners still hold their bytes.
                        barrier.wait();
                        charge.is_ok()
                    })
                })
                .collect();
            assert_eq!(
                workers
                    .into_iter()
                    .map(|w| w.join().unwrap() as usize)
                    .sum::<usize>(),
                2
            );
        });
        let usage = budget.reserve(Owner::DirectStorage, 61).err().unwrap();
        assert_eq!((usage.loose_shadows, usage.direct_storage), (40, 0));
        drop(pinned);
        assert!(budget.reserve(Owner::DirectStorage, 100).is_ok());
    }
}
