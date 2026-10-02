//! ACP-configopties en oudere mode/model-antwoorden krijgen hetzelfde veldcontract.
use super::*;
/// Eén instelling met de methode die deze agent daarvoor aanbiedt.
#[derive(Default)]
pub struct Setting {
    /// Agent-ID, bijvoorbeeld reasoning_effort.
    pub id: String,
    /// Naam voor de browser.
    pub name: String,
    /// mode, model of thought_level; onbekende categorieën blijven behouden.
    pub category: String,
    /// Door de agent gerapporteerde waarde.
    pub current: String,
    /// De aangeboden waarden in hun oorspronkelijke volgorde.
    pub values: d::List<d::AgentOption>,
    /// session/set_mode, session/set_model of session/set_config_option.
    pub method: String,
    /// Expliciete _meta.kind=full_access heeft voorrang op naamherkenning.
    pub full_access: String,
}
fn field<'a>(value: &'a Value, key: &str) -> &'a Value {
    value
        .as_object()
        .and_then(|v| v.get(key))
        .unwrap_or(&Value::Null)
}
fn string(value: &Value, key: &str) -> d::Fallible<String> {
    match field(value, key) {
        Value::Null => Ok(String::new()),
        value => String::from_value(value),
    }
}
fn array<'a>(value: &'a Value, key: &str) -> &'a [Value] {
    field(value, key).as_array().unwrap_or_default()
}
fn lower(value: &str) -> d::Fallible<String> {
    let mut out = try_string(value)?;
    out.make_ascii_lowercase();
    Ok(out)
}
fn preferred(value: &Value, first: &str, second: &str) -> d::Fallible<String> {
    let first = string(value, first)?;
    if first.is_empty() {
        string(value, second)
    } else {
        Ok(first)
    }
}
impl Wire for Setting {
    fn to_value(&self) -> d::Fallible<Value> {
        object(&[
            ("ID", self.id.to_value()?),
            ("Name", self.name.to_value()?),
            ("Category", self.category.to_value()?),
            ("Current", self.current.to_value()?),
            ("Values", self.values.to_value()?),
            ("Method", self.method.to_value()?),
            ("FullAccess", self.full_access.to_value()?),
        ])
    }
    fn from_value(value: &Value) -> d::Fallible<Self> {
        Ok(Self {
            id: string(value, "ID")?,
            name: string(value, "Name")?,
            category: string(value, "Category")?,
            current: string(value, "Current")?,
            values: d::List::from_value(field(value, "Values"))?,
            method: string(value, "Method")?,
            full_access: string(value, "FullAccess")?,
        })
    }
}
impl Setting {
    /// Iedere instelling gebruikt de exacte parameternaam van haar onderhandelde methode.
    pub fn params(&self, session: &str, value: &str) -> d::Fallible<Value> {
        let mut params = Object::new();
        params.push("sessionId", Value::string(session)?)?;
        match self.method.as_str() {
            "session/set_mode" => params.push("modeId", Value::string(value)?)?,
            "session/set_model" => params.push("modelId", Value::string(value)?)?,
            _ => {
                params.push("configId", self.id.to_value()?)?;
                params.push("value", Value::string(value)?)?;
            }
        }
        Ok(Value::Object(params))
    }
    /// De capsule is de bestaande sandbox; alleen een werkelijk aangeboden mode wordt gekozen.
    pub fn full_access(&self) -> d::Fallible<Option<&str>> {
        if !self.full_access.is_empty() {
            return Ok((self.full_access != self.current).then_some(&self.full_access));
        }
        for value in self.values.iter() {
            let name = lower(&value.value)?;
            if name.contains("full-access")
                || name.contains("full_access")
                || name.contains("bypass")
                || name == "yolo"
            {
                return Ok((value.value != self.current).then_some(value.value.as_str()));
            }
        }
        Ok(None)
    }
}
/// Configopties winnen van oudere dedicated mode/model-staat van dezelfde categorie.
pub fn normalize(created: &Value) -> d::Fallible<Vec<Setting>> {
    let mut settings = Vec::new();
    let options = array(created, "configOptions");
    if options.len() > 128 {
        return Err(invalid("ACP settings", "too many config options"));
    }
    for option in options {
        let id = string(option, "id")?;
        let mut category = string(option, "category")?;
        if category.is_empty() {
            let name = lower(&id)?;
            category = try_string(if name.contains("model") {
                "model"
            } else if ["reason", "effort", "thought", "think"]
                .iter()
                .any(|part| name.contains(part))
            {
                "thought_level"
            } else if name.contains("mode") {
                "mode"
            } else {
                &id
            })?;
        }
        let mut setting = Setting {
            id,
            name: string(option, "name")?,
            category,
            current: field(option, "currentValue")
                .as_str()
                .map(try_string)
                .transpose()?
                .unwrap_or_default(),
            values: d::List::default(),
            method: try_string("session/set_config_option")?,
            full_access: String::new(),
        };
        let values = array(option, "options");
        if values.len() > 1024 {
            return Err(invalid("ACP settings", "too many option values"));
        }
        for value in values {
            setting.values.push(d::AgentOption {
                value: string(value, "value")?,
                name: string(value, "name")?,
                description: string(value, "description")?,
            })?;
            if field(field(value, "_meta"), "kind").as_str() == Some("full_access") {
                setting.full_access = string(value, "value")?;
            }
        }
        d::try_push(&mut settings, setting)?;
    }
    for (category, key, available, current, method, title) in [
        (
            "mode",
            "modes",
            "availableModes",
            "currentModeId",
            "session/set_mode",
            "Mode",
        ),
        (
            "model",
            "models",
            "availableModels",
            "currentModelId",
            "session/set_model",
            "Model",
        ),
    ] {
        let source = field(created, key);
        let values = array(source, available);
        if values.is_empty() || settings.iter().any(|s| s.category == category) {
            continue;
        }
        if values.len() > 1024 {
            return Err(invalid("ACP settings", "too many mode/model values"));
        }
        let mut setting = Setting {
            id: try_string(category)?,
            name: try_string(title)?,
            category: try_string(category)?,
            current: string(source, current)?,
            values: d::List::default(),
            method: try_string(method)?,
            full_access: String::new(),
        };
        for value in values {
            let id = if category == "mode" {
                string(value, "id")?
            } else {
                preferred(value, "modelId", "value")?
            };
            if field(field(value, "_meta"), "kind").as_str() == Some("full_access") {
                setting.full_access = id.try_clone()?;
            }
            setting.values.push(d::AgentOption {
                value: id,
                name: preferred(value, "name", "title")?,
                description: string(value, "description")?,
            })?;
        }
        d::try_push(&mut settings, setting)?;
    }
    Ok(settings)
}
/// De opties gaan ongewijzigd naar het bestaande laag-/browsermodel.
pub fn options(
    settings: &[Setting],
    agent_name: &str,
    now: &d::Timestamp,
) -> d::Fallible<d::AgentOptions> {
    let mut options = d::AgentOptions {
        agent_name: try_string(agent_name)?,
        fetched_at: now.try_clone()?,
        ..Default::default()
    };
    for setting in settings {
        match setting.category.as_str() {
            "model" => options.models = setting.values.try_clone()?,
            "mode" => options.modes = setting.values.try_clone()?,
            "thought_level" => options.reasoning_efforts = setting.values.try_clone()?,
            _ => {}
        }
    }
    Ok(options)
}
/// Bestaande auto-acceptvoorkeur: allow_always, dan allow_once, daarna een niet-rejectoptie.
pub fn allow_option(params: &Value) -> d::Fallible<Option<(String, String)>> {
    let choices = array(params, "options");
    for kind in ["allow_always", "allow_once", ""] {
        for choice in choices {
            let id = field(choice, "optionId").as_str().unwrap_or("");
            let candidate = field(choice, "kind").as_str().unwrap_or("");
            if !id.is_empty()
                && (candidate == kind || (kind.is_empty() && !candidate.starts_with("reject")))
            {
                return Ok(Some((try_string(id)?, string(choice, "name")?)));
            }
        }
    }
    Ok(None)
}
