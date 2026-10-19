//! Hop: wat de service aan de agent vraagt, en de stroom van `/v1/events`.
//!
//! Een HopOS-node houdt hoogstens drie stromen tegelijk open, dus volgt de
//! service alleen `/v1/events`; logs zijn een momentopname (`follow=0`) van de
//! ring die Hop per taak bewaart (50 regels, 5 minuten na het einde).
use super::{BELL, DIRTY, FORCE, OWNER};
use alloc::{boxed::Box, string::String, vec::Vec};
use applib::{EXEC, log};
use core::{cell::RefCell, ops::ControlFlow, time::Duration};
use hoplib::{Client, Event, Job, Topic, hopos::Agent};
use spin_domain::{
    self as d, List, Wire,
    json::Value,
    protocol::{self as p, WireMessage},
    try_string,
};
use spin_service::{self as core_, Mask, Right, Throttle};
use sync::{Either, select};

/// De agent en de begrenzers; één keer gemaakt bij de start en nooit vrijgegeven.
pub(crate) struct Hop {
    agent: Agent,
    tag: String,
    restarts: RefCell<Throttle>,
    crashes: RefCell<Throttle>,
}

/// Een fout richting Spin: één leesbare zin.
type Reply<T> = Result<T, String>;
fn hop_error(error: hoplib::Error) -> String {
    alloc::format!("hop: {error}")
}
fn data_error(error: d::Error) -> String {
    alloc::format!("{error}")
}

/// `HOP` in de URL is de agent van deze node (slot 1).
pub(crate) fn agent_url(env: Option<&str>) -> String {
    let raw = env.filter(|s| !s.is_empty()).unwrap_or("http://HOP:8080");
    let [a, b, c, d] = applib::net::slot_ip(1);
    let ip = alloc::format!("{a}.{b}.{c}.{d}");
    match raw.split_once("://") {
        Some((scheme, rest)) => alloc::format!("{scheme}://{}", rest.replacen("HOP", &ip, 1)),
        None => alloc::format!("http://{}", raw.replacen("HOP", &ip, 1)),
    }
}

/// De agent van deze service, voor de rest van zijn leven.
pub(crate) fn start(
    agent: &str,
    key: Option<&str>,
    tag: String,
) -> Result<&'static Hop, &'static str> {
    let client = Client::new(agent, key).map_err(|_| "HOP_AGENT is not a URL")?;
    Ok(Box::leak(Box::new(Hop {
        agent: Agent::new(client, EXEC.get()),
        tag,
        restarts: RefCell::new(Throttle::new(core_::RESTART_EVERY_NS)),
        crashes: RefCell::new(Throttle::new(core_::CRASH_EVERY_NS)),
    })))
}

