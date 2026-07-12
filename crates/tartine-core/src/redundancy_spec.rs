//! Human-friendly grammar for `RedundancyScheme` (DESIGN.md §10):
//!
//! ```text
//! <N>                          -- N replicas, any disk        ("3")
//! <slot>(,<slot>)*             -- one slot per replica, in order
//! <slot> ::= "any" | "hdd" | "ssd" | "nvme" | "disk:" <uuid-hex>
//! "rs:" <k> "+" <m>            -- reserved erasure coding syntax (§16.6)
//! ```
//!
//! Examples straight from the design discussion this grammar exists to
//! serve: `"ssd"` (unreplicated, on SSD), `"disk:<uuid>"` (unreplicated,
//! pinned), `"3"` (3x, whichever disks), `"hdd,ssd"` (one HDD + one SSD,
//! so reads can hit the SSD copy).
//!
//! This lives in `tartine-core` (not `tartine-kcore`) because it's pure
//! userspace convenience — `tartinectl` parses a spec into a
//! `RedundancyScheme` client-side and issues the *structured* ioctl
//! (`kernel/tartine.h`'s `struct tartine_set_redundancy`), so the kernel
//! itself never has to parse this string. The one place a string does
//! reach the kernel directly is `setxattr(2)` on
//! `user.tartine.redundancy` (DESIGN.md §9.2's xattr convenience path,
//! mirrored here for §10) — `kernel/tartine_main.c` documents that as a
//! TODO with its own small hand-written parser, not this one, since it
//! has to be `#![no_std]`-compatible C, not this crate's `std` code.

use tartine_proto::{DiskClass, RedundancyScheme, ReplicaSlot, Uuid};

#[derive(Debug, PartialEq, Eq)]
pub enum ParseError {
    Empty,
    UnknownSlot(String),
    BadUuid(String),
    BadErasureCoding(String),
    TooManySlots(usize),
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ParseError::Empty => write!(f, "empty redundancy spec"),
            ParseError::UnknownSlot(s) => write!(
                f,
                "unknown slot {s:?} (expected any/hdd/ssd/nvme/disk:<uuid>)"
            ),
            ParseError::BadUuid(s) => write!(f, "invalid disk id {s:?}"),
            ParseError::BadErasureCoding(s) => {
                write!(f, "invalid erasure coding spec {s:?} (expected rs:<k>+<m>)")
            }
            ParseError::TooManySlots(n) => write!(
                f,
                "{n} slots exceeds the maximum of {}",
                tartine_kcore::placement::MAX_SELECT
            ),
        }
    }
}

impl std::error::Error for ParseError {}

fn parse_uuid_hex(s: &str) -> Result<Uuid, ParseError> {
    let digits: String = s.chars().filter(|c| *c != '-').collect();
    u128::from_str_radix(&digits, 16)
        .map(Uuid)
        .map_err(|_| ParseError::BadUuid(s.to_string()))
}

fn parse_slot(term: &str) -> Result<ReplicaSlot, ParseError> {
    match term {
        "any" => Ok(ReplicaSlot::AnyOfClass(None)),
        "hdd" => Ok(ReplicaSlot::AnyOfClass(Some(DiskClass::Hdd))),
        "ssd" => Ok(ReplicaSlot::AnyOfClass(Some(DiskClass::Ssd))),
        "nvme" => Ok(ReplicaSlot::AnyOfClass(Some(DiskClass::Nvme))),
        _ => {
            if let Some(id) = term.strip_prefix("disk:") {
                Ok(ReplicaSlot::Pinned(parse_uuid_hex(id)?))
            } else {
                Err(ParseError::UnknownSlot(term.to_string()))
            }
        }
    }
}

