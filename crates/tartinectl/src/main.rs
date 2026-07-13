//! `tartinectl` — thin CLI issuing real `ioctl(2)`s against a mounted
//! tartine filesystem (DESIGN.md §3), the same shape as `btrfs-progs`.
//! The same commands work unchanged against the kernel module
//! (production, `kernel/`) or the FUSE prototype (`crates/tartine-fuse`,
//! `crates/tartined`), since both implement the identical ioctl numbers
//! (`kernel/tartine.h` / `tartine-fuse/src/ioctl.rs` — this binary reuses
//! the latter directly rather than redefining the wire format a third
//! time). `disk add/remove`, `meta set-disks`, and
//! `fs set-default-redundancy` remain unimplemented (IMPLEMENTATION_PLAN.md
//! P1.7 and later): they need a rebalancer/pool-topology mechanism that
//! doesn't exist in the prototype yet.

use std::fs::OpenOptions;
use std::os::unix::io::AsRawFd;

use tartine_fuse::ioctl;

fn main() {
    let args: Vec<String> = std::env::args().collect();
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
            Some("set-redundancy") => set_redundancy(arg(&args, 3), arg(&args, 4)),
            Some("get-redundancy") => get_redundancy(arg(&args, 3)),
            _ => usage(),
        },
        Some("convert") => convert(arg(&args, 2), args.iter().any(|a| a == "--wait")),
        Some("fs") => match args.get(2).map(String::as_str) {
            Some("set-default-redundancy") => {
                unimplemented(&["fs", "set-default-redundancy", "<spec>"])
            }
            _ => usage(),
        },
        _ => usage(),
    }
}

fn arg<'a>(args: &'a [String], i: usize) -> &'a str {
    args.get(i).map(String::as_str).unwrap_or_else(|| usage())
}

fn open_for_ioctl(path: &str) -> std::fs::File {
    match OpenOptions::new().read(true).write(true).open(path) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("tartinectl: cannot open {path}: {e}");
            std::process::exit(1);
        }
    }
}

fn check_ioctl(ret: i32, action: &str) {
    if ret != 0 {
        eprintln!(
            "tartinectl: {action} failed: {}",
            std::io::Error::last_os_error()
        );
        std::process::exit(1);
    }
}

/// `tartinectl file set-redundancy <path> <spec>`: parses `<spec>` with
/// `tartine_core::redundancy_spec` (DESIGN.md §10's grammar — "ssd",
/// "disk:<uuid>", "3", "hdd,ssd", "rs:4+2") and issues
/// `TARTINE_IOC_SET_REDUNDANCY`.
fn set_redundancy(path: &str, spec: &str) {
    let scheme = match tartine_core::redundancy_spec::parse(spec) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("tartinectl: invalid redundancy spec {spec:?}: {e}");
            std::process::exit(2);
        }
    };
    let file = open_for_ioctl(path);
    let wire = ioctl::encode_set_redundancy(&scheme);
    // SAFETY: `wire` is a valid, fully-initialized REDUNDANCY_WIRE_SIZE
    // buffer for the duration of the call, matching what
    // TARTINE_IOC_SET_REDUNDANCY expects to read.
    let ret = unsafe {
        libc::ioctl(
            file.as_raw_fd(),
            ioctl::TARTINE_IOC_SET_REDUNDANCY as libc::c_ulong,
            wire.as_ptr(),
        )
    };
    check_ioctl(ret, "set-redundancy");
    println!("tartinectl: {path}: redundancy set to {spec:?}");
}

fn get_redundancy(path: &str) {
    let file = open_for_ioctl(path);
    let mut wire = [0u8; ioctl::REDUNDANCY_WIRE_SIZE];
    // SAFETY: `wire` is a valid, writable REDUNDANCY_WIRE_SIZE buffer for
    // the duration of the call, matching what TARTINE_IOC_GET_REDUNDANCY
    // expects to fill in.
    let ret = unsafe {
        libc::ioctl(
            file.as_raw_fd(),
            ioctl::TARTINE_IOC_GET_REDUNDANCY as libc::c_ulong,
            wire.as_mut_ptr(),
        )
    };
    check_ioctl(ret, "get-redundancy");
    match ioctl::decode_set_redundancy(&wire) {
        Some(scheme) => println!(
            "tartinectl: {path}: redundancy = {}",
            tartine_core::redundancy_spec::format(&scheme)
        ),
        None => {
            eprintln!("tartinectl: malformed redundancy response");
            std::process::exit(1);
        }
    }
}

/// `tartinectl convert <path> [--wait]`: issues
/// `TARTINE_IOC_MAKE_WRITABLE` (DESIGN.md §9.2).
fn convert(path: &str, wait: bool) {
    let file = open_for_ioctl(path);
    let flags: u32 = if wait {
        ioctl::TARTINE_CONVERT_FLAG_WAIT
    } else {
        0
    };
    // SAFETY: `&flags` is a valid `u32` for the duration of the call,
    // matching what TARTINE_IOC_MAKE_WRITABLE expects to read.
    let ret = unsafe {
        libc::ioctl(
            file.as_raw_fd(),
            ioctl::TARTINE_IOC_MAKE_WRITABLE as libc::c_ulong,
            &flags,
        )
    };
    check_ioctl(ret, "convert");
    println!(
        "tartinectl: {path}: conversion {}",
        if wait {
            "complete"
        } else {
            "started (async — poll with GET_STATE)"
        }
    );
}

fn unimplemented(command: &[&str]) -> ! {
    eprintln!(
        "tartinectl {}: not implemented yet — needs the rebalancer/pool-topology mechanism from IMPLEMENTATION_PLAN.md P1.7",
        command.join(" ")
    );
    std::process::exit(1);
}

fn usage() -> ! {
    eprintln!(
        "usage: tartinectl <disk add|disk remove|meta set-disks|file set-redundancy|file get-redundancy|convert|fs set-default-redundancy> ..."
    );
    std::process::exit(2);
}
