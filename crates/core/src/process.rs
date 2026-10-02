//! Een begrensde procesopdracht; alleen de hostadapter mag hem uitvoeren.
use alloc::{string::String, vec::Vec};
use spin_domain::{self as d, Map, try_push, try_string};

/// Boven deze grens wordt een opdracht geweigerd vóór er een OS-proces bestaat.
pub const ARGUMENT_BYTES: usize = 1 << 20;
/// Data voor één proces; programma, argv en omgeving blijven afzonderlijk.
pub struct Command {
    /// Het expliciete programma; argumenten worden nooit door een shell geïnterpreteerd.
    pub program: String,
    /// Argumenten in de gegeven volgorde.
    pub args: Vec<String>,
    /// Alleen expliciet ingestelde omgevingsvariabelen.
    pub environment: Map<String>,
    /// De gesloten invoerstroom; langlevende streams gebruiken de raw adapter.
    pub input: Vec<u8>,
    /// Monotone deadline in milliseconden na spawn.
    pub timeout_ms: u64,
    /// Gezamenlijke limiet voor stdout en stderr.
    pub output_limit: usize,
    /// Beide kindkanalen delen één stroom, in de volgorde waarin ze schrijven.
    pub merge_stderr: bool,
    argument_bytes: usize,
}
impl Command {
    /// Begint een opdracht met een deadline van één minuut en 8 MiB antwoordbudget.
    pub fn new(program: &str) -> d::Fallible<Self> {
        if program.is_empty() || program.len() > 4096 || program.contains('\0') {
            return Err(crate::validation::invalid(
                "program",
                "invalid program name",
            ));
        }
        Ok(Self {
            program: try_string(program)?,
            args: Vec::new(),
            environment: Map::new(),
            input: Vec::new(),
            timeout_ms: 60_000,
            output_limit: 8 << 20,
            merge_stderr: false,
            argument_bytes: program.len(),
        })
    }
    /// Controleert ook rechtstreeks gewijzigde publieke velden vóór spawn.
    pub fn validate(&self) -> d::Fallible {
        if self.program.is_empty()
            || self.program.len() > 4096
            || self.program.contains('\0')
            || self.args.len() > 256
            || self.environment.len() > 128
            || self.input.len() > 16 << 20
            || self.output_limit > 64 << 20
            || self.timeout_ms == 0
            || self.timeout_ms > 24 * 60 * 60 * 1000
        {
            return Err(crate::validation::invalid(
                "process",
                "invalid process budget",
            ));
        }
        let mut remaining = ARGUMENT_BYTES - self.program.len();
        for value in self.args.iter().map(String::as_str).chain(
            self.environment
                .iter()
                .flat_map(|(key, value)| [key, value.as_str()]),
        ) {
            if value.contains('\0') || value.len() > remaining {
                return Err(crate::validation::invalid(
                    "process",
                    "argument budget exceeded",
                ));
            }
            remaining -= value.len();
        }
        if self
            .environment
            .iter()
            .any(|(key, _)| key.is_empty() || key.contains('='))
        {
            return Err(crate::validation::invalid(
                "environment",
                "invalid variable name",
            ));
        }
        Ok(())
    }
    /// Eén argv-waarde, zonder quoting of evaluatie.
    pub fn arg(&mut self, value: &str) -> d::Fallible {
        if value.contains('\0')
            || self.args.len() >= 256
            || value.len() > ARGUMENT_BYTES.saturating_sub(self.argument_bytes)
        {
            return Err(crate::validation::invalid(
                "argv",
                "argument budget exceeded",
            ));
        }
        try_push(&mut self.args, try_string(value)?)?;
        self.argument_bytes += value.len();
        Ok(())
    }
    /// Zet een kleine expliciete environment; geheimen hoeven daardoor niet in argv.
    pub fn env(&mut self, name: &str, value: &str) -> d::Fallible {
        if name.is_empty()
            || name.contains(['=', '\0'])
            || value.contains('\0')
            || self.environment.len() >= 128
            || value.len() + name.len() > ARGUMENT_BYTES.saturating_sub(self.argument_bytes)
        {
            return Err(crate::validation::invalid(
                "environment",
                "environment budget exceeded",
            ));
        }
        self.environment
            .insert(try_string(name)?, try_string(value)?)?;
        self.argument_bytes += value.len() + name.len();
        Ok(())
    }
    /// Begrensde stdin wordt faalbaar overgenomen; het transport levert terugdruk.
    pub fn input(&mut self, data: &[u8]) -> d::Fallible {
        if data.len() > 16 << 20 {
            return Err(crate::validation::invalid("stdin", "input exceeds 16 MiB"));
        }
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(data.len())
            .map_err(|_| d::Error::OutOfMemory)?;
        bytes.extend_from_slice(data);
        self.input = bytes;
        Ok(())
    }
}
