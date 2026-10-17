//! Git-remotes blijven credential-vrije, begrensde metadata.
use crate::validation::normalized;
use alloc::string::String;
use spin_domain::{self as d, Fallible, try_string};

/// De bestaande Git-schemes, zonder HTTP-userinfo, querydata of SSH-wachtwoord.
pub fn valid_remote(value: &str) -> bool {
    if value.is_empty() || value.bytes().any(|b| b <= b' ' || b == 127) {
        return false;
    }
    if let Some(scp) = value.strip_prefix("git@") {
        return scp.contains(':');
    }
    let Some((scheme, rest)) = value.split_once("://") else {
        return false;
    };
    if !matches!(scheme, "http" | "https" | "git" | "ssh") {
        return false;
    }
    let mut escapes = value.bytes();
    while let Some(b) = escapes.next() {
        if b == b'%'
            && !(escapes.next().is_some_and(|b| b.is_ascii_hexdigit())
                && escapes.next().is_some_and(|b| b.is_ascii_hexdigit()))
        {
            return false;
        }
    }
    let (before_fragment, fragment) = rest.split_once('#').unwrap_or((rest, ""));
    if !fragment.is_empty() {
        return false;
    }
    let (before_query, query) = before_fragment
        .split_once('?')
        .unwrap_or((before_fragment, ""));
    if !query.is_empty() {
        return false;
    }
    let authority = before_query.split('/').next().unwrap_or_default();
    if authority.is_empty() {
        return false;
    }
    let host = if let Some((userinfo, host)) = authority.rsplit_once('@') {
        if scheme != "ssh" || userinfo.contains(':') {
            return false;
        }
        host
    } else {
        authority
    };
    if host.contains([' ', '\\', '"', '<', '>', '^', '`', '{', '|', '}']) {
        return false;
    }
    let port = if let Some(bracket) = host.strip_prefix('[') {
        let Some((_, suffix)) = bracket.split_once(']') else {
            return false;
        };
        if suffix.is_empty() {
            None
        } else if let Some(port) = suffix.strip_prefix(':') {
            Some(port)
        } else {
            return false;
        }
    } else {
        if host.contains(['[', ']', '%']) {
            return false;
        }
        host.rsplit_once(':').map(|(_, p)| p)
    };
    port.is_none_or(|p| p.bytes().all(|b| b.is_ascii_digit()))
}
/// Onbekende scopes gebruiken de contextafhankelijke standaard zoals voorheen.
pub fn credential_scope(value: &str, fallback: &str) -> Fallible<String> {
    let value = normalized(value)?;
    if matches!(
        value.as_str(),
        d::CREDENTIAL_SCOPE_USER | d::CREDENTIAL_SCOPE_GLOBAL | d::CREDENTIAL_SCOPE_PUBLIC
    ) {
        Ok(value)
    } else {
        try_string(fallback)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn remotes_do_not_embed_http_tokens_or_ssh_passwords() {
        for remote in [
            "https://github.com/a/b.git",
            "git@github.com:a/b.git",
            "ssh://git@example.com:2222/repo",
            "git://example.com/repo",
            "https://[::1]:3000/repo",
        ] {
            assert!(valid_remote(remote), "{remote}");
        }
        for remote in [
            "https://token@github.com/a/b",
            "ssh://git:password@host/repo",
            "file:///tmp/repo",
            "https://host/repo?token=secret",
            "https://host/repo#branch",
            "-option",
            "https://host:bad/repo",
            "https://host/repo%xx",
        ] {
            assert!(!valid_remote(remote), "{remote}");
        }
    }
}
