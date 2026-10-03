use crate::ConnectionTarget;
use anyhow::{Context as _, Result, bail};
use async_tungstenite::WebSocketStream;
use async_tungstenite::tokio::{ConnectStream, TokioAdapter, client_async, connect_async};
pub(crate) use async_tungstenite::tungstenite::Error as WebSocketError;
use async_tungstenite::tungstenite::{
    Message,
    handshake::client::{Request, Response},
};
use futures::StreamExt;
use std::{path::Path, process::Stdio, time::Duration};
use tokio::{
    io::{Join, join},
    process::{Child, ChildStdin, ChildStdout, Command},
};
use url::Url;

type SshStream = WebSocketStream<TokioAdapter<Join<ChildStdout, ChildStdin>>>;

pub(crate) enum Socket {
    Direct(WebSocketStream<ConnectStream>),
    Ssh {
        stream: SshStream,
        _tunnel: OwnedChild,
    },
}

impl Socket {
    pub(crate) async fn send(&mut self, message: Message) -> Result<(), WebSocketError> {
        match self {
            Self::Direct(stream) => stream.send(message).await,
            Self::Ssh { stream, .. } => stream.send(message).await,
        }
    }

    pub(crate) async fn next(&mut self) -> Option<Result<Message, WebSocketError>> {
        match self {
            Self::Direct(stream) => stream.next().await,
            Self::Ssh { stream, .. } => stream.next().await,
        }
    }

    pub(crate) async fn close(
        &mut self,
        frame: Option<async_tungstenite::tungstenite::protocol::CloseFrame>,
    ) -> Result<(), WebSocketError> {
        match self {
            Self::Direct(stream) => stream.close(frame).await,
            Self::Ssh { stream, .. } => stream.close(frame).await,
        }
    }
}

pub(crate) struct OwnedChild(Option<Child>);

impl Drop for OwnedChild {
    fn drop(&mut self) {
        let Some(mut child) = self.0.take() else {
            return;
        };
        if let Err(error) = child.start_kill() {
            log::warn!("Could not stop owned Paseo SSH tunnel: {error}");
        }
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                if let Err(error) = child.wait().await {
                    log::warn!("Could not reap owned Paseo SSH tunnel: {error}");
                }
            });
        }
    }
}

pub fn parse_ssh_uri(uri: &str) -> Result<ConnectionTarget> {
    let url = Url::parse(uri).context("invalid Paseo SSH URI")?;
    if url.scheme() != "ssh"
        || url.password().is_some()
        || !matches!(url.path(), "" | "/")
        || url.fragment().is_some()
    {
        bail!("Paseo SSH URI must have no password, path, or fragment");
    }
    let host = url
        .host_str()
        .context("Paseo SSH host is required")?
        .trim_matches(['[', ']']);
    let username = (!url.username().is_empty()).then(|| url.username().to_owned());
    validate_ssh_destination(host, username.as_deref())?;
    let mut daemon_port = 6767;
    let mut saw_daemon_port = false;
    for (key, value) in url.query_pairs() {
        if key != "daemonPort" || saw_daemon_port {
            bail!("unsupported Paseo SSH URI option");
        }
        daemon_port = value
            .parse::<u16>()
            .ok()
            .filter(|port| *port != 0)
            .context("invalid Paseo daemon port")?;
        saw_daemon_port = true;
    }
    let ssh_port = url.port().unwrap_or(22);
    if ssh_port == 0 {
        bail!("invalid Paseo SSH port");
    }
    Ok(ConnectionTarget::Ssh {
        host: host.to_owned(),
        username,
        ssh_port,
        daemon_port,
    })
}

