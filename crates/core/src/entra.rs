//! Microsoft Entra ID sign-in for SQL Server connections: picks the flow for the
//! connection's auth method, raises the browser / device-code prompts, keeps access tokens
//! in memory until they expire and refresh tokens in the secret store.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use secrecy::SecretString;
use switchyard_db::DbAuthMethod;
use switchyard_db::entra::{Entra, EntraApp, Token};
use switchyard_store::{DbConnection, ProfileId, SecretRef, SecretStore};
use tokio::sync::oneshot;
use tracing::{debug, info};

use crate::bus::{Event, PromptAnswer};
use crate::error::{CoreError, Result};
use crate::prompts::BusPrompter;

/// How long the user has to finish signing in.
const SIGN_IN_TIMEOUT: Duration = Duration::from_secs(5 * 60);
/// Tokens closer than this to expiry are renewed before connecting.
const EXPIRY_MARGIN: Duration = Duration::from_secs(5 * 60);
/// Secret-store purpose for a connection's refresh token.
const REFRESH_PURPOSE: &str = "entra-refresh";

struct Cached {
    access: SecretString,
    expires_at: Instant,
}

/// Entra tokens for the service.
pub(crate) struct EntraSignIn {
    prompter: Arc<BusPrompter>,
    secrets: Arc<dyn SecretStore>,
    cache: Mutex<HashMap<ProfileId, Cached>>,
    /// One sign-in at a time, so two tabs connecting together open one browser window.
    busy: tokio::sync::Mutex<()>,
}

impl EntraSignIn {
    pub(crate) fn new(prompter: Arc<BusPrompter>, secrets: Arc<dyn SecretStore>) -> Self {
        Self {
            prompter,
            secrets,
            cache: Mutex::default(),
            busy: tokio::sync::Mutex::new(()),
        }
    }

    fn cache(&self) -> std::sync::MutexGuard<'_, HashMap<ProfileId, Cached>> {
        self.cache.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Drop the in-memory access token for a connection.
    pub(crate) fn drop_cached(&self, id: &ProfileId) {
        self.cache().remove(id);
    }

    /// Forget a connection's tokens (profile deleted, or "sign out").
    pub(crate) async fn forget(&self, id: &ProfileId) -> Result<()> {
        self.cache().remove(id);
        let key = SecretRef::for_profile(id, REFRESH_PURPOSE);
        self.blocking(move |s| s.delete(&key)).await
    }

    async fn blocking<T: Send + 'static>(
        &self,
        f: impl FnOnce(&dyn SecretStore) -> std::result::Result<T, switchyard_store::StoreError>
        + Send
        + 'static,
    ) -> Result<T> {
        let secrets = self.secrets.clone();
        tokio::task::spawn_blocking(move || f(secrets.as_ref()))
            .await
            .map_err(|e| CoreError::Internal(e.to_string()))?
            .map_err(CoreError::from)
    }

    /// An access token for `c`. `secret` is the password or client secret for the methods
    /// that use one.
    pub(crate) async fn token(
        &self,
        c: &DbConnection,
        secret: Option<SecretString>,
    ) -> Result<SecretString> {
        let _one = self.busy.lock().await;
        if let Some(t) = self.cache().get(&c.id)
            && t.expires_at > Instant::now() + EXPIRY_MARGIN
        {
            return Ok(t.access.clone());
        }
        let entra = Entra::new()?;
        let token = match c.auth {
            DbAuthMethod::EntraServicePrincipal => {
                let app = EntraApp::resolve(c.tenant.as_deref(), Some(&c.user))?;
                let secret = secret.ok_or_else(|| missing("client secret"))?;
                entra.client_credentials(&app, &secret).await?
            }
            DbAuthMethod::EntraPassword => {
                let app = self.app(c)?;
                let secret = secret.ok_or_else(|| missing("password"))?;
                entra.password(&app, &c.user, &secret).await?
            }
            DbAuthMethod::EntraInteractive | DbAuthMethod::EntraDeviceCode => {
                let app = self.app(c)?;
                match self.refreshed(&entra, &app, &c.id).await {
                    Some(t) => t,
                    None => self.sign_in(&entra, &app, c).await?,
                }
            }
            other => {
                return Err(CoreError::Internal(format!(
                    "{other:?} is not a Microsoft Entra method"
                )));
            }
        };
        if let Some(refresh) = &token.refresh {
            let key = SecretRef::for_profile(&c.id, REFRESH_PURPOSE);
            let refresh = refresh.clone();
            self.blocking(move |s| s.set(&key, &refresh)).await?;
        }
        let access = token.access.clone();
        self.cache().insert(
            c.id.clone(),
            Cached {
                access: token.access,
                expires_at: token.expires_at,
            },
        );
        Ok(access)
    }

    fn app(&self, c: &DbConnection) -> Result<EntraApp> {
        Ok(EntraApp::resolve(
            c.tenant.as_deref(),
            c.entra_client_id.as_deref(),
        )?)
    }

    /// A token from the stored refresh token, if there is one and it still works.
    async fn refreshed(&self, entra: &Entra, app: &EntraApp, id: &ProfileId) -> Option<Token> {
        let key = SecretRef::for_profile(id, REFRESH_PURPOSE);
        let stored = self.blocking(move |s| s.get(&key)).await.ok()??;
        match entra.refresh(app, &stored).await {
            Ok(t) => Some(t),
            Err(e) => {
                // Expired, revoked, or a policy change: sign in again.
                debug!(error = %e, "entra refresh failed");
                None
            }
        }
    }

    async fn sign_in(&self, entra: &Entra, app: &EntraApp, c: &DbConnection) -> Result<Token> {
        info!(connection = %c.name, method = ?c.auth, "entra sign-in");
        let connection = if c.name.trim().is_empty() {
            c.server.clone()
        } else {
            c.name.clone()
        };
        if c.auth == DbAuthMethod::EntraDeviceCode {
            let dc = entra.device_code(app).await?;
            let (request, answer) = self.prompter.ask_with_id(|request| Event::EntraDeviceCode {
                request,
                connection,
                code: dc.user_code.clone(),
                url: dc.verification_uri.clone(),
                message: dc.message.clone(),
            });
            let r = wait(entra.finish_device_code(app, &dc), answer).await;
            self.prompter.close(request);
            r
        } else {
            let hint = (!c.user.trim().is_empty()).then_some(c.user.as_str());
            let flow = entra.begin_interactive(app, hint).await?;
            let url = flow.url.clone();
            let (request, answer) = self.prompter.ask_with_id(|request| Event::EntraSignIn {
                request,
                connection,
                url,
            });
            let r = wait(entra.finish_interactive(app, flow), answer).await;
            self.prompter.close(request);
            r
        }
    }
}

/// Run a sign-in until it finishes, the user cancels, or time runs out.
async fn wait(
    flow: impl Future<Output = switchyard_db::Result<Token>>,
    answer: oneshot::Receiver<PromptAnswer>,
) -> Result<Token> {
    tokio::select! {
        r = tokio::time::timeout(SIGN_IN_TIMEOUT, flow) => match r {
            Ok(t) => Ok(t?),
            Err(_) => Err(CoreError::Db(switchyard_db::DbError::Connect(
                "Microsoft sign-in timed out".into(),
            ))),
        },
        // Any answer (or the dialog going away) cancels.
        _ = answer => Err(CoreError::Db(switchyard_db::DbError::Cancelled)),
    }
}

fn missing(what: &str) -> CoreError {
    CoreError::Db(switchyard_db::DbError::Connect(format!(
        "the {what} is not saved for this connection"
    )))
}
