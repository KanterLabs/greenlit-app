//! The shared GitHub configuration-variable/secret naming rule.
//!
//! Both `${{ vars.* }}` and `${{ secrets.* }}` names follow the identical
//! character/prefix rule, documented separately for each:
//! <https://docs.github.com/en/actions/reference/workflows-and-actions/variables#naming-conventions-for-configuration-variables>
//! ("may only contain alphanumeric characters ([a-z], [A-Z], [0-9]) or
//! underscores (_). Spaces are not allowed... must not start with the
//! GITHUB_ prefix... must not start with a number... are case-insensitive")
//! and
//! <https://docs.github.com/en/actions/reference/security/secrets>
//! ("Can only contain alphanumeric characters ([a-z], [A-Z], [0-9]) or
//! underscores (_)... Must not start with the GITHUB_ prefix... Must not
//! start with a number... case insensitive when referenced"). One rule, one
//! implementation (`crate::vars::validate_name` and
//! `crate::secrets::validate_name` both re-export this), kept in its own
//! module rather than either resolution chain depending on the other.

/// Validates a configuration-variable or secret name against GitHub's
/// shared naming rule. GitHub stores/compares names case-insensitively; this
/// function validates the character set and prefix only (case folding is
/// each caller's own concern — see `crate::vars::canonical`/
/// `crate::secrets::canonical`), and deliberately invents no name-length
/// cap since GitHub documents only a value-size limit, not a name-length
/// one.
pub(crate) fn validate_configuration_name(name: &str) -> Result<(), &'static str> {
    if name.is_empty() {
        return Err("the name must not be empty");
    }
    if name
        .get(.."GITHUB_".len())
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("GITHUB_"))
    {
        return Err("names must not start with GITHUB_");
    }
    if name.as_bytes().first().is_some_and(u8::is_ascii_digit) {
        return Err("names must not start with a digit");
    }
    if !name
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    {
        return Err("names may contain only ASCII letters, digits, and underscores");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_ordinary_names() {
        assert!(validate_configuration_name("MODE").is_ok());
        assert!(validate_configuration_name("_leading_underscore").is_ok());
    }

    #[test]
    fn rejects_the_reserved_github_prefix_case_insensitively() {
        assert!(validate_configuration_name("GITHUB_TOKEN").is_err());
        assert!(validate_configuration_name("github_token").is_err());
    }

    #[test]
    fn rejects_a_leading_digit() {
        assert!(validate_configuration_name("1MODE").is_err());
    }

    #[test]
    fn rejects_non_alphanumeric_bytes() {
        assert!(validate_configuration_name("BAD-NAME").is_err());
        assert!(validate_configuration_name("").is_err());
    }
}
