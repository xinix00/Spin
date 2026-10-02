//! Een composition is een stapel eigen diffs; de eigenaar houdt de images.

use alloc::vec::Vec;
use spin_domain::{Artifact, Composition, Name, try_push};

/// Grenzen aan de planning, onafhankelijk van de JSON-bodygrens.
pub const MAX_LAYERS: usize = 4096;
/// Hoogstens zoveel versies doorzoeken naar een vervangende snapshot.
pub const MAX_SUCCESSORS: usize = 64;

/// Een onuitvoerbare stapel, met het betrokken ID.
#[derive(Debug, PartialEq)]
pub enum Error {
    /// Geen bekende lagen geselecteerd.
    Empty(Name),
    /// De onderste laag heeft geen bruikbare basis.
    NoBase(Name),
    /// Een verwijderde diff wordt door geen latere versie gedragen.
    MissingImage(Name),
    /// De stapel overschrijdt het vaste budget.
    TooMany(usize),
    /// De heap heeft geen ruimte voor het plan.
    OutOfMemory,
}
impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Empty(id) => write!(f, "composition {id} has no layers"),
            Self::NoBase(id) => write!(
                f,
                "layer {id} at the bottom of the stack has no restorable image"
            ),
            Self::MissingImage(id) => write!(
                f,
                "layer {id} has no restorable image and no newer version above it in the stack"
            ),
            Self::TooMany(n) => write!(f, "layer count {n} exceeds {MAX_LAYERS}"),
            Self::OutOfMemory => f.write_str("layer plan allocation failed"),
        }
    }
}
impl core::error::Error for Error {}
/// Het resultaat van een laagbewerking.
pub type Result<T> = core::result::Result<T, Error>;

/// Een laag boven de basis, geleend zolang de eigenaar het plan uitvoert.
#[derive(Debug)]
pub struct LayerStep<'a> {
    /// De image waarvan de bestanden nodig zijn.
    pub artifact: &'a Artifact,
    /// Kopieer het hele bestandssysteem in plaats van alleen de eigen diff.
    pub full: bool,
}
/// De basis plus de geordende stappen daarboven.
#[derive(Debug)]
pub struct LayerPlan<'a> {
    /// De diepste image die exact de prefix van de stapel bevat.
    pub base: &'a Artifact,
    /// Iedere resterende laag in toepassingsvolgorde.
    pub steps: Vec<LayerStep<'a>>,
}
impl<'a> LayerPlan<'a> {
    /// De images die deze runner nodig heeft.
    pub fn needed(&self) -> impl Iterator<Item = &'a Artifact> + '_ {
        core::iter::once(self.base).chain(self.steps.iter().map(|s| s.artifact))
    }
}

fn restorable(a: &Artifact) -> bool {
    a.snapshot.driver == "docker"
        && a.snapshot.restorable
        && !a.snapshot.r#ref.is_empty()
        && a.snapshot_pruned_at.is_none()
}
fn push<T>(v: &mut Vec<T>, item: T) -> Result<()> {
    try_push(v, item).map_err(|_| Error::OutOfMemory)
}

// De Go-map bewaart de laatste waarde bij een dubbel ID.
fn find<'a>(all: &'a [Artifact], id: &str) -> Option<&'a Artifact> {
    all.iter().rev().find(|a| a.id == id)
}

/// Maakt een imageplan zonder images te kopiëren of runtime-staat te wijzigen.
pub fn plan_layers<'a>(
    composition: &Composition,
    artifacts: &'a [Artifact],
) -> Result<LayerPlan<'a>> {
    if artifacts.len() > MAX_LAYERS {
        return Err(Error::TooMany(artifacts.len()));
    }
    if composition.layers.len() > MAX_LAYERS {
        return Err(Error::TooMany(composition.layers.len()));
    }
    let mut order: Vec<&Artifact> = Vec::new();
    for id in composition.layers.iter() {
        if order.iter().any(|a| a.id == *id) {
            continue;
        }
        if let Some(a) = find(artifacts, id) {
            push(&mut order, a)?;
        }
    }
    let first = order
        .first()
        .ok_or(Error::Empty(Name::new(&composition.id)))?;
    let mut base_index = None;
    for (index, a) in order.iter().enumerate() {
        if !restorable(a) {
            continue;
        }
        let members = closure(a, artifacts)?;
        if members.len() == index + 1
            && order
                .iter()
                .take(index + 1)
                .all(|a| members.contains(&a.id.as_str()))
        {
            base_index = Some(index);
        }
    }
    let base_index = base_index.ok_or(Error::NoBase(Name::new(&first.id)))?;
    let base = order
        .get(base_index)
        .copied()
        .ok_or(Error::NoBase(Name::new(&first.id)))?;
    let mut plan = LayerPlan {
        base,
        steps: Vec::new(),
    };
    let mut carried: Vec<&str> = Vec::new();
    for (index, artifact) in order.iter().enumerate().skip(base_index + 1) {
        if !restorable(artifact) {
            let next = successor(artifact, index, &order, artifacts)
                .ok_or(Error::MissingImage(Name::new(&artifact.id)))?;
            push(&mut carried, next)?;
            continue;
        }
        let full = carried.contains(&artifact.id.as_str())
            || artifact.parent_artifact_ids.is_empty()
            || artifact.parent_artifact_ids.iter().any(|p| {
                order
                    .iter()
                    .position(|a| a.id == *p)
                    .is_none_or(|at| at >= index)
            });
        push(&mut plan.steps, LayerStep { artifact, full })?;
    }
    Ok(plan)
}

