//! SSH connection and proxy utilities.

use crate::tls::{TlsOptions, build_rustls_config, grpc_client, require_tls_materials};
use miette::{IntoDiagnostic, Result, WrapErr};
use navigator_core::proto::CreateSshSessionRequest;
use rustls::pki_types::ServerName;
use std::io::{IsTerminal, Write};
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;

struct SshSessionConfig {
    proxy_command: String,
}

async fn ssh_session_config(server: &str, id: &str, tls: &TlsOptions) -> Result<SshSessionConfig> {
    let mut client = grpc_client(server, tls).await?;
    let response = client
        .create_ssh_session(CreateSshSessionRequest {
            sandbox_id: id.to_string(),
        })
        .await
        .into_diagnostic()?;
    let session = response.into_inner();

    let exe = std::env::current_exe()
        .into_diagnostic()
        .wrap_err("failed to resolve navigator executable")?;
    let exe_command = shell_escape(&exe.to_string_lossy());

    let gateway_url = format!(
        "{}://{}:{}{}",
        session.gateway_scheme, session.gateway_host, session.gateway_port, session.connect_path
    );
    let proxy_command = format!(
        "{exe_command} ssh-proxy --gateway {} --sandbox-id {} --token {}",
        gateway_url, session.sandbox_id, session.token,
    );

    Ok(SshSessionConfig { proxy_command })
}

fn ssh_base_command(proxy_command: &str) -> Command {
    let mut command = Command::new("ssh");
    command
        .arg("-o")
        .arg(format!("ProxyCommand={proxy_command}"))
        .arg("-o")
        .arg("StrictHostKeyChecking=no")
        .arg("-o")
        .arg("UserKnownHostsFile=/dev/null")
        .arg("-o")
        .arg("GlobalKnownHostsFile=/dev/null");
    command
}

/// Connect to a sandbox via SSH.
pub async fn sandbox_connect(server: &str, id: &str, tls: &TlsOptions) -> Result<()> {
    let session = ssh_session_config(server, id, tls).await?;

    let mut command = ssh_base_command(&session.proxy_command);
    command
        .arg("-tt")
        .arg("-o")
        .arg("RequestTTY=force")
        .arg("-o")
        .arg("SetEnv=TERM=xterm-256color")
        .arg("sandbox")
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit());

    if std::io::stdin().is_terminal() {
        #[cfg(unix)]
        {
            let err = command.exec();
            return Err(miette::miette!("failed to exec ssh: {err}"));
        }
    }

    let status = tokio::task::spawn_blocking(move || command.status())
        .await
        .into_diagnostic()?
        .into_diagnostic()?;

    if !status.success() {
        return Err(miette::miette!("ssh exited with status {status}"));
    }

    Ok(())
}

/// Execute a command in a sandbox via SSH.
pub async fn sandbox_exec(
    server: &str,
    id: &str,
    command: &[String],
    tty: bool,
    tls: &TlsOptions,
) -> Result<()> {
    if command.is_empty() {
        return Err(miette::miette!("no command provided"));
    }

    let session = ssh_session_config(server, id, tls).await?;
    let mut ssh = ssh_base_command(&session.proxy_command);

    if tty {
        ssh.arg("-tt")
            .arg("-o")
            .arg("RequestTTY=force")
            .arg("-o")
            .arg("SetEnv=TERM=xterm-256color");
    } else {
        ssh.arg("-T").arg("-o").arg("RequestTTY=no");
    }

    let command_str = command
        .iter()
        .map(|arg| shell_escape(arg))
        .collect::<Vec<_>>()
        .join(" ");

    ssh.arg("sandbox")
        .arg(command_str)
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit());

    let status = tokio::task::spawn_blocking(move || ssh.status())
        .await
        .into_diagnostic()?
        .into_diagnostic()?;

    if !status.success() {
        return Err(miette::miette!("ssh exited with status {status}"));
    }

    Ok(())
}

