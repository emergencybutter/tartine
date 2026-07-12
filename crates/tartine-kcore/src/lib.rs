//! The kernel-safe core: placement (HRW) and the append-only /
//! converting / writable state machine, as plain functions with no
//! allocation and no floating point, exposed over a C ABI.
//!
//! This crate is built two ways (see `Cargo.toml`'s `freestanding`
//! feature and DESIGN.md's "kernel/" section for the full rationale):
//!
//! - **As a plain Rust dependency of `tartine-fuse`** (feature off): an
//!   ordinary `std`-linked `rlib`. The FUSE prototype calls these
//!   functions directly, so it validates the exact logic that will later
//!   ship in the kernel module — not a lookalike reimplementation.
//! - **As the `staticlib` linked into the C kernel module** (feature on):
//!   built `#![no_std]` with its own panic handler, since there is no
//!   libstd (or libc) underneath a kernel module at all. This is also
//!   why the crate has zero dependencies and never touches `alloc`:
//!   heap allocation across the FFI boundary would mean either wiring a
//!   custom `#[global_allocator]` backed by the kernel's `kmalloc`, or
//!   passing a Rust allocator handle across the boundary — both solvable,
//!   but unnecessary complexity for functions that are naturally
//!   stateless (score/select over caller-owned arrays, classify a
//!   read-only mode+offset+size triple). Every function here takes
//!   borrowed slices/primitives in and writes into caller-provided
//!   buffers.
//!
//! What building `freestanding` in *this* sandbox does and doesn't prove:
//! it compiles clean as `#![no_std]` for the host target, which catches
//! any accidental `std`/`alloc` dependency in the logic itself. It does
//! **not** prove the resulting object is safe to link into a running
//! kernel — that additionally requires the kernel-specific codegen flags
//! described in DESIGN.md (no red zone, kernel code model, no SIMD/FP
//! instructions, resolving `memcpy`/`memset`/etc. against the kernel's
//! own definitions instead of libc's), which need a real kernel build
//! tree to select and verify and aren't available here.

#![cfg_attr(all(not(test), feature = "freestanding"), no_std)]

mod hash;
pub mod placement;
pub mod write_path;

#[cfg(all(not(test), feature = "freestanding"))]
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {}
}
