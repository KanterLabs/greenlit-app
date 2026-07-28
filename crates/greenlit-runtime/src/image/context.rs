//! Build-context assembly and content addressing for the base image.
//!
//! The Docker `/build` endpoint takes a tar of the build context. Greenlit
//! assembles it entirely in memory (`AGENTS.md` "Tech": no shelling out, one
//! distributable binary) from two entries: the embedded `Dockerfile` and the
//! embedded `greenlit-init` helper. The image is content-addressed — its tag
//! carries a hash of everything that determines the built image, so identical
//! inputs reuse an existing image and any change to the Dockerfile, the base
//! release, or the helper produces a fresh tag.

use std::io;

use tar::{Builder, Header};

/// In-context filename of the Dockerfile (the `dockerfile` build parameter).
pub(crate) const DOCKERFILE_NAME: &str = "Dockerfile";

/// In-context filename of the embedded helper. The Dockerfile `COPY greenlit-init`
/// line reads exactly this name.
pub(crate) const INIT_CONTEXT_NAME: &str = "greenlit-init";

/// Tar mode for the Dockerfile (regular, world-readable).
const MODE_FILE: u32 = 0o644;
/// Tar mode for the helper (executable — it is the container entrypoint).
const MODE_EXEC: u32 = 0o755;

/// FNV-1a 64-bit offset basis.
const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
/// FNV-1a 64-bit prime.
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// A deterministic 16-hex-digit content hash over the image's inputs.
///
/// FNV-1a is used rather than a cryptographic digest because this is a
/// cache/identity key, not a security boundary: it only needs to be stable and
/// collision-free across the handful of distinct base images a repo builds. A
/// dependency-free hand-rolled hash keeps it deterministic across toolchains
/// (unlike `std`'s `DefaultHasher`) and adds no crate.
pub(crate) fn content_hash(dockerfile: &str, base_ref: &str, init_bytes: &[u8]) -> String {
    let mut hash = FNV_OFFSET;
    hash = fnv1a(dockerfile.as_bytes(), hash);
    hash = fnv1a(base_ref.as_bytes(), hash);
    hash = fnv1a(init_bytes, hash);
    format!("{hash:016x}")
}

/// Fold `data` into a running FNV-1a hash.
fn fnv1a(data: &[u8], mut hash: u64) -> u64 {
    for &byte in data {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

/// Assemble the in-memory tar build context: the Dockerfile plus the helper.
///
/// # Errors
///
/// Returns the underlying [`io::Error`] if writing an entry into the in-memory
/// archive fails — practically impossible for a `Vec` sink, but surfaced rather
/// than unwrapped.
pub(crate) fn build_context_tar(dockerfile: &str, init_bytes: &[u8]) -> io::Result<Vec<u8>> {
    let mut builder = Builder::new(Vec::new());
    append_entry(
        &mut builder,
        DOCKERFILE_NAME,
        dockerfile.as_bytes(),
        MODE_FILE,
    )?;
    append_entry(&mut builder, INIT_CONTEXT_NAME, init_bytes, MODE_EXEC)?;
    builder.into_inner()
}

/// Append one regular-file entry with an explicit mode.
fn append_entry(
    builder: &mut Builder<Vec<u8>>,
    name: &str,
    data: &[u8],
    mode: u32,
) -> io::Result<()> {
    let mut header = Header::new_gnu();
    header.set_size(data.len() as u64);
    header.set_mode(mode);
    header.set_cksum();
    builder.append_data(&mut header, name, data)
}