impl Hop {
    /// Het antwoord op één aanvraag van Spin.
    pub(crate) async fn answer(&self, method: &str, payload: &Value) -> Reply<Value> {
        match method {
            p::METHOD_SERVICE_STATUS => self.status().await,
            p::METHOD_SERVICE_LOGS => self.logs(payload).await,
            p::METHOD_SERVICE_RESTART => self.restart(payload).await,
            _ => Err(String::from("service method is not available")),
        }
    }
    pub(crate) async fn status(&self) -> Reply<Value> {
        let jobs = self.agent.jobs().await.map_err(hop_error)?;
        let tasks = self.agent.tasks().await.map_err(hop_error)?;
        core_::status(&jobs, &tasks, &self.tag)
            .and_then(|s| s.to_value())
            .map_err(data_error)
    }
    /// De job achter een aanvraag, als de tags dat recht geven.
    async fn job(&self, payload: &Value, right: Right) -> Reply<(Job, p::ServicePayload)> {
        let request = p::ServicePayload::from_value(payload).map_err(data_error)?;
        let jobs = self.agent.jobs().await.map_err(hop_error)?;
        let job = core_::find(&jobs, &self.tag, &request.app, &request.service, right)
            .map_err(data_error)?
            .map_err(String::from)?;
        Ok((job.clone(), request))
    }
    async fn logs(&self, payload: &Value) -> Reply<Value> {
        let (job, request) = self.job(payload, Right::Logs).await?;
        let lines = self.snapshot(&job, &request.stream).await?;
        p::ServiceLogs { lines }.to_value().map_err(data_error)
    }
    /// De ringen van alle taken van `job`, per stroom, gemaskeerd.
    async fn snapshot(&self, job: &Job, stream: &str) -> Reply<List<String>> {
        let status = self.agent.job_status(&job.name).await.map_err(hop_error)?;
        let mask = Mask::of(job).map_err(data_error)?;
        let tasks: Vec<(&str, &str)> = status
            .tasks
            .iter()
            .flat_map(|a| {
                a.tasks
                    .iter()
                    .flatten()
                    .filter(|t| t.job_name == job.name)
                    .map(|t| (a.agent.as_str(), t.id.as_str()))
            })
            .collect();
        let mut lines = List::new();
        for (agent, task) in &tasks {
            for which in ["stdout", "stderr"] {
                if !stream.is_empty() && stream != which {
                    continue;
                }
                let path = alloc::format!("/v1/agents/{agent}/logs/{task}/{which}?follow=0");
                let request = self
                    .agent
                    .client()
                    .request("GET", &path, Vec::new())
                    .map_err(hop_error)?;
                let body = ring(&request).await?;
                let prefix = if tasks.len() > 1 {
                    alloc::format!("[{} {which}] ", task.get(..8).unwrap_or(task))
                } else {
                    alloc::format!("[{which}] ")
                };
                core_::log_lines(&body, &prefix, &mask, &mut lines).map_err(data_error)?;
            }
        }
        core_::keep_last(&mut lines);
        Ok(lines)
    }
    /// Dezelfde spec opnieuw: Hop rolt hem uit en haalt de nieuwe release;
    /// faalt de nieuwe, dan blijft de oude draaien.
    async fn restart(&self, payload: &Value) -> Reply<Value> {
        let (job, request) = self.job(payload, Right::Restart).await?;
        let now = applib::clock::now_ns();
        if let Err(wait) = self
            .restarts
            .borrow_mut()
            .admit(&job.name, now)
            .map_err(data_error)?
        {
            return Err(alloc::format!(
                "{} was restarted less than 2 minutes ago; try again in {wait} s",
                request.service
            ));
        }
        log!(
            "SPIN_SERVICE_RESTART app={} service={} job={}",
            request.app,
            request.service,
            job.name
        );
        let applied = self.agent.apply(&job).await.map_err(|error| {
            alloc::format!(
                "hop gave no answer ({error}); the restart may still be rolling out, check the status"
            )
        })?;
        if !applied.error.is_empty() {
            return Err(alloc::format!("hop: {}", applied.error));
        }
        p::ServiceRestart {
            status: applied.status,
            policy: applied.policy,
        }
        .to_value()
        .map_err(data_error)
    }
    /// Een taak van `name` crashte: Spin hoort het, met de laatste regels als dat mag.
    async fn crashed(&self, name: &str) -> Reply<()> {
        let jobs = self.agent.jobs().await.map_err(hop_error)?;
        let Some(job) = jobs.iter().find(|j| j.name == name) else {
            return Ok(());
        };
        let Some(e) = core_::exposure(job, &self.tag).map_err(data_error)? else {
            return Ok(());
        };
        let now = applib::clock::now_ns();
        if self
            .crashes
            .borrow_mut()
            .admit(name, now)
            .map_err(data_error)?
            .is_err()
        {
            return Ok(());
        }
        let lines = if e.logs {
            self.snapshot(job, "").await?
        } else {
            List::new()
        };
        log!(
            "SPIN_SERVICE_CRASH app={} service={} job={name}",
            e.app,
            e.service
        );
        let message = WireMessage {
            version: p::PROTOCOL_VERSION,
            r#type: try_string(p::MESSAGE_EVENT).map_err(data_error)?,
            method: try_string(p::METHOD_SERVICE_CRASH).map_err(data_error)?,
            payload: d::RawJson(Some(
                p::ServiceCrash {
                    app: e.app,
                    service: e.service,
                    job: try_string(name).map_err(data_error)?,
                    lines,
                }
                .to_value()
                .map_err(data_error)?,
            )),
            ..Default::default()
        };
        let queued = OWNER.borrow_mut().enqueue(&message).map_err(data_error)?;
        if !queued {
            return Err(String::from("outbox is full"));
        }
        BELL.set();
        Ok(())
    }
}

/// Hoe lang een logstroom na zijn kop mag lopen voordat de momentopname af is.
const RING_WAIT: Duration = Duration::from_secs(2);

