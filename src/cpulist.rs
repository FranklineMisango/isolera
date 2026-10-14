//! CPU set manipulation: the ``cpulist`` dialect used by the kernel.
//!
//! Linux expresses CPU sets in three interchangeable ways and Isolera must handle
//! all of them, because a single run touches each kind:
//!
//! * lists - ``0-3,7``: ``isolcpus``, ``nohz_full``, ``smp_affinity_list``;
//! * hex bitmasks - ``0000000f``: ``smp_affinity``, ``default_smp_affinity``;
//! * per-CPU directories - ``cpu0``, ``cpu1``: ``/sys/devices/system/cpu``.
//!
//! `CpuSet` is a `BTreeSet<u32>` so iteration order is deterministic, which keeps
//! generated configuration and diffs stable across runs.

use std::collections::BTreeSet;

use crate::error::{Error, Kind, Result};

/// An ordered set of logical CPU indices.
pub type CpuSet = BTreeSet<u32>;

/// Largest CPU index Isolera accepts, guarding against a typo such as
/// ``0-99999999`` allocating gigabytes.
pub const MAX_CPU: u32 = 65536;

fn bad(message: String) -> Error {
    Error::new(Kind::Config, message)
}

fn check(index: u32) -> Result<u32> {
    if index >= MAX_CPU {
        return Err(bad(format!(
            "CPU index {index} exceeds the supported maximum of {}",
            MAX_CPU - 1
        )));
    }
    Ok(index)
}

/// Parse ``"0-3,7"`` into a set. Accepts surrounding whitespace and an empty
/// string (the empty set), but rejects malformed ranges and reversed bounds.
pub fn parse(spec: &str) -> Result<CpuSet> {
    let mut set = CpuSet::new();
    let trimmed = spec.trim();
    if trimmed.is_empty() {
        return Ok(set);
    }
    for raw_item in trimmed.split(',') {
        let item = raw_item.trim();
        if item.is_empty() {
            return Err(bad(format!("malformed cpu list {spec:?}: empty item")));
        }
        match item.split_once('-') {
            None => {
                let index: u32 = item
                    .parse()
                    .map_err(|_| bad(format!("malformed cpu index {item:?} in {spec:?}")))?;
                set.insert(check(index)?);
            }
            Some((low, high)) => {
                let low: u32 = low
                    .trim()
                    .parse()
                    .map_err(|_| bad(format!("malformed range start in {item:?}")))?;
                let high: u32 = high
                    .trim()
                    .parse()
                    .map_err(|_| bad(format!("malformed range end in {item:?}")))?;
                if high < low {
                    return Err(bad(format!("reversed range {item:?} in {spec:?}")));
                }
                check(high)?;
                for index in low..=high {
                    set.insert(index);
                }
            }
        }
    }
    Ok(set)
}

/// Render a set back into kernel list form, compressing runs of three or more.
pub fn format(set: &CpuSet) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut iter = set.iter().copied().peekable();
    while let Some(start) = iter.next() {
        let mut end = start;
        while let Some(next) = iter.peek().copied() {
            if next == end + 1 {
                end = next;
                iter.next();
            } else {
                break;
            }
        }
        if end == start {
            parts.push(start.to_string());
        } else if end == start + 1 {
            parts.push(start.to_string());
            parts.push(end.to_string());
        } else {
            parts.push(format!("{start}-{end}"));
        }
    }
    parts.join(",")
}

/// Number of 32-bit words the kernel prints for a machine with `cpu_count` CPUs.
pub fn hex_groups(cpu_count: usize) -> usize {
    let words = cpu_count.div_ceil(32);
    words.max(1)
}

/// Render a set as the comma-separated 32-bit hex mask used by ``smp_affinity``.
pub fn to_hex_mask(set: &CpuSet, cpu_count: usize) -> String {
    let groups = hex_groups(cpu_count);
    let mut words = vec![0u32; groups];
    for cpu in set {
        let word = (*cpu as usize) / 32;
        let bit = (*cpu as usize) % 32;
        if word < groups {
            words[word] |= 1u32 << bit;
        }
    }
    words
        .iter()
        .rev()
        .map(|word| format!("{word:08x}"))
        .collect::<Vec<_>>()
        .join(",")
}

/// Parse a hex bitmask (``"00000003"`` or ``"00000000,00000003"``).
pub fn from_hex_mask(mask: &str) -> Result<CpuSet> {
    let mut set = CpuSet::new();
    let trimmed = mask.trim();
    if trimmed.is_empty() {
        return Ok(set);
    }
    let groups: Vec<&str> = trimmed.split(',').collect();
    for (position, group) in groups.iter().enumerate() {
        let word_index = groups.len() - 1 - position;
        let word = u32::from_str_radix(group.trim(), 16).map_err(|_| {
            bad(format!(
                "malformed hex cpu mask {mask:?}: {group:?} is not hexadecimal"
            ))
        })?;
        for bit in 0..32u32 {
            if word & (1u32 << bit) != 0 {
                set.insert(check(word_index as u32 * 32 + bit)?);
            }
        }
    }
    Ok(set)
}

/// Every CPU index in ``0..cpu_count`` that is not in `set`.
pub fn complement(set: &CpuSet, cpu_count: usize) -> CpuSet {
    (0..cpu_count as u32)
        .filter(|cpu| !set.contains(cpu))
        .collect()
}

/// True when every member of `set` is also in `other`.
pub fn is_subset(set: &CpuSet, other: &CpuSet) -> bool {
    set.iter().all(|cpu| other.contains(cpu))
}

/// True when the two sets share at least one CPU.
pub fn intersects(set: &CpuSet, other: &CpuSet) -> bool {
    set.iter().any(|cpu| other.contains(cpu))
}

