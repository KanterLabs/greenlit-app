//! Greenlit's private, container-only init helper. It mounts the overlayfs
//! (read-only repo lower layer + container-local upper) that exposes the
//! merged workspace, then execs the job command. Its bytes are embedded in
//! `litci` and extracted only into an image build context — it is never
//! installed as a host command.
//!
//! Phase 2 (execution) fills this in. This is the one crate permitted
//! `unsafe`, confined to a single documented mount-syscall module with
//! explicit safety invariants; every other crate keeps `#![forbid(unsafe_code)]`.

fn main() {
    eprintln!("greenlit-init: overlay-mount helper not yet implemented");
    std::process::exit(1);
}