/// Leest een logstroom tot zijn einde of tot [`RING_WAIT`]: Rust-Hop sluit een
/// momentopname (`follow=0`) zelf, Go-Hop (v1) negeert `follow` en blijft volgen.
/// Een half binnengekomen regel aan het eind valt weg.
async fn ring(request: &hoplib::Request) -> Reply<Vec<u8>> {
    let exec = EXEC.get();
    let mut header = leanhttp::Header::new();
    for (name, value) in &request.headers {
        header
            .set(name, value)
            .map_err(|e| alloc::format!("hop: {e:?}"))?;
    }
    let call = leanhttp::Call {
        method: request.method,
        url: &request.url,
        header,
        header_timeout: Some(Duration::from_secs(10)),
        ..leanhttp::Call::default()
    };
    let mut dial = hoplib::hopos::SlotDial::new(exec);
    let mut response = leanhttp::fetch(&mut dial, call)
        .await
        .map_err(|e| alloc::format!("hop: {e:?}"))?;
    let mut body = Vec::new();
    if response.status != 200 {
        return Ok(body);
    }
    let deadline = applib::clock::now_ns()
        .saturating_add(u64::try_from(RING_WAIT.as_nanos()).unwrap_or(u64::MAX));
    let mut buf = [0; 4096];
    loop {
        match select(response.read(&mut buf), exec.until(deadline)).await {
            Either::Left(Ok(0)) | Either::Right(()) => return Ok(body),
            Either::Left(Ok(n)) => {
                if body.len() + n > hoplib::MAX_BODY || body.try_reserve(n).is_err() {
                    return Ok(body);
                }
                body.extend_from_slice(&buf[..n]);
            }
            Either::Left(Err(e)) => return Err(alloc::format!("hop: {e:?}")),
        }
    }
}

/// Volgt `/v1/events` zolang de service leeft; een crash wordt een event voor Spin.
///
/// Een breuk staat één keer in de log zolang zijn reden dezelfde blijft: een
/// Go-Hop (v1) stuurt geen keepalive, en daar sluit de stilte van hoplib de
/// stroom elke minuut dat er niets gebeurt; hij gaat meteen weer open.
pub(crate) async fn watch(hop: &'static Hop) {
    let mut last = String::new();
    hop.agent
        .events(
            async |event: &Event| {
                // Elke melding kan de status veranderen; de publisher bundelt ze.
                DIRTY.set();
                if let Ok(Topic::Task { job, event }) = event.topic()
                    && event == "crash"
                    && let Err(error) = hop.crashed(&job).await
                {
                    log!("SPIN_SERVICE_CRASH_LOST job={job} error={error}");
                }
                ControlFlow::Continue(())
            },
            |error, retry| {
                let why = alloc::format!("{error}");
                if why != last {
                    log!("SPIN_SERVICE_EVENTS_LOST error={why} retry_in={retry:?} (herhalingen met deze reden niet opnieuw gemeld)");
                    last = why;
                }
            },
        )
        .await;
}

/// Duwt de status naar Spin zodra Hop iets meldt, hoogstens eens per seconde,
/// en alleen als hij veranderde of Spin hem na een welcome nog niet heeft.
pub(crate) async fn publish(hop: &'static Hop) {
    let exec = EXEC.get();
    let mut last = String::new();
    loop {
        DIRTY.wait().await;
        exec.after(Duration::from_secs(1)).await;
        DIRTY.take();
        let force = FORCE.take();
        let json = match hop
            .status()
            .await
            .and_then(|v| v.to_json().map_err(data_error))
        {
            Ok(json) => json,
            Err(error) => {
                log!("SPIN_SERVICE_STATUS_FAILED error={error}");
                // Opnieuw proberen, ook als Hop even niets meldt.
                if force {
                    FORCE.set();
                }
                exec.after(Duration::from_secs(5)).await;
                DIRTY.set();
                continue;
            }
        };
        if json == last && !force {
            continue;
        }
        let queued = Value::from_json(json.as_bytes())
            .map_err(data_error)
            .and_then(|payload| {
                let message = WireMessage {
                    version: p::PROTOCOL_VERSION,
                    r#type: try_string(p::MESSAGE_EVENT).map_err(data_error)?,
                    method: try_string(p::METHOD_SERVICE_STATUS).map_err(data_error)?,
                    payload: d::RawJson(Some(payload)),
                    ..Default::default()
                };
                OWNER.borrow_mut().enqueue(&message).map_err(data_error)
            });
        match queued {
            Ok(true) => {
                last = json;
                BELL.set();
            }
            Ok(false) | Err(_) => {
                log!("SPIN_SERVICE_STATUS_FAILED error=outbox is full");
                FORCE.set();
                DIRTY.set();
            }
        }
    }
}
