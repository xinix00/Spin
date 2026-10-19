//! De kern van spin-service, zonder I/O: wat een Hop-cluster aan Spin toont.
//!
//! Een job bestaat voor Spin alleen als zijn tag `spin` gelijk is aan de
//! filter van deze service (`SPIN_SERVICE_TAG`). De tags zijn van wie de
//! job-spec post; Spin kan zichzelf niets toekennen:
//!
//! | tag            | betekenis                                              |
//! |----------------|--------------------------------------------------------|
//! | `spin`         | welke Spin hem ziet; zonder deze tag is hij onzichtbaar |
//! | `spin-app`     | `app/omgeving`; jobs met dezelfde waarde zijn één app  |
//! | `spin-service` | de naam in Spin; standaard de jobnaam zonder app-deel  |
//! | `spin-allow`   | bovenop status en events: `logs`, `restart`            |
//!
//! Logregels verlaten het cluster gemaskeerd: elke env-waarde van de job
//! (vanaf [`MASK_MIN`] tekens) wordt `•••`.
#![no_std]
#![forbid(unsafe_code)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]
extern crate alloc;

use alloc::{string::String, vec::Vec};
use hoplib::{AgentTasks, Job, Task, TaskState, Time};
use spin_core::validation::{invalid, text};
use spin_domain::{self as d, List, Timestamp, protocol as p, try_push, try_string};

/// De tag met de Spin die een job ziet.
pub const TAG_SPIN: &str = "spin";
/// De tag met `app/omgeving`.
pub const TAG_APP: &str = "spin-app";
/// De tag met de naam van de service in Spin.
pub const TAG_SERVICE: &str = "spin-service";
/// De tag met de rechten bovenop status en events.
pub const TAG_ALLOW: &str = "spin-allow";
/// De tag van hoplb met het adres van de service.
pub const TAG_URLPREFIX: &str = "hoplb-urlprefix";
/// Het recht om logregels te lezen.
pub const ALLOW_LOGS: &str = "logs";
/// Het recht om te herstarten.
pub const ALLOW_RESTART: &str = "restart";
/// Kortere env-waarden (`true`, `8080`) blijven staan: ze zijn geen geheim en
/// zouden elke regel onleesbaar maken.
pub const MASK_MIN: usize = 6;
/// Een logregel langer dan dit wordt afgekapt.
pub const LINE_MAX: usize = 2048;
/// Zoveel regels gaan hoogstens in één antwoord of crash-melding.
pub const LINES_MAX: usize = 400;
/// Hoe lang een service na een herstart niet opnieuw herstart.
pub const RESTART_EVERY_NS: u64 = 120 * 1_000_000_000;
/// Hoe vaak één service hoogstens een crash meldt.
pub const CRASH_EVERY_NS: u64 = 60 * 1_000_000_000;

/// Wat een job aan Spin toont.
#[derive(Debug, PartialEq)]
pub struct Exposure {
    /// `app/omgeving`.
    pub app: String,
    /// De naam binnen de app.
    pub service: String,
    /// Of logregels het cluster mogen verlaten.
    pub logs: bool,
    /// Of Spin mag herstarten.
    pub restart: bool,
}

/// Wat `job` toont aan de Spin met filter `tag`; `None` als hij onzichtbaar is.
pub fn exposure(job: &Job, tag: &str) -> d::Fallible<Option<Exposure>> {
    let tag = tag.trim();
    if tag.is_empty() || job.tags.get(TAG_SPIN).map(|v| v.trim()) != Some(tag) {
        return Ok(None);
    }
    let app = match job.tags.get(TAG_APP).map(|v| v.trim()) {
        Some(app) if !app.is_empty() => app,
        _ => job.name.as_str(),
    };
    let service = match job.tags.get(TAG_SERVICE).map(|v| v.trim()) {
        Some(service) if !service.is_empty() => try_string(service)?,
        _ => {
            // `easyshop/test` + job `easyshop-test-api` is service `api`.
            let prefix = text(format_args!("{}-", app.replace('/', "-")))?;
            try_string(job.name.strip_prefix(prefix.as_str()).unwrap_or(&job.name))?
        }
    };
    let allow = job.tags.get(TAG_ALLOW).map_or("", |v| v.as_str());
    let has = |right: &str| allow.split(',').any(|r| r.trim() == right);
    Ok(Some(Exposure {
        app: try_string(app)?,
        service,
        logs: has(ALLOW_LOGS),
        restart: has(ALLOW_RESTART),
    }))
}

