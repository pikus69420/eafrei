use std::{
    collections::{hash_map::RandomState, HashMap},
    hash::BuildHasher,
    net::IpAddr,
    sync::Mutex,
    time::{Duration, Instant},
};
const WINDOW: Duration = Duration::from_secs(60);
const SHARDS: usize = 16;
const SLOTS: usize = 256;
struct Bucket {
    start: Instant,
    used: u32,
}
struct Shard {
    sweep: Instant,
    map: HashMap<IpAddr, Bucket>,
}
pub struct Limiter {
    hash: RandomState,
    shards: [Mutex<Shard>; SHARDS],
}
impl Default for Limiter {
    fn default() -> Self {
        Self {
            hash: RandomState::new(),
            shards: std::array::from_fn(|_| {
                Mutex::new(Shard {
                    sweep: Instant::now(),
                    map: HashMap::new(),
                })
            }),
        }
    }
}
impl Limiter {
    pub fn allows(&self, addr: IpAddr, limit: u32, now: Instant) -> bool {
        if limit == 0 {
            return true;
        }
        let idx = self.hash.hash_one(addr) as usize % SHARDS;
        let mut s = self.shards[idx].lock().unwrap_or_else(|p| p.into_inner());
        if now.saturating_duration_since(s.sweep) >= WINDOW {
            s.map
                .retain(|_, v| now.saturating_duration_since(v.start) < WINDOW);
            s.sweep = now;
        }
        if s.map.len() >= SLOTS && !s.map.contains_key(&addr) {
            return false;
        }
        let b = s.map.entry(addr).or_insert(Bucket {
            start: now,
            used: 0,
        });
        if now.saturating_duration_since(b.start) >= WINDOW {
            b.start = now;
            b.used = 0;
        }
        if b.used >= limit {
            return false;
        }
        b.used += 1;
        true
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn windows() {
        let l = Limiter::default();
        let a = "127.0.0.1".parse().unwrap();
        let t = Instant::now();
        assert!(l.allows(a, 1, t));
        assert!(!l.allows(a, 1, t));
        assert!(l.allows(a, 1, t + WINDOW));
        assert!(l.allows(a, 0, t + WINDOW));
    }
}
