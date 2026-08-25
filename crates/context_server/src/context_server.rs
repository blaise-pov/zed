pub mod client;
pub mod listener;
pub mod oauth;
pub mod protocol;
#[cfg(any(test, feature = "test-support"))]
pub mod test;
pub mod transport;
pub mod types;

use collections::HashMap;
use http_client::HttpClient;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use std::{fmt::Display, path::PathBuf};

use anyhow::{Context as _, Result};
use client::Client;
use gpui::AsyncApp;
use parking_lot::RwLock;
pub use settings::ContextServerCommand;
use url::Url;

use crate::oauth::WwwAuthenticate;
use crate::transport::HttpTransport;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ContextServerId(pub Arc<str>);

impl Display for ContextServerId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

enum ContextServerTransport {
    Stdio(ContextServerCommand, Option<PathBuf>),
    Custom(Arc<dyn crate::transport::Transport>),
}

/// Expands `${VAR}` references in a context server command against the
/// editor process's environment, so secrets can stay out of settings files
/// (which are often committed to the repo). A referenced variable that is
/// not set is an error: the server fails loudly instead of silently
/// receiving the literal `${...}` text.
fn expand_env_vars(value: &str) -> Result<String> {
    let mut expanded = String::with_capacity(value.len());
    let mut rest = value;
    while let Some(start) = rest.find("${") {
        expanded.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find('}') else {
            anyhow::bail!("unterminated ${{ in context server command value {value:?}");
        };
        let name = &after[..end];
        let resolved = std::env::var(name).with_context(|| {
            format!(
                "environment variable {name:?} referenced by a context server is not set"
            )
        })?;
        expanded.push_str(&resolved);
        rest = &after[end + 1..];
    }
    expanded.push_str(rest);
    Ok(expanded)
}

fn expand_env_vars_in_args(args: &[String]) -> Result<Vec<String>> {
    args.iter()
        .map(|arg| expand_env_vars(arg))
        .collect()
}

fn expand_env_vars_in_env(
    env: &HashMap<String, String>,
) -> Result<HashMap<String, String>> {
    env.iter()
        .map(|(key, value)| Ok((key.clone(), expand_env_vars(value)?)))
        .collect()
}

pub struct ContextServer {
    id: ContextServerId,
    client: RwLock<Option<Arc<crate::protocol::InitializedContextServerProtocol>>>,
    configuration: ContextServerTransport,
    request_timeout: Option<Duration>,
}

impl ContextServer {
    pub fn stdio(
        id: ContextServerId,
        command: ContextServerCommand,
        working_directory: Option<Arc<Path>>,
    ) -> Self {
        Self {
            id,
            client: RwLock::new(None),
            configuration: ContextServerTransport::Stdio(
                command,
                working_directory.map(|directory| directory.to_path_buf()),
            ),
            request_timeout: None,
        }
    }

    pub fn http(
        id: ContextServerId,
        endpoint: &Url,
        headers: HashMap<String, String>,
        http_client: Arc<dyn HttpClient>,
        executor: gpui::BackgroundExecutor,
        request_timeout: Option<Duration>,
    ) -> Result<Self> {
        let transport = match endpoint.scheme() {
            "http" | "https" => {
                log::info!("Using HTTP transport for {}", endpoint);
                let transport =
                    HttpTransport::new(http_client, endpoint.to_string(), headers, executor);
                Arc::new(transport) as _
            }
            _ => anyhow::bail!("unsupported MCP url scheme {}", endpoint.scheme()),
        };
        Ok(Self::new_with_timeout(id, transport, request_timeout))
    }

    pub fn new(id: ContextServerId, transport: Arc<dyn crate::transport::Transport>) -> Self {
        Self::new_with_timeout(id, transport, None)
    }

    pub fn new_with_timeout(
        id: ContextServerId,
        transport: Arc<dyn crate::transport::Transport>,
        request_timeout: Option<Duration>,
    ) -> Self {
        Self {
            id,
            client: RwLock::new(None),
            configuration: ContextServerTransport::Custom(transport),
            request_timeout,
        }
    }

    pub fn id(&self) -> ContextServerId {
        self.id.clone()
    }

    pub fn client(&self) -> Option<Arc<crate::protocol::InitializedContextServerProtocol>> {
        self.client.read().clone()
    }

