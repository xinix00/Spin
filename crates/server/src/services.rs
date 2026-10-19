//! De spin-services van de clusters: hun live status, hun crashes, en de
//! aanroepen voor logs en herstart vanuit een workflowstap.
//!
//! Een service duwt zijn status bij elke wijziging in Hop; Spin houdt die in
//! het geheugen (een herstart van Spin wist hem, de service duwt opnieuw na
//! de welcome). Wat een stap mag, is het minste van drie: de tags in het
//! cluster (`spin-allow`), de apps van de repository en de `app_access` van de
//! stap.
use super::*;
use crate::capsules::Action;
use crate::workflow_mcp::{object, short_text, tool, tool_result};
use alloc::vec::Vec;
use d::{List, protocol as p, try_string};
use spin_core::validation::text;

/// Zoveel bewaarde crashes per cluster; de oudste valt weg.
const CRASHES: usize = 32;

/// Wat één spin-service meldde.
pub(crate) struct Cluster {
    client: String,
    status: p::ServiceStatus,
    crashes: Vec<Crash>,
}
struct Crash {
    app: String,
    service: String,
    at: Timestamp,
    lines: List<String>,
}
/// Een aanroep van een stap aan een service; het antwoord wordt een tool-resultaat.
pub(crate) struct ServiceCall {
    pub(crate) object: String,
    id: Value,
    method: &'static str,
    cluster: String,
    app: String,
    service: String,
}
/// Een app van de Job: de sleutel die de agent ziet, het cluster en de app.
struct Linked {
    key: String,
    cluster: String,
    app: String,
}

/// Hoe ver `access` gaat: niets, status, logs, herstart.
fn level(access: &str) -> u8 {
    match access {
        d::APP_ACCESS_STATUS => 1,
        d::APP_ACCESS_LOGS => 2,
        d::APP_ACCESS_RESTART => 3,
        _ => 0,
    }
}
fn arg<'a>(args: &'a Value, key: &str) -> &'a str {
    args.as_object()
        .and_then(|o| o.get(key))
        .and_then(Value::as_str)
        .map_or("", str::trim)
}