fn closure<'a>(root: &'a Artifact, all: &'a [Artifact]) -> Result<Vec<&'a str>> {
    let mut found = Vec::new();
    push(&mut found, root.id.as_str())?;
    let mut cursor = 0;
    // Geen recursie: ook een lange of cyclische parentketen blijft begrensd.
    while let Some(id) = found.get(cursor).copied() {
        if let Some(a) = find(all, id) {
            for parent in a.parent_artifact_ids.iter() {
                if find(all, parent).is_some() && !found.contains(&parent.as_str()) {
                    push(&mut found, parent.as_str())?;
                }
            }
        }
        cursor += 1;
    }
    Ok(found)
}
fn successor<'a>(
    artifact: &'a Artifact,
    index: usize,
    order: &[&Artifact],
    all: &'a [Artifact],
) -> Option<&'a str> {
    let mut next = artifact.superseded_by.as_str();
    for _ in 0..MAX_SUCCESSORS {
        if next.is_empty() {
            return None;
        }
        let newer = find(all, next)?;
        if order
            .iter()
            .position(|a| a.id == next)
            .is_some_and(|at| at > index)
            && restorable(newer)
        {
            return Some(next);
        }
        next = &newer.superseded_by;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use spin_domain::{Wire, try_string};
    fn artifacts() -> Vec<Artifact> {
        let data = [br#"{"id":"git","snapshot":{"driver":"docker","ref":"git","restorable":true}}"#.as_slice(),
        br#"{"id":"v1","parent_artifact_ids":["git"],"superseded_by":"v2","snapshot":{"driver":"docker","ref":"v1","restorable":true}}"#,
        br#"{"id":"v2","parent_artifact_ids":["v1"],"snapshot":{"driver":"docker","ref":"v2","restorable":true}}"#,
        br#"{"id":"cred","parent_artifact_ids":["v1"],"snapshot":{"driver":"docker","ref":"cred","restorable":true}}"#,
        br#"{"id":"dotnet","snapshot":{"driver":"docker","ref":"dotnet","restorable":true}}"#];
        data.iter()
            .map(|s| Artifact::from_json(s).unwrap())
            .collect()
    }
    fn composition(ids: &[&str]) -> Composition {
        let mut c = Composition::default();
        for id in ids {
            c.layers.push(try_string(id).unwrap()).unwrap();
        }
        c
    }
    #[test]
    fn deepest_exact_prefix_and_unrelated_root() {
        let all = artifacts();
        let plan = plan_layers(&composition(&["git", "v1", "cred", "dotnet"]), &all).unwrap();
        assert_eq!(plan.base.id, "cred");
        assert_eq!(plan.steps.len(), 1);
        assert!(plan.steps[0].full);
    }
    #[test]
    fn credential_over_edited_tool_is_diff() {
        let all = artifacts();
        let plan = plan_layers(&composition(&["git", "v1", "v2", "cred"]), &all).unwrap();
        assert_eq!(plan.base.id, "v2");
        assert_eq!(plan.steps[0].artifact.id, "cred");
        assert!(!plan.steps[0].full);
    }
    #[test]
    fn pruned_version_carried_by_successor() {
        let mut all = artifacts();
        all[1].snapshot_pruned_at = Some(Default::default());
        let plan = plan_layers(&composition(&["dotnet", "git", "v1", "v2", "cred"]), &all).unwrap();
        assert_eq!(plan.base.id, "dotnet");
        let steps: Vec<_> = plan
            .steps
            .iter()
            .map(|s| (s.artifact.id.as_str(), s.full))
            .collect();
        assert_eq!(steps, [("git", true), ("v2", true), ("cred", false)]);
        assert_eq!(plan.needed().count(), 4);
    }
    #[test]
    fn missing_cycle_and_duplicates() {
        let mut all = artifacts();
        assert!(matches!(
            plan_layers(&composition(&["missing"]), &all),
            Err(Error::Empty(_))
        ));
        assert!(matches!(
            plan_layers(&composition(&["v1"]), &all),
            Err(Error::NoBase(_))
        ));
        let plan = plan_layers(&composition(&["git", "git", "missing"]), &all).unwrap();
        assert_eq!(plan.needed().count(), 1);
        all[1].snapshot.restorable = false;
        all[1].superseded_by = try_string("v1").unwrap();
        assert!(matches!(
            plan_layers(&composition(&["git", "v1"]), &all),
            Err(Error::MissingImage(_))
        ));
    }
}