    /// The authentication challenge from the last `401 Unauthorized` response
    /// this server's transport gave up on, if any. See
    /// [`crate::transport::Transport::auth_challenge`].
    pub fn auth_challenge(&self) -> Option<WwwAuthenticate> {
        match &self.configuration {
            ContextServerTransport::Stdio(..) => None,
            ContextServerTransport::Custom(transport) => transport.auth_challenge(),
        }
    }

    pub async fn start(&self, cx: &AsyncApp) -> Result<()> {
        self.initialize(self.new_client(cx)?).await
    }

    fn new_client(&self, cx: &AsyncApp) -> Result<Client> {
        Ok(match &self.configuration {
            ContextServerTransport::Stdio(command, working_directory) => {
                let args = expand_env_vars_in_args(&command.args)?;
                let env = match &command.env {
                    Some(env) => Some(expand_env_vars_in_env(env)?),
                    None => None,
                };
                Client::stdio(
                    client::ContextServerId(self.id.0.clone()),
                    client::ModelContextServerBinary {
                        executable: Path::new(&command.path).to_path_buf(),
                        args,
                        env,
                        timeout: command.timeout,
                    },
                    working_directory,
                    cx.clone(),
                )?
            }
            ContextServerTransport::Custom(transport) => Client::new(
                client::ContextServerId(self.id.0.clone()),
                self.id().0,
                transport.clone(),
                self.request_timeout,
                cx.clone(),
            )?,
        })
    }

    async fn initialize(&self, client: Client) -> Result<()> {
        log::debug!("starting context server {}", self.id);
        let protocol = crate::protocol::ModelContextProtocol::new(client);
        let client_info = types::Implementation {
            name: "Zed".to_string(),
            title: None,
            version: env!("CARGO_PKG_VERSION").to_string(),
            description: None,
        };
        let initialized_protocol = protocol.initialize(client_info).await?;

        log::debug!(
            "context server {} initialized: {:?}",
            self.id,
            initialized_protocol.initialize,
        );

        *self.client.write() = Some(Arc::new(initialized_protocol));
        Ok(())
    }

    pub fn stop(&self) -> Result<()> {
        let mut client = self.client.write();
        if let Some(protocol) = client.take() {
            drop(protocol);
        }
        Ok(())
    }
}

#[cfg(test)]
mod env_expansion_tests {
    use super::*;

    #[test]
    fn expands_variables_from_environment() {
        // Unique name so parallel tests cannot race on it.
        unsafe { std::env::set_var("ZED_TEST_EXPAND_PRESENT", "secret-value") };

        assert_eq!(
            expand_env_vars("token=${ZED_TEST_EXPAND_PRESENT}").unwrap(),
            "token=secret-value"
        );
        assert_eq!(
            expand_env_vars("${ZED_TEST_EXPAND_PRESENT}-${ZED_TEST_EXPAND_PRESENT}").unwrap(),
            "secret-value-secret-value"
        );
        // Plain values pass through untouched, including dollar signs
        // without brace syntax.
        assert_eq!(expand_env_vars("plain").unwrap(), "plain");
        assert_eq!(expand_env_vars("$HOME").unwrap(), "$HOME");
    }

    #[test]
    fn missing_variable_is_an_error_naming_the_variable() {
        let error = expand_env_vars("token=${ZED_TEST_EXPAND_MISSING}").unwrap_err();
        let message = format!("{error:#}");
        assert!(
            message.contains("ZED_TEST_EXPAND_MISSING"),
            "unexpected error: {message}"
        );
    }

    #[test]
    fn unterminated_reference_is_an_error() {
        let error = expand_env_vars("token=${ZED_TEST_EXPAND_UNTERMINATED").unwrap_err();
        assert!(
            format!("{error}").contains("unterminated"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn args_and_env_maps_are_expanded() {
        unsafe { std::env::set_var("ZED_TEST_EXPAND_ARG", "--flag") };
        unsafe { std::env::set_var("ZED_TEST_EXPAND_TOKEN", "tok") };

        let args = expand_env_vars_in_args(&[
            "run".to_string(),
            "${ZED_TEST_EXPAND_ARG}".to_string(),
        ])
        .unwrap();
        assert_eq!(args, vec!["run", "--flag"]);

        let mut env = HashMap::default();
        env.insert("TOKEN".to_string(), "${ZED_TEST_EXPAND_TOKEN}".to_string());
        let env = expand_env_vars_in_env(&env).unwrap();
        assert_eq!(env.get("TOKEN").map(String::as_str), Some("tok"));
    }
}
