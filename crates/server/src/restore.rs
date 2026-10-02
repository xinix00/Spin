//! A restore survives its initiating browser, and invalidates every old runtime binding.
use super::*;
use spin_store::backup::{RestoreReply as R, RestoreRequest as Q};
pub(crate) struct Job {
    id: String,
    pub(super) done: bool,
    error: bool,
    current: u64,
    total: u64,
    expires: u64,
    result: Option<Value>,
}
impl Job {
    fn response(&self, status: u16) -> Result<Response> {
        Response::json(
            status,
            &http::object(&[
                ("id", Value::string(&self.id)?),
                (
                    "status",
                    Value::string(if self.error {
                        "error"
                    } else if self.done {
                        "complete"
                    } else {
                        "running"
                    })?,
                ),
                (
                    "stage",
                    Value::string(if self.done { "complete" } else { "validate" })?,
                ),
                (
                    "message",
                    Value::string(if self.error {
                        "Backup kon niet worden hersteld"
                    } else if self.done {
                        "Restore compleet"
                    } else {
                        "Backup wordt gecontroleerd"
                    })?,
                ),
                (
                    "error",
                    Value::string(if self.error {
                        "Backup kon niet worden hersteld; controleer het archief en de serverlog"
                    } else {
                        ""
                    })?,
                ),
                ("current", Value::uint(self.current)),
                ("total", Value::uint(self.total)),
                (
                    "expires_at",
                    Timestamp::from_time(d::Time(self.expires))?.to_value()?,
                ),
                (
                    "result",
                    self.result
                        .as_ref()
                        .map(Value::try_clone)
                        .transpose()?
                        .unwrap_or(Value::Null),
                ),
            ])?,
        )
    }
}
impl<P: Persistence> Server<P> {
    pub(crate) fn replica_route(
        &mut self,
        req: &Request<'_>,
        user: &d::User,
        now: &Timestamp,
        runtime: &mut impl Runtime,
    ) -> Result<Option<Response>> {
        if !matches!(
            (req.method, req.path),
            ("GET", "/api/replica/points") | ("POST", "/api/replica/restore")
        ) {
            return Ok(None);
        }
        if user.role != d::USER_ADMIN {
            return Err(Error::Http(403, "admin role required"));
        }
        if req.method == "GET" {
            let mut points = d::List::new();
            for point in self.store.replica_points()?.into_vec() {
                points.push(http::object(&[
                    ("generation", Value::string(&point.generation)?),
                    ("at", Value::string(&point.at)?),
                    ("level", Value::int(i64::from(point.level))),
                    ("current", Value::Bool(point.current)),
                ])?)?;
            }
            return Ok(Some(Response::json(
                200,
                &http::object(&[("points", points.to_value()?)])?,
            )?));
        }
        let value: Value = Self::decode(req)?;
        let fields = value
            .as_object()
            .ok_or(Error::Http(400, "generation is required"))?;
        let generation = fields
            .get("generation")
            .and_then(Value::as_str)
            .ok_or(Error::Http(400, "generation is required"))?
            .trim();
        if generation.is_empty() || generation.len() > 255 {
            return Err(Error::Http(400, "invalid generation"));
        }
        let at = fields
            .get("at")
            .filter(|v| !matches!(v, Value::Null))
            .map(Timestamp::from_value)
            .transpose()?;
        let id = runtime.next("restore")?;
        Ok(Some(self.start_restore_request(
            &id,
            Q::Replica {
                generation: d::try_string(generation)?,
                at,
            },
            now,
        )?))
    }
    pub(crate) fn restore_status(
        &self,
        req: &Request<'_>,
        now: &Timestamp,
    ) -> Result<Option<Response>> {
        let Some(id) = req.path.strip_prefix("/api/restores/") else {
            return Ok(None);
        };
        if req.method != "GET" {
            return Err(Error::Http(405, "method not allowed"));
        }
        // The random upload identity is the status capability, also after restoring clears browser sessions.
        let job = self
            .restores
            .iter()
            .find(|j| j.id == id && j.expires > now.time().map(|t| t.0).unwrap_or(u64::MAX))
            .ok_or(Error::Http(404, "restore status not found or expired"))?;
        Ok(Some(job.response(200)?))
    }
    pub(crate) fn restore_available(&self) -> Result {
        if self.backup_active() {
            return Err(Error::Http(409, "another backup or restore is active"));
        }
        if !self.terminals.is_empty()
            || self.agents.iter().any(|a| !a.closed)
            || self.calls.iter().any(|c| !c.finished)
            || self.network.iter().any(|c| c.response.is_none())
            || self.operations.iter().any(|o| !o.finished())
            || self.login_operations.iter().any(|o| o.response.is_none())
            || !self.options.is_empty()
        {
            return Err(Error::Http(
                409,
                "stop active terminals, chats and background Job launches before restoring a backup",
            ));
        }
        Ok(())
    }
    pub(crate) fn start_restore(
        &mut self,
        id: &str,
        object: i64,
        now: &Timestamp,
    ) -> Result<Response> {
        self.start_restore_request(id, Q::Begin(object), now)
    }
    fn start_restore_request(&mut self, id: &str, request: Q, now: &Timestamp) -> Result<Response> {
        self.restore_available()?;
        let current = now.time()?.0;
        self.restores.retain(|j| j.expires > current);
        if self.restores.len() >= 32 {
            self.restores.remove(0);
        }
        self.restores
            .try_reserve(1)
            .map_err(|_| d::Error::OutOfMemory)?;
        let job = Job {
            id: d::try_string(id)?,
            done: false,
            error: false,
            current: 0,
            total: 0,
            expires: current.saturating_add(6 * 3600 * 1_000_000_000),
            result: None,
        };
        let mut response = job.response(202)?;
        response.header(
            "Location",
            &spin_core::validation::text(format_args!("/api/restores/{id}"))?,
        )?;
        if let Err(error) = self.store.stage_restore(request) {
            let _ = self.store.stage_restore(Q::Abort);
            return Err(error.into());
        }
        self.restores.push(job);
        Ok(response)
    }
    /// Advance one extraction/hash block before yielding back to HTTP transport.
    pub fn maintain_restores(&mut self, now: &Timestamp, runtime: &mut impl Runtime) -> Result {
        let Some(index) = self.restores.iter().position(|j| !j.done) else {
            return Ok(());
        };
        let outcome = (|| -> Result<Option<Value>> {
            if self.restores[index].expires <= now.time()?.0 {
                return Err(Error::Http(408, "restore deadline reached"));
            }
            match self.store.stage_restore(Q::Step)? {
                R::Progress(current, total) => {
                    self.restores[index].current = current;
                    self.restores[index].total = total;
                    Ok(None)
                }
                R::Prepared(portable) => {
                    let state = spin_store::backup::prepare(
                        portable.json.as_bytes(),
                        &portable.master_key,
                        now,
                        || {
                            runtime
                                .next("login")
                                .map_err(|_| spin_security::Error::Payload)
                        },
                    )?;
                    let result = http::object(&[
                        ("status", Value::string("restored")?),
                        ("users", Value::uint(state.users.len() as u64)),
                        ("jobs", Value::uint(state.jobs.len() as u64)),
                        (
                            "templates",
                            Value::uint(state.workflow_templates.len() as u64),
                        ),
                        ("deliverables", Value::uint(state.deliverables.len() as u64)),
                        (
                            "attachments",
                            Value::uint(state.job_attachments.len() as u64),
                        ),
                        (
                            "snapshots",
                            Value::uint(
                                state
                                    .artifacts
                                    .iter()
                                    .filter(|(_, a)| {
                                        a.snapshot.restorable && a.snapshot_pruned_at.is_none()
                                    })
                                    .count() as u64,
                            ),
                        ),
                    ])?;
                    self.store.install_restore(state)?;
                    // No fallible publication after the confirmed transaction.
                    self.csrf = Map::new();
                    self.attempts = Map::new();
                    self.runners.clear();
                    self.calls.clear();
                    self.terminals.clear();
                    self.agents.clear();
                    self.app_starts = Map::new();
                    self.oauth_attempts = Map::new();
                    self.network.clear();
                    self.refresh_after = Map::new();
                    self.operations.clear();
                    self.login_operations.clear();
                    self.options.clear();
                    self.backup_tickets = Map::new();
                    self.refresh_checked = None;
                    self.watch_stamps = Map::new();
                    self.attachment_stamps = Map::new();
                    self.last_launch_sweep = None;
                    self.uploads.retain(|u| u.id == self.restores[index].id);
                    Ok(Some(result))
                }
                R::Done => Err(Error::Http(500, "restore unexpectedly stopped")),
            }
        })();
        match outcome {
            Ok(None) => Ok(()),
            Ok(Some(result)) => {
                self.restores[index].result = Some(result);
                self.restores[index].done = true;
                // Cleanup failure does not change a confirmed restore into a failed one.
                self.store.stage_restore(Q::Abort)?;
                Ok(())
            }
            Err(error) => {
                self.restores[index].done = true;
                self.restores[index].error = true;
                let _ = self.store.stage_restore(Q::Abort);
                Err(error)
            }
        }
    }
}