/// Parses one comma-separated redundancy spec into a `RedundancyScheme`.
/// A bare integer (`"3"`) is sugar for that many `AnyOfClass(None)`
/// slots — the common case, and backward-compatible with a plain
/// replication-factor number.
pub fn parse(spec: &str) -> Result<RedundancyScheme, ParseError> {
    let spec = spec.trim();
    if spec.is_empty() {
        return Err(ParseError::Empty);
    }

    if let Some(rs) = spec.strip_prefix("rs:") {
        let (k, m) = rs
            .split_once('+')
            .ok_or_else(|| ParseError::BadErasureCoding(spec.to_string()))?;
        let data_shards: u8 = k
            .parse()
            .map_err(|_| ParseError::BadErasureCoding(spec.to_string()))?;
        let parity_shards: u8 = m
            .parse()
            .map_err(|_| ParseError::BadErasureCoding(spec.to_string()))?;
        return Ok(RedundancyScheme::ErasureCoded {
            data_shards,
            parity_shards,
        });
    }

    if let Ok(n) = spec.parse::<usize>() {
        if n > tartine_kcore::placement::MAX_SELECT {
            return Err(ParseError::TooManySlots(n));
        }
        return Ok(RedundancyScheme::Replicated(vec![
            ReplicaSlot::AnyOfClass(
                None
            );
            n
        ]));
    }

    let slots: Vec<ReplicaSlot> = spec
        .split(',')
        .map(|t| parse_slot(t.trim()))
        .collect::<Result<_, _>>()?;
    if slots.len() > tartine_kcore::placement::MAX_SELECT {
        return Err(ParseError::TooManySlots(slots.len()));
    }
    Ok(RedundancyScheme::Replicated(slots))
}

/// Formats a `RedundancyScheme` back into spec syntax — round-trips
/// through `parse` (except that `parse("3")` normalizes to the
/// `"any,any,any"` form on the way back out, since the count-sugar
/// direction only goes one way).
pub fn format(scheme: &RedundancyScheme) -> String {
    match scheme {
        RedundancyScheme::Replicated(slots) => {
            slots.iter().map(format_slot).collect::<Vec<_>>().join(",")
        }
        RedundancyScheme::ErasureCoded {
            data_shards,
            parity_shards,
        } => format!("rs:{data_shards}+{parity_shards}"),
    }
}

fn format_slot(slot: &ReplicaSlot) -> String {
    match slot {
        ReplicaSlot::AnyOfClass(None) => "any".to_string(),
        ReplicaSlot::AnyOfClass(Some(DiskClass::Hdd)) => "hdd".to_string(),
        ReplicaSlot::AnyOfClass(Some(DiskClass::Ssd)) => "ssd".to_string(),
        ReplicaSlot::AnyOfClass(Some(DiskClass::Nvme)) => "nvme".to_string(),
        ReplicaSlot::Pinned(id) => format!("disk:{:032x}", id.0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_count_is_n_any_slots() {
        assert_eq!(
            parse("3").unwrap(),
            RedundancyScheme::Replicated(vec![ReplicaSlot::AnyOfClass(None); 3])
        );
    }

    #[test]
    fn unreplicated_on_ssd() {
        assert_eq!(
            parse("ssd").unwrap(),
            RedundancyScheme::Replicated(vec![ReplicaSlot::AnyOfClass(Some(DiskClass::Ssd))])
        );
    }

    #[test]
    fn unreplicated_pinned_to_a_disk() {
        let spec = "disk:00000000000000000000000000002a";
        assert_eq!(
            parse(spec).unwrap(),
            RedundancyScheme::Replicated(vec![ReplicaSlot::Pinned(Uuid(42))])
        );
    }

    #[test]
    fn one_hdd_one_ssd() {
        assert_eq!(
            parse("hdd,ssd").unwrap(),
            RedundancyScheme::Replicated(vec![
                ReplicaSlot::AnyOfClass(Some(DiskClass::Hdd)),
                ReplicaSlot::AnyOfClass(Some(DiskClass::Ssd)),
            ])
        );
    }

    #[test]
    fn erasure_coding_syntax_parses_but_is_reserved() {
        assert_eq!(
            parse("rs:4+2").unwrap(),
            RedundancyScheme::ErasureCoded {
                data_shards: 4,
                parity_shards: 2
            }
        );
    }

    #[test]
    fn round_trips_through_format() {
        for spec in ["any", "ssd", "hdd,ssd", "rs:4+2"] {
            let scheme = parse(spec).unwrap();
            assert_eq!(parse(&format(&scheme)).unwrap(), scheme);
        }
    }

    #[test]
    fn rejects_garbage() {
        assert!(matches!(parse("banana"), Err(ParseError::UnknownSlot(_))));
        assert!(matches!(parse(""), Err(ParseError::Empty)));
        assert!(matches!(parse("disk:not-hex"), Err(ParseError::BadUuid(_))));
        assert!(matches!(
            parse("rs:4"),
            Err(ParseError::BadErasureCoding(_))
        ));
    }
}