/// Union of two sets.
pub fn union(set: &CpuSet, other: &CpuSet) -> CpuSet {
    set.union(other).copied().collect()
}

/// Members of `set` that are not in `other`.
pub fn difference(set: &CpuSet, other: &CpuSet) -> CpuSet {
    set.difference(other).copied().collect()
}

/// Members present in both sets.
pub fn intersection(set: &CpuSet, other: &CpuSet) -> CpuSet {
    set.intersection(other).copied().collect()
}

/// Split a set into `parts` roughly equal chunks, used to spread IRQ queues or
/// trading threads across isolated cores instead of piling them onto one CPU.
pub fn chunk(set: &CpuSet, parts: usize) -> Vec<CpuSet> {
    if parts == 0 {
        return Vec::new();
    }
    let ordered: Vec<u32> = set.iter().copied().collect();
    let buckets = parts.min(ordered.len().max(1));
    let mut chunks: Vec<CpuSet> = vec![CpuSet::new(); buckets];
    for (position, cpu) in ordered.iter().enumerate() {
        chunks[position % buckets].insert(*cpu);
    }
    chunks
}

/// The lowest CPU in the set, if any.
pub fn first(set: &CpuSet) -> Option<u32> {
    set.iter().copied().next()
}

/// The highest CPU in the set, if any.
pub fn last(set: &CpuSet) -> Option<u32> {
    set.iter().copied().next_back()
}

/// True when the set contains no index gaps.
pub fn is_contiguous(set: &CpuSet) -> bool {
    match (first(set), last(set)) {
        (Some(low), Some(high)) => (high - low + 1) as usize == set.len(),
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(items: &[u32]) -> CpuSet {
        items.iter().copied().collect()
    }

    #[test]
    fn parses_single_and_ranges() {
        assert_eq!(parse("").unwrap(), set(&[]));
        assert_eq!(parse("3").unwrap(), set(&[3]));
        assert_eq!(parse("0-3").unwrap(), set(&[0, 1, 2, 3]));
        assert_eq!(parse("0-1,4,6-7").unwrap(), set(&[0, 1, 4, 6, 7]));
        assert_eq!(parse(" 2 , 5-6 ").unwrap(), set(&[2, 5, 6]));
    }

    #[test]
    fn rejects_malformed_specs() {
        assert!(parse("1-").is_err());
        assert!(parse("x").is_err());
        assert!(parse("3-1").is_err());
        assert!(parse("1,,2").is_err());
        assert!(parse("0-99999999").is_err());
    }

    #[test]
    fn formats_with_run_compression() {
        assert_eq!(format(&set(&[])), "");
        assert_eq!(format(&set(&[2])), "2");
        assert_eq!(format(&set(&[2, 3])), "2,3");
        assert_eq!(format(&set(&[2, 3, 4])), "2-4");
        assert_eq!(format(&set(&[0, 1, 4, 6, 7, 8])), "0,1,4,6-8");
        assert_eq!(format(&set(&[10, 9])), "9,10");
    }

    #[test]
    fn round_trips_through_format() {
        for spec in ["0", "0-3", "0-1,4,6-7", "1-2,5"] {
            let parsed = parse(spec).unwrap();
            assert_eq!(parse(&format(&parsed)).unwrap(), parsed);
        }
    }

    #[test]
    fn hex_mask_uses_32_bit_groups() {
        assert_eq!(to_hex_mask(&set(&[]), 8), "00000000");
        assert_eq!(to_hex_mask(&set(&[0]), 8), "00000001");
        assert_eq!(to_hex_mask(&set(&[0, 1]), 8), "00000003");
        assert_eq!(to_hex_mask(&set(&[4]), 8), "00000010");
        assert_eq!(to_hex_mask(&set(&[32]), 64), "00000001,00000000");
        assert_eq!(to_hex_mask(&set(&[0, 63]), 64), "80000000,00000001");
    }

    #[test]
    fn hex_mask_round_trips() {
        let spec = set(&[0, 1, 2, 3, 31, 32, 63]);
        assert_eq!(from_hex_mask(&to_hex_mask(&spec, 64)).unwrap(), spec);
        assert_eq!(from_hex_mask("1F").unwrap(), set(&[0, 1, 2, 3, 4]));
        assert_eq!(from_hex_mask("").unwrap(), set(&[]));
        assert!(from_hex_mask("zz").is_err());
    }

    #[test]
    fn set_algebra_is_consistent() {
        let a = set(&[0, 1, 2]);
        let b = set(&[2, 3]);
        assert_eq!(union(&a, &b), set(&[0, 1, 2, 3]));
        assert_eq!(intersection(&a, &b), set(&[2]));
        assert_eq!(difference(&a, &b), set(&[0, 1]));
        assert_eq!(complement(&set(&[1]), 4), set(&[0, 2, 3]));
        assert!(is_subset(&set(&[1]), &a));
        assert!(!is_subset(&set(&[9]), &a));
        assert!(intersects(&a, &b));
    }

    #[test]
    fn chunk_distributes_round_robin() {
        let chunks = chunk(&set(&[2, 3, 4, 5]), 2);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0], set(&[2, 4]));
        assert_eq!(chunks[1], set(&[3, 5]));
        assert!(chunk(&set(&[2, 3]), 0).is_empty());
        assert_eq!(chunk(&set(&[]), 4).len(), 1);
    }

    #[test]
    fn contiguity_and_bounds() {
        assert!(is_contiguous(&set(&[])));
        assert!(is_contiguous(&set(&[4, 5, 6])));
        assert!(!is_contiguous(&set(&[4, 6])));
        assert_eq!(first(&set(&[7, 9])), Some(7));
        assert_eq!(last(&set(&[7, 9])), Some(9));
        assert_eq!(first(&set(&[])), None);
    }
}
