//! Jobattachments worden eerst gestaged en krijgen pas daarna een duurzame Job-binding.
use crate::snapshot::by_time;
use crate::{Error, Persistence, Result, Store};
use spin_core::validation::normalized;
use spin_domain::{self as d, List, Timestamp, TryClone, try_string};
impl<P: Persistence> Store<P> {
    /// Een Job krijgt maximaal acht bijlagen en veertig MiB aan inhoud.
    pub fn create_job_attachment(
        &mut self,
        req: d::CreateJobAttachmentRequest,
        now: &Timestamp,
    ) -> Result<d::JobAttachment> {
        let a = d::JobAttachment {
            id: try_string(req.id.trim())?,
            job_id: try_string(req.job_id.trim())?,
            name: try_string(req.name.trim())?,
            media_type: try_string(req.media_type.trim())?,
            size: req.size,
            sha256: try_string(req.sha256.trim())?,
            capsule_path: try_string(req.capsule_path.trim())?,
            created_by: normalized(&req.operator)?,
            created_at: now.try_clone()?,
        };
        if a.created_by.is_empty()
            || !a.id.starts_with("att_")
            || a.name.is_empty()
            || a.name.len() > 180
            || a.media_type.is_empty()
            || a.size < 1
            || a.size > 15 << 20
            || a.sha256.len() != 64
            || !a.capsule_path.starts_with("/spin/job-attachments/")
        {
            return Err(Error::Conflict("invalid Job attachment metadata"));
        }
        self.edit(|state| {
            if state.job_attachments.get(&a.id).is_some() {
                return Err(Error::Conflict("attachment already exists"));
            }
            if !a.job_id.is_empty() {
                let job = state.jobs.get_mut(&a.job_id).ok_or(Error::NotFound)?;
                if !job.allows_operator(&a.created_by)
                    || matches!(job.status.as_str(), d::JOB_DONE | d::JOB_CANCELLED)
                    || job.attachment_ids.len() >= 8
                {
                    return Err(Error::Conflict("Job cannot accept this attachment"));
                }
                let mut total = a.size;
                for id in job.attachment_ids.iter() {
                    if let Some(old) = state.job_attachments.get(id) {
                        total = total
                            .checked_add(old.size)
                            .ok_or(Error::Conflict("attachment size overflow"))?;
                    }
                }
                if total > 40 << 20 {
                    return Err(Error::Conflict(
                        "Job attachments may be at most 40 MiB in total",
                    ));
                }
                job.attachment_ids.push(a.id.try_clone()?)?;
                job.updated_at = now.try_clone()?;
            }
            state
                .job_attachments
                .insert(a.id.try_clone()?, a.try_clone()?)?;
            Ok(a)
        })
    }
    /// Gestagede bijlagen zijn privé; een gekoppelde bijlage is collaboratief leesbaar.
    pub fn job_attachment(&self, id: &str, operator: &str) -> Result<&d::JobAttachment> {
        let a = self
            .state
            .job_attachments
            .get(id.trim())
            .ok_or(Error::NotFound)?;
        let operator = normalized(operator)?;
        if operator.is_empty() || (a.job_id.is_empty() && a.created_by != operator) {
            return Err(Error::Conflict("attachment belongs to another operator"));
        }
        if !a.job_id.is_empty() && self.state.jobs.get(&a.job_id).is_none() {
            return Err(Error::NotFound);
        }
        Ok(a)
    }
    /// De bijlagen staan in aanmaakvolgorde.
    pub fn job_attachments(&self, id: &str) -> Result<List<d::JobAttachment>> {
        let mut out = List::new();
        for (_, a) in self
            .state
            .job_attachments
            .iter()
            .filter(|(_, a)| a.job_id == id.trim())
        {
            out.push(a.try_clone()?)?;
        }
        by_time(&mut out, |a| &a.created_at, false)?;
        Ok(out)
    }
    /// Alleen de maker verwijdert een bijlage die nog niet bij een Job hoort.
    pub fn delete_staged_job_attachment(
        &mut self,
        id: &str,
        operator: &str,
    ) -> Result<d::JobAttachment> {
        let operator = normalized(operator)?;
        let id = id.trim();
        self.edit(|state| {
            let a = state.job_attachments.get(id).ok_or(Error::NotFound)?;
            if !a.job_id.is_empty() || a.created_by != operator {
                return Err(Error::Conflict(
                    "attachment is bound or belongs to another operator",
                ));
            }
            state.job_attachments.remove(id).ok_or(Error::NotFound)
        })
    }
}