/// Sync local files into the sandbox using rsync over SSH.
pub async fn sandbox_rsync(
    server: &str,
    id: &str,
    repo_root: &Path,
    files: &[String],
    tls: &TlsOptions,
) -> Result<()> {
    if files.is_empty() {
        return Ok(());
    }

    let session = ssh_session_config(server, id, tls).await?;

    let ssh_command = format!(
        "ssh -o {} -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o GlobalKnownHostsFile=/dev/null",
        shell_escape(&format!("ProxyCommand={}", session.proxy_command))
    );

    let mut rsync = Command::new("rsync");
    rsync
        .arg("-az")
        .arg("--from0")
        .arg("--files-from=-")
        .arg("--relative")
        .arg("-e")
        .arg(ssh_command)
        .arg(".")
        .arg("sandbox:/sandbox")
        .current_dir(repo_root)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit());

    let mut child = rsync.spawn().into_diagnostic()?;
    if let Some(mut stdin) = child.stdin.take() {
        for path in files {
            let entry = format!("./{path}");
            stdin.write_all(entry.as_bytes()).into_diagnostic()?;
            stdin.write_all(&[0]).into_diagnostic()?;
        }
    }

    let status = tokio::task::spawn_blocking(move || child.wait())
        .await
        .into_diagnostic()?
        .into_diagnostic()?;

    if !status.success() {
        return Err(miette::miette!("rsync exited with status {status}"));
    }

    Ok(())
}

fn shell_escape(value: &str) -> String {
    if value.is_empty() {
        return "''".to_string();
    }

    let safe = value
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'/' | b'-' | b'_'));
    if safe {
        return value.to_string();
    }

    let escaped = value.replace('\'', "'\"'\"'");
    format!("'{escaped}'")
}

/// Run the SSH proxy, connecting stdin/stdout to the gateway.
pub async fn sandbox_ssh_proxy(
    gateway_url: &str,
    sandbox_id: &str,
    token: &str,
    tls: &TlsOptions,
) -> Result<()> {
    let url: url::Url = gateway_url
        .parse()
        .into_diagnostic()
        .wrap_err("invalid gateway URL")?;

    let scheme = url.scheme();
    let gateway_host = url
        .host_str()
        .ok_or_else(|| miette::miette!("gateway URL missing host"))?;
    let gateway_port = url
        .port_or_known_default()
        .ok_or_else(|| miette::miette!("gateway URL missing port"))?;
    let connect_path = url.path();

    let mut stream: Box<dyn ProxyStream> =
        connect_gateway(scheme, gateway_host, gateway_port, tls).await?;

    let request = format!(
        "CONNECT {connect_path} HTTP/1.1\r\nHost: {gateway_host}\r\nX-Sandbox-Id: {sandbox_id}\r\nX-Sandbox-Token: {token}\r\n\r\n"
    );
    stream
        .write_all(request.as_bytes())
        .await
        .into_diagnostic()?;

    let status = read_connect_status(&mut stream).await?;
    if status != 200 {
        return Err(miette::miette!(
            "gateway CONNECT failed with status {status}"
        ));
    }

    let (mut reader, mut writer) = tokio::io::split(stream);
    let mut stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();

    tokio::try_join!(
        tokio::io::copy(&mut stdin, &mut writer),
        tokio::io::copy(&mut reader, &mut stdout)
    )
    .into_diagnostic()?;

    Ok(())
}

async fn connect_gateway(
    scheme: &str,
    host: &str,
    port: u16,
    tls: &TlsOptions,
) -> Result<Box<dyn ProxyStream>> {
    let tcp = TcpStream::connect((host, port)).await.into_diagnostic()?;
    if scheme.eq_ignore_ascii_case("https") {
        let materials = require_tls_materials(&format!("https://{host}:{port}"), tls)?;
        let config = build_rustls_config(&materials)?;
        let connector = TlsConnector::from(Arc::new(config));
        let server_name = ServerName::try_from(host.to_string())
            .map_err(|_| miette::miette!("invalid server name: {host}"))?;
        let tls = connector
            .connect(server_name, tcp)
            .await
            .into_diagnostic()?;
        Ok(Box::new(tls))
    } else {
        Ok(Box::new(tcp))
    }
}

async fn read_connect_status(stream: &mut dyn ProxyStream) -> Result<u16> {
    let mut buf = Vec::new();
    let mut temp = [0u8; 1024];
    loop {
        let n = stream.read(&mut temp).await.into_diagnostic()?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&temp[..n]);
        if buf.windows(4).any(|win| win == b"\r\n\r\n") {
            break;
        }
        if buf.len() > 8192 {
            break;
        }
    }
    let text = String::from_utf8_lossy(&buf);
    let line = text.lines().next().unwrap_or("");
    let status = line
        .split_whitespace()
        .nth(1)
        .unwrap_or("0")
        .parse::<u16>()
        .unwrap_or(0);
    Ok(status)
}

trait ProxyStream: AsyncRead + AsyncWrite + Unpin + Send {}

impl<T> ProxyStream for T where T: AsyncRead + AsyncWrite + Unpin + Send {}
