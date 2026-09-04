//! The btrfs usage parser. `btrfs filesystem usage` is the only thing that reports a btrfs pool
//! honestly — `df` on a btrfs filesystem reports allocation, not usage, and reads far under the
//! point at which allocations start failing (which is exactly what `PoolAlmostFull` exists to
//! catch). Untrusted text: this runs on whatever the installed btrfs-progs prints.

use kloudlite_git_agent::stats::{parse_btrfs_usage, statvfs_usage};

const USAGE: &str = "\
Overall:
    Device size:                1000000000000
    Device allocated:            600000000000
    Device unallocated:          400000000000
    Device missing:                        0
    Used:                        499999997952
    Free (estimated):            480000000000      (min: 280000000000)
    Data ratio:                            1.00
";

#[test]
fn parses_device_size_and_used_in_bytes() {
    let (used, total) = parse_btrfs_usage(USAGE).expect("the -b output must parse");
    assert_eq!(total, 1_000_000_000_000);
    assert_eq!(used, 499_999_997_952);
}

/// `Used:` appears again under each device section; the Overall figure is the first and the only
/// one that means the whole pool.
#[test]
fn takes_the_overall_used_not_a_per_device_one() {
    let text = format!("{USAGE}\nData,single: Size:600000000000, Used:1\n   /dev/sdb  600000000000\n");
    let (used, _) = parse_btrfs_usage(&text).unwrap();
    assert_eq!(used, 499_999_997_952);
}

/// A non-btrfs mount, a missing binary, an error message on stdout — anything unparsable must be
/// `None`, so the caller falls back rather than exporting a zero that reads as an empty pool.
#[test]
fn unparsable_output_is_none_rather_than_zero() {
    assert!(parse_btrfs_usage("").is_none());
    assert!(parse_btrfs_usage("ERROR: not a btrfs filesystem").is_none());
    assert!(parse_btrfs_usage("Overall:\n    Device size:  not-a-number\n").is_none());
}

/// The fallback has to work on the machine running the test — `/` exists everywhere, including in
/// CI and on the developer's Mac.
#[test]
fn statvfs_reports_a_plausible_root_filesystem() {
    let (used, total) = statvfs_usage("/").expect("/ must be statvfs-able");
    assert!(total > 0, "a filesystem with zero total blocks is not a filesystem");
    assert!(used <= total, "used {used} exceeds total {total}");
}

#[test]
fn statvfs_of_a_missing_path_is_none() {
    assert!(statvfs_usage("/definitely/not/a/path/here").is_none());
}