pub(crate) fn websocket_url(target: &ConnectionTarget) -> Result<String> {
    match target {
        ConnectionTarget::Direct {
            websocket_url,
            editor_ssh,
        } => {
            let url = Url::parse(websocket_url).context("invalid Paseo WebSocket URL")?;
            if !matches!(url.scheme(), "ws" | "wss")
                || url.host_str().is_none()
                || url.port_or_known_default().is_none()
                || url.port() == Some(0)
                || !url.username().is_empty()
                || url.password().is_some()
                || url.query().is_some()
                || url.fragment().is_some()
            {
                bail!("Paseo URL must be a ws or wss URL without credentials, query, or fragment");
            }
            if let Some(editor_ssh) = editor_ssh {
                parse_ssh_uri(editor_ssh).context("invalid editor SSH mapping")?;
            }
            Ok(websocket_url.clone())
        }
        ConnectionTarget::Ssh {
            host,
            username,
            ssh_port,
            daemon_port,
        } => {
            validate_ssh_destination(host, username.as_deref())?;
            if *ssh_port == 0 || *daemon_port == 0 {
                bail!("Paseo SSH and daemon ports must be between 1 and 65535");
            }
            Ok(format!("ws://127.0.0.1:{daemon_port}/ws"))
        }
    }
}

fn validate_ssh_destination(host: &str, username: Option<&str>) -> Result<()> {
    if host.is_empty()
        || host.starts_with('-')
        || host.chars().any(|character| {
            character.is_whitespace()
                || character.is_control()
                || matches!(character, '@' | '/' | '\\' | '%')
        })
    {
        bail!("invalid Paseo SSH host");
    }
    if username.is_some_and(|username| {
        username.is_empty()
            || username.starts_with('-')
            || username.chars().any(|character| {
                character.is_whitespace()
                    || character.is_control()
                    || matches!(character, '@' | '/' | '\\' | '%')
            })
    }) {
        bail!("invalid Paseo SSH username");
    }
    Ok(())
}

fn ssh_command(
    executable: &Path,
    host: &str,
    username: Option<&str>,
    ssh_port: u16,
    daemon_port: u16,
) -> Command {
    let mut command = Command::new(executable);
    command.args([
        "-T",
        "-o",
        "BatchMode=yes",
        "-o",
        "ConnectTimeout=10",
        "-o",
        "ClearAllForwardings=yes",
        "-o",
        "ExitOnForwardFailure=yes",
        "-p",
    ]);
    command.arg(ssh_port.to_string());
    command.arg("-W").arg(format!("127.0.0.1:{daemon_port}"));
    let destination =
        username.map_or_else(|| host.to_owned(), |username| format!("{username}@{host}"));
    command.arg(destination);
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    command
}

/// Names a WebSocket failure without its details. The connection errors above drop the details
/// because a handshake error can echo the request, whose subprotocol header holds the password.
pub(crate) fn websocket_error_kind(error: &WebSocketError) -> String {
    match error {
        WebSocketError::ConnectionClosed => "connection closed".into(),
        WebSocketError::AlreadyClosed => "already closed".into(),
        WebSocketError::Io(error) => format!("I/O error: {}", error.kind()),
        WebSocketError::Tls(_) => "TLS error".into(),
        WebSocketError::Capacity(_) => "message too large".into(),
        WebSocketError::Protocol(_) => "protocol error".into(),
        WebSocketError::WriteBufferFull(_) => "write buffer full".into(),
        WebSocketError::Utf8(_) => "invalid UTF-8".into(),
        WebSocketError::AttackAttempt => "attack attempt".into(),
        WebSocketError::Url(_) => "invalid URL".into(),
        WebSocketError::Http(response) => format!("HTTP status {}", response.status()),
        WebSocketError::HttpFormat(_) => "invalid HTTP".into(),
    }
}

