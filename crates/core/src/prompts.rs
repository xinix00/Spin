//! Begrensde workflowinstructies uit een bevestigde domeinsnapshot.
use crate::validation::{invalid, normalized, text};
use alloc::string::String;
use spin_domain::{self as d, Map, TryClone, try_push_str, try_string};
/// De prompttekst van een stap. Agents weigeren grote prompts hard (codex:
/// 1.048.576 tekens voor tekst én ingebedde bijlagen samen, 05-10-2026).
pub const PROMPT_TEXT_BYTES: usize = 512 << 10;
struct Prompt(String);
impl Prompt {
    fn push(&mut self, value: &str) -> d::Fallible {
        if self.0.len().saturating_add(value.len()) > PROMPT_TEXT_BYTES {
            return Err(invalid("prompt", "workflow context exceeds byte budget"));
        }
        try_push_str(&mut self.0, value)
    }
    fn write(&mut self, value: core::fmt::Arguments<'_>) -> d::Fallible {
        self.push(&text(value)?)
    }
}
fn indent(value: &str, prefix: &str) -> d::Fallible<String> {
    let mut out = String::new();
    for (index, line) in value.split('\n').enumerate() {
        if index != 0 {
            try_push_str(&mut out, "\n")?;
            try_push_str(&mut out, prefix)?;
        }
        try_push_str(&mut out, line)?;
    }
    Ok(out)
}
fn latest<'a>(snapshot: &'a d::Snapshot, job: &str) -> d::Fallible<Map<&'a d::Deliverable>> {
    let mut out: Map<&d::Deliverable> = Map::new();
    for deliverable in snapshot.deliverables.iter().filter(|d| d.job_id == job) {
        let key = normalized(&deliverable.name)?;
        if out
            .get(&key)
            .is_none_or(|old| old.revision < deliverable.revision)
        {
            out.insert(key, deliverable)?;
        }
    }
    Ok(out)
}
fn shape(deliverable: &d::Deliverable) -> d::Fallible<String> {
    let Some(bundle) = deliverable
        .bundle
        .as_ref()
        .filter(|_| d::deliverable_is_bundle(&deliverable.kind))
    else {
        return try_string("Markdown");
    };
    if bundle.folder {
        text(format_args!(
            "{}, {} bestanden",
            if bundle.entry.is_empty() {
                "map"
            } else {
                "map met index.html"
            },
            bundle.files
        ))
    } else {
        bundle.content_type.try_clone()
    }
}
fn ask(definition: &d::DeliverableDefinition) -> d::Fallible<String> {
    let (description, extension) = match definition.kind.as_str() {
        d::DELIVERABLE_KIND_PDF => ("Eén PDF-bestand", ".pdf"),
        d::DELIVERABLE_KIND_IMAGE => ("Eén afbeelding (png, jpg, gif, webp, svg)", ".png"),
        d::DELIVERABLE_KIND_FOLDER => (
            "Een map met minstens één bestand; met index.html erin (eigen CSS, JS en afbeeldingen mogen los) toont Spin de pagina",
            "/",
        ),
        d::DELIVERABLE_KIND_FILE => ("Eén bestand, welke vorm ook", ".<ext>"),
        _ => ("Markdown-bestand", ".md"),
    };
    text(format_args!(
        "{description}; bijvoorbeeld {}/{}{extension}",
        d::DELIVERABLE_DIRECTORY,
        d::deliverable_slug(&definition.name)?
    ))
}
/// Dezelfde fasecontext bij launch en bij de eerste chat na een verse agentsessie.
pub fn workflow(
    snapshot: &d::Snapshot,
    job: &d::Job,
    session: &d::Session,
    run: &d::PhaseRun,
    phase: &d::WorkflowPhase,
) -> d::Fallible<String> {
    let mut prompt = Prompt(String::new());
    let latest = latest(snapshot, &job.id)?;
    let source = snapshot
        .jobs
        .iter()
        .find(|source| !job.forked_from_job_id.is_empty() && source.id == job.forked_from_job_id);
    let reference = if job.reference.is_empty() {
        String::new()
    } else {
        text(format_args!("Referentie: {}\n", job.reference))?
    };
    prompt.write(format_args!("Je voert Spin workflowfase {:?} uit (poging {}).\n\nJOB\nNaam: {}\n{}{}: {}\n\nINSTRUCTIES\n{}\n", phase.name, run.attempt, job.title, reference, if phase.id == d::BRAINSTORM_PHASE_ID { "Concept goal" } else { "Goal" }, job.objective, phase.instructions))?;
    if !job.branch.is_empty() {
        let base = if job.base_ref.trim().is_empty() {
            "de basisbranch"
        } else {
            job.base_ref.trim()
        };
        prompt.push("\nGIT\n")?;
        let repositories = job.job_repositories()?;
        if repositories.len() > 1 {
            prompt.write(format_args!("Deze Job werkt in meer repositories; elke repository staat in zijn eigen map onder {}:\n", d::WORKSPACE_ROOT))?;
            for repository in repositories.iter() {
                if repository.mode == d::REPOSITORY_MODE_REFERENCE {
                    prompt.write(format_args!("- {} · {} · ALLEEN TER REFERENTIE op branch {}: lees erin, wijzig er niets en push er niets; wat je erin verandert gaat nergens heen.\n", repository.directory()?, repository.name, repository.base_ref))?;
                } else {
                    prompt.write(format_args!("- {} · {} · AANPASSEN: dezelfde Job-branch en jouw branch als hieronder, basis {}.\n", repository.directory()?, repository.name, repository.base_ref))?;
                }
            }
            prompt.push("\nDe branches hieronder gelden in elke repository die je aanpast; de basisbranch is per repository de genoemde.\n")?;
        }
        prompt.write(format_args!("Basisbranch: {base} · waar deze Job uiteindelijk op landt; lokaal origin/{base}.\nJob-branch: {} · het geaccepteerde werk van alle eerdere fases van deze Job; lokaal origin/{}. Elke fase komt hierop als één commit.\nJouw branch: {} · HEAD in deze workspace, begonnen op de Job-branch.\n", job.branch, job.branch, session.git_ref))?;
        if let Some(source) = source.filter(|source| !source.branch.is_empty()) {
            prompt.write(format_args!("Vorige Job-branch: {} · het werk van de Job waar deze een vervolg op is; lokaal origin/{}, alleen ter inzage.\n", source.branch, source.branch))?;
        }
    }
    if !job.forked_from_job_id.is_empty() {
        let source = source.ok_or_else(|| invalid("prompt", "fork source Job is unavailable"))?;
        prompt.write(format_args!("\nVERVOLGCONTEXT\nDeze Job is een vervolg op de afgesloten Job {:?}. Wat daar gemaakt is staat op branch {} (lokaal origin/{}); deze Job begint op de basisbranch en landt daar ook.\nOorspronkelijke goal: {}\n", source.title, source.branch, source.branch, source.objective))?;
        let mut first = true;
        for attachment in snapshot
            .job_attachments
            .iter()
            .filter(|a| a.job_id == source.id)
        {
            if first {
                prompt.push("Bijlagen uit die Job zijn opnieuw read-only beschikbaar:\n")?;
                first = false;
            }
            prompt.write(format_args!(
                "- {} ({}): {}\n",
                attachment.name, attachment.media_type, attachment.capsule_path
            ))?;
        }
        let source_latest = self::latest(snapshot, &source.id)?;
        if !source_latest.is_empty() {
            prompt.write(format_args!("De laatste documenten uit die Job staan als bestanden in {}; lees wat je nodig hebt:\n", d::PREVIOUS_JOB_DELIVERABLE_DIRECTORY))?;
            for (_, deliverable) in source_latest.iter() {
                prompt.write(format_args!(
                    "- {} · {} (revisie {})\n",
                    deliverable.capsule_path_in(d::PREVIOUS_JOB_DELIVERABLE_DIRECTORY)?,
                    deliverable.name,
                    deliverable.revision
                ))?;
            }
        }
        for run in snapshot
            .phase_runs
            .iter()
            .filter(|run| run.job_id == source.id)
        {
            if let Some(result) = &run.action_result
                && !result.url.is_empty()
            {
                prompt.write(format_args!("Remote resultaat: {}\n", result.url))?;
            }
        }
    }
    let mut first = true;
    for attachment in snapshot
        .job_attachments
        .iter()
        .filter(|a| a.job_id == job.id)
    {
        if first {
            prompt.push("\nJOB-BIJLAGEN\nDeze immutable bestanden zijn door een gebruiker als bron toegevoegd. Bekijk de relevante bijlagen daadwerkelijk; wijzig ze niet en kopieer ze niet naar Git.\n")?;
            first = false;
        }
        prompt.write(format_args!(
            "- {} ({}, {} bytes): {}\n",
            attachment.name, attachment.media_type, attachment.size, attachment.capsule_path
        ))?;
    }
    if phase.allow_changes {
        prompt.push("\nREPOSITORYBELEID\nJe mag bestanden in de repository wijzigen. Commit of push niet zelf: bij accept maakt Spin zo nodig één resultaatcommit en publiceert die naar de Job-branch.\n")?;
    } else {
        prompt.push("\nREPOSITORYBELEID\nDeze fase levert geen codewijzigingen op. Je mag in deze wegwerp-workspace wel vrij werken: restore, build, tests en experimenten, ook als dat bestanden in de repository schrijft of aanpast. Niets daarvan gaat mee: bij accept bevestigt Spin de onveranderde Git-basis van deze Session en commit niets. Commit of push niet zelf")?;
        if !phase.deliverables.is_empty() {
            prompt.push("; lever je bevindingen op in de hieronder gevraagde documenten")?;
        }
        prompt.push(".\n")?;
    }
    if !latest.is_empty() || !phase.deliverables.is_empty() {
        prompt.write(format_args!("\nDELIVERABLES\nDe deliverables van deze Job staan in {}, elk als bestand of map; lees wat je nodig hebt.\n", d::DELIVERABLE_DIRECTORY))?;
        for (_, deliverable) in latest.iter() {
            prompt.write(format_args!(
                "- {}: {} (revisie {}, {})\n",
                deliverable.name,
                deliverable.capsule_path()?,
                deliverable.revision,
                shape(deliverable)?
            ))?;
        }
        if !phase.inject.is_empty() {
            prompt.push("Verplichte context voor deze stap: ")?;
            for (i, name) in phase.inject.iter().enumerate() {
                if i != 0 {
                    prompt.push(", ")?;
                }
                prompt.push(name)?;
            }
            prompt.push(".\n")?;
        }
    }
    if !phase.deliverables.is_empty() {
        prompt.write(format_args!("\nOP TE LEVEREN\nMaak of bewerk het bestand op schijf en zet het met put_deliverable(name, path) als nieuwe revisie; het pad ligt altijd binnen {}. Gebruik de naam exact zoals vermeld.\n", d::DELIVERABLE_DIRECTORY))?;
        for definition in phase.deliverables.iter() {
            prompt.write(format_args!(
                "- {} ({}): {}\n  {}\n",
                definition.name,
                if definition.required {
                    "VERPLICHT"
                } else {
                    "OPTIONEEL"
                },
                if definition.description.trim().is_empty() {
                    "Deliverable voor deze workflowfase"
                } else {
                    definition.description.trim()
                },
                ask(definition)?
            ))?;
        }
    }
    let mut comments_written = false;
    for deliverable in snapshot
        .deliverables
        .iter()
        .filter(|deliverable| deliverable.job_id == job.id)
    {
        if latest
            .get(&normalized(&deliverable.name)?)
            .is_none_or(|current| current.id != deliverable.id)
        {
            continue;
        }
        let mut first = true;
        for comment in snapshot
            .deliverable_comments
            .iter()
            .filter(|c| c.deliverable_id == deliverable.id)
        {
            if !comments_written {
                prompt.push("\nCOMMENTS OP ACTUELE DELIVERABLES\nVerwerk deze opmerkingen. Ze horen bij de momenteel laatste revisies en staan los van de ACCEPT/REJECT-route.\n")?;
                comments_written = true;
            }
            if first {
                prompt.write(format_args!(
                    "\n{} (revisie {})\n",
                    deliverable.name, deliverable.revision
                ))?;
                first = false;
            }
            if comment.selected_text.trim().is_empty() {
                prompt.write(format_args!(
                    "- {} over de hele revisie:\n\n  {}\n",
                    comment.author,
                    indent(comment.body.trim(), "  ")?
                ))?;
            } else {
                prompt.write(format_args!(
                    "- {} bij de tekst:\n  > {}\n\n  Opmerking: {}\n",
                    comment.author,
                    indent(comment.selected_text.trim(), "  > ")?,
                    indent(comment.body.trim(), "  ")?
                ))?;
            }
        }
    }
    let mut first = true;
    for question in snapshot.workflow_questions.iter().filter(|q| {
        q.job_id == job.id
            && q.kind == "agent"
            && q.status == "answered"
            && !matches!(q.answer.as_str(), "chat" | "retry" | "closed" | "")
    }) {
        if first {
            prompt.push("\nVASTGELEGDE BESLUITEN\n")?;
            first = false;
        }
        if question.answer == "answered" && !question.items.is_empty() {
            for item in question.items.iter() {
                prompt.write(format_args!("- {} → {}\n", item.question, item.answer))?;
            }
        } else {
            prompt.write(format_args!("- {} → ", question.question))?;
            for ch in question.answer.chars().flat_map(char::to_uppercase) {
                let mut buffer = [0; 4];
                prompt.push(ch.encode_utf8(&mut buffer))?;
            }
            if !question.reason.is_empty() {
                prompt.write(format_args!(": {}", question.reason))?;
            }
            prompt.push("\n")?;
        }
    }
    let history = snapshot
        .phase_runs
        .iter()
        .filter(|p| p.job_id == job.id && p.id != run.id);
    let mut rejection = 0;
    let mut first = true;
    for previous in history.clone() {
        let mut last_reviewed = "";
        for outcome in previous
            .agent_outcomes
            .iter()
            .filter(|o| o.outcome == "reject" && !o.detail.trim().is_empty())
        {
            last_reviewed = outcome.detail.trim();
            rejection += 1;
            if first {
                prompt.push("\nGEGEVEN FEEDBACK\n")?;
                first = false;
            }
            prompt.write(format_args!(
                "- {}, poging {}, afwijzing {rejection}: {last_reviewed}\n",
                previous.phase_name, previous.attempt
            ))?;
        }
        let own = previous.reject_reason.trim();
        if own.is_empty() || own == last_reviewed {
            continue;
        }
        if first {
            prompt.push("\nGEGEVEN FEEDBACK\n")?;
            first = false;
        }
        if last_reviewed.is_empty() {
            rejection += 1;
            prompt.write(format_args!(
                "- {}, poging {}, afwijzing {rejection}: {own}\n",
                previous.phase_name, previous.attempt
            ))?;
        } else {
            prompt.write(format_args!(
                "- {}, poging {}, afwijzing door de gebruiker: {own}\n",
                previous.phase_name, previous.attempt
            ))?;
        }
    }
    if phase.resolve_merge {
        let base = if job.base_ref.trim().is_empty() {
            "de basisbranch"
        } else {
            job.base_ref.trim()
        };
        prompt.write(format_args!("\nMERGE OPLOSSEN\nDeze stap bestaat om {base} in de Job-branch te krijgen:\n1. De merge staat al klaar: Spin heeft `git merge origin/{base}` in deze workspace gestart, dus de conflictmarkers staan in de bestanden en MERGE_HEAD is gezet. Staat hij er niet, voer hem dan zelf uit; haal nooit zelf iets op, je hebt geen Git-credentials.\n"))?;
        prompt.push("2. Los elk conflictbestand op. Wat deze Job maakte blijft van de Job; wat anderen intussen op de basisbranch veranderden blijft van hen; raakt een bestand beide, voeg dan beide kanten samen.\n3. `git add` de opgeloste bestanden en rond af met `git commit` zonder de tekst te veranderen. Dit is de enige stap waar je zelf commit; pushen doe je nooit.\n4. Bouw de merge nooit met de hand na: geen bestanden overschrijven, geen diff toepassen, geen nieuwe branch. Zonder echte merge-commit mislukt de Merge-stap opnieuw.\n5. Sluit af met accept en schrijf in één zin welke bestanden conflicteerden en hoe je ze hebt opgelost.\nWeigert git met \"refusing to merge unrelated histories\" of vindt hij geen merge-base, reject dan met precies die melding: de workspace is dan verkeerd klaargezet en Spin lost dat op, niet jij.\n")?;
    }
    if run.restarts > 0 {
        prompt.write(format_args!("\nOPNIEUW GESTART\nDeze poging is {} keer door de gebruiker opnieuw gestart met een verse agent. De workspace is bewaard: wat er al gedaan was staat erin, kijk daar eerst naar en ga verder in plaats van opnieuw te beginnen.\n", run.restarts))?;
        for note in run.restart_notes.iter() {
            prompt.write(format_args!("- Aanwijzing van de gebruiker: {note}\n"))?;
        }
        if !run.restart_transcript.is_empty() {
            prompt.push("\nGESPREK TOT DE HERSTART\nDe gebruiker ging terug naar een punt in het gesprek van de vorige poging en paste zijn bericht daar aan; dat aangepaste bericht is de laatste aanwijzing hierboven. Dit was het gesprek tot dat punt; wat daarna kwam vervalt.\n")?;
            for line in run.restart_transcript.iter() {
                prompt.write(format_args!(
                    "{}: {}\n",
                    if line.role == "user" {
                        "Gebruiker"
                    } else {
                        "Agent"
                    },
                    indent(&line.text, "  ")?
                ))?;
            }
        }
    }
    let mut first = true;
    for previous in history.filter(|p| !p.reject_reason.is_empty()) {
        for revision in snapshot
            .code_review_revisions
            .iter()
            .filter(|r| r.context_phase_run_id == previous.id)
        {
            for comment in snapshot
                .code_review_comments
                .iter()
                .filter(|c| c.revision_id == revision.id)
            {
                if first {
                    prompt.push("\nCODECOMMENTS UIT AFGEWEZEN REVIEWS\nDeze opmerkingen horen bij immutable diffrevisies. Verwerk ze in deze poging; verander de oude comments zelf nooit.\n")?;
                    first = false;
                }
                let location = if comment.end_line > comment.start_line {
                    text(format_args!(
                        "{}:{}-{}",
                        comment.path, comment.start_line, comment.end_line
                    ))?
                } else {
                    text(format_args!("{}:{}", comment.path, comment.start_line))?
                };
                prompt.write(format_args!(
                    "- {} ({}, {}) door {}:\n    {}\n  Opmerking: {}\n",
                    location,
                    comment.side,
                    previous.phase_name,
                    comment.author,
                    indent(comment.selected.trim(), "    ")?,
                    indent(comment.body.trim(), "  ")?
                ))?;
            }
        }
    }
    if phase.id == d::BRAINSTORM_PHASE_ID {
        prompt.push("\nWERKWIJZE\nDit is een chat over de concept goal hierboven: praat, vraag door, stel aanscherpingen voor. Ga niets uitzoeken of onderzoeken tot de gebruiker daarom vraagt; wacht op wat hij wil bespreken en reageer daarop. Je enige workflowtool is start_process(goal); roep die pas aan als de gebruiker het eens is met de definitieve goal, en beëindig daarna je beurt. De goal mag Markdown zijn (koppen, lijsten, acceptatiecriteria) en wordt zo op de Job getoond en aan elke stap meegegeven. Commit of push nooit; verander niets in de repository.\n")?;
    } else {
        prompt.push("\nWERKWIJZE\nGebruik uitsluitend de aangeboden Spin workflowtools om workflowstate te wijzigen. ask stelt één formulier met één of meer vragen, elk met de antwoordopties die je verwacht; stel alleen wat je niet zelf kunt uitzoeken en bundel alles in één ask. ")?;
        prompt.push(if phase.deliverables.is_empty() { "Deze fase vraagt geen deliverables; put_deliverable is daarom niet beschikbaar en je hoeft niets op te leveren. " } else { "Zet ieder hierboven gevraagd document of visueel resultaat met put_deliverable als revisie; dat overschrijft de vorige revisie van deze stap. " })?;
        prompt.push("Commit of push nooit zelf. Sluit de fase altijd af met accept, of reject met een concrete reden. ACCEPT laat Spin de Session gecontroleerd in de Job-branch opnemen.\n")?;
    }
    Ok(prompt.0)
}
/// Antwoorden van één formulier blijven één vervolgbericht in dezelfde agentsessie.
pub fn answers(question: &d::WorkflowQuestion) -> d::Fallible<String> {
    let mut prompt = Prompt(try_string("De gebruiker heeft je vragen beantwoord:\n")?);
    for (index, item) in question.items.iter().enumerate() {
        prompt.write(format_args!(
            "{}. {}\n   → {}{}\n",
            index + 1,
            item.question,
            item.answer,
            if item.other && !item.options.is_empty() {
                " (eigen antwoord, geen van de opties)"
            } else {
                ""
            }
        ))?;
    }
    prompt.push("Ga verder met de fase op basis van deze antwoorden.")?;
    Ok(prompt.0)
}
