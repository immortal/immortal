//! Shared lexical contract for service identities.
//!
//! Service names cross configuration, filesystem, reconciliation, and control
//! protocol boundaries. Keeping one bounded ASCII grammar prevents any layer
//! from accepting an identity which another layer cannot represent safely.

/// Hard upper bound for a UTF-8 service name.
pub const MAX_SERVICE_NAME_BYTES: usize = 255;

pub(crate) fn is_safe_service_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_SERVICE_NAME_BYTES
        && name != "."
        && name != ".."
        && !name.starts_with('.')
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
}

#[cfg(test)]
mod tests {
    use super::{MAX_SERVICE_NAME_BYTES, is_safe_service_name};

    #[test]
    fn service_name_accepts_only_bounded_visible_ascii_identities() {
        for name in ["api", "api-v2", "api_worker", "api.worker"] {
            assert!(is_safe_service_name(name));
        }
        for name in [
            "",
            ".",
            "..",
            ".hidden",
            "api/service",
            "api service",
            "café",
        ] {
            assert!(!is_safe_service_name(name));
        }
        assert!(is_safe_service_name(&"a".repeat(MAX_SERVICE_NAME_BYTES)));
        assert!(!is_safe_service_name(
            &"a".repeat(MAX_SERVICE_NAME_BYTES + 1)
        ));
    }
}
