//! A small generator seeded from the estate's seed, a key — a repository, a subject, a project —
//! and a day, so the same page shows the same thing every time and any period agrees with any
//! other (SplitMix64).

use chrono::{DateTime, Duration, TimeZone, Utc};

pub const DAY: i64 = 86_400;

pub struct Dice(u64);

impl Dice {
    pub fn seeded(seed: u64, key: &str, day: i64) -> Self {
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325 ^ seed.wrapping_mul(0xff51_afd7_ed55_8ccd);
        for byte in key.bytes() {
            hash = (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3);
        }
        Self(hash ^ (day as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15))
    }

    pub fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    pub fn unit(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }

    pub fn chance(&mut self, p: f64) -> bool {
        self.unit() < p
    }

    pub fn between(&mut self, (low, high): (f64, f64)) -> f64 {
        low + (high - low) * self.unit()
    }

    /// Spread evenly across orders of magnitude, as waits and outages are.
    pub fn spread(&mut self, (low, high): (f64, f64)) -> f64 {
        (low.ln() + (high.ln() - low.ln()) * self.unit()).exp()
    }

    /// How many of something happen, around `expected` (Poisson).
    pub fn count(&mut self, expected: f64) -> usize {
        let floor = (-expected).exp();
        let (mut count, mut product) = (0, self.unit());
        while product > floor && count < 40 {
            count += 1;
            product *= self.unit();
        }
        count
    }

    pub fn sha(&mut self) -> String {
        format!("{:016x}{:016x}{:08x}", self.next(), self.next(), self.next() as u32)
    }

    pub fn pick<'a>(&mut self, from: &[&'a str]) -> &'a str {
        from[(self.next() % from.len() as u64) as usize]
    }
}

pub fn midnight_of(index: i64) -> Option<DateTime<Utc>> {
    Utc.timestamp_opt(index * DAY, 0).single()
}

pub fn hours(hours: f64) -> Duration {
    Duration::seconds((hours * 3_600.0) as i64)
}

pub fn minutes(minutes: f64) -> Duration {
    Duration::seconds((minutes * 60.0) as i64)
}

/// How far a year's improvement has come by `midnight`: 0 a year ago, 1 today.
pub fn progress(now: DateTime<Utc>, midnight: DateTime<Utc>) -> f64 {
    (1.0 - (now - midnight).num_days() as f64 / 365.0).clamp(0.0, 1.0)
}
