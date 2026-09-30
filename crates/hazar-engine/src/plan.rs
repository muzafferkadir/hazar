use crate::meta::PartState;

pub const MAX_CONNECTIONS: usize = 16;
pub const DEFAULT_CONNECTIONS: usize = 8;
pub const DEFAULT_MIN_PART_SIZE: u64 = 1 << 20; // 1 MiB

#[derive(Debug, Clone, Copy)]
pub struct PlanOptions {
    pub connections: usize,
    pub min_part_size: u64,
}

impl Default for PlanOptions {
    fn default() -> Self {
        Self {
            connections: DEFAULT_CONNECTIONS,
            min_part_size: DEFAULT_MIN_PART_SIZE,
        }
    }
}

/// Split `size` into at most `connections` contiguous parts, never smaller than
/// `min_part_size` (so tiny files stay on a single connection).
pub fn plan_parts(size: u64, opts: PlanOptions) -> Vec<PartState> {
    if size == 0 {
        return Vec::new();
    }
    let min_part = opts.min_part_size.max(1);
    let max_parts = opts.connections.clamp(1, MAX_CONNECTIONS);
    let affordable = (size / min_part).max(1) as usize;
    let count = affordable.min(max_parts);
    let base = size / count as u64;
    let mut parts = Vec::with_capacity(count);
    let mut start = 0u64;
    for i in 0..count {
        let len = if i == count - 1 { size - start } else { base };
        parts.push(PartState {
            index: i as u32,
            start,
            end: start + len - 1,
            written: 0,
        });
        start += len;
    }
    parts
}

/// More ranges than workers keeps fast workers busy without changing active ranges.
/// A bounded plan avoids one task per range and limits resume metadata size.
pub fn plan_work(size: u64, opts: PlanOptions) -> Vec<PartState> {
    if size == 0 {
        return Vec::new();
    }
    let workers = opts.connections.clamp(1, MAX_CONNECTIONS);
    let chunk = (size / (workers as u64 * 4)).max(opts.min_part_size.max(1));
    let count = size.div_ceil(chunk).min(4096);
    let chunk = size.div_ceil(count);
    (0..count)
        .map(|index| {
            let start = index * chunk;
            PartState {
                index: index as u32,
                start,
                end: (start + chunk).min(size) - 1,
                written: 0,
            }
        })
        .filter(|p| p.start < size)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(connections: usize, min: u64) -> PlanOptions {
        PlanOptions {
            connections,
            min_part_size: min,
        }
    }

    #[test]
    fn splits_evenly_and_covers_every_byte() {
        let parts = plan_parts(10_000_000, opts(8, 1));
        assert_eq!(parts.len(), 8);
        assert_eq!(parts[0].start, 0);
        assert_eq!(parts.last().unwrap().end, 9_999_999);
        for pair in parts.windows(2) {
            assert_eq!(pair[0].end + 1, pair[1].start);
        }
    }

    #[test]
    fn respects_min_part_size() {
        let parts = plan_parts(3 * 1024 * 1024, opts(8, 1024 * 1024));
        assert_eq!(parts.len(), 3);
    }

    #[test]
    fn small_files_use_one_connection() {
        let parts = plan_parts(1024, opts(8, 1024 * 1024));
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].length(), 1024);
    }

    #[test]
    fn empty_file_has_no_parts() {
        assert!(plan_parts(0, opts(8, 1)).is_empty());
    }

    #[test]
    fn connection_count_is_capped() {
        let parts = plan_parts(1 << 40, opts(64, 1));
        assert_eq!(parts.len(), MAX_CONNECTIONS);
    }
}