pub(crate) async fn connect_socket(
    target: &ConnectionTarget,
    request: Request,
    ssh_executable: &Path,
) -> Result<(Socket, Response)> {
    match target {
        ConnectionTarget::Direct { .. } => {
            let (stream, response) =
                tokio::time::timeout(Duration::from_secs(15), connect_async(request))
                    .await
                    .context("Paseo WebSocket connection timed out")?
                    .map_err(|error| {
                        log::warn!(
                            "Paseo WebSocket connection failed: {}",
                            websocket_error_kind(&error)
                        );
                        anyhow::anyhow!("Paseo WebSocket connection failed")
                    })?;
            Ok((Socket::Direct(stream), response))
        }
        ConnectionTarget::Ssh {
            host,
            username,
            ssh_port,
            daemon_port,
        } => {
            let mut child = ssh_command(
                ssh_executable,
                host,
                username.as_deref(),
                *ssh_port,
                *daemon_port,
            )
            .spawn()
            .context("could not start Paseo SSH tunnel")?;
            let stdout = child
                .stdout
                .take()
                .context("Paseo SSH stdout unavailable")?;
            let stdin = child.stdin.take().context("Paseo SSH stdin unavailable")?;
            let tunnel = OwnedChild(Some(child));
            let stream = join(stdout, stdin);
            let (stream, response) =
                tokio::time::timeout(Duration::from_secs(15), client_async(request, stream))
                    .await
                    .context("Paseo SSH WebSocket handshake timed out")?
                    .map_err(|error| {
                        log::warn!(
                            "Paseo SSH WebSocket handshake failed: {}",
                            websocket_error_kind(&error)
                        );
                        anyhow::anyhow!("Paseo SSH WebSocket handshake failed")
                    })?;
            Ok((
                Socket::Ssh {
                    stream,
                    _tunnel: tunnel,
                },
                response,
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ssh_uri_with_separate_ssh_and_daemon_ports() {
        let target =
            parse_ssh_uri("ssh://alice@example.com:2222?daemonPort=7777").expect("valid SSH URI");
        assert_eq!(
            target,
            ConnectionTarget::Ssh {
                host: "example.com".into(),
                username: Some("alice".into()),
                ssh_port: 2222,
                daemon_port: 7777,
            }
        );
    }

    #[test]
    fn rejects_credentials_and_invalid_hosts() {
        for uri in [
            "ssh://alice:secret@example.com",
            "ssh://-bad",
            "ssh://host/path",
            "ssh://host:0",
            "ssh://host?daemonPort=0",
            "ssh://host?daemonPort=123&daemonPort=456",
            "ssh://host?token=secret",
        ] {
            assert!(parse_ssh_uri(uri).is_err(), "accepted invalid SSH URI");
        }
        for url in [
            "ws://user:secret@localhost/ws",
            "ws://localhost/ws?password=secret",
            "ws://localhost:0/ws",
            "https://localhost/ws",
        ] {
            assert!(
                websocket_url(&ConnectionTarget::Direct {
                    websocket_url: url.into(),
                    editor_ssh: None
                })
                .is_err(),
                "accepted invalid WebSocket URL"
            );
        }
        assert!(
            websocket_url(&ConnectionTarget::Direct {
                websocket_url: "ws://localhost/ws".into(),
                editor_ssh: Some("ssh://user:secret@host".into()),
            })
            .is_err()
        );
    }

    #[test]
    fn ssh_command_uses_noninteractive_forwarding_args() {
        let command = ssh_command(Path::new("ssh"), "example.com", Some("alice"), 2222, 7777);
        let args: Vec<_> = command
            .as_std()
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args,
            [
                "-T",
                "-o",
                "BatchMode=yes",
                "-o",
                "ConnectTimeout=10",
                "-o",
                "ClearAllForwardings=yes",
                "-o",
                "ExitOnForwardFailure=yes",
                "-p",
                "2222",
                "-W",
                "127.0.0.1:7777",
                "alice@example.com"
            ]
        );
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn dropping_owned_tunnel_reaps_only_its_child() {
        let child = Command::new("sleep")
            .arg("60")
            .spawn()
            .expect("start owned process");
        let process_id = child.id().expect("owned process ID");
        let child_path = format!("/proc/{process_id}");
        assert!(Path::new(&child_path).exists());
        drop(OwnedChild(Some(child)));
        tokio::time::timeout(Duration::from_secs(5), async {
            while Path::new(&child_path).exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("owned child was not reaped");
    }
}
