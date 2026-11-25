//! Thread numbering and exchange between the host halves of a console wave.

use super::{Attr, Bin, Lower, Rmw, Term, V, flags, meeting_mode, pred};

impl Lower<'_> {
    /// A console wave64 program in host waves of 32: the first console thread of this host wave
    /// (0, 32, 64, ...). A GPU need not give a host wave consecutive thread indices (NVIDIA's
    /// 8 x 8 groups), so each wave takes the next 32 from an LDS counter instead: every thread
    /// zeroes it, the group meets, each wave's first lane adds 32 and the wave takes what it
    /// found. Branch-free, in the entry block, which every thread runs.
    fn wave_base(&mut self) -> Option<V> {
        let slot = self.halves?;
        if let Some(v) = self.wave_base {
            return Some(v);
        }
        let (e, i32t) = (self.s.entry, self.s.t.i32);
        let lds = self.s.lds();
        let k = self.s.m.i32(slot);
        let ptr = self.s.m.element(e, lds, k);
        let zero = self.s.m.i32(0);
        self.s.m.store(e, ptr, zero);
        self.group_sync(e);
        let lane = self.s.op(e, "waveGetLaneIndex", Attr::ReadOnly, i32t, 111, &[]);
        let first = self.s.m.cmp(e, pred::EQ, lane, zero);
        let k32 = self.s.m.i32(32);
        let add = self.s.m.select(e, first, k32, zero);
        let ptr = self.s.m.element(e, lds, k);
        let old = self.s.m.atomic(e, Rmw::Add, ptr, add);
        let base = self.s.op(e, "waveReadLaneFirst.i32", Attr::NoUnwind, i32t, 118, &[old]);
        self.s.flags |= flags::WAVE_OPS;
        self.wave_base = Some(base);
        Some(base)
    }

    /// The console thread index of this thread in the group: `wave_base` plus the host lane
    /// (host waves of 32), else the wave's first thread index rounded down plus the lane.
    pub(super) fn console_thread(&mut self) -> V {
        let (e, i32t) = (self.s.entry, self.s.t.i32);
        let lane = self.s.op(e, "waveGetLaneIndex", Attr::ReadOnly, i32t, 111, &[]);
        if let Some(base) = self.wave_base() {
            return self.s.m.bin(e, Bin::Add, base, lane);
        }
        let flat = self.s.op(e, "flattenedThreadIdInGroup.i32", Attr::ReadNone, i32t, 96, &[]);
        let first = self.s.op(e, "waveReadLaneFirst.i32", Attr::NoUnwind, i32t, 118, &[flat]);
        let wave = self.s.wave_size.unwrap_or(32);
        let mask = self.s.m.i32(!(wave - 1));
        let base = self.s.m.bin(e, Bin::And, first, mask);
        self.s.m.bin(e, Bin::Add, base, lane)
    }

    /// This thread's console lane (0..63): its relabelled thread index (`thread_id`), whose host
    /// wave holds a 32-aligned run of console lanes. Without `exchange`: the host lane.
    pub(super) fn console_lane(&mut self) -> V {
        if self.exchange.is_none() {
            return self.lane_index();
        }
        if let Some(v) = self.clane {
            return v;
        }
        let (e, i32t) = (self.s.entry, self.s.t.i32);
        let f = self.console_thread();
        let _ = i32t;
        let k63 = self.s.m.i32(63);
        let cl = self.s.m.bin(e, Bin::And, f, k63);
        self.s.flags |= flags::WAVE_OPS;
        self.clane = Some(cl);
        cl
    }

    /// A group barrier with an LDS fence in `block`.
    fn group_sync(&mut self, block: usize) {
        let mode = self.s.m.i32(meeting_mode());
        let void = self.s.t.void;
        self.s.op(block, "barrier", Attr::NoDuplicate, void, 80, &[mode]);
    }

    /// `exchange`: each host wave gives `mine` (the same in all its lanes) for its half; both get
    /// (lower half's, upper half's). A group with no upper half gets 0 for it.
    pub(super) fn exchange_halves(&mut self, at: usize, mine: V) -> (V, V) {
        let Some((slot, small, _)) = self.exchange.as_ref().map(|(a, b, _)| (*a, *b, ())) else {
            let z = self.s.m.i32(0);
            return (mine, z);
        };
        if small {
            let z = self.s.m.i32(0);
            return (mine, z);
        }
        if self.local {
            // each host wave on its own: its half holds `mine`, the other nothing
            let cl = self.console_lane();
            let (k32, z) = (self.s.m.i32(32), self.s.m.i32(0));
            let upper = self.s.m.cmp(at, pred::IUGE, cl, k32);
            let lo = self.s.m.select(at, upper, z, mine);
            let hi = self.s.m.select(at, upper, mine, z);
            return (lo, hi);
        }
        // the pair of dwords of this thread's console wave: its half's, then both
        let pair = self.exchange_pair(slot);
        let cl = self.console_lane();
        let five = self.s.m.i32(5);
        let half = self.s.m.bin(at, Bin::LShr, cl, five);
        let index = self.s.m.bin(at, Bin::Add, pair, half);
        let lds = self.s.lds();
        let ptr = self.s.m.element(at, lds, index);
        self.s.m.store(at, ptr, mine);
        let at = self.meet_at(at);
        let one = self.s.m.i32(1);
        let k1 = self.s.m.bin(at, Bin::Add, pair, one);
        let p0 = self.s.m.element(at, lds, pair);
        let p1 = self.s.m.element(at, lds, k1);
        let lo = self.s.m.load(at, p0);
        let hi = self.s.m.load(at, p1);
        // the slots are written again only after every thread read them
        self.meet_at(at);
        (lo, hi)
    }

    /// The host waves of a console wave meet at the end of `block`; returns the block code
    /// after the meeting goes in (`block`). A group barrier with an LDS fence; with
    /// `pair_meet`, a wait for this console wave's other host wave only
    /// ([`Self::pair_wait`]), which needs `block` to be the current one.
    pub(super) fn meet_at(&mut self, block: usize) -> usize {
        if self.pair_meet.is_none() {
            self.group_sync(block);
            return block;
        }
        assert_eq!(block, self.cur, "a pair meeting away from the current block");
        self.pair_wait();
        block
    }

    /// [`Self::meet_at`] the current block.
    pub(super) fn meet(&mut self) {
        let c = self.cur;
        self.meet_at(c);
    }

    /// This console wave's meeting counter (`pair_meet`): zeroed in the entry block by its
    /// threads before a group barrier there, which every thread runs.
    fn pair_counter(&mut self) -> V {
        if let Some(v) = self.pair_counter {
            return v;
        }
        let base = self.pair_meet.expect("pair meetings");
        let e = self.s.entry;
        let thread = self.console_thread();
        let six = self.s.m.i32(6);
        let wave = self.s.m.bin(e, Bin::LShr, thread, six);
        let k = self.s.m.i32(base);
        let index = self.s.m.bin(e, Bin::Add, k, wave);
        let lds = self.s.lds();
        let ptr = self.s.m.element(e, lds, index);
        let zero = self.s.m.i32(0);
        self.s.m.store(e, ptr, zero);
        // (a wait for the group whatever `VICTORY_GROUP_SYNC` says: the zero must land first)
        let mode = self.s.m.i32(1 | 8);
        let void = self.s.t.void;
        self.s.op(e, "barrier", Attr::NoDuplicate, void, 80, &[mode]);
        self.pair_counter = Some(index);
        index
    }

    /// Waits at the end of the current block for the other host wave of this console wave
    /// (`pair_meet`). The code so far moves to a block of its own
    /// ([`super::bitcode::Module::split_before`]) and the wait goes between it and the current
    /// block, so code added to the current block later, by callers holding it too, runs
    /// after the wait. Each host wave's first lane adds 1 to the console wave's counter;
    /// the wave that found an even count (2k) arrived first and spins
    /// until the count reaches 2k + 2. The counter only grows, so a wave that already left
    /// for the next meeting does not confuse one still spinning here. LDS fences before and
    /// after order the halves' LDS accesses as a barrier would. After `PAIR_SPINS` reads the
    /// wave goes on regardless: wrong values rather than a hung card. Such a wait also sets
    /// `PAIR_GONE` in the counter (the other host wave left the program, at the loop limit
    /// say), and every later wait of the console wave passes at once instead of spinning out
    /// again at every other meeting.
    fn pair_wait(&mut self) {
        const PAIR_SPINS: u32 = 1 << 18;
        const PAIR_GONE: u32 = 1 << 31;
        let index = self.pair_counter();
        let after = self.cur;
        let pre = self.s.m.split_before(after);
        // (the start of this IR block, which later back edges and joins go to)
        for b in self.block.iter_mut().filter(|b| **b == Some(after)) {
            *b = Some(pre);
        }
        self.cur = pre;
        let (i32t, i1t, void) = (self.s.t.i32, self.s.t.i1, self.s.t.void);
        let fence = self.s.m.i32(8);
        self.s.op(self.cur, "barrier", Attr::NoDuplicate, void, 80, &[fence]);
        self.s.flags |= flags::WAVE_OPS;
        let first = self.s.op(self.cur, "waveIsFirstLane", Attr::NoUnwind, i1t, 110, &[]);
        let (zero, one) = (self.s.m.i32(0), self.s.m.i32(1));
        let at = self.begin_guarded(Some(first));
        let lds = self.s.lds();
        let ptr = self.s.m.element(self.cur, lds, index);
        let old = self.s.m.atomic(self.cur, Rmw::Add, ptr, one);
        let old = self.end_guarded(at, &[(old, zero)])[0];
        let old = self.s.op(self.cur, "waveReadLaneFirst.i32", Attr::NoUnwind, i32t, 118, &[old]);
        let even = self.s.m.i32(!1u32);
        let from = self.s.m.bin(self.cur, Bin::And, old, even);
        let before = self.cur;
        let head = self.s.m.new_block();
        self.s.m.terminate(before, Term::Br(head));
        self.cur = head;
        let spins = self.s.m.phi(head, i32t);
        let at = self.begin_guarded(Some(first));
        let lds = self.s.lds();
        let ptr = self.s.m.element(self.cur, lds, index);
        let now = self.s.m.atomic(self.cur, Rmw::Or, ptr, zero);
        let now = self.end_guarded(at, &[(now, zero)])[0];
        let c = self.cur;
        let now = self.s.op(c, "waveReadLaneFirst.i32", Attr::NoUnwind, i32t, 118, &[now]);
        let since = self.s.m.bin(c, Bin::Sub, now, from);
        let two = self.s.m.i32(2);
        let both = self.s.m.cmp(c, pred::SGE, since, two);
        let next = self.s.m.bin(c, Bin::Add, spins, one);
        let limit = self.s.m.i32(PAIR_SPINS);
        let over = self.s.m.cmp(c, pred::IUGE, next, limit);
        let gone_bit = self.s.m.i32(PAIR_GONE);
        let gone = self.s.m.bin(c, Bin::And, now, gone_bit);
        let gone = self.s.m.cmp(c, pred::NE, gone, zero);
        let done = self.s.m.bin(c, Bin::Or, both, over);
        let done = self.s.m.bin(c, Bin::Or, done, gone);
        // (ran out here: the other host wave is taken to be gone)
        let alone = self.s.m.cmp(c, pred::SLT, since, two);
        let alone = self.s.m.bin(c, Bin::And, alone, over);
        let exit = self.s.m.new_block();
        self.s.m.terminate(c, Term::CondBr(done, exit, head));
        self.s.m.set_phi(spins, vec![(zero, before), (next, c)]);
        self.cur = exit;
        let mark = self.s.m.bin(exit, Bin::And, alone, first);
        let at = self.begin_guarded(Some(mark));
        let lds = self.s.lds();
        let ptr = self.s.m.element(self.cur, lds, index);
        self.s.m.atomic(self.cur, Rmw::Or, ptr, gone_bit);
        self.end_guarded(at, &[]);
        self.s.op(self.cur, "barrier", Attr::NoDuplicate, void, 80, &[fence]);
        self.s.m.terminate(self.cur, Term::Br(after));
        self.cur = after;
    }

    /// The first of the two exchange dwords of this thread's console wave (made in the entry
    /// block): a group of several console waves has a pair for each.
    pub(super) fn exchange_pair(&mut self, slot: u32) -> V {
        if let Some(v) = self.pair {
            return v;
        }
        let e = self.s.entry;
        let thread = self.console_thread();
        let six = self.s.m.i32(6);
        let wave = self.s.m.bin(e, Bin::LShr, thread, six);
        let one = self.s.m.i32(1);
        let twice = self.s.m.bin(e, Bin::Shl, wave, one);
        let base = self.s.m.i32(slot);
        let v = self.s.m.bin(e, Bin::Add, base, twice);
        self.pair = Some(v);
        v
    }
}
