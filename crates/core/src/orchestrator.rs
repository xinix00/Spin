//! Aanbevelingen uit een onveranderlijke Job/Session/Result-graaf.

use d::{Fallible, List, Recommendation, Snapshot, try_string};
use spin_domain as d;

/// Adviseert begrensde acties; autorisatie en uitvoering blijven bij de server.
pub fn recommend(snapshot: &Snapshot) -> Fallible<List<Recommendation>> {
    let mut out = List::default();
    for job in snapshot.jobs.iter() {
        if let Some(advice) = recommend_job(snapshot, job)? {
            out.push(advice)?;
        }
    }
    // Geen allocatie voor de sortering; gelijke prioriteiten zijn gelijkwaardig.
    out.as_mut_slice()
        .sort_unstable_by_key(|r| core::cmp::Reverse(r.priority));
    Ok(out)
}

fn recommend_job(snapshot: &Snapshot, job: &d::Job) -> Fallible<Option<Recommendation>> {
    if job.status == d::JOB_DONE || job.status == d::JOB_CANCELLED {
        return Ok(None);
    }
    let mut out = Recommendation {
        job_id: try_string(&job.id)?,
        ..Default::default()
    };
    let sessions = || snapshot.sessions.iter().filter(|s| s.job_id == job.id);
    if let Some(session) =
        sessions().find(|s| s.status != d::SESSION_COMPLETED && s.status != d::SESSION_CANCELLED)
    {
        let frozen = session.status == d::SESSION_FROZEN;
        out.action = try_string(if frozen {
            "restore_session"
        } else {
            "continue_session"
        })?;
        out.reason = try_string(if frozen {
            "De Session is frozen; restore het warmste compatibele Checkpoint."
        } else {
            "Er is al een uitvoerbare of actieve Session; behoud continuity en wacht op haar Result."
        })?;
        out.session_id = try_string(&session.id)?;
        out.checkpoint_id = try_string(&session.current_checkpoint_id)?;
        out.priority = 40;
        return Ok(Some(out));
    }
    for result in snapshot
        .results
        .iter()
        .filter(|r| r.job_id == job.id && r.status == d::RESULT_SUCCESS)
    {
        out.result_ids.push(try_string(&result.id)?)?;
    }
    match out.result_ids.len() {
        0 => {}
        1 => {
            out.action = try_string("select_result")?;
            out.reason = try_string(
                "Er is één succesvol Result met bewijs; leg het voor aan een menselijke reviewer.",
            )?;
            out.priority = 80;
            return Ok(Some(out));
        }
        _ => {
            out.action = try_string("start_critic")?;
            out.reason = try_string(
                "Er zijn meerdere succesvolle kandidaat-Results; laat ze vergelijken of synthetiseren vóór finale selectie.",
            )?;
            out.priority = 90;
            return Ok(Some(out));
        }
    }
    if let Some(result) = snapshot
        .results
        .iter()
        .rev()
        .find(|r| r.job_id == job.id && r.status != d::RESULT_SUCCESS)
    {
        out.action = try_string("fork_session")?;
        out.reason = try_string(
            "De laatste Session leverde geen volledig succesvol Result; maak een gerichte reparatiefork vanaf haar Result-Checkpoint.",
        )?;
        out.session_id = try_string(&result.session_id)?;
        out.checkpoint_id = try_string(&result.checkpoint_id)?;
        out.result_ids.push(try_string(&result.id)?)?;
        out.priority = 70;
    } else if let Some(session) = sessions().rev().find(|s| s.status == d::SESSION_COMPLETED) {
        out.action = try_string("inspect_session")?;
        out.reason = try_string(
            "Een completed Session mist een bruikbaar Result; inspecteer de contractinvariant.",
        )?;
        out.session_id = try_string(&session.id)?;
        out.priority = 60;
    } else {
        out.action = try_string("start_session")?;
        out.reason = try_string(
            "De Job heeft nog geen actieve Session of Result; start een primaire Session vanaf de Job-root.",
        )?;
        out.priority = 50;
    }
    Ok(Some(out))
}

#[cfg(test)]
mod tests {
    use super::*;
    use d::Wire;
    #[test]
    fn multiple_successes_need_critic() {
        let snapshot = Snapshot::from_json(br#"{"jobs":[{"id":"job-1","status":"comparing"}],"sessions":[{"id":"s1","job_id":"job-1","status":"completed"},{"id":"s2","job_id":"job-1","status":"completed"}],"results":[{"id":"r1","job_id":"job-1","status":"success"},{"id":"r2","job_id":"job-1","status":"success"}]}"#).unwrap();
        let r = recommend(&snapshot).unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].action, "start_critic");
        assert_eq!(r[0].result_ids.len(), 2);
    }
    #[test]
    fn partial_result_repairs_from_checkpoint() {
        let snapshot = Snapshot::from_json(br#"{"jobs":[{"id":"j","status":"active"}],"sessions":[{"id":"s","job_id":"j","status":"completed"}],"results":[{"id":"r","job_id":"j","session_id":"s","checkpoint_id":"c","status":"partial"}]}"#).unwrap();
        let r = recommend(&snapshot).unwrap();
        assert_eq!(r[0].action, "fork_session");
        assert_eq!(r[0].checkpoint_id, "c");
    }
    #[test]
    fn active_sessions_and_closed_jobs() {
        let mut snapshot = Snapshot::from_json(br#"{"jobs":[{"id":"j","status":"active"}],"sessions":[{"id":"s","job_id":"j","status":"frozen"}],"results":[{"id":"r","job_id":"j","status":"success"}]}"#).unwrap();
        assert_eq!(recommend(&snapshot).unwrap()[0].action, "restore_session");
        snapshot.jobs.as_mut_slice()[0].status = try_string(d::JOB_DONE).unwrap();
        assert!(recommend(&snapshot).unwrap().is_empty());
    }
}
