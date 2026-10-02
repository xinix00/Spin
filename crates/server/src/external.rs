//! Uitgaande HTTP-opdrachten reizen als waarden; alleen de app verwerkt hun resultaat.
use super::*;
use alloc::vec::Vec;
use d::{List, try_string};
/// Een begrensd uitgaand providerverzoek, uitsluitend gemaakt door de serverlogica.
pub struct NetworkRequest {
    /// Correlatie met de eigenaar; geen providersecret.
    pub id: String,
    /// HTTP-methode.
    pub method: String,
    /// Volledige URL; de runtime volgt nooit redirects met credentials.
    pub url: String,
    /// Koppen, inclusief credentials alleen voor de bedoelde origin.
    pub headers: List<(String, String)>,
    /// Hoogstens 1 MiB requestinhoud.
    pub body: Vec<u8>,
}
/// Het begrensde providerantwoord, zonder transport of gedeelde toestand.
pub struct NetworkResponse {
    /// HTTP-status van de provider.
    pub status: u16,
    /// Hoogstens 1 MiB inhoud.
    pub body: Vec<u8>,
}
/// Een browser wacht op een uitgaande provideroperatie.
pub struct NetworkWait {
    pub(crate) id: String,
}
// Hoogstens 32 opdrachten; iedere variant bezit zijn eigen begrensde providerstaat.
#[allow(clippy::large_enum_variant)]
pub(crate) enum Work {
    OAuth(crate::oauth::Exchange),
    Pull(crate::pulls::Pull),
    Refresh(d::GitAccount),
}
pub(crate) struct Call {
    pub(crate) id: String,
    pub(crate) request: Option<NetworkRequest>,
    pub(crate) work: Work,
    pub(crate) expires: u64,
    pub(crate) response: Option<Response>,
}
impl<P: Persistence> Server<P> {
    /// Draagt één klaarstaand verzoek over aan de begrensde transportpool.
    pub fn take_network_request(&mut self) -> Option<NetworkRequest> {
        self.network.iter_mut().find_map(|c| c.request.take())
    }
    /// De transporttaak geeft alleen bytes terug; credentials en mutaties blijven hier.
    pub fn finish_network(
        &mut self,
        id: &str,
        reply: Result<NetworkResponse>,
        now: &Timestamp,
        random: &mut impl Runtime,
    ) -> Result {
        let Some(index) = self
            .network
            .iter()
            .position(|c| c.id == id && c.response.is_none())
        else {
            return Ok(());
        };
        let pull = match &self.network[index].work {
            Work::Pull(work) => Some(work.session.try_clone()?),
            _ => None,
        };
        let refresh = matches!(self.network[index].work, Work::Refresh(_));
        let response = if refresh {
            self.finish_refresh(index, reply, now).map(Some)
        } else if pull.is_some() {
            self.advance_pull(index, reply, now, random)
        } else {
            self.advance_oauth(index, reply, now, random)
        };
        match response {
            Ok(None) => {}
            Ok(Some(response)) => self.network[index].response = Some(response),
            Err(_) if refresh => {
                self.network[index].response = Some(Response::empty(204)?);
            }
            Err(_) if pull.is_some() => {
                // A stale workflow or failed Store commit is local to this provider
                // request. Retire it before surfacing the error to the runtime log.
                self.network[index].response = Some(Response::empty(204)?);
                if let Some(id) = &pull {
                    let result=self.finish_action(id,"reject","De GitHub-actie is niet afgerond; controleer de Git-verbinding en probeer opnieuw.",true,now,random);
                    self.network.remove(index);
                    return result.map(|_| ());
                }
            }
            Err(_) => {
                self.network[index].response =
                    Some(crate::oauth::redirect("/?git_oauth=failed#git")?)
            }
        }
        if (pull.is_some() || refresh) && self.network[index].response.is_some() {
            self.network.remove(index);
        }
        Ok(())
    }
    /// Een verbroken browserverbinding laat het providerresultaat veilig afmaken.
    pub fn poll_network(&mut self, wait: &NetworkWait) -> Result<Option<Response>> {
        let index = self
            .network
            .iter()
            .position(|c| c.id == wait.id)
            .ok_or(Error::Http(404, "provider operation expired"))?;
        if self.network[index].response.is_none() {
            return Ok(None);
        }
        Ok(self.network.remove(index).response)
    }
    pub(crate) fn queue_network(
        &mut self,
        mut request: NetworkRequest,
        work: Work,
        now: &Timestamp,
        random: &mut impl Runtime,
    ) -> Result<NetworkWait> {
        let time = now.time()?.0;
        self.network.retain(|c| c.expires > time);
        if self.network.len() >= 32 {
            return Err(Error::Http(503, "provider request capacity reached"));
        }
        let id = random.next("net")?;
        request.id = id.try_clone()?;
        let wait = NetworkWait {
            id: id.try_clone()?,
        };
        d::try_push(
            &mut self.network,
            Call {
                id,
                request: Some(request),
                work,
                expires: time.saturating_add(600_000_000_000),
                response: None,
            },
        )?;
        Ok(wait)
    }
}
pub(crate) fn request(
    method: &str,
    url: &str,
    token: &str,
    body: Vec<u8>,
    content_type: &str,
) -> Result<NetworkRequest> {
    if body.len() > 1 << 20 {
        return Err(Error::Http(413, "provider request exceeds budget"));
    }
    let mut headers = List::new();
    for (key, value) in [
        ("Accept", "application/json"),
        ("User-Agent", "Spin"),
        ("Content-Type", content_type),
    ] {
        if !value.is_empty() {
            headers.push((try_string(key)?, try_string(value)?))?;
        }
    }
    if !token.is_empty() {
        headers.push((
            try_string("Authorization")?,
            spin_core::validation::text(format_args!("Bearer {token}"))?,
        ))?;
    }
    Ok(NetworkRequest {
        id: String::new(),
        method: try_string(method)?,
        url: try_string(url)?,
        headers,
        body,
    })
}
