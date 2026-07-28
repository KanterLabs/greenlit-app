//! Credential-free normalization of the Git origin retained in a snapshot.

/// Returns a usable credential-free identity for an origin URL.
///
/// Network URLs retain only a recognized transport, host, path, and a
/// non-secret SSH username. HTTP userinfo is always discarded because bearer
/// tokens are commonly placed in either userinfo field. Query and fragment
/// data are never part of the repository identity and are discarded in full.
/// Unknown URI schemes and structurally ambiguous percent escapes fail closed.
pub(super) fn credential_free_identity(origin: &str) -> Result<String, ()> {
    if origin.is_empty() || origin.chars().any(char::is_control) {
        return Err(());
    }

    let sanitized = if let Some((scheme, remainder)) = origin.split_once("://") {
        sanitize_uri(scheme, remainder)?
    } else {
        sanitize_scp_or_path(origin)?
    };
    if sanitized.is_empty() || contains_bearer_shape(&decoded_rounds(&sanitized)?) {
        return Err(());
    }
    Ok(sanitized)
}

fn sanitize_uri(scheme: &str, remainder: &str) -> Result<String, ()> {
    if scheme.is_empty()
        || !scheme.bytes().enumerate().all(|(index, byte)| {
            byte.is_ascii_alphabetic()
                || (index > 0 && (byte.is_ascii_digit() || matches!(byte, b'+' | b'-' | b'.')))
        })
    {
        return Err(());
    }
    let scheme = scheme.to_ascii_lowercase();
    let identity = strip_query_and_fragment(remainder);
    match scheme.as_str() {
        "http" | "https" | "git" => {
            let (authority, path) = split_authority(identity)?;
            let host = authority
                .rsplit_once('@')
                .map_or(authority, |(_, host)| host);
            validate_authority(host)?;
            Ok(format!("{scheme}://{host}{path}"))
        }
        "ssh" | "git+ssh" | "ssh+git" => {
            let (authority, path) = split_authority(identity)?;
            let authority = sanitize_ssh_authority(authority)?;
            Ok(format!("{scheme}://{authority}{path}"))
        }
        "file" => {
            if !identity.starts_with('/') && !identity.starts_with("localhost/") {
                return Err(());
            }
            decoded_rounds(identity)?;
            Ok(format!("{scheme}://{identity}"))
        }
        _ => Err(()),
    }
}

fn strip_query_and_fragment(value: &str) -> &str {
    let end = value
        .char_indices()
        .find_map(|(index, character)| matches!(character, '?' | '#').then_some(index))
        .unwrap_or(value.len());
    &value[..end]
}

fn split_authority(value: &str) -> Result<(&str, &str), ()> {
    let split = value.find('/').unwrap_or(value.len());
    let (authority, path) = value.split_at(split);
    if authority.is_empty() {
        return Err(());
    }
    Ok((authority, path))
}

fn sanitize_ssh_authority(authority: &str) -> Result<String, ()> {
    let Some((userinfo, host)) = authority.rsplit_once('@') else {
        validate_authority(authority)?;
        return Ok(authority.to_string());
    };
    let username = userinfo.split_once(':').map_or(userinfo, |(name, _)| name);
    if username.is_empty()
        || !username
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
        || contains_bearer_shape(username.as_bytes())
    {
        return Err(());
    }
    validate_authority(host)?;
    Ok(format!("{username}@{host}"))
}

fn validate_authority(authority: &str) -> Result<(), ()> {
    if authority.is_empty()
        || authority
            .bytes()
            .any(|byte| byte.is_ascii_whitespace() || matches!(byte, b'%' | b'\\' | b'@'))
    {
        return Err(());
    }
    Ok(())
}

fn sanitize_scp_or_path(origin: &str) -> Result<String, ()> {
    let identity = strip_query_and_fragment(origin);
    if identity != origin {
        // In a path or SCP-like remote these bytes are not unambiguously URL
        // query/fragment delimiters, so changing them would silently select a
        // different repository.
        return Err(());
    }
    if let Some(colon) = identity.find(':') {
        let authority = &identity[..colon];
        let path = &identity[colon + 1..];
        let looks_like_scp = authority.contains('@') || authority.contains('.');
        if !looks_like_scp || path.is_empty() {
            return Err(());
        }
        let host = authority
            .rsplit_once('@')
            .map_or(authority, |(username, host)| {
                if username.is_empty()
                    || !username.bytes().all(|byte| {
                        byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-')
                    })
                    || contains_bearer_shape(username.as_bytes())
                {
                    return "";
                }
                host
            });
        validate_authority(host)?;
    }
    decoded_rounds(identity)?;
    Ok(identity.to_string())
}

fn decoded_rounds(value: &str) -> Result<Vec<u8>, ()> {
    let mut decoded = value.as_bytes().to_vec();
    for _ in 0..3 {
        if !decoded.contains(&b'%') {
            break;
        }
        decoded = percent_decode(&decoded)?;
    }
    if decoded.contains(&b'%') {
        // Do not retain an encoding that would disclose a credential only
        // after more decoding rounds than Greenlit's bounded matcher applies.
        Err(())
    } else {
        Ok(decoded)
    }
}

fn percent_decode(value: &[u8]) -> Result<Vec<u8>, ()> {
    let mut decoded = Vec::with_capacity(value.len());
    let mut index = 0;
    while index < value.len() {
        if value[index] != b'%' {
            decoded.push(value[index]);
            index += 1;
            continue;
        }
        let high = value
            .get(index + 1)
            .copied()
            .and_then(hex_value)
            .ok_or(())?;
        let low = value
            .get(index + 2)
            .copied()
            .and_then(hex_value)
            .ok_or(())?;
        decoded.push((high << 4) | low);
        index += 3;
    }
    Ok(decoded)
}

fn hex_value(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

fn contains_bearer_shape(value: &[u8]) -> bool {
    let lower = value.iter().map(u8::to_ascii_lowercase).collect::<Vec<_>>();
    if lower
        .windows(b"authorization".len())
        .any(|window| window == b"authorization")
        || lower
            .windows(b"bearer ".len())
            .any(|window| window == b"bearer ")
    {
        return true;
    }
    [
        b"ghp_".as_slice(),
        b"gho_",
        b"ghu_",
        b"ghs_",
        b"ghr_",
        b"github_pat_",
    ]
    .iter()
    .any(|prefix| {
        lower
            .windows(prefix.len())
            .enumerate()
            .any(|(index, window)| {
                if window != *prefix {
                    return false;
                }
                lower[index + prefix.len()..]
                    .iter()
                    .take_while(|byte| byte.is_ascii_alphanumeric() || **byte == b'_')
                    .take(12)
                    .count()
                    >= 12
            })
    })
}
