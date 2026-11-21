//! Open-addressing hash maps keyed by nonzero u64 (0 marks an empty slot; ID keys are never 0).

#[inline(always)]
pub fn mix(mut k: u64) -> u64 {
    // MurmurHash3 fmix64: every key bit affects every hash bit.
    k ^= k >> 33;
    k = k.wrapping_mul(0xff51_afd7_ed55_8ccd);
    k ^= k >> 33;
    k = k.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
    k ^= k >> 33;
    k
}

/// Hints the CPU to start loading the cache line holding `p`.
#[inline(always)]
pub fn prefetch<T>(p: &T) {
    #[cfg(target_arch = "x86_64")]
    // SAFETY: prefetching is a hint; it never faults and `p` is a valid reference anyway.
    unsafe {
        std::arch::x86_64::_mm_prefetch(p as *const T as *const i8, std::arch::x86_64::_MM_HINT_T0);
    }
    #[cfg(not(target_arch = "x86_64"))]
    let _ = p;
}

pub struct Map<V: Copy + Default> {
    slots: Vec<(u64, V)>,
    mask: usize,
    len: usize,
}

impl<V: Copy + Default> Map<V> {
    pub fn with_capacity(n: usize) -> Self {
        let cap = (n.max(8) * 2).next_power_of_two();
        Self { slots: vec![(0, V::default()); cap], mask: cap - 1, len: 0 }
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.len
    }

    #[inline]
    pub fn get(&self, k: u64) -> Option<V> {
        let mut i = mix(k) as usize & self.mask;
        loop {
            let (kk, v) = self.slots[i];
            if kk == k {
                return Some(v);
            }
            if kk == 0 {
                return None;
            }
            i = (i + 1) & self.mask;
        }
    }

    /// Returns the value for `k`, inserting `make()` first if absent; the bool says whether it was inserted.
    #[inline]
    pub fn get_or_insert_with(&mut self, k: u64, make: impl FnOnce() -> V) -> (V, bool) {
        if (self.len + 1) * 2 > self.slots.len() {
            self.grow();
        }
        let mut i = mix(k) as usize & self.mask;
        loop {
            let (kk, v) = self.slots[i];
            if kk == k {
                return (v, false);
            }
            if kk == 0 {
                let v = make();
                self.slots[i] = (k, v);
                self.len += 1;
                return (v, true);
            }
            i = (i + 1) & self.mask;
        }
    }

    pub fn insert(&mut self, k: u64, v: V) {
        if (self.len + 1) * 2 > self.slots.len() {
            self.grow();
        }
        let mut i = mix(k) as usize & self.mask;
        loop {
            let kk = self.slots[i].0;
            if kk == k || kk == 0 {
                if kk == 0 {
                    self.len += 1;
                }
                self.slots[i] = (k, v);
                return;
            }
            i = (i + 1) & self.mask;
        }
    }

    fn grow(&mut self) {
        let old = std::mem::take(&mut self.slots);
        let cap = (old.len() * 2).max(16);
        self.slots = vec![(0, V::default()); cap];
        self.mask = cap - 1;
        self.len = 0;
        for (k, v) in old {
            if k != 0 {
                self.insert(k, v);
            }
        }
    }

    /// Starts loading the slot where a lookup of `k` begins.
    #[inline(always)]
    pub fn prefetch(&self, k: u64) {
        prefetch(&self.slots[mix(k) as usize & self.mask]);
    }

    /// Empties the map, keeping capacity for at least `n` keys.
    pub fn reset(&mut self, n: usize) {
        let cap = (n.max(8) * 2).next_power_of_two();
        if cap > self.slots.len() || cap * 8 < self.slots.len() {
            self.slots = vec![(0, V::default()); cap];
            self.mask = cap - 1;
        } else {
            self.slots.iter_mut().for_each(|s| s.0 = 0);
        }
        self.len = 0;
    }

}