/// Een recht dat een aanvraag nodig heeft.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Right {
    /// Alleen zien dat hij er is.
    Status,
    /// Logregels lezen.
    Logs,
    /// Herstarten.
    Restart,
}

/// De job achter `app` + `service`, als die getoond wordt en `right` mag.
pub fn find<'a>(
    jobs: &'a [Job],
    tag: &str,
    app: &str,
    service: &str,
    right: Right,
) -> d::Fallible<Result<&'a Job, &'static str>> {
    for job in jobs {
        let Some(e) = exposure(job, tag)? else {
            continue;
        };
        if e.app != app || e.service != service {
            continue;
        }
        return Ok(match right {
            Right::Logs if !e.logs => Err("logs are not allowed for this service (spin-allow)"),
            Right::Restart if !e.restart => {
                Err("restart is not allowed for this service (spin-allow)")
            }
            _ => Ok(job),
        });
    }
    Ok(Err("unknown service"))
}

/// Het overzicht van alle getoonde jobs, per app en op naam gesorteerd.
pub fn status(jobs: &[Job], tasks: &[AgentTasks], tag: &str) -> d::Fallible<p::ServiceStatus> {
    let mut apps: Vec<p::ServiceApp> = Vec::new();
    for job in jobs {
        let Some(e) = exposure(job, tag)? else {
            continue;
        };
        let state = service_state(job, &e, tasks)?;
        match apps.iter_mut().find(|a| a.name == e.app) {
            Some(app) => app.services.push(state)?,
            None => {
                let mut services = List::new();
                services.push(state)?;
                try_push(
                    &mut apps,
                    p::ServiceApp {
                        name: e.app,
                        services,
                    },
                )?;
            }
        }
    }
    apps.sort_unstable_by(|a, b| a.name.cmp(&b.name));
    let mut out = List::new();
    for mut app in apps {
        app.services
            .as_mut_slice()
            .sort_unstable_by(|a, b| a.name.cmp(&b.name));
        out.push(app)?;
    }
    Ok(p::ServiceStatus { apps: out })
}

fn service_state(job: &Job, e: &Exposure, tasks: &[AgentTasks]) -> d::Fallible<p::ServiceState> {
    let mine: Vec<&Task> = tasks
        .iter()
        .filter_map(|a| a.tasks.as_ref())
        .flatten()
        .filter(|t| t.job_name == job.name)
        .collect();
    let mut allow = List::new();
    if e.logs {
        allow.push(try_string(ALLOW_LOGS)?)?;
    }
    if e.restart {
        allow.push(try_string(ALLOW_RESTART)?)?;
    }
    // De slechtste toestand telt: een gefaalde taak verbergt zich niet achter een gezonde.
    let rank = |s: TaskState| match s {
        TaskState::Failed => 4,
        TaskState::Stopping => 3,
        TaskState::Queued | TaskState::Downloading => 2,
        TaskState::Running | TaskState::System => 1,
    };
    let state = match mine.iter().map(|t| t.state).max_by_key(|s| rank(*s)) {
        Some(s) => s.as_str(),
        None => "missing",
    };
    let latest = |pick: fn(&Task) -> Time| {
        mine.iter()
            .map(|t| pick(t))
            .filter(|t| !t.is_zero())
            .max()
            .map(Timestamp::from_time)
            .transpose()
    };
    Ok(p::ServiceState {
        name: try_string(&e.service)?,
        job: try_string(&job.name)?,
        allow,
        url: try_string(job.tags.get(TAG_URLPREFIX).map_or("", |v| v.as_str()))?,
        count: job.count,
        running: mine
            .iter()
            .filter(|t| t.state == TaskState::Running)
            .count()
            .try_into()
            .unwrap_or(i64::MAX),
        state: try_string(state)?,
        restarts: mine.iter().map(|t| t.restart_count).sum(),
        started_at: latest(|t| t.started_at)?,
        last_failed_at: latest(|t| t.last_failed_at)?,
    })
}

