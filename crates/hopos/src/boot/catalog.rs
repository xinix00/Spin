//! Small S3 domain index, independent of database segment counts.
use super::*;
use alloc::string::String;
use spin_runtime::tenancy::{TENANTS, Tenants};
fn enabled(app: &App) -> bool {
    app.env("SPIN_DATABASE").is_none_or(|s| s.is_empty())
        && !app
            .env("SPIN_REPLICATION")
            .unwrap_or("")
            .eq_ignore_ascii_case("off")
}
fn prefix(app: &App) -> Result<String> {
    let prefix = app
        .env("SPIN_S3_PREFIX")
        .unwrap_or("spin")
        .trim_matches('/');
    spin_core::validation::text(format_args!(
        "{}/.spin-domains/",
        if prefix.is_empty() { "spin" } else { prefix }
    ))
    .map_err(Error::from)
}
fn client(app: &App) -> Result<leans3::Client> {
    let env = |key| app.env(key).unwrap_or("").trim();
    for key in [
        "SPIN_S3_ENDPOINT",
        "SPIN_S3_BUCKET",
        "SPIN_S3_ACCESS_KEY",
        "SPIN_S3_SECRET_KEY",
    ] {
        if env(key).is_empty() {
            return Err(Error::Http(500, "Replica needs SPIN_S3 configuration"));
        }
    }
    Ok(leans3::Client {
        endpoint: spin_domain::try_string(env("SPIN_S3_ENDPOINT"))?,
        bucket: spin_domain::try_string(env("SPIN_S3_BUCKET"))?,
        region: spin_domain::try_string(if env("SPIN_S3_REGION").is_empty() {
            "us-east-1"
        } else {
            env("SPIN_S3_REGION")
        })?,
        access_key_id: spin_domain::try_string(env("SPIN_S3_ACCESS_KEY"))?,
        secret_access_key: spin_domain::try_string(env("SPIN_S3_SECRET_KEY"))?,
        session_token: String::new(),
        path_style: true,
        now: Some(|| applib::app().and_then(|a| a.wall_ns()).unwrap_or(0) / 1_000_000_000),
    })
}
pub(super) async fn register(app: &'static App, domain: &str) -> Result {
    if !enabled(app) {
        return Ok(());
    }
    let key = spin_core::validation::text(format_args!("{}{domain}", prefix(app)?))?;
    let mut network = crate::s3::network();
    client(app)?
        .put(
            &mut network,
            &key,
            b"spin-domain-v1\n",
            &leans3::PutOptions::default(),
        )
        .await
        .map_err(|error| {
            crate::s3::report(&network);
            failure(error)
        })?;
    Ok(())
}
pub(super) async fn discover(app: &'static App, tenants: &Tenants) -> Result {
    if !enabled(app) {
        return Ok(());
    }
    let prefix = prefix(app)?;
    let mut network = crate::s3::network();
    let (keys, truncated) = client(app)?
        .list(&mut network, &prefix, TENANTS)
        .await
        .map_err(|error| {
            crate::s3::report(&network);
            failure(error)
        })?;
    if truncated {
        return Err(Error::Http(
            503,
            "Replica domain catalog exceeds native tenant capacity",
        ));
    }
    for key in keys {
        if let Some(domain) = key.strip_prefix(&prefix) {
            tenants.discover(domain)?;
        }
    }
    Ok(())
}
