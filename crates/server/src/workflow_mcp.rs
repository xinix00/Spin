//! Capsule-capabilities voor workflowtools, met een bevestigde Git-publicatie vóór ACCEPT.
use super::*;
use crate::capsules::Action;
use alloc::vec::Vec;
use d::{List, protocol as p, try_string};
use spin_core::validation::text;
use spin_store::{Context, Mutation};
pub(crate) struct Acceptance {
    pub(crate) session: String,
    pub(crate) composition: String,
    reply: AcceptanceReply,
    detail: String,
    steps: Vec<Step>,
    at: usize,
}
enum AcceptanceReply {
    Mcp(Value),
    Human { question: String, actor: String },
}
impl AcceptanceReply {
    fn copy(&self) -> Result<Self> {
        Ok(match self {
            Self::Mcp(id) => Self::Mcp(id.try_clone()?),
            Self::Human { question, actor } => Self::Human {
                question: question.try_clone()?,
                actor: actor.try_clone()?,
            },
        })
    }
    fn error(&self, message: &str) -> Result<Response> {
        match self {
            Self::Mcp(id) => tool_result(id, message, true),
            Self::Human { .. } => {
                Response::json(502, &object(&[("error", Value::string(message)?)])?)
            }
        }
    }
}
pub(crate) struct Delivery {
    pub(crate) session: String,
    pub(crate) composition: String,
    id: Value,
    name: String,
    path: String,
    bundle: bool,
}
struct Step {
    method: &'static str,
    payload: Value,
}
fn field<'a>(value: &'a Value, key: &str) -> &'a Value {
    value
        .as_object()
        .and_then(|v| v.get(key))
        .unwrap_or(&Value::Null)
}
fn string<'a>(value: &'a Value, key: &str) -> &'a str {
    field(value, key).as_str().unwrap_or("")
}
fn object(fields: &[(&str, Value)]) -> Result<Value> {
    http::object(fields)
}
fn result(id: &Value, value: Value) -> Result<Response> {
    Response::json(
        200,
        &object(&[
            ("jsonrpc", Value::string("2.0")?),
            ("id", id.try_clone()?),
            ("result", value),
        ])?,
    )
}
fn rpc_error(status: u16, id: &Value, code: i64, message: &str) -> Result<Response> {
    Response::json(
        status,
        &object(&[
            ("jsonrpc", Value::string("2.0")?),
            ("id", id.try_clone()?),
            (
                "error",
                object(&[
                    ("code", code.to_value()?),
                    ("message", Value::string(message)?),
                ])?,
            ),
        ])?,
    )
}
fn tool_result(id: &Value, message: &str, failed: bool) -> Result<Response> {
    let mut blocks = List::new();
    blocks.push(object(&[
        ("type", Value::string("text")?),
        ("text", Value::string(message)?),
    ])?)?;
    result(
        id,
        object(&[
            ("content", blocks.to_value()?),
            ("isError", failed.to_value()?),
        ])?,
    )
}
fn schema(properties: Value, required: &[&str]) -> Result<Value> {
    let mut schema = d::json::Object::new();
    schema.push("type", Value::string("object")?)?;
    schema.push("properties", properties)?;
    schema.push("additionalProperties", false.to_value()?)?;
    if !required.is_empty() {
        let mut names = List::new();
        for key in required {
            names.push(try_string(key)?)?;
        }
        schema.push("required", names.to_value()?)?;
    }
    Ok(Value::Object(schema))
}
fn tool(
    name: &str,
    title: &str,
    description: &str,
    properties: Value,
    required: &[&str],
) -> Result<Value> {
    object(&[
        ("name", Value::string(name)?),
        ("title", Value::string(title)?),
        ("description", Value::string(description)?),
        ("inputSchema", schema(properties, required)?),
    ])
}
fn short_text(description: &str) -> Result<Value> {
    object(&[
        ("type", Value::string("string")?),
        ("description", Value::string(description)?),
    ])
}
fn clamp(value: &str, limit: usize) -> Result<String> {
    let mut end = value.len().min(limit);
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    Ok(try_string(&value[..end])?)
}
impl<P: Persistence> Server<P> {
    fn begin_workflow_delivery(
        &mut self,
        session: &str,
        id: &Value,
        name: &str,
        path: &str,
        now: &Timestamp,
        runtime: &mut impl Runtime,
    ) -> Result<Outcome> {
        let view = self.store.workflow_for_session(session)?;
        if view.run.status != d::PHASE_RUN_RUNNING {
            return Err(Error::Http(409, "phase is not running"));
        }
        let definition = view
            .phase
            .deliverables
            .iter()
            .find(|d| d.name.eq_ignore_ascii_case(name))
            .ok_or(Error::Http(
                409,
                "deliverable is not declared by this phase",
            ))?;
        if !path.starts_with('/') {
            return Err(Error::Http(409, "deliverable path must be absolute"));
        }
        let path = spin_core::archive::clean_path(path)?;
        if !path.starts_with("root/deliverables/") {
            return Err(Error::Http(
                409,
                "deliverable path must lie inside /root/deliverables",
            ));
        }
        let path = text(format_args!("/{path}"))?;
        let bundle = d::deliverable_is_bundle(&definition.kind);
        if !bundle
            && !path
                .get(path.len().saturating_sub(3)..)
                .is_some_and(|suffix| suffix.eq_ignore_ascii_case(".md"))
        {
            return Err(Error::Http(409, "Markdown deliverable requires a .md file"));
        }
        let session_record = self.store.session(session)?;
        let composition = self
            .store
            .composition(&session_record.prepared_composition_id)?;
        let capsule = composition
            .runtime
            .as_ref()
            .filter(|r| r.status != "stopped")
            .ok_or(Error::Http(409, "session has no running capsule"))?;
        let client = capsule.client_id.try_clone()?;
        let (method, payload) = if bundle {
            (
                p::METHOD_BUNDLE_DELIVERABLE,
                p::BundleDeliverablePayload {
                    runtime: capsule.try_clone()?,
                    path: path.try_clone()?,
                }
                .to_value()?,
            )
        } else {
            let mut paths = List::new();
            paths.push(path.try_clone()?)?;
            (
                p::METHOD_READ_TRACKED,
                p::TrackedFilesPayload {
                    runtime: capsule.try_clone()?,
                    paths,
                    ..Default::default()
                }
                .to_value()?,
            )
        };
        let delivery = Delivery {
            session: try_string(session)?,
            composition: composition.id.try_clone()?,
            id: id.try_clone()?,
            name: definition.name.try_clone()?,
            path,
            bundle,
        };
        Ok(Outcome::Capsule(self.enqueue_call(
            Action::Delivery(delivery),
            &client,
            method,
            &payload,
            now,
            runtime,
        )?))
    }
    pub(crate) fn finish_workflow_delivery(
        &mut self,
        index: usize,
        message: &p::WireMessage,
        now: &Timestamp,
        runtime: &mut impl Runtime,
    ) -> Result<Response> {
        let Action::Delivery(work) = &self.calls[index].action else {
            return Err(Error::Http(500, "missing deliverable operation"));
        };
        let result = (|| -> Result<String> {
            if !message.error.is_empty() {
                return Ok(message.error.try_clone()?);
            }
            let payload = message.payload.0.as_ref().unwrap_or(&Value::Null);
            let (bundle, content) = if work.bundle {
                (
                    Some(d::DeliverableBundle::from_value(payload)?),
                    String::new(),
                )
            } else {
                let files = d::WireMap::<d::Bytes>::from_value(payload)?;
                let bytes = files
                    .get(&work.path)
                    .and_then(|bytes| bytes.0.as_deref())
                    .ok_or(Error::Http(
                        409,
                        "deliverable file is missing or exceeds the file limit",
                    ))?;
                (None, spin_core::docker::utf8(bytes, false)?)
            };
            let id = runtime.next("del")?;
            let saved = self.store.put_workflow_deliverable(
                &work.session,
                &work.name,
                &content,
                bundle,
                Context { now, id: &id },
            )?;
            text(format_args!(
                "Deliverable {} revisie {} is opgeslagen; de reviewer ziet hem in Spin.",
                saved.name, saved.revision
            ))
            .map_err(Into::into)
        })();
        match result {
            Ok(text) => tool_result(&work.id, &text, !message.error.is_empty()),
            Err(error) => tool_result(&work.id, &text(format_args!("{error}"))?, true),
        }
    }
    pub(crate) fn workflow_mcp(
        &mut self,
        request: &Request<'_>,
        now: &Timestamp,
        runtime: &mut impl Runtime,
    ) -> Result<Option<Outcome>> {
        let Some(session) = request
            .path
            .strip_prefix("/api/workflow/mcp/")
            .filter(|id| !id.is_empty() && !id.contains('/'))
        else {
            return Ok(None);
        };
        let token = request
            .header("Authorization")
            .strip_prefix("Bearer ")
            .unwrap_or("")
            .trim();
        let expected = self.store.workflow_token(session);
        // De runtime logt elke geweigerde aanvraag (SPIN_REQUEST_FAILED error=HTTP 401: ...);
        // de tekst zegt of er nog een token voor de sessie bestaat, zodat een agent die
        // na het sluiten of herplannen van zijn stap nog een MCP-call doet herkenbaar is.
        if expected.is_empty() {
            return Err(Error::Http(
                401,
                "invalid workflow token: no token is stored for this session (step closed, requeued or finished)",
            ));
        }
        if token.is_empty()
            || !spin_security::constant_time_eq(
                expected.as_bytes(),
                spin_security::digest_hex(token.as_bytes())?.as_bytes(),
            )
        {
            return Err(Error::Http(
                401,
                "invalid workflow token: the token does not match the stored one (a newer agent owns this session)",
            ));
        }
        if !request.header("Origin").trim().is_empty() {
            return Err(Error::Http(
                403,
                "browser origins are not accepted by the internal MCP endpoint",
            ));
        }
        if request.method != "POST" {
            return Err(Error::Http(405, "method not allowed"));
        }
        if request.body.len() > 3 << 20 {
            return Err(Error::Http(413, "workflow request too large"));
        }
        let json = match Value::from_json(request.body) {
            Ok(value) => value,
            Err(_) => {
                return Ok(Some(Outcome::Response(rpc_error(
                    400,
                    &Value::Null,
                    -32700,
                    "invalid JSON-RPC request",
                )?)));
            }
        };
        let id = field(&json, "id");
        if string(&json, "jsonrpc") != "2.0" || string(&json, "method").trim().is_empty() {
            return Ok(Some(Outcome::Response(rpc_error(
                400,
                id,
                -32700,
                "invalid JSON-RPC request",
            )?)));
        }
        if json.as_object().is_none_or(|o| o.get("id").is_none()) {
            return Ok(Some(Outcome::Response(Response::empty(202)?)));
        }
        let params = field(&json, "params");
        let response = match string(&json, "method") {
            "initialize" => {
                let version = string(params, "protocolVersion");
                result(id, object(&[("protocolVersion", Value::string(if version.is_empty() { "2025-06-18" } else { version })?), ("capabilities", Value::from_json(br#"{"tools":{"listChanged":false}}"#)?), ("serverInfo", Value::from_json(br#"{"name":"spin-workflow","title":"Spin Workflow","version":"0.3.0"}"#)?)])?)?
            }
            "ping" => result(id, object(&[])?)?,
            "tools/list" => match self.workflow_tools(session) {
                Ok(tools) => result(id, object(&[("tools", tools)])?)?,
                Err(error) => rpc_error(200, id, -32603, &text(format_args!("{error}"))?)?,
            },
            "tools/call" => {
                if params.as_object().is_none()
                    || (!matches!(field(params, "arguments"), Value::Null)
                        && field(params, "arguments").as_object().is_none())
                {
                    rpc_error(200, id, -32602, "invalid tool arguments")?
                } else {
                    match self.call_workflow_tool(
                        session,
                        id,
                        string(params, "name").trim(),
                        field(params, "arguments"),
                        now,
                        runtime,
                    ) {
                        Ok(outcome) => return Ok(Some(outcome)),
                        Err(error) => tool_result(id, &text(format_args!("{error}"))?, true)?,
                    }
                }
            }
            _ => rpc_error(200, id, -32601, "method not found")?,
        };
        Ok(Some(Outcome::Response(response)))
    }
    fn workflow_tools(&self, session: &str) -> Result<Value> {
        let view = self.store.workflow_for_session(session)?;
        let mut tools = List::new();
        if view.run.status != d::PHASE_RUN_RUNNING {
            return Ok(tools.to_value()?);
        }
        if view.phase.id == d::BRAINSTORM_PHASE_ID {
            tools.push(tool("start_process", "Start het proces", "Leg de goal vast waar de brainstorm op uitkwam en start daarmee de gewone flow van de Template. Roep dit pas aan als de gebruiker het eens is met de goal. De goal is Markdown en wordt zo getoond: gebruik koppen, lijsten en acceptatiecriteria waar dat helpt.", object(&[("goal", short_text("De goal van de Job, in Markdown: wat er klaar moet zijn en waaraan je dat ziet")?)])?, &["goal"])?)?;
            return Ok(tools.to_value()?);
        }
        let options = object(&[
            ("type", Value::string("array")?),
            ("maxItems", 8_i64.to_value()?),
            ("items", object(&[("type", Value::string("string")?)])?),
            (
                "description",
                Value::string("Verwachte antwoorden, in de volgorde die je aanbeveelt")?,
            ),
        ])?;
        let questions = object(&[
            ("type", Value::string("array")?),
            ("minItems", 1_i64.to_value()?),
            ("maxItems", 6_i64.to_value()?),
            ("description", Value::string("Eén formulier met vragen")?),
            (
                "items",
                schema(
                    object(&[
                        ("question", short_text("Eén concrete vraag")?),
                        ("options", options),
                    ])?,
                    &["question"],
                )?,
            ),
        ])?;
        tools.push(tool("ask", "Vraag de gebruiker", "Stel de gebruiker één of meer concrete vragen tegelijk en pauzeer deze fase tot alles beantwoord is. Geef per vraag de antwoorden die je verwacht als options; de gebruiker kan altijd een eigen antwoord typen, dus voeg zelf geen optie 'anders' toe. Bundel alles wat je nu wilt weten in één ask.", object(&[("question", short_text("Verkorte vorm: één open vraag zonder opties")?), ("questions", questions)])?, &[])?)?;
        tools.push(tool("accept", "Accepteer fase", "Markeer deze fase als geslaagd en volg de geconfigureerde accept-overgang. De samenvatting wordt als Markdown getoond aan wie het besluit neemt: gebruik koppen, lijsten en code waar dat helpt.", object(&[("summary", short_text("Wat je opleverde en waarom het klaar is, in Markdown")?)])?, &[])?)?;
        tools.push(tool("reject", "Wijs fase af", "Wijs deze fase af met een concrete reden en volg de geconfigureerde reject-overgang. De reden wordt als Markdown getoond aan wie het besluit neemt: gebruik koppen, lijsten en code waar dat helpt.", object(&[("reason", short_text("Wat er ontbreekt of fout is en wat er moet gebeuren, in Markdown")?)])?, &["reason"])?)?;
        if !view.phase.deliverables.is_empty() {
            let mut names = List::new();
            for definition in view.phase.deliverables.iter() {
                names.push(definition.name.try_clone()?)?;
            }
            tools.push(tool("put_deliverable", "Lever deliverable op", "Zet een bestand of map uit /root/deliverables als nieuwe revisie van een deliverable: een Markdown-bestand voor een document, een map met index.html (eigen CSS, JS en afbeeldingen mogen los) of een afbeelding of PDF voor een visuele deliverable. Bewerk het bestand op schijf en put het opnieuw voor een volgende versie.", object(&[("name", object(&[("type", Value::string("string")?), ("enum", names.to_value()?)])?), ("path", short_text("Absoluut pad binnen /root/deliverables, bijvoorbeeld /root/deliverables/fo.md of /root/deliverables/website/")?)])?, &["name", "path"])?)?;
        }
        Ok(tools.to_value()?)
    }
    fn call_workflow_tool(
        &mut self,
        session: &str,
        id: &Value,
        name: &str,
        args: &Value,
        now: &Timestamp,
        runtime: &mut impl Runtime,
    ) -> Result<Outcome> {
        if self.acceptance_in_flight(session) {
            return Err(Error::Http(409, "workflow acceptance is in progress"));
        }
        let message = match name {
            "ask" => {
                let mut questions = List::new();
                if let Some(items) = field(args, "questions").as_array() {
                    for item in items {
                        if item.as_object().is_none() {
                            continue;
                        }
                        let mut options = List::new();
                        if let Some(values) = field(item, "options").as_array() {
                            for value in values {
                                if let Some(value) = value.as_str() {
                                    options.push(try_string(value)?)?;
                                }
                            }
                        }
                        questions.push(d::WorkflowQuestionItem {
                            question: try_string(string(item, "question").trim())?,
                            options,
                            ..Default::default()
                        })?;
                    }
                }
                if questions.is_empty() && !string(args, "question").trim().is_empty() {
                    questions.push(d::WorkflowQuestionItem {
                        question: try_string(string(args, "question").trim())?,
                        ..Default::default()
                    })?;
                }
                let question_id = runtime.next("que")?;
                let question = self.store.ask_workflow_questions(
                    session,
                    &questions,
                    Context {
                        now,
                        id: &question_id,
                    },
                )?;
                text(format_args!(
                    "{} klaar voor de gebruiker: {}. Beëindig nu je beurt; dezelfde ACP Session wordt met de antwoorden hervat.",
                    if question.items.len() > 1 {
                        text(format_args!("{} vragen staan", question.items.len()))?
                    } else {
                        try_string("Vraag staat")?
                    },
                    question.question
                ))?
            }
            "start_process" => {
                let (created, _) = self.store.start_process(
                    session,
                    string(args, "goal").trim(),
                    Mutation { now, ids: runtime },
                )?;
                let _ = self.schedule_session(&created.session, now, runtime);
                try_string(
                    "De goal staat vast en stap 1 van de Template start. Deze brainstorm is klaar; beëindig je beurt.",
                )?
            }
            "put_deliverable" => {
                return self.begin_workflow_delivery(
                    session,
                    id,
                    string(args, "name").trim(),
                    string(args, "path").trim(),
                    now,
                    runtime,
                );
            }
            "accept" => {
                return self.begin_workflow_accept(
                    session,
                    id,
                    string(args, "summary").trim(),
                    now,
                    runtime,
                );
            }
            "reject" => {
                return Ok(Outcome::Response(self.finish_workflow_tool(
                    session,
                    id,
                    "reject",
                    string(args, "reason").trim(),
                    now,
                    runtime,
                )?));
            }
            _ => return Err(Error::Http(400, "unknown workflow tool")),
        };
        Ok(Outcome::Response(tool_result(id, &message, false)?))
    }
    pub(crate) fn workspace_authentication(
        &self,
        workspace: &d::GitWorkspace,
        operator: &str,
    ) -> Result<Option<d::engine::GitAuthentication>> {
        if workspace.credential_scope == d::CREDENTIAL_SCOPE_PUBLIC
            && workspace.account_id.is_empty()
        {
            return if workspace.author_name.is_empty() && workspace.author_email.is_empty() {
                Ok(None)
            } else {
                Ok(Some(d::engine::GitAuthentication {
                    author_name: workspace.author_name.try_clone()?,
                    author_email: workspace.author_email.try_clone()?,
                    ..Default::default()
                }))
            };
        }
        let account = if !workspace.account_id.is_empty() && workspace.credential_scope.is_empty() {
            self.store.git_account(&workspace.account_id, operator)?
        } else {
            self.store
                .resolve_git_workspace_account(workspace, operator)?
        };
        self.credential_ready(account)?;
        Ok(Some(d::engine::GitAuthentication {
            username: try_string(if account.provider == "gitlab" {
                "oauth2"
            } else {
                &account.login
            })?,
            password: account.access_token.try_clone()?,
            author_name: if account.name.is_empty() {
                workspace.author_name.try_clone()?
            } else {
                account.name.try_clone()?
            },
            author_email: if account.email.is_empty() {
                workspace.author_email.try_clone()?
            } else {
                account.email.try_clone()?
            },
        }))
    }
    fn finish_workflow_tool(
        &mut self,
        session: &str,
        id: &Value,
        outcome: &str,
        detail: &str,
        now: &Timestamp,
        runtime: &mut impl Runtime,
    ) -> Result<Response> {
        let advance = self.store.complete_workflow_phase(
            session,
            outcome,
            detail,
            false,
            Mutation { now, ids: runtime },
        )?;
        let message = if let Some(next) = &advance.next_session {
            let _ = self.schedule_session(next, now, runtime);
            text(format_args!(
                "Fase afgerond. {} is als nieuwe Session gestart.",
                advance.phase_run.phase_name
            ))?
        } else if let Some(question) = advance.question {
            text(format_args!(
                "De workflow wacht op de gebruiker: {}",
                question.question
            ))?
        } else {
            try_string("Workflow afgerond.")?
        };
        tool_result(id, &message, false)
    }
    fn begin_workflow_accept(
        &mut self,
        session: &str,
        id: &Value,
        detail: &str,
        now: &Timestamp,
        runtime: &mut impl Runtime,
    ) -> Result<Outcome> {
        self.begin_accept(
            session,
            AcceptanceReply::Mcp(id.try_clone()?),
            detail,
            now,
            runtime,
        )
    }
    pub(crate) fn human_accept_route(
        &mut self,
        request: &Request<'_>,
        actor: &str,
        now: &Timestamp,
        runtime: &mut impl Runtime,
    ) -> Result<Option<Outcome>> {
        let Some(question) = request
            .path
            .strip_prefix("/api/workflow/questions/")
            .and_then(|s| s.strip_suffix("/answer"))
            .filter(|s| !s.contains('/'))
        else {
            return Ok(None);
        };
        if request.method != "POST" {
            return Ok(None);
        }
        let value: d::AnswerWorkflowQuestionRequest = Self::decode(request)?;
        let snapshot = self.store.snapshot()?;
        let question = snapshot
            .workflow_questions
            .iter()
            .find(|q| q.id == question && q.status == "open")
            .ok_or(Error::Http(404, "question not found"))?;
        if self.acceptance_in_flight(&question.session_id) {
            return Err(Error::Http(409, "workflow acceptance is in progress"));
        }
        if !value.action.eq_ignore_ascii_case("accept") {
            return Ok(None);
        }
        self.store
            .validate_workflow_question_transition(&question.id, "accept")?;
        self.begin_accept(
            &question.session_id,
            AcceptanceReply::Human {
                question: question.id.try_clone()?,
                actor: try_string(actor)?,
            },
            &value.reason,
            now,
            runtime,
        )
        .map(Some)
    }
    fn acceptance_in_flight(&self, session: &str) -> bool {
        self.calls.iter().any(|c| {
            !c.finished && matches!(&c.action, Action::Workflow(work) if work.session == session)
        })
    }
    fn finish_accept(
        &mut self,
        session: &str,
        reply: &AcceptanceReply,
        detail: &str,
        now: &Timestamp,
        runtime: &mut impl Runtime,
    ) -> Result<Response> {
        match reply {
            AcceptanceReply::Mcp(id) => {
                self.finish_workflow_tool(session, id, "accept", detail, now, runtime)
            }
            AcceptanceReply::Human { question, actor } => {
                let advance = self.store.answer_workflow_question(
                    question,
                    actor,
                    "accept",
                    detail,
                    Mutation { now, ids: runtime },
                )?;
                if let Some(next) = &advance.next_session {
                    let _ = self.schedule_session(next, now, runtime);
                }
                Response::json(200, &advance)
            }
        }
    }
    fn begin_accept(
        &mut self,
        session: &str,
        reply: AcceptanceReply,
        detail: &str,
        now: &Timestamp,
        runtime: &mut impl Runtime,
    ) -> Result<Outcome> {
        if self.acceptance_in_flight(session) {
            return Err(Error::Http(409, "workflow acceptance is in progress"));
        }
        if matches!(reply, AcceptanceReply::Mcp(_)) {
            self.store
                .validate_workflow_phase_transition(session, "accept")?;
        }
        let view = self.store.workflow_for_session(session)?;
        let expected = if matches!(reply, AcceptanceReply::Human { .. }) {
            d::PHASE_RUN_PENDING
        } else {
            d::PHASE_RUN_RUNNING
        };
        if view.run.status != expected {
            return Err(Error::Http(409, "phase is not running"));
        }
        for definition in view.phase.deliverables.iter().filter(|d| d.required) {
            if !view.deliverables.iter().any(|d| {
                d.phase_run_id == view.run.id && d.name.eq_ignore_ascii_case(&definition.name)
            }) {
                return Err(Error::Http(409, "required deliverable is missing"));
            }
        }
        if (matches!(reply, AcceptanceReply::Mcp(_))
            && (view.phase.accept.ask_user
                || view
                    .phase
                    .accept
                    .target
                    .eq_ignore_ascii_case(d::WORKFLOW_TARGET_ASK_USER)))
            || (matches!(reply, AcceptanceReply::Human { .. })
                && view.phase.executor == d::WORKFLOW_EXECUTOR_ACTION)
        {
            return Ok(Outcome::Response(
                self.finish_accept(session, &reply, detail, now, runtime)?,
            ));
        }
        let record = self.store.session(session)?;
        let composition = match self.store.composition(&record.prepared_composition_id) {
            Ok(composition) => composition.try_clone()?,
            Err(spin_store::Error::NotFound) => d::Composition {
                id: record.prepared_composition_id.try_clone()?,
                session_id: record.id.try_clone()?,
                operator: record.operator.try_clone()?,
                workspaces: self.job_workspaces(&view.job)?,
                ..Default::default()
            },
            Err(error) => return Err(error.into()),
        };
        if composition.runtime.as_ref().is_some_and(|r| r.stop_pending) {
            return Err(Error::Http(409, "capsule is stopping"));
        }
        let capsule = composition
            .runtime
            .as_ref()
            .filter(|r| r.status != "stopped");
        let subject = clamp(
            &text(format_args!("workflow({}): accepted", view.phase.id))?,
            200,
        )?;
        let summary = if detail.is_empty() {
            if view.run.summary.is_empty() {
                "Phase accepted"
            } else {
                &view.run.summary
            }
        } else {
            detail
        };
        let accepted_by = match &reply {
            AcceptanceReply::Mcp(_) => try_string("agent")?,
            AcceptanceReply::Human { actor, .. } => text(format_args!("user:{actor}"))?,
        };
        let body = clamp(
            &text(format_args!(
                "{}\n\nSpin-Job: {}\nSpin-Session: {session}\nSpin-Phase: {}\nSpin-Accepted-By: {accepted_by}",
                clamp(summary, 2000)?,
                view.job.id,
                view.phase.id
            ))?,
            4000,
        )?;
        let mut steps = Vec::new();
        for workspace in composition.changed_workspaces() {
            if steps.len() >= 32 {
                return Err(Error::Http(413, "too many workflow repositories"));
            }
            let authentication = self.workspace_authentication(workspace, &composition.operator)?;
            let step = if let Some(capsule) = capsule {
                Step {
                    method: p::METHOD_ACCEPT_WORKSPACE,
                    payload: p::AcceptWorkspacePayload {
                        runtime: capsule.try_clone()?,
                        acceptance: d::engine::WorkspaceAcceptance {
                            path: workspace.path.try_clone()?,
                            base_branch: workspace.bootstrap_ref.try_clone()?,
                            allow_changes: view.phase.allow_changes,
                            commit_subject: subject.try_clone()?,
                            commit_body: body.try_clone()?,
                            remote_ref: view.job.branch.try_clone()?,
                            authentication,
                        },
                    }
                    .to_value()?,
                }
            } else if view.phase.allow_changes {
                Step {
                    method: p::METHOD_ACCEPT_REPOSITORY,
                    payload: p::RepositoryAcceptPayload {
                        acceptance: d::engine::RepositoryAcceptance {
                            remote_url: workspace.remote_url.try_clone()?,
                            cache_key: workspace.repository_id.try_clone()?,
                            session_ref: record.git_ref.try_clone()?,
                            job_ref: view.job.branch.try_clone()?,
                            bootstrap_ref: workspace.bootstrap_ref.try_clone()?,
                            allow_changes: true,
                            commit_subject: subject.try_clone()?,
                            commit_body: body.try_clone()?,
                            authentication,
                        },
                    }
                    .to_value()?,
                }
            } else {
                continue;
            };
            d::try_push(&mut steps, step)?;
        }
        if steps.is_empty() {
            return Ok(Outcome::Response(
                self.finish_accept(session, &reply, detail, now, runtime)?,
            ));
        }
        let client = if let Some(capsule) = capsule {
            capsule.client_id.try_clone()?
        } else {
            self.choose_runner(now)?
        };
        let first = steps[0].payload.try_clone()?;
        let method = steps[0].method;
        let work = Acceptance {
            session: try_string(session)?,
            composition: self
                .store
                .session(session)?
                .prepared_composition_id
                .try_clone()?,
            reply,
            detail: try_string(detail)?,
            steps,
            at: 0,
        };
        Ok(Outcome::Capsule(self.enqueue_call(
            Action::Workflow(work),
            &client,
            method,
            &first,
            now,
            runtime,
        )?))
    }
    pub(crate) fn advance_workflow_accept(
        &mut self,
        index: usize,
        client: &str,
        message: &p::WireMessage,
        now: &Timestamp,
        runtime: &mut impl Runtime,
    ) -> Result<Option<Response>> {
        let Action::Workflow(work) = &self.calls[index].action else {
            return Err(Error::Http(500, "missing workflow operation"));
        };
        if !message.error.is_empty() {
            return Ok(Some(work.reply.error(&message.error)?));
        }
        let parsed = d::engine::WorkspaceAcceptanceResult::from_value(
            message.payload.0.as_ref().unwrap_or(&Value::Null),
        )?;
        if parsed.head.is_empty() {
            return Ok(Some(
                work.reply
                    .error("Git acceptance returned no confirmed HEAD")?,
            ));
        }
        if let Some(next) = work.steps.get(work.at + 1) {
            let method = next.method;
            let payload = next.payload.try_clone()?;
            self.continue_capsule(index, client, method, &payload, runtime)?;
            if let Action::Workflow(work) = &mut self.calls[index].action {
                work.at += 1;
            }
            return Ok(None);
        }
        let session = work.session.try_clone()?;
        let reply = work.reply.copy()?;
        let detail = work.detail.try_clone()?;
        match self.finish_accept(&session, &reply, &detail, now, runtime) {
            Ok(response) => Ok(Some(response)),
            Err(error) => Ok(Some(reply.error(&text(format_args!("{error}"))?)?)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Memory;
    impl Persistence for Memory {
        fn save(&mut self, _: &d::state::PersistedState) -> spin_store::Result {
            Ok(())
        }
    }
    fn equal(a: &Value, b: &Value) -> bool {
        match (a, b) {
            (Value::Object(a), Value::Object(b)) => {
                a.iter().count() == b.iter().count()
                    && a.iter()
                        .all(|(key, value)| b.get(key).is_some_and(|other| equal(value, other)))
            }
            (Value::Array(a), Value::Array(b)) => {
                a.len() == b.len() && a.iter().zip(b).all(|(a, b)| equal(a, b))
            }
            _ => a.to_json().unwrap() == b.to_json().unwrap(),
        }
    }
    #[test]
    fn workflow_tool_schemas_match_go_including_empty_tools_for_completed_phases() {
        let contracts =
            Value::from_json(include_bytes!("../../core/tests/fixtures/prompts.json")).unwrap();
        for contract in contracts.as_array().unwrap() {
            let item = contract.as_object().unwrap();
            let job = d::Job::from_value(item.get("job").unwrap()).unwrap();
            let session = d::Session::from_value(item.get("session").unwrap()).unwrap();
            let run = d::PhaseRun::from_value(item.get("run").unwrap()).unwrap();
            let id = session.id.try_clone().unwrap();
            let mut state = d::state::PersistedState::default();
            state.jobs.insert(job.id.try_clone().unwrap(), job).unwrap();
            state
                .sessions
                .insert(session.id.try_clone().unwrap(), session)
                .unwrap();
            state
                .phase_runs
                .insert(run.id.try_clone().unwrap(), run)
                .unwrap();
            let server = Server::new(Store::new(state, Memory));
            let actual = server.workflow_tools(&id).unwrap();
            let expected = item.get("tools").unwrap();
            assert!(
                equal(&actual, expected),
                "actual={} expected={}",
                actual.to_json().unwrap(),
                expected.to_json().unwrap()
            );
        }
    }
}
