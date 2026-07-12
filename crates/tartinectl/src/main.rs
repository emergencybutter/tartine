//! `tartinectl` — thin CLI issuing `ioctl(2)`s against a mounted tartine
//! filesystem (DESIGN.md §3), the same shape as `btrfs-progs`. The same
//! commands work unchanged against the kernel module (production,
//! `kernel/`) or the FUSE prototype (`crates/tartine-fuse`,
//! `crates/tartined`), since both implement the identical ioctl numbers
//! (`kernel/tartine.h` / `tartine-fuse/src/ioctl.rs`). Command surface
//! mirrors the examples used throughout DESIGN.md; each arm here is
//! where an actual `ioctl(2)` call against the mountpoint (or
//! `/dev/tartine-ctl` for pre-mount device scan) would go.

use std::env;

fn main() {
    let args: Vec<String> = env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("disk") => match args.get(2).map(String::as_str) {
            Some("add") => unimplemented(&[
                "disk",
                "add",
                "<device>",
                "[--role data,metadata]",
                "[--weight W]",
            ]),
            Some("remove") => unimplemented(&["disk", "remove", "<disk-id>", "[--drain|--force]"]),
            _ => usage(),
        },
        Some("meta") => match args.get(2).map(String::as_str) {
            Some("set-disks") => unimplemented(&["meta", "set-disks", "<disk-a>", "<disk-b>"]),
            _ => usage(),
        },
        Some("file") => match args.get(2).map(String::as_str) {
            Some("set-redundancy") => set_redundancy(args.get(3), args.get(4)),
            Some("get-redundancy") => unimplemented(&["file", "get-redundancy", "<path>"]),
            _ => usage(),
        },
        Some("convert") => unimplemented(&["convert", "<path>", "[--wait]"]),
        Some("fs") => match args.get(2).map(String::as_str) {
            Some("set-default-redundancy") => {
                unimplemented(&["fs", "set-default-redundancy", "<spec>"])
            }
            _ => usage(),
        },
        _ => usage(),
    }
}

/// `tartinectl file set-redundancy <path> <spec>` — the one command that
/// does real work in this skeleton rather than just printing what it
/// would do: it parses `<spec>` with `tartine_core::redundancy_spec`
/// (DESIGN.md §10's grammar — "ssd", "disk:<uuid>", "3", "hdd,ssd",
/// "rs:4+2") and reports back the resulting `RedundancyScheme`, so the
/// parser and its error messages are exercised end-to-end even though
/// the actual `TARTINE_IOC_SET_REDUNDANCY` ioctl call is still a stub.
fn set_redundancy(path: Option<&String>, spec: Option<&String>) {
    let (Some(path), Some(spec)) = (path, spec) else {
        eprintln!("usage: tartinectl file set-redundancy <path> <spec>");
        std::process::exit(2);
    };

    match tartine_core::redundancy_spec::parse(spec) {
        Ok(scheme) => {
            eprintln!("tartinectl file set-redundancy {path} {spec:?}: parsed as {scheme:?}");
            eprintln!(
                "design skeleton only — would issue TARTINE_IOC_SET_REDUNDANCY against {path} (see DESIGN.md §10)"
            );
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("tartinectl: invalid redundancy spec {spec:?}: {e}");
            std::process::exit(2);
        }
    }
}

fn unimplemented(command: &[&str]) {
    eprintln!(
        "tartinectl {}: design skeleton only — would issue an ioctl(2) against the mount (see DESIGN.md)",
        command.join(" ")
    );
    std::process::exit(1);
}

fn usage() {
    eprintln!(
        "usage: tartinectl <disk add|disk remove|meta set-disks|file set-redundancy|file get-redundancy|convert|fs set-default-redundancy> ..."
    );
    std::process::exit(2);
}
