use ::redis::RedisError;
use std::time::Duration;

mod counters_cache;
mod redis_async;
mod redis_cached;
mod redis_sync;
mod scripts;

pub const DEFAULT_FLUSHING_PERIOD_SEC: u64 = 1;
pub const DEFAULT_BATCH_SIZE: usize = 100;
pub const DEFAULT_MAX_CACHED_COUNTERS: usize = 10000;
pub const DEFAULT_RESPONSE_TIMEOUT_MS: u64 = 350;

use crate::counter::Counter;
use crate::storage::{Authorization, StorageErr};
pub use redis_async::AsyncRedisStorage;
pub use redis_cached::CachedRedisStorage;
pub use redis_cached::CachedRedisStorageBuilder;
pub use redis_sync::RedisStorage;

impl From<RedisError> for StorageErr {
    fn from(e: RedisError) -> Self {
        let transient = e.is_timeout()
            || e.is_connection_dropped()
            || e.is_cluster_error()
            || e.is_connection_refusal();
        Self {
            msg: e.to_string(),
            source: Some(Box::new(e)),
            transient,
        }
    }
}

pub fn is_limited(
    counters: &mut [Counter],
    delta: u64,
    script_res: Vec<Option<i64>>,
) -> Option<Authorization> {
    let mut counter_vals: Vec<Option<i64>> = vec![];
    let mut counter_ttls_msecs: Vec<Option<i64>> = vec![];

    for val_ttl_pair in script_res.chunks(2) {
        counter_vals.push(val_ttl_pair[0]);
        counter_ttls_msecs.push(val_ttl_pair[1]);
    }

    let mut first_limited = None;
    for (i, counter) in counters.iter_mut().enumerate() {
        // remaining  = max - (curr_val + delta)
        let remaining = counter
            .max_value()
            .checked_sub((counter_vals[i].unwrap_or(0) as u64) + delta);
        counter.set_remaining(remaining.unwrap_or_default());
        let expires_in = counter_ttls_msecs[i]
            .map(|x| {
                if x >= 0 {
                    Duration::from_millis(x as u64)
                } else {
                    counter.window()
                }
            })
            .unwrap_or(counter.window());

        counter.set_expires_in(expires_in);
        if first_limited.is_none() && remaining.is_none() {
            first_limited = Some(Authorization::Limited(
                counter.limit().name().map(|n| n.to_owned()),
            ))
        }
    }
    first_limited
}
