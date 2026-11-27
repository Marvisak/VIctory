//! Partial dispatch and host-computed descriptor tables.

use super::{Lower, Val, Why};
use crate::emit::UserData;
use crate::emit::bitcode::{Bin, Term, V, pred};
use crate::emit::shader::USER_DISPATCH_THREADS_X;
use crate::native::RootType;
use crate::resource::Site;

impl Lower<'_> {
    pub(super) fn host_word(&mut self, root: RootType, slot: u8, dword: u32) -> V {
        let known = self
            .user_reads
            .values()
            .find(|u| u.root == root && u.slot == slot && u.dword == dword)
            .copied();
        let u = match known {
            Some(u) => u,
            None => {
                let mut index = self
                    .x
                    .native
                    .layout
                    .iter()
                    .map(|e| u32::from(e.user_data) + e.sgprs)
                    .max()
                    .unwrap_or(0);
                while self.user_reads.contains_key(&index) {
                    index += 1;
                }
                let u = UserData {
                    index,
                    slot,
                    root,
                    dword,
                };
                self.user_reads.insert(index, u);
                u
            }
        };
        self.user_at(u)
    }

    pub(super) fn partial_dispatch_prologue(&mut self, partial: bool) {
        if !partial {
            return;
        }
        let original = self.s.entry;
        let width = self.s.threads.expect("compute thread count")[0];
        let limit = self.host_word(RootType::Other(USER_DISPATCH_THREADS_X), 0, width);
        let Val::I32(group) = self.compute_id("groupId.i32", 94, 0) else {
            unreachable!()
        };
        let Val::I32(thread) = self.thread_id(0) else {
            unreachable!()
        };
        let width = self.s.m.i32(width);
        let first = self.s.m.bin(original, Bin::Mul, group, width);
        let x = self.s.m.bin(original, Bin::Add, first, thread);
        let zero = self.s.m.i32(0);
        let unlimited = self.s.m.cmp(original, pred::EQ, limit, zero);
        let inside = self.s.m.cmp(original, pred::IULT, x, limit);
        let active = self.s.m.bin(original, Bin::Or, unlimited, inside);
        let body = self.s.m.new_block();
        let done = self.s.m.new_block();
        self.s.m.terminate(original, Term::CondBr(active, body, done));
        self.s.m.terminate(done, Term::Ret);
        self.s.entry = body;
    }

    pub(super) fn view_address_indexed(
        &mut self,
        site: Site,
        first: u8,
    ) -> Result<Option<(V, V, V, V)>, Why> {
        let a = *self
            .scalar
            .get(&site)
            .ok_or("an untracked descriptor load")?;
        let root = self
            .x
            .native
            .layout
            .get(a.param)
            .map(|p| p.root)
            .ok_or("a descriptor outside the tables")?;
        if !matches!(root, RootType::Table | RootType::ResourceHeap) || a.dynamic.is_none() {
            return Ok(None);
        }
        let c = self.cur;
        let slot = self.heap_index(site, first, false)?;
        let r = self.s.read_view_slot(c, slot);
        let words = [0, 1, 2, 3].map(|word| self.s.m.extract(c, r, word));
        let [lo, hi, len, stride] = words.map(|word| self.as_int(word));
        Ok(Some((lo, hi, len, stride)))
    }
}
