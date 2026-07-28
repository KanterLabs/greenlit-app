//! The run's `ACTIONS_RUNTIME_TOKEN`.
//!
//! Two jobs at once, which is why this is not just a random string.
//!
//! **It authenticates.** The shim is reachable from the job network, so this
//! token is the only thing between one container and another run's cache. It
//! is drawn from the kernel CSPRNG, not derived from the pid and clock.
//!
//! **It carries the run's identity.** `upload-artifact` does not read a
//! backend id from the environment — it *decodes this token as a JWT* and
//! pulls the ids out of the `scp` claim
//! (`packages/artifact/src/internal/shared/util.ts`,
//! `getBackendIdsFromToken`), looking for a space-separated scope of the form
//! `Actions.Results:<workflowRunBackendId>:<workflowJobRunBackendId>` with
//! exactly three colon-separated parts. A token that is merely random fails
//! with `Invalid token specified: Cannot read properties of undefined
//! (reading 'replace')` — which is what the `full-ci` fixture hit the first
//! time a real `upload-artifact` reached the shim, and which no synthetic
//! twirp test could have caught, because those tests pass the backend ids
//! explicitly.
//!
//! Nothing verifies the signature. Greenlit's shim compares the whole token
//! as an opaque bearer string, and there is no second party to prove
//! anything to — so the "signature" segment is simply more entropy, which is
//! what keeps the token unguessable while also making it decode.
//!
//! The run id is constant for the whole `litci run`, which is what lets an
//! artifact uploaded in one job be downloaded in another: the store scopes
//! artifacts by that id.

/// A minted runtime token, plus the identity encoded in it.
#[derive(Debug, Clone)]
pub(crate) struct RuntimeToken {
    /// The token handed to the job as `ACTIONS_RUNTIME_TOKEN`.
    pub value: String,
    /// The signature blob URLs carry in place of a bearer header.
    ///
    /// Blob clients send no `Authorization`: `@azure/storage-blob` treats a
    /// signed URL as self-authorizing, and `actions/cache` downloads its
    /// `archiveLocation` with a bare HTTP client. Kept distinct from the
    /// bearer token so the token never lands in a URL.
    pub url_signature: String,
}

/// Mints a token for one run.
///
/// Returns `None` when the CSPRNG cannot be read. The caller then runs with
/// no shim at all rather than with a predictable credential — an honest
/// cache miss beats a guessable token.
pub(crate) fn mint() -> Option<RuntimeToken> {
    let run_id = random_hex(16)?;
    let job_id = random_hex(16)?;
    let signature = random_hex(32)?;

    let header = base64url(br#"{"alg":"HS256","typ":"JWT"}"#);
    let claims = format!(r#"{{"scp":"Actions.Results:{run_id}:{job_id}"}}"#);
    let payload = base64url(claims.as_bytes());

    Some(RuntimeToken {
        value: format!("{header}.{payload}.{signature}"),
        url_signature: random_hex(32)?,
    })
}

/// `count` random bytes as lowercase hex, straight from the kernel CSPRNG.
fn random_hex(count: usize) -> Option<String> {
    use std::io::Read;
    let mut bytes = vec![0_u8; count];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(&mut bytes))
        .ok()?;
    Some(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

/// Base64url without padding, as JWT requires.
///
/// Hand-rolled rather than pulling in a base64 crate for one 20-line
/// function used in exactly one place.
fn base64url(input: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let mut buffer = [0_u8; 3];
        buffer[..chunk.len()].copy_from_slice(chunk);
        let packed =
            (u32::from(buffer[0]) << 16) | (u32::from(buffer[1]) << 8) | u32::from(buffer[2]);

        // One output character per 6 bits, minus the characters that would
        // encode only padding.
        let characters = chunk.len() + 1;
        for index in 0..characters {
            let shift = 18 - 6 * index;
            let value = ((packed >> shift) & 0b0011_1111) as usize;
            out.push(char::from(ALPHABET[value]));
        }
    }
    out
}
