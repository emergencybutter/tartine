//! `tartined` — the FUSE **prototype's** daemon (IMPLEMENTATION_PLAN.md
//! P1.5): assembles a `Pool` from a set of disk-backing files and
//! mounts it via `fuser`. This validates the design end-to-end in
//! userspace before it's committed to kernel code (`kernel/`, the
//! actual production target — see DESIGN.md's opening note and §3/§4).
//!
//! ```text
//! tartined --disk a.img[:hdd|ssd|nvme] [--disk b.img ...] [--redb path] <mountpoint>
//! ```
//!
//! At least 1 `--disk` is required; 2 or more get real 2-disk metadata
//! replication (DESIGN.md §6). With exactly 1, the pool runs with a
//! replication factor of 1 — the same disk holds both the metadata WAL
//! and file data, and both metadata and default per-file redundancy
//! degrade accordingly (DESIGN.md §6's `min(2, disk count)` rule; see
//! `tartine_meta::pool`'s module doc comment).
//!
//! If every `--disk` path already exists, the pool is reopened
//! (`Pool::open_with_classes`, replaying the WAL and recovery-scanning
//! each disk's segment region); otherwise it's freshly formatted
//! (`Pool::format_with_classes`). The optional `:class` suffix is the
//! manual-override half of DESIGN.md §10.1's disk-class story (real
//! rotational-flag auto-detection isn't implemented); a disk with no
//! suffix defaults to `hdd`.
//!
//! Not implemented (see IMPLEMENTATION_PLAN.md's remaining Phase 1
//! milestones): the rebalancer, scrubber, and disk add/remove (P1.7),
//! repair-on-corruption (P1.8), and the async/bounded-delta-pass
//! materializer (P1.9) — `TARTINE_IOC_MAKE_WRITABLE` only supports the
//! synchronous (`--wait`) path today.

use std::path::PathBuf;

use fuser::{Config, MountOption};
use tartine_meta::pool::Pool;
use tartine_proto::DiskClass;

fn parse_class(s: &str) -> DiskClass {
    match s {
        "hdd" => DiskClass::Hdd,
        "ssd" => DiskClass::Ssd,
        "nvme" => DiskClass::Nvme,
        other => {
            eprintln!("tartined: unknown disk class {other:?} (expected hdd/ssd/nvme)");
            std::process::exit(2);
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mut disks: Vec<PathBuf> = Vec::new();
    let mut classes: Vec<DiskClass> = Vec::new();
    let mut redb_path: Option<PathBuf> = None;
    let mut mountpoint: Option<PathBuf> = None;

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--disk" => {
                i += 1;
                let spec = args.get(i).map(String::as_str).unwrap_or_else(|| usage());
                let (path, class) = match spec.split_once(':') {
                    Some((p, c)) => (p, parse_class(c)),
                    None => (spec, DiskClass::Hdd),
                };
                disks.push(PathBuf::from(path));
                classes.push(class);
            }
            "--redb" => {
                i += 1;
                redb_path = Some(PathBuf::from(args.get(i).unwrap_or_else(|| usage())));
            }
            other => {
                if mountpoint.is_some() {
                    usage();
                }
                mountpoint = Some(PathBuf::from(other));
            }
        }
        i += 1;
    }

    let (Some(mountpoint), false) = (mountpoint, disks.is_empty()) else {
        usage();
    };
    let redb_path = redb_path.unwrap_or_else(|| disks[0].with_extension("redb"));

    if disks.len() == 1 {
        eprintln!(
            "tartined: single-disk mode — metadata has no replica and files default to \
             unreplicated storage (DESIGN.md §6's min(2, disk count) replication factor); \
             add a second --disk for real redundancy"
        );
    }

    let all_exist = disks.iter().all(|p| p.exists());
    let pool = if all_exist {
        eprintln!("tartined: reopening existing pool ({} disks)", disks.len());
        Pool::open_with_classes(&disks, &classes, &redb_path)
    } else {
        eprintln!("tartined: formatting a new pool ({} disks)", disks.len());
        Pool::format_with_classes(&disks, &classes, &redb_path)
    };
    let pool = match pool {
        Ok(p) => p,
        Err(e) => {
            eprintln!("tartined: failed to open pool: {e}");
            std::process::exit(1);
        }
    };

    let fs = tartine_fuse::TartineFs::new(pool);
    // `Config` is `#[non_exhaustive]` (fuser reserves the right to add
    // fields), so it's built via `default()` + field assignment rather
    // than struct-literal syntax.
    let mut config = Config::default();
    config.mount_options = vec![
        MountOption::FSName("tartine".to_string()),
        MountOption::Subtype("tartine".to_string()),
    ];

    eprintln!("tartined: mounting on {}", mountpoint.display());
    if let Err(e) = fuser::mount2(fs, &mountpoint, &config) {
        eprintln!("tartined: mount failed: {e}");
        std::process::exit(1);
    }
}

fn usage() -> ! {
    eprintln!("usage: tartined --disk <path>[:hdd|ssd|nvme] [--disk <path>[:class] ...] [--redb <path>] <mountpoint>");
    eprintln!(
        "  (at least 1 --disk path required; the first two, if present, form the metadata group)"
    );
    std::process::exit(2);
}
