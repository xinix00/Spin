//! De veldgebonden versleuteling en de loginmigratie van de opgeslagen state.
use crate::{Cipher, Entropy, Error, Result, decode_base64, encode_base64};
use alloc::string::String;
use spin_domain::{self as d, TryClone, state::PersistedState, try_push_str};

fn purpose(parts: &[&str]) -> Result<String> {
    let mut value = String::new();
    for part in parts {
        try_push_str(&mut value, part)?;
    }
    Ok(value)
}

fn visit_strings(
    state: &mut PersistedState,
    mut transform: impl FnMut(&str, &str) -> Result<String>,
) -> Result {
    for (id, server) in state.mcp_servers.iter_mut() {
        for (kind, secrets) in [("env", &mut server.env), ("header", &mut server.headers)] {
            for secret in secrets.as_mut_slice() {
                secret.value = transform(
                    &secret.value,
                    &purpose(&["mcp:", id, ":", kind, ":", &secret.name])?,
                )?;
            }
        }
    }
    for (id, account) in state.git_accounts.iter_mut() {
        account.access_token = transform(
            &account.access_token,
            &purpose(&["git-account:", id, ":access"])?,
        )?;
        account.refresh_token = transform(
            &account.refresh_token,
            &purpose(&["git-account:", id, ":refresh"])?,
        )?;
    }
    state.worker_token = transform(&state.worker_token, "runners:worker-token")?;
    for (provider, config) in state.git_oauth_configurations.iter_mut() {
        config.client_secret = transform(
            &config.client_secret,
            &purpose(&["git-oauth:", provider, ":client-secret"])?,
        )?;
    }
    Ok(())
}

impl Cipher {
    /// Maakt een opslagkopie; de actieve state bevat nooit ciphertext.
    pub fn encrypt_state(
        &self,
        state: &PersistedState,
        entropy: &mut impl Entropy,
    ) -> Result<PersistedState> {
        // Een nog niet gemigreerde invoer mag niet stil zijn logins verliezen.
        if !state.login_states.is_empty() {
            return Err(Error::Payload);
        }
        let mut sealed = state.try_clone()?;
        visit_strings(&mut sealed, |value, purpose| {
            self.encrypt(value, purpose, entropy)
        })?;
        for (id, login) in sealed.logins.iter_mut() {
            for (path, data) in login.files.iter_mut() {
                let encoded = encode_base64(data.0.as_deref().unwrap_or_default(), true)?;
                let value =
                    self.encrypt(&encoded, &purpose(&["login:", id, ":", path])?, entropy)?;
                data.0 = Some(value.into_bytes());
            }
        }
        Ok(sealed)
    }

    /// Ontsleutelt alle velden vóór publicatie en migreert logins van vóór v1.28.53.
    /// De runtime levert voor iedere nieuwe legacy-login een unieke `lgn`-ID.
    pub fn decrypt_state(
        &self,
        sealed: &PersistedState,
        mut login_id: impl FnMut() -> Result<String>,
    ) -> Result<PersistedState> {
        let mut state = sealed.try_clone()?;
        visit_strings(&mut state, |value, purpose| self.decrypt(value, purpose))?;
        for (id, login) in state.logins.iter_mut() {
            self.decrypt_files(&mut login.files, id)?;
        }
        for (key, legacy) in state.login_states.iter_mut() {
            self.decrypt_files(&mut legacy.files, key)?;
            if legacy.files.is_empty() || state.logins.iter().any(|(_, login)| login.key == *key) {
                continue;
            }
            let id = login_id()?;
            if id.is_empty() || state.logins.get(&id).is_some() {
                return Err(Error::Payload);
            }
            let login = d::Login {
                id: id.try_clone()?,
                key: d::try_string(key)?,
                number: 1,
                files: legacy.files.try_clone()?,
                created_at: legacy.updated_at.try_clone()?,
                updated_at: legacy.updated_at.try_clone()?,
                ..d::Login::default()
            };
            state.logins.insert(id, login)?;
        }
        state.login_states = d::WireMap::default();
        Ok(state)
    }

    fn decrypt_files(&self, files: &mut d::WireMap<d::Bytes>, id: &str) -> Result {
        for (path, bytes) in files.iter_mut() {
            let sealed = core::str::from_utf8(bytes.0.as_deref().unwrap_or_default())
                .map_err(|_| Error::Payload)?;
            let value = self.decrypt(sealed, &purpose(&["login:", id, ":", path])?)?;
            bytes.0 = Some(decode_base64(&value)?);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use d::Wire;
    struct Nonce(u8);
    impl Entropy for Nonce {
        fn fill(&mut self, bytes: &mut [u8]) -> Result {
            bytes.fill(self.0);
            self.0 += 1;
            Ok(())
        }
    }
    #[test]
    fn state_roundtrip_binds_each_secret_to_its_field() {
        let cipher = Cipher::new([7; 32]);
        let state = PersistedState::from_json(br#"{
          "mcp_servers":{"m":{"env":[{"name":"TOKEN","value":"environment"}],"headers":[{"name":"Auth","value":"header"}]}},
          "git_accounts":{"a":{"access_token":"access","refresh_token":"refresh"}},
          "worker_token":"worker", "git_oauth_configurations":{"github":{"client_secret":"oauth"}},
          "logins":{"lgn1":{"key":"derek/tool:codex","files":{"/auth":"AP8B"}}}
        }"#).unwrap();
        let sealed = cipher.encrypt_state(&state, &mut Nonce(0)).unwrap();
        assert_eq!(sealed.worker_token.get(..7), Some("enc:v1:"));
        assert_ne!(sealed.logins, state.logins);
        assert_eq!(
            cipher
                .decrypt_state(&sealed, || Err(Error::Payload))
                .unwrap(),
            state
        );
        let mut misplaced = sealed.try_clone().unwrap();
        let account = misplaced.git_accounts.get_mut("a").unwrap();
        core::mem::swap(&mut account.access_token, &mut account.refresh_token);
        assert_eq!(
            cipher
                .decrypt_state(&misplaced, || Err(Error::Payload))
                .unwrap_err(),
            Error::Authentication
        );
        assert_eq!(state.worker_token, "worker");
    }
    #[test]
    fn legacy_login_is_migrated_once_with_the_old_aad() {
        let cipher = Cipher::new([7; 32]);
        let mut state = PersistedState::default();
        let key = "derek/tool:codex";
        let mut files = d::WireMap::new();
        let sealed = cipher
            .encrypt(
                "AP8B",
                &purpose(&["login:", key, ":/auth"]).unwrap(),
                &mut Nonce(0),
            )
            .unwrap();
        files
            .insert("/auth".into(), d::Bytes(Some(sealed.into_bytes())))
            .unwrap();
        state
            .login_states
            .insert(
                key.into(),
                d::state::LegacyLoginState {
                    key: key.into(),
                    files,
                    ..Default::default()
                },
            )
            .unwrap();
        let migrated = cipher.decrypt_state(&state, || Ok("lgn1".into())).unwrap();
        assert!(migrated.login_states.is_empty());
        assert_eq!(
            migrated
                .logins
                .get("lgn1")
                .unwrap()
                .files
                .get("/auth")
                .unwrap()
                .0
                .as_deref(),
            Some([0, 255, 1].as_slice())
        );
        let sealed = cipher.encrypt_state(&migrated, &mut Nonce(2)).unwrap();
        assert_eq!(
            cipher
                .decrypt_state(&sealed, || Err(Error::Payload))
                .unwrap(),
            migrated
        );
    }
}