impl<P: Persistence> Server<P> {
    fn is_service(&self, client: &str) -> bool {
        self.runners.iter().any(|peer| {
            peer.client().id == client
                && peer
                    .client()
                    .capabilities
                    .tools
                    .iter()
                    .any(|t| t == p::TOOL_SERVICE)
        })
    }
    /// Een event van een service; `false` als het geen service-event is.
    pub(crate) fn service_event(
        &mut self,
        client: &str,
        message: &p::WireMessage,
        now: &Timestamp,
    ) -> Result<bool> {
        if !matches!(
            message.method.as_str(),
            p::METHOD_SERVICE_STATUS | p::METHOD_SERVICE_CRASH
        ) || !self.is_service(client)
        {
            return Ok(false);
        }
        let payload = message.payload.0.as_ref().unwrap_or(&Value::Null);
        let index = match self.services.iter().position(|c| c.client == client) {
            Some(index) => index,
            None => {
                self.services
                    .try_reserve(1)
                    .map_err(|_| d::Error::OutOfMemory)?;
                self.services.push(Cluster {
                    client: try_string(client)?,
                    status: p::ServiceStatus::default(),
                    crashes: Vec::new(),
                });
                self.services.len() - 1
            }
        };
        if message.method == p::METHOD_SERVICE_STATUS {
            self.services[index].status = p::ServiceStatus::from_value(payload)?;
        } else {
            let crash = p::ServiceCrash::from_value(payload)?;
            note(
                &mut self.notes,
                format_args!(
                    "SPIN_SERVICE_CRASH client={client} app={} service={} lines={}",
                    crash.app,
                    crash.service,
                    crash.lines.as_slice().len()
                ),
            );
            let crashes = &mut self.services[index].crashes;
            crashes.retain(|c| c.app != crash.app || c.service != crash.service);
            if crashes.len() >= CRASHES {
                crashes.remove(0);
            }
            crashes.try_reserve(1).map_err(|_| d::Error::OutOfMemory)?;
            crashes.push(Crash {
                app: crash.app,
                service: crash.service,
                at: now.try_clone()?,
                lines: crash.lines,
            });
        }
        self.display_changed();
        Ok(true)
    }
    /// De clusters voor `/api/state`: naam, verbonden, apps en de tijd van crashes (zonder regels).
    pub(crate) fn services_value(&self) -> Result<Value> {
        let mut out = List::new();
        for cluster in &self.services {
            let peer = self
                .runners
                .iter()
                .find(|p| p.client().id == cluster.client);
            let mut crashes = List::new();
            for crash in &cluster.crashes {
                crashes.push(object(&[
                    ("app", Value::string(&crash.app)?),
                    ("service", Value::string(&crash.service)?),
                    ("at", crash.at.to_value()?),
                ])?)?;
            }
            out.push(object(&[
                ("client", Value::string(&cluster.client)?),
                (
                    "name",
                    Value::string(peer.map_or("", |p| p.client().name.as_str()))?,
                ),
                (
                    "connected",
                    peer.is_some_and(|p| p.is_connected()).to_value()?,
                ),
                ("apps", cluster.status.apps.to_value()?),
                ("crashes", crashes.to_value()?),
            ])?)?;
        }
        Ok(out.to_value()?)
    }
    /// De verbonden service met deze naam.
    fn service_client(&self, cluster: &str) -> Option<&str> {
        self.runners
            .iter()
            .find(|peer| {
                peer.is_connected()
                    && peer.client().name == cluster
                    && peer
                        .client()
                        .capabilities
                        .tools
                        .iter()
                        .any(|t| t == p::TOOL_SERVICE)
            })
            .map(|peer| peer.client().id.as_str())
    }
    fn cluster(&self, name: &str) -> Option<&Cluster> {
        let client = self.service_client(name)?;
        self.services.iter().find(|c| c.client == client)
    }
    /// De apps van alle repositories van de Job; dubbele appnamen krijgen hun cluster erbij.
    fn linked_apps(&self, job: &d::Job) -> Result<Vec<Linked>> {
        let snapshot = self.store.snapshot()?;
        let mut ids = Vec::new();
        ids.try_reserve(1 + job.repositories.as_slice().len())
            .map_err(|_| d::Error::OutOfMemory)?;
        ids.push(job.git_repository_id.as_str());
        for repo in job.repositories.iter() {
            ids.push(repo.repository_id.as_str());
        }
        let mut linked: Vec<Linked> = Vec::new();
        for repository in snapshot.git_repositories.iter() {
            if !ids.contains(&repository.id.as_str()) {
                continue;
            }
            for entry in repository.apps.iter() {
                let Some((cluster, app)) = entry.split_once(':') else {
                    continue;
                };
                if linked.iter().any(|l| l.cluster == cluster && l.app == app) {
                    continue;
                }
                linked.try_reserve(1).map_err(|_| d::Error::OutOfMemory)?;
                linked.push(Linked {
                    key: try_string(app)?,
                    cluster: try_string(cluster)?,
                    app: try_string(app)?,
                });
            }
        }
        for i in 0..linked.len() {
            if linked.iter().filter(|l| l.app == linked[i].app).count() > 1 {
                linked[i].key = text(format_args!("{}:{}", linked[i].cluster, linked[i].app))?;
            }
        }
        Ok(linked)
    }
    /// De app-tools van een stap, als de stap en de repository ze toestaan.
    pub(crate) fn app_tools(&self, session: &str, tools: &mut List<Value>) -> Result {
        let view = self.store.workflow_for_session(session)?;
        let access = level(&view.phase.app_access);
        let linked = self.linked_apps(&view.job)?;
        if access == 0 || linked.is_empty() {
            return Ok(());
        }
        let mut keys = List::new();
        for l in &linked {
            keys.push(l.key.try_clone()?)?;
        }
        let app = object(&[
            ("type", Value::string("string")?),
            ("enum", keys.to_value()?),
            (
                "description",
                Value::string(if linked.len() == 1 {
                    "De app; mag weg, er is er één"
                } else {
                    "De app/omgeving"
                })?,
            ),
        ])?;
        let required: &[&str] = if linked.len() == 1 { &[] } else { &["app"] };
        tools.push(tool(
            "app_status",
            "Status van de apps",
            "De services van de draaiende app(s) van deze Job op Hop: toestand, aantal taken, herstarts, sinds wanneer, het adres en de laatste crash. Zonder app: alle apps van de Job.",
            object(&[("app", app.try_clone()?)])?,
            &[],
        )?)?;
        if access >= 2 {
            let mut with_service = List::new();
            with_service.push(try_string("service")?)?;
            for key in required {
                with_service.push(try_string(key)?)?;
            }
            let names: Vec<&str> = with_service.iter().map(String::as_str).collect();
            tools.push(tool(
                "app_logs",
                "Logs van een service",
                "De laatste logregels (stdout en stderr) van één service, met geheimen gemaskeerd. Hop bewaart 50 regels per stroom en nog 5 minuten na het einde van een taak; een eerdere crash staat er met zijn bewaarde regels onder.",
                object(&[
                    ("app", app.try_clone()?),
                    ("service", short_text("De service, zoals app_status hem noemt")?),
                    (
                        "stream",
                        object(&[
                            ("type", Value::string("string")?),
                            ("enum", {
                                let mut streams = List::new();
                                streams.push(try_string("stdout")?)?;
                                streams.push(try_string("stderr")?)?;
                                streams.to_value()?
                            }),
                        ])?,
                    ),
                ])?,
                &names,
            )?)?;
            if access >= 3 {
                tools.push(tool(
                    "app_restart",
                    "Herstart een service",
                    "Herstart één service: Hop rolt dezelfde spec opnieuw uit en haalt daarbij de nieuwste release. Faalt de nieuwe, dan blijft de oude draaien. Hoogstens eens per twee minuten per service. Kijk daarna met app_status of de taak opnieuw gestart is (sinds-tijd) en met app_logs of je versie draait.",
                    object(&[
                        ("app", app),
                        ("service", short_text("De service, zoals app_status hem noemt")?),
                    ])?,
                    &names,
                )?)?;
            }
        }
        Ok(())
    }
    /// Voert een app-tool uit; logs en herstart gaan naar de service van het cluster.
    pub(crate) fn call_app_tool(
        &mut self,
        session: &str,
        id: &Value,
        name: &str,
        args: &Value,
        now: &Timestamp,
        runtime: &mut impl Runtime,
    ) -> Result<Outcome> {
        let view = self.store.workflow_for_session(session)?;
        let access = level(&view.phase.app_access);
        let needed = match name {
            "app_status" => 1,
            "app_logs" => 2,
            _ => 3,
        };
        let refuse = |message: &str| -> Result<Outcome> {
            Ok(Outcome::Response(tool_result(id, message, true)?))
        };
        if access < needed {
            return refuse("Deze stap mag dat niet (app-toegang in de Template).");
        }
        let linked = self.linked_apps(&view.job)?;
        let wanted = arg(args, "app");
        let chosen: Vec<&Linked> = linked
            .iter()
            .filter(|l| wanted.is_empty() || l.key == wanted)
            .collect();
        if chosen.is_empty() {
            return refuse("Onbekende app voor deze Job.");
        }
        if name == "app_status" {
            let mut out = String::new();
            for l in &chosen {
                self.write_status(&mut out, l, access)?;
            }
            return Ok(Outcome::Response(tool_result(id, &out, false)?));
        }
        if chosen.len() > 1 {
            return refuse("Noem de app: deze Job heeft er meer dan één.");
        }
        let l = chosen[0];
        let service = arg(args, "service");
        if service.is_empty() {
            return refuse("Noem de service.");
        }
        let Some(client) = self
            .service_client(&l.cluster)
            .map(try_string)
            .transpose()?
        else {
            return refuse("De spin-service van dit cluster is niet verbonden.");
        };
        let method = if name == "app_logs" {
            p::METHOD_SERVICE_LOGS
        } else {
            p::METHOD_SERVICE_RESTART
        };
        if method == p::METHOD_SERVICE_RESTART {
            note(
                &mut self.notes,
                format_args!(
                    "SPIN_APP_RESTART session={session} cluster={} app={} service={service}",
                    l.cluster, l.app
                ),
            );
        }
        let payload = p::ServicePayload {
            app: l.app.try_clone()?,
            service: try_string(service)?,
            stream: try_string(arg(args, "stream"))?,
        };
        let call = ServiceCall {
            object: runtime.next("svc")?,
            id: id.try_clone()?,
            method,
            cluster: l.cluster.try_clone()?,
            app: l.app.try_clone()?,
            service: try_string(service)?,
        };
        Ok(Outcome::Capsule(self.enqueue_call(
            Action::Service(call),
            &client,
            method,
            &payload,
            now,
            runtime,
        )?))
    }
    fn write_status(&self, out: &mut String, l: &Linked, access: u8) -> Result {
        let line = |out: &mut String, args: core::fmt::Arguments<'_>| -> Result {
            out.push_str(&text(args)?);
            out.push('\n');
            Ok(())
        };
        let Some(cluster) = self.cluster(&l.cluster) else {
            return line(
                out,
                format_args!(
                    "{} ({}): de spin-service van dit cluster is niet verbonden.\n",
                    l.key, l.cluster
                ),
            );
        };
        let Some(app) = cluster.status.apps.iter().find(|a| a.name == l.app) else {
            return line(
                out,
                format_args!(
                    "{} ({}): het cluster toont deze app niet (tag spin-app).\n",
                    l.key, l.cluster
                ),
            );
        };
        line(out, format_args!("{} ({})", l.key, l.cluster))?;
        for s in app.services.iter() {
            let mut rights = Vec::new();
            for right in s.allow.iter() {
                let step = if right == "logs" { 2 } else { 3 };
                if access >= step {
                    rights.push(right.as_str());
                }
            }
            let crash = cluster
                .crashes
                .iter()
                .find(|c| c.app == l.app && c.service == s.name)
                .map(|c| c.at.as_str())
                .unwrap_or("");
            line(
                out,
                format_args!(
                    "- {}: {} · {}/{} taken · {} herstarts{}{}{}{}",
                    s.name,
                    s.state,
                    s.running,
                    if s.count < 0 {
                        "elke node".into()
                    } else {
                        alloc::format!("{}", s.count.max(1))
                    },
                    s.restarts,
                    s.started_at
                        .as_ref()
                        .map_or(String::new(), |t| alloc::format!(" · sinds {}", t.as_str())),
                    if s.url.is_empty() {
                        String::new()
                    } else {
                        alloc::format!(" · {}", s.url)
                    },
                    if crash.is_empty() {
                        String::new()
                    } else {
                        alloc::format!(" · laatste crash {crash}")
                    },
                    if rights.is_empty() {
                        String::new()
                    } else {
                        alloc::format!(" · mag: {}", rights.join(", "))
                    },
                ),
            )?;
        }
        out.push('\n');
        Ok(())
    }
    /// Het antwoord van een service als tool-resultaat.
    pub(crate) fn finish_service_call(
        &self,
        index: usize,
        message: &p::WireMessage,
    ) -> Result<Response> {
        let Action::Service(work) = &self.calls[index].action else {
            return Err(Error::Http(500, "missing service call"));
        };
        if !message.error.is_empty() {
            return tool_result(&work.id, &message.error, true);
        }
        let payload = message.payload.0.as_ref().unwrap_or(&Value::Null);
        let mut out = String::new();
        if work.method == p::METHOD_SERVICE_LOGS {
            let logs = p::ServiceLogs::from_value(payload)?;
            if logs.lines.is_empty() {
                out.push_str("(geen regels: Hop bewaart 50 regels per stroom en nog 5 minuten na het einde van een taak)\n");
            }
            for line in logs.lines.iter() {
                out.push_str(line);
                out.push('\n');
            }
            let crash = self.cluster(&work.cluster).and_then(|c| {
                c.crashes
                    .iter()
                    .find(|c| c.app == work.app && c.service == work.service)
            });
            if let Some(crash) = crash.filter(|c| !c.lines.is_empty()) {
                out.push_str(&text(format_args!(
                    "\nBewaard bij de crash van {}:\n",
                    crash.at.as_str()
                ))?);
                for line in crash.lines.iter() {
                    out.push_str(line);
                    out.push('\n');
                }
            }
        } else {
            let restart = p::ServiceRestart::from_value(payload)?;
            out = text(format_args!(
                "Herstart ingediend ({}, {}). Kijk met app_status of de taak opnieuw gestart is en met app_logs of je versie draait.",
                restart.status,
                if restart.policy.is_empty() {
                    "rolling"
                } else {
                    restart.policy.as_str()
                }
            ))?;
        }
        tool_result(&work.id, &out, false)
    }
}
