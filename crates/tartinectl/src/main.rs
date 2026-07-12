//! `tartinectl` — CLI over `tartined`'s control socket (DESIGN.md §3).
//! Command surface mirrors the examples used throughout DESIGN.md; each
//! arm here is where a `tonic` client call to `tartined` would go.

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
            Some("set-replication") => unimplemented(&["file", "set-replication", "<path>", "<n>"]),
            _ => usage(),
        },
        Some("convert") => unimplemented(&["convert", "<path>", "[--wait]"]),
        Some("fs") => match args.get(2).map(String::as_str) {
            Some("set-default-replication") => {
                unimplemented(&["fs", "set-default-replication", "<n>"])
            }
            _ => usage(),
        },
        _ => usage(),
    }
}

fn unimplemented(command: &[&str]) {
    eprintln!(
        "tartinectl {}: design skeleton only — would call tartined's control socket (see DESIGN.md)",
        command.join(" ")
    );
    std::process::exit(1);
}

fn usage() {
    eprintln!(
        "usage: tartinectl <disk add|disk remove|meta set-disks|file set-replication|convert|fs set-default-replication> ..."
    );
    std::process::exit(2);
}
