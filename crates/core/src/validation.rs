//! Normalisatie en invoergrenzen uit de bestaande Store, zonder staat.
use alloc::string::String;
use spin_domain::{self as d, Fallible, List, Name, TryClone, try_push_str, try_string};

/// Een domeinconflict met veldnaam en een vaste, leesbare reden.
pub fn invalid(field: &str, why: &'static str) -> d::Error {
    d::Error::Invalid {
        field: Name::new(field),
        why,
    }
}
/// Kleine letters en geen omringende witruimte, met faalbare allocatie.
pub fn normalized(value: &str) -> Fallible<String> {
    let mut out = String::new();
    for c in value.trim().chars() {
        let c = c.to_lowercase().next().unwrap_or(c);
        let mut bytes = [0; 4];
        try_push_str(&mut out, c.encode_utf8(&mut bytes))?;
    }
    Ok(out)
}
/// Of een naam in een selector of fase-ID past.
pub fn valid_token(value: &str) -> bool {
    let value = value.trim();
    !value.is_empty()
        && value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
}
/// Normaliseert een uitbreidbare `kind:name`-selector.
pub fn selector(value: &str) -> Fallible<String> {
    let out = normalized(value)?;
    if !out
        .split_once(':')
        .is_some_and(|(kind, name)| valid_token(kind) && valid_token(name))
    {
        return Err(invalid(value, "selector must be kind:name"));
    }
    Ok(out)
}
/// Selectors zonder lege waarden of dubbelen, met behoud van de volgorde.
pub fn selectors(values: &[String]) -> Fallible<List<String>> {
    let mut out = List::new();
    for value in values {
        if value.trim().is_empty() {
            continue;
        }
        let value = selector(value)?;
        if !out.contains(&value) {
            out.push(value)?;
        }
    }
    Ok(out)
}
/// De bestaande Git-branchgrens, inclusief ticketnummers met `#`.
pub fn valid_git_base_ref(value: &str) -> bool {
    !value.is_empty()
        && !value.starts_with(['-', '.'])
        && !value.ends_with('.')
        && !value.contains("..")
        && !value.contains("@{")
        && !value.contains(['\r', '\n', '\t', ' ', '~', '^', ':', '?', '*', '[', '\\'])
        && value
            .split('/')
            .all(|p| !p.is_empty() && !p.ends_with(".lock"))
}
/// Een ticketreferentie past in één branchsegment.
pub fn valid_job_reference(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && !value.starts_with('.')
        && !value.ends_with(".lock")
        && value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"#.-_".contains(&c))
}
/// Alleen de expliciete credentialscopes zijn geldig.
pub fn valid_credential_scope(value: &str) -> bool {
    matches!(
        value,
        d::CREDENTIAL_SCOPE_USER | d::CREDENTIAL_SCOPE_GLOBAL | d::CREDENTIAL_SCOPE_PUBLIC
    )
}
/// Of deze operator een user-scoped artifact kan gebruiken.
pub fn can_use_artifact(operator: &str, artifact: &d::Artifact) -> Fallible<bool> {
    Ok(artifact.scope != d::SCOPE_USER || artifact.subject == normalized(operator)?)
}
/// De voorrangsregel bij meerdere artifacts met dezelfde selector.
pub fn artifact_scope_rank(artifact: &d::Artifact, operator: &str) -> Fallible<i32> {
    Ok(match artifact.scope.as_str() {
        d::SCOPE_USER => {
            if artifact.subject == normalized(operator)? {
                4
            } else {
                -1
            }
        }
        d::SCOPE_PROJECT => 3,
        d::SCOPE_TEAM => 2,
        d::SCOPE_GLOBAL => 1,
        _ => 0,
    })
}
/// Behoudt alleen veilige absolute paden, gesorteerd en zonder dubbelen.
pub fn clean_tracked_paths(paths: &[String]) -> Fallible<List<String>> {
    let mut out = List::new();
    for path in paths {
        let path = path.trim();
        if path.is_empty()
            || path == "/"
            || !path.starts_with('/')
            || path.contains("/../")
            || path.ends_with("/..")
            || path.contains([' ', '\t', '\r', '\n', '\'', '"', '\\', '*', '?', '[', ']'])
            || out.iter().any(|p: &String| p == path)
        {
            continue;
        }
        out.push(try_string(path)?)?;
    }
    out.as_mut_slice().sort_unstable();
    Ok(out)
}
/// Een nieuwere enablement vervangt alleen de gelijknamige descriptor.
pub fn merge_enablements(
    base: &[d::Enablement],
    overrides: &[d::Enablement],
) -> Fallible<List<d::Enablement>> {
    let mut out = List::new();
    for item in base {
        out.push(item.try_clone()?)?;
    }
    for item in overrides {
        match out
            .as_mut_slice()
            .iter_mut()
            .find(|old| old.name == item.name)
        {
            Some(old) => *old = item.try_clone()?,
            None => out.push(item.try_clone()?)?,
        }
    }
    Ok(out)
}
/// Normaliseert capabilities; onbekende namen blijven uitbreidbare metadata.
pub fn normalize_enablements(values: &[d::Enablement]) -> Fallible<List<d::Enablement>> {
    let mut out = List::new();
    for item in values {
        let mut item = item.try_clone()?;
        item.name = normalized(&item.name)?;
        item.command = try_string(item.command.trim())?;
        item.transport = normalized(&item.transport)?;
        if !valid_token(&item.name) {
            return Err(invalid(&item.name, "invalid enabled capability"));
        }
        if item.protocol_version < 0 {
            return Err(invalid(&item.name, "protocol version cannot be negative"));
        }
        if item.name == "acp" {
            if item.transport.is_empty() {
                item.transport = try_string("stdio")?;
            }
            if item.protocol_version == 0 {
                item.protocol_version = 1;
            }
        }
        match out
            .as_mut_slice()
            .iter_mut()
            .find(|old: &&mut d::Enablement| old.name == item.name)
        {
            Some(old) => *old = item,
            None => out.push(item)?,
        }
    }
    Ok(out)
}

/// Formatteert faalbaar, ook voor foutmeldingen en gegenereerde IDs.
pub fn text(args: core::fmt::Arguments<'_>) -> Fallible<String> {
    struct Buffer(String);
    impl core::fmt::Write for Buffer {
        fn write_str(&mut self, value: &str) -> core::fmt::Result {
            try_push_str(&mut self.0, value).map_err(|_| core::fmt::Error)
        }
    }
    let mut out = Buffer(String::new());
    core::fmt::write(&mut out, args).map_err(|_| d::Error::OutOfMemory)?;
    Ok(out.0)
}
