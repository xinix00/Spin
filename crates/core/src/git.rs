//! Git-remotes en apprecepten blijven credential-vrije, begrensde metadata.
use crate::validation::{invalid, normalized, text, valid_token};
use alloc::string::String;
use spin_domain::{self as d, Fallible, List, Map, try_string};

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
/// Een appservice heeft een run-command óf image, unieke naam en geldige poorten.
pub fn app_services(values: List<d::AppService>) -> Fallible<List<d::AppService>> {
    let mut out = List::default();
    let mut seen = Map::new();
    for mut service in values.into_vec() {
        service.name = normalized(&service.name)?;
        service.image = try_string(service.image.trim())?;
        service.run = try_string(service.run.trim())?;
        service.env = normalized(&service.env)?;
        if !valid_token(&service.name) || seen.contains_key(&service.name) {
            return Err(invalid("services", "service needs a unique token name"));
        }
        seen.insert(try_string(&service.name)?, true)?;
        if service.run.is_empty() == service.image.is_empty() {
            return Err(invalid(
                "services",
                "service needs either a run command or an image",
            ));
        }
        if !service.env.is_empty() && !valid_token(&service.env) {
            return Err(invalid("services", "service env must be a token name"));
        }
        let mut prepare = List::new();
        for command in service.prepare.iter() {
            if !command.trim().is_empty() {
                prepare.push(try_string(command.trim())?)?;
            }
        }
        service.prepare = if service.image.is_empty() {
            prepare
        } else {
            List::default()
        };
        let mut ports = List::new();
        for port in service.ports.iter() {
            if *port < 1 || *port > 65535 {
                return Err(invalid("services", "port out of range"));
            }
            if !ports.contains(port) {
                ports.push(*port)?;
            }
        }
        service.ports = ports;
        out.push(service)?;
    }
    Ok(out)
}
/// Het bestaande name:ip-formaat voor Docker's add-host; dubbele namen zijn ongeldig.
pub fn service_hosts(values: &[String]) -> Fallible<List<String>> {
    let mut out = List::default();
    let mut seen = Map::new();
    for entry in values {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }
        let mut fields = entry.split([':', ' ', '\t', '=']).filter(|s| !s.is_empty());
        let (Some(name), Some(address)) = (fields.next(), fields.next()) else {
            return Err(invalid("service_hosts", "host entry must be name:ip"));
        };
        if fields.next().is_some() || address.parse::<core::net::IpAddr>().is_err() {
            return Err(invalid(
                "service_hosts",
                "host entry has no valid IP address",
            ));
        }
        let name = normalized(name)?;
        if name.contains(['/', '@', '?', '#', '\r', '\n', '\t', ' ']) || seen.contains_key(&name) {
            return Err(invalid(
                "service_hosts",
                "host name is invalid or duplicated",
            ));
        }
        seen.insert(try_string(&name)?, true)?;
        out.push(text(format_args!("{name}:{address}"))?)?;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use d::Wire;
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
    #[test]
    fn services_preserve_command_order_but_deduplicate_ports() {
        let input=List::<d::AppService>::from_json(br#"[{"name":" APP ","run":" npm start ","prepare":[" npm ci ","", " npm ci "],"ports":[8080,8080,3000]}]"#).unwrap();
        let out = app_services(input).unwrap();
        assert_eq!(out[0].name, "app");
        assert_eq!(out[0].prepare.len(), 2);
        assert_eq!(&*out[0].ports, &[8080, 3000]);
        assert!(
            app_services(
                List::from_json(br#"[{"name":"x","run":"run","image":"image"}]"#).unwrap()
            )
            .is_err()
        );
        assert!(service_hosts(&["db:127.0.0.1".into(), "DB 127.0.0.2".into()]).is_err());
        assert_eq!(
            &*service_hosts(&["DB = 127.0.0.1".into()]).unwrap(),
            &[String::from("db:127.0.0.1")]
        );
    }
}
