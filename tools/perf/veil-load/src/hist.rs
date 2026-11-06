//! Log-linear latency histogram (about 1.5 % relative error, nanosecond input).

const SUB_BITS: u32 = 6;
const SUB: usize = 1 << SUB_BITS;
const BUCKETS: usize = 64 - SUB_BITS as usize;

#[derive(Clone)]
pub struct Hist {
    counts: Vec<u64>,
    pub total: u64,
    pub sum_ns: u128,
    pub max_ns: u64,
}

fn index(v: u64) -> usize {
    if v < SUB as u64 {
        return v as usize;
    }
    let msb = 63 - v.leading_zeros();
    let shift = msb - SUB_BITS;
    let sub = ((v >> shift) as usize) & (SUB - 1);
    (shift as usize + 1) * SUB + sub
}

fn value(i: usize) -> u64 {
    if i < SUB {
        return i as u64;
    }
    let shift = (i / SUB - 1) as u32;
    let sub = (i % SUB) as u64;
    ((SUB as u64 + sub) << shift) + (1u64 << shift) / 2
}

impl Hist {
    pub fn new() -> Hist {
        Hist {
            counts: vec![0; (BUCKETS + 1) * SUB],
            total: 0,
            sum_ns: 0,
            max_ns: 0,
        }
    }

    #[inline]
    pub fn record(&mut self, ns: u64) {
        self.counts[index(ns)] += 1;
        self.total += 1;
        self.sum_ns += ns as u128;
        if ns > self.max_ns {
            self.max_ns = ns;
        }
    }

    pub fn merge(&mut self, o: &Hist) {
        for (a, b) in self.counts.iter_mut().zip(o.counts.iter()) {
            *a += *b;
        }
        self.total += o.total;
        self.sum_ns += o.sum_ns;
        self.max_ns = self.max_ns.max(o.max_ns);
    }

    pub fn percentile(&self, p: f64) -> u64 {
        if self.total == 0 {
            return 0;
        }
        let want = ((self.total as f64) * p / 100.0).ceil().max(1.0) as u64;
        let mut seen = 0;
        for (i, c) in self.counts.iter().enumerate() {
            seen += *c;
            if seen >= want {
                return value(i).min(self.max_ns);
            }
        }
        self.max_ns
    }

    pub fn mean(&self) -> u64 {
        if self.total == 0 {
            0
        } else {
            (self.sum_ns / self.total as u128) as u64
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentiles_are_within_the_error() {
        let mut h = Hist::new();
        for v in 1..=100_000u64 {
            h.record(v * 1000);
        }
        let p50 = h.percentile(50.0) as f64;
        let p99 = h.percentile(99.0) as f64;
        assert!((p50 / 50_000_000.0 - 1.0).abs() < 0.02, "{}", p50);
        assert!((p99 / 99_000_000.0 - 1.0).abs() < 0.02, "{}", p99);
        assert_eq!(h.total, 100_000);
    }

    #[test]
    fn index_is_monotonic() {
        let mut last = 0;
        for v in (0..1_000_000u64).step_by(7) {
            let i = index(v);
            assert!(i >= last);
            last = i;
        }
    }
}
