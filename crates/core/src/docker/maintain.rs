//! Spin is eigenaar van de Docker-daemon van zijn runners en houdt hem schoon.
//!
//! Twee Spins kunnen één daemon delen. Wat het `spin.server`-label van een Spin
//! draagt, ruimt alleen die Spin op; wat geen eigenaar heeft (geen label: van
//! vóór 3.0.50, of niet van Spin), mag elke runner opruimen. Niets ruimde op:
//! op 27-10-2026 vulde Docker.raw (390 GiB) de schijf van de Mac mini, waarna
//! elke Docker-opdracht vastliep en agents niet meer startten.
use super::*;

impl Docker {
    /// Houdt hoogstens `keep` Session-volumes van deze Spin zonder container.
    ///
    /// Zo'n volume hoort bij een gestopte Session; een herstart begint dan weer
    /// van haar gepushte branch. De nieuwste blijven staan voor een Retry.
    pub async fn prune_workspaces(
        &self,
        executor: &mut impl Executor,
        keep: usize,
    ) -> Result<usize> {
        let listing = self
            .control(
                executor,
                &[
                    "volume",
                    "ls",
                    "--quiet",
                    "--filter",
                    "dangling=true",
                    "--filter",
                    "label=spin.kind=workspace",
                ],
            )
            .await?;
        let mut command = self.command(&[
            "volume",
            "inspect",
            "--format",
            "{{.CreatedAt}}\t{{index .Labels \"spin.server\"}}\t{{.Name}}",
        ])?;
        let mut listed = 0;
        for name in listing.lines().map(str::trim).filter(|n| !n.is_empty()).take(1024) {
            command.arg(name)?;
            listed += 1;
        }
        if listed <= keep {
            return Ok(0);
        }
        let output = checked(executor.run(command).await?)?;
        let mut entries = Vec::new();
        for line in output.lines() {
            let mut fields = line.split('\t').map(str::trim);
            let created = fields.next().unwrap_or("");
            let server = fields.next().unwrap_or("");
            let name = fields.next().unwrap_or("");
            if !name.starts_with("spin-work") || server.is_empty() || !self.owns(server) {
                continue;
            }
            d::try_push(&mut entries, (created, name))?;
        }
        entries.sort_unstable_by(|a, b| b.cmp(a));
        let mut removed = 0;
        for (_, name) in entries.iter().skip(keep) {
            // Een volume dat intussen weer een container heeft, weigert Docker.
            if self.control(executor, &["volume", "rm", name]).await.is_ok() {
                removed += 1;
            }
        }
        Ok(removed)
    }
    /// Ruimt op wat geen Spin bezit en niets meer gebruikt: gestopte containers
    /// en volumes zonder `spin.server`-label, en images zonder tag.
    ///
    /// Draaiende containers blijven staan, net als alles met het label van een
    /// Spin. `until=1h` spaart wat een runner net aanmaakte en nog gaat starten
    /// of taggen (een buildcontainer, een tussenlaag).
    pub async fn prune_ownerless(&self, executor: &mut impl Executor) -> Result<()> {
        let mut first = None;
        for args in [
            &[
                "container",
                "prune",
                "--force",
                "--filter",
                "label!=spin.server",
                "--filter",
                "until=1h",
            ][..],
            &[
                "volume",
                "prune",
                "--all",
                "--force",
                "--filter",
                "label!=spin.server",
            ][..],
            &["image", "prune", "--force", "--filter", "until=1h"][..],
        ] {
            if let Err(error) = self.control(executor, args).await {
                first.get_or_insert(error);
            }
        }
        first.map_or(Ok(()), Err)
    }
}