/// De env-waarden van een job, langste eerst, zodat een waarde die een
/// andere bevat in zijn geheel wegvalt.
pub struct Mask(Vec<String>);
impl Mask {
    /// Het masker van `job`.
    pub fn of(job: &Job) -> d::Fallible<Self> {
        let mut values = Vec::new();
        for (_, value) in job.env.iter() {
            if value.chars().count() >= MASK_MIN {
                try_push(&mut values, try_string(value)?)?;
            }
        }
        values.sort_unstable_by_key(|v| core::cmp::Reverse(v.len()));
        values.dedup();
        Ok(Self(values))
    }
    /// `line` zonder env-waarden en hoogstens [`LINE_MAX`] bytes lang.
    pub fn apply(&self, line: &str) -> d::Fallible<String> {
        let mut out = try_string(line)?;
        for value in &self.0 {
            if out.contains(value.as_str()) {
                out = out.replace(value.as_str(), "•••");
            }
        }
        if out.len() > LINE_MAX {
            let mut end = LINE_MAX;
            while !out.is_char_boundary(end) {
                end -= 1;
            }
            out.truncate(end);
            out.push('…');
        }
        Ok(out)
    }
}

/// De regels uit een SSE-momentopname van een Hop-log (`data: …` per regel),
/// gemaskeerd, met `prefix` ervoor.
pub fn log_lines(body: &[u8], prefix: &str, mask: &Mask, out: &mut List<String>) -> d::Fallible {
    let mut events = hoplib::Events::new();
    events
        .push(body)
        .map_err(|_| invalid("logs", "log snapshot too large"))?;
    while let Some(event) = events
        .next()
        .map_err(|_| invalid("logs", "invalid log snapshot"))?
    {
        if !event.kind.is_empty() {
            continue;
        }
        for line in event.data.split('\n') {
            out.push(text(format_args!("{prefix}{}", mask.apply(line)?))?)?;
        }
    }
    Ok(())
}

/// Houdt `lines` op de laatste [`LINES_MAX`].
pub fn keep_last(lines: &mut List<String>) {
    let skip = lines.as_slice().len().saturating_sub(LINES_MAX);
    let mut seen = 0;
    lines.retain(|_| {
        seen += 1;
        seen > skip
    });
}

/// Hoogstens eens per `every` nanoseconden per job; de klok is monotoon.
pub struct Throttle {
    every: u64,
    last: Vec<(String, u64)>,
}
impl Throttle {
    /// Een lege begrenzer.
    pub const fn new(every: u64) -> Self {
        Self {
            every,
            last: Vec::new(),
        }
    }
    /// `Ok` en onthouden, of hoeveel seconden er nog te wachten zijn.
    pub fn admit(&mut self, job: &str, now: u64) -> d::Fallible<Result<(), u64>> {
        if let Some((_, at)) = self.last.iter_mut().find(|(j, _)| j == job) {
            let next = at.saturating_add(self.every);
            if now < next {
                return Ok(Err((next - now).div_ceil(1_000_000_000)));
            }
            *at = now;
            return Ok(Ok(()));
        }
        try_push(&mut self.last, (try_string(job)?, now))?;
        Ok(Ok(()))
    }
}

#[cfg(test)]
mod tests;
