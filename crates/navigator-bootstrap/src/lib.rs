use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use bollard::Docker;
use bollard::container::{
    Config, CreateContainerOptions, InspectContainerOptions, LogOutput, RemoveContainerOptions,
    StartContainerOptions,
};
use bollard::errors::Error as BollardError;
use bollard::exec::CreateExecOptions;
use bollard::image::CreateImageOptions;
use bollard::models::HealthStatusEnum;
use bollard::network::{CreateNetworkOptions, InspectNetworkOptions};
use bollard::service::{HostConfig, PortBinding};
use bollard::volume::{CreateVolumeOptions, RemoveVolumeOptions};
use futures::StreamExt;
use miette::{IntoDiagnostic, Result, WrapErr};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::time::Duration;

const DEFAULT_IMAGE_NAME: &str = "navigator-cluster";
const NETWORK_NAME: &str = "navigator-cluster";
const KUBECONFIG_PATH: &str = "/etc/rancher/k3s/k3s.yaml";
const CLI_SECRET_NAME: &str = "navigator-cli-client";
const NAV_GATEWAY_TLS_ENABLED_ENV: &str = "NAV_GATEWAY_TLS_ENABLED";
const HELMCHART_MANIFEST_PATHS: [&str; 2] = [
    "/var/lib/rancher/k3s/server/manifests/navigator-helmchart.yaml",
    "/opt/navigator/manifests/navigator-helmchart.yaml",
];

#[derive(Debug, Clone)]
pub struct DeployOptions {
    pub name: String,
    pub image_ref: Option<String>,
}

impl DeployOptions {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            image_ref: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ClusterHandle {
    name: String,
    kubeconfig_path: PathBuf,
    docker: Docker,
}

impl ClusterHandle {
    pub fn kubeconfig_path(&self) -> &Path {
        &self.kubeconfig_path
    }

    pub async fn stop(&self) -> Result<()> {
        stop_container(&self.docker, &container_name(&self.name)).await
    }

    pub async fn destroy(&self) -> Result<()> {
        destroy_cluster_resources(&self.docker, &self.name, &self.kubeconfig_path).await
    }
}

pub async fn deploy_cluster(options: DeployOptions) -> Result<ClusterHandle> {
    let docker = Docker::connect_with_local_defaults().into_diagnostic()?;
    let name = options.name;
    let image_ref = options.image_ref.unwrap_or_else(default_cluster_image_ref);
    let kubeconfig_path = stored_kubeconfig_path(&name)?;

    ensure_network(&docker).await?;
    ensure_volume(&docker, &volume_name(&name)).await?;
    ensure_image(&docker, &image_ref).await?;

    ensure_container(&docker, &name, &image_ref).await?;
    start_container(&docker, &name).await?;

    let raw_kubeconfig = wait_for_kubeconfig(&docker, &name).await?;
    let rewritten = rewrite_kubeconfig(&raw_kubeconfig, &name);
    store_kubeconfig(&kubeconfig_path, &rewritten)?;
    wait_for_cluster_ready(&docker, &name).await?;
    fetch_and_store_cli_mtls(&docker, &name).await?;

    Ok(ClusterHandle {
        name,
        kubeconfig_path,
        docker,
    })
}

pub fn cluster_handle(name: &str) -> Result<ClusterHandle> {
    let docker = Docker::connect_with_local_defaults().into_diagnostic()?;
    let kubeconfig_path = stored_kubeconfig_path(name)?;
    Ok(ClusterHandle {
        name: name.to_string(),
        kubeconfig_path,
        docker,
    })
}

pub async fn ensure_cluster_image(version: &str) -> Result<String> {
    let docker = Docker::connect_with_local_defaults().into_diagnostic()?;
    let image_ref = format!("{DEFAULT_IMAGE_NAME}:{version}");
    ensure_image(&docker, &image_ref).await?;
    Ok(image_ref)
}

pub fn stored_kubeconfig_path(name: &str) -> Result<PathBuf> {
    let base = xdg_config_dir()?;
    Ok(base
        .join("navigator")
        .join("clusters")
        .join(name)
        .join("kubeconfig"))
}

pub fn print_kubeconfig(name: &str) -> Result<()> {
    let path = stored_kubeconfig_path(name)?;
    let contents = std::fs::read_to_string(&path)
        .into_diagnostic()
        .wrap_err_with(|| format!("failed to read kubeconfig at {}", path.display()))?;
    print!("{contents}");
    Ok(())
}

pub fn update_local_kubeconfig(name: &str, target_path: &Path) -> Result<()> {
    let stored_path = stored_kubeconfig_path(name)?;
    let stored_contents = std::fs::read_to_string(&stored_path)
        .into_diagnostic()
        .wrap_err_with(|| format!("failed to read kubeconfig at {}", stored_path.display()))?;
    let stored_config: Kubeconfig = serde_yaml::from_str(&stored_contents)
        .into_diagnostic()
        .wrap_err("failed to parse stored kubeconfig")?;

    let mut target_config = if target_path.exists() {
        let contents = std::fs::read_to_string(target_path)
            .into_diagnostic()
            .wrap_err_with(|| format!("failed to read kubeconfig at {}", target_path.display()))?;
        serde_yaml::from_str(&contents)
            .into_diagnostic()
            .wrap_err("failed to parse target kubeconfig")?
    } else {
        Kubeconfig::default()
    };

    merge_kubeconfig(&mut target_config, stored_config);

    if target_config.api_version.is_empty() {
        target_config.api_version = "v1".to_string();
    }
    if target_config.kind.is_empty() {
        target_config.kind = "Config".to_string();
    }

    if let Some(parent) = target_path.parent() {
        std::fs::create_dir_all(parent)
            .into_diagnostic()
            .wrap_err_with(|| format!("failed to create {}", parent.display()))?;
    }

    let rendered = serde_yaml::to_string(&target_config)
        .into_diagnostic()
        .wrap_err("failed to serialize kubeconfig")?;
    std::fs::write(target_path, rendered)
        .into_diagnostic()
        .wrap_err_with(|| format!("failed to write kubeconfig to {}", target_path.display()))?;
    Ok(())
}

pub fn default_local_kubeconfig_path() -> Result<PathBuf> {
    if let Ok(paths) = std::env::var("KUBECONFIG")
        && let Some(first) = paths.split(':').next()
        && !first.is_empty()
    {
        return Ok(PathBuf::from(first));
    }

    let home = std::env::var("HOME")
        .into_diagnostic()
        .wrap_err("HOME is not set")?;
    Ok(PathBuf::from(home).join(".kube").join("config"))
}

fn xdg_config_dir() -> Result<PathBuf> {
    if let Ok(path) = std::env::var("XDG_CONFIG_HOME") {
        return Ok(PathBuf::from(path));
    }
    let home = std::env::var("HOME")
        .into_diagnostic()
        .wrap_err("HOME is not set")?;
    Ok(PathBuf::from(home).join(".config"))
}

fn default_cluster_image_ref() -> String {
    if let Ok(image) = std::env::var("NAVIGATOR_CLUSTER_IMAGE")
        && !image.trim().is_empty()
    {
        return image;
    }
    let tag = std::env::var("IMAGE_TAG")
        .ok()
        .filter(|val| !val.trim().is_empty())
        .unwrap_or_else(|| "dev".to_string());
    format!("{DEFAULT_IMAGE_NAME}:{tag}")
}

fn container_name(name: &str) -> String {
    format!("navigator-cluster-{name}")
}

fn volume_name(name: &str) -> String {
    format!("navigator-cluster-{name}")
}

async fn ensure_network(docker: &Docker) -> Result<()> {
    match docker
        .inspect_network(NETWORK_NAME, None::<InspectNetworkOptions<String>>)
        .await
    {
        Ok(_) => return Ok(()),
        Err(err) if is_not_found(&err) => {}
        Err(err) => return Err(err).into_diagnostic(),
    }

    docker
        .create_network(CreateNetworkOptions {
            name: NETWORK_NAME.to_string(),
            check_duplicate: true,
            driver: "bridge".to_string(),
            attachable: true,
            ..Default::default()
        })
        .await
        .into_diagnostic()
        .wrap_err("failed to create Docker network")?;
    Ok(())
}

async fn ensure_volume(docker: &Docker, name: &str) -> Result<()> {
    match docker.inspect_volume(name).await {
        Ok(_) => return Ok(()),
        Err(err) if is_not_found(&err) => {}
        Err(err) => return Err(err).into_diagnostic(),
    }

    docker
        .create_volume(CreateVolumeOptions {
            name: name.to_string(),
            ..Default::default()
        })
        .await
        .into_diagnostic()
        .wrap_err("failed to create Docker volume")?;
    Ok(())
}

async fn ensure_image(docker: &Docker, image_ref: &str) -> Result<()> {
    match docker.inspect_image(image_ref).await {
        Ok(_) => return Ok(()),
        Err(err) if is_not_found(&err) => {}
        Err(err) => return Err(err).into_diagnostic(),
    }

    let options = CreateImageOptions {
        from_image: image_ref.to_string(),
        ..Default::default()
    };
    let mut stream = docker.create_image(Some(options), None, None);
    while let Some(result) = stream.next().await {
        result.into_diagnostic()?;
    }
    Ok(())
}

async fn ensure_container(docker: &Docker, name: &str, image_ref: &str) -> Result<()> {
    let container_name = container_name(name);
    let inspect = docker
        .inspect_container(&container_name, None::<InspectContainerOptions>)
        .await;
    if inspect.is_ok() {
        return Ok(());
    }

    if let Err(err) = inspect
        && !is_not_found(&err)
    {
        return Err(err).into_diagnostic();
    }

    let mut port_bindings = HashMap::new();
    port_bindings.insert(
        "6443/tcp".to_string(),
        Some(vec![PortBinding {
            host_ip: Some("0.0.0.0".to_string()),
            host_port: Some("6443".to_string()),
        }]),
    );
    port_bindings.insert(
        "80/tcp".to_string(),
        Some(vec![PortBinding {
            host_ip: Some("0.0.0.0".to_string()),
            host_port: Some("80".to_string()),
        }]),
    );
    port_bindings.insert(
        "30051/tcp".to_string(),
        Some(vec![PortBinding {
            host_ip: Some("0.0.0.0".to_string()),
            host_port: Some("8080".to_string()),
        }]),
    );
    port_bindings.insert(
        "443/tcp".to_string(),
        Some(vec![PortBinding {
            host_ip: Some("0.0.0.0".to_string()),
            host_port: Some("443".to_string()),
        }]),
    );

    // Zero-sized map values required by bollard API for exposed ports
    #[allow(clippy::zero_sized_map_values)]
    let exposed_ports = HashMap::from([
        ("6443/tcp".to_string(), HashMap::new()),
        ("80/tcp".to_string(), HashMap::new()),
        ("30051/tcp".to_string(), HashMap::new()),
        ("443/tcp".to_string(), HashMap::new()),
    ]);

    let host_config = HostConfig {
        privileged: Some(true),
        port_bindings: Some(port_bindings),
        binds: Some(vec![format!("{}:/var/lib/rancher/k3s", volume_name(name))]),
        network_mode: Some(NETWORK_NAME.to_string()),
        // Add host.docker.internal mapping for DNS resolution
        // This allows the entrypoint script to configure CoreDNS to use the host gateway
        extra_hosts: Some(vec!["host.docker.internal:host-gateway".to_string()]),
        ..Default::default()
    };

    let cmd = vec![
        "server".to_string(),
        "--disable=traefik".to_string(),
        "--tls-san=127.0.0.1".to_string(),
        "--tls-san=localhost".to_string(),
        "--tls-san=host.docker.internal".to_string(),
    ];

    let config = Config {
        image: Some(image_ref.to_string()),
        cmd: Some(cmd),
        exposed_ports: Some(exposed_ports),
        host_config: Some(host_config),
        ..Default::default()
    };

    docker
        .create_container(
            Some(CreateContainerOptions {
                name: container_name,
                platform: None,
            }),
            config,
        )
        .await
        .into_diagnostic()
        .wrap_err("failed to create cluster container")?;
    Ok(())
}

async fn start_container(docker: &Docker, name: &str) -> Result<()> {
    let container_name = container_name(name);
    let response = docker
        .start_container(&container_name, None::<StartContainerOptions<String>>)
        .await;
    match response {
        Ok(()) => Ok(()),
        Err(err) if is_conflict(&err) => Ok(()),
        Err(err) => Err(err)
            .into_diagnostic()
            .wrap_err("failed to start cluster container"),
    }
}

async fn stop_container(docker: &Docker, container_name: &str) -> Result<()> {
    let response = docker.stop_container(container_name, None).await;
    match response {
        Ok(()) => Ok(()),
        Err(err) if is_conflict(&err) => Ok(()),
        Err(err) if is_not_found(&err) => Ok(()),
        Err(err) => Err(err).into_diagnostic(),
    }
}

async fn destroy_cluster_resources(
    docker: &Docker,
    name: &str,
    kubeconfig_path: &Path,
) -> Result<()> {
    let container_name = container_name(name);
    let volume_name = volume_name(name);

    let _ = stop_container(docker, &container_name).await;

    let remove_container = docker
        .remove_container(
            &container_name,
            Some(RemoveContainerOptions {
                force: true,
                ..Default::default()
            }),
        )
        .await;
    if let Err(err) = remove_container
        && !is_not_found(&err)
    {
        return Err(err).into_diagnostic();
    }

    let remove_volume = docker
        .remove_volume(&volume_name, Some(RemoveVolumeOptions { force: true }))
        .await;
    if let Err(err) = remove_volume
        && !is_not_found(&err)
    {
        return Err(err).into_diagnostic();
    }

    let _ = std::fs::remove_file(kubeconfig_path);

    cleanup_network_if_unused(docker).await?;
    Ok(())
}

async fn cleanup_network_if_unused(docker: &Docker) -> Result<()> {
    let network = docker
        .inspect_network(NETWORK_NAME, None::<InspectNetworkOptions<String>>)
        .await;
    let network = match network {
        Ok(info) => info,
        Err(err) if is_not_found(&err) => return Ok(()),
        Err(err) => return Err(err).into_diagnostic(),
    };

    if let Some(containers) = network.containers
        && !containers.is_empty()
    {
        return Ok(());
    }

    docker
        .remove_network(NETWORK_NAME)
        .await
        .into_diagnostic()
        .wrap_err("failed to remove Docker network")?;
    Ok(())
}

async fn wait_for_kubeconfig(docker: &Docker, name: &str) -> Result<String> {
    let container_name = container_name(name);
    let attempts = 30;
    for attempt in 0..attempts {
        match exec_capture(
            docker,
            &container_name,
            vec!["cat".to_string(), KUBECONFIG_PATH.to_string()],
        )
        .await
        {
            Ok(output) if is_valid_kubeconfig(&output) => return Ok(output),
            Ok(_) => {}
            Err(err) if attempt + 1 < attempts => {
                let _ = err;
            }
            Err(err) => return Err(err),
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }

    Err(miette::miette!("timed out waiting for kubeconfig"))
}

async fn wait_for_cluster_ready(docker: &Docker, name: &str) -> Result<()> {
    let container_name = container_name(name);
    let attempts = 180;
    for attempt in 0..attempts {
        let inspect = docker
            .inspect_container(&container_name, None::<InspectContainerOptions>)
            .await
            .into_diagnostic()?;
        let status = inspect
            .state
            .and_then(|state| state.health)
            .and_then(|health| health.status);

        match status {
            Some(HealthStatusEnum::HEALTHY) => return Ok(()),
            Some(HealthStatusEnum::UNHEALTHY) if attempt + 1 == attempts => {
                return Err(miette::miette!("cluster health check reported unhealthy"));
            }
            Some(HealthStatusEnum::NONE | HealthStatusEnum::EMPTY) | None if attempt == 0 => {
                return Err(miette::miette!(
                    "cluster container does not expose a health check"
                ));
            }
            _ => {}
        }

        tokio::time::sleep(Duration::from_secs(2)).await;
    }

    Err(miette::miette!(
        "timed out waiting for cluster health check"
    ))
}

fn is_valid_kubeconfig(output: &str) -> bool {
    output.contains("apiVersion:") && output.contains("clusters:")
}

async fn exec_capture(docker: &Docker, container_name: &str, cmd: Vec<String>) -> Result<String> {
    let (output, _status) = exec_capture_with_exit(docker, container_name, cmd).await?;
    Ok(output)
}

async fn exec_capture_with_exit(
    docker: &Docker,
    container_name: &str,
    cmd: Vec<String>,
) -> Result<(String, i64)> {
    let exec = docker
        .create_exec(
            container_name,
            CreateExecOptions {
                attach_stdout: Some(true),
                attach_stderr: Some(true),
                cmd: Some(cmd),
                ..Default::default()
            },
        )
        .await
        .into_diagnostic()?
        .id;

    let start = docker.start_exec(&exec, None).await.into_diagnostic()?;
    let mut buffer = String::new();
    if let bollard::exec::StartExecResults::Attached { mut output, .. } = start {
        while let Some(item) = output.next().await {
            let log = item.into_diagnostic()?;
            match log {
                LogOutput::StdOut { message }
                | LogOutput::StdErr { message }
                | LogOutput::Console { message } => {
                    buffer.push_str(&String::from_utf8_lossy(&message));
                }
                LogOutput::StdIn { .. } => {}
            }
        }
    }

    let mut exit_code = None;
    for _ in 0..20 {
        let inspect = docker.inspect_exec(&exec).await.into_diagnostic()?;
        if let Some(code) = inspect.exit_code {
            exit_code = Some(code);
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    Ok((buffer, exit_code.unwrap_or(1)))
}

fn store_kubeconfig(path: &Path, contents: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .into_diagnostic()
            .wrap_err_with(|| format!("failed to create {}", parent.display()))?;
    }
    std::fs::write(path, contents)
        .into_diagnostic()
        .wrap_err_with(|| format!("failed to write kubeconfig to {}", path.display()))?;
    Ok(())
}

fn rewrite_kubeconfig(contents: &str, cluster_name: &str) -> String {
    let mut replaced = Vec::new();
    for line in contents.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("server:") {
            let indent_len = line.len() - trimmed.len();
            let indent = &line[..indent_len];
            replaced.push(format!("{indent}server: https://127.0.0.1:6443"));
            continue;
        }
        // Rename default cluster/context/user to the cluster name
        // Handle both "name: default" and "- name: default" (YAML list item)
        if trimmed == "name: default" || trimmed == "- name: default" {
            let indent_len = line.len() - trimmed.len();
            let indent = &line[..indent_len];
            let prefix = if trimmed.starts_with("- ") { "- " } else { "" };
            replaced.push(format!("{indent}{prefix}name: {cluster_name}"));
            continue;
        }
        if trimmed == "cluster: default" || trimmed == "user: default" {
            let indent_len = line.len() - trimmed.len();
            let indent = &line[..indent_len];
            let key = trimmed.split(':').next().unwrap_or("cluster");
            replaced.push(format!("{indent}{key}: {cluster_name}"));
            continue;
        }
        if trimmed == "current-context: default" {
            replaced.push(format!("current-context: {cluster_name}"));
            continue;
        }
        replaced.push(line.to_string());
    }

    let mut output = replaced.join("\n");
    if contents.ends_with('\n') {
        output.push('\n');
    }
    output
}

struct CliMtlsBundle {
    ca: Vec<u8>,
    cert: Vec<u8>,
    key: Vec<u8>,
}

async fn fetch_and_store_cli_mtls(docker: &Docker, name: &str) -> Result<()> {
    let attempts = 90;
    let backoff = Duration::from_secs(2);

    if !gateway_tls_enabled(docker, name).await? {
        return Ok(());
    }

    for attempt in 0..attempts {
        match fetch_cli_mtls_bundle(docker, name).await {
            Ok(Some(bundle)) => {
                store_cli_mtls_bundle(name, bundle)?;
                return Ok(());
            }
            Ok(None) if attempt + 1 < attempts => {
                tokio::time::sleep(backoff).await;
            }
            Ok(None) => {
                return Err(miette::miette!(
                    "timed out waiting for CLI mTLS secret {CLI_SECRET_NAME}"
                ));
            }
            Err(err) => {
                return Err(miette::miette!(
                    "failed to fetch CLI mTLS secret {CLI_SECRET_NAME}: {err}"
                ));
            }
        }
    }

    Err(miette::miette!(
        "timed out waiting for CLI mTLS secret {CLI_SECRET_NAME}"
    ))
}

async fn gateway_tls_enabled(docker: &Docker, name: &str) -> Result<bool> {
    if let Ok(value) = std::env::var(NAV_GATEWAY_TLS_ENABLED_ENV) {
        return parse_bool_env(&value)
            .wrap_err_with(|| format!("{NAV_GATEWAY_TLS_ENABLED_ENV} must be true or false"));
    }

    let container_name = container_name(name);
    for path in HELMCHART_MANIFEST_PATHS {
        if let Some(contents) = read_container_file(docker, &container_name, path).await? {
            if let Some(enabled) = parse_gateway_tls_enabled_from_helmchart(&contents)? {
                return Ok(enabled);
            }
        }
    }

    Err(miette::miette!(
        "failed to determine gateway TLS configuration from {NAV_GATEWAY_TLS_ENABLED_ENV} or HelmChart manifest"
    ))
}

async fn read_container_file(
    docker: &Docker,
    container_name: &str,
    path: &str,
) -> Result<Option<String>> {
    let (output, status) = exec_capture_with_exit(
        docker,
        container_name,
        vec!["cat".to_string(), path.to_string()],
    )
    .await?;
    if status != 0 {
        return Ok(None);
    }
    Ok(Some(output))
}

fn parse_gateway_tls_enabled_from_helmchart(contents: &str) -> Result<Option<bool>> {
    let helmchart: serde_yaml::Value = serde_yaml::from_str(contents)
        .into_diagnostic()
        .wrap_err("failed to parse HelmChart manifest")?;
    let values_content = helmchart
        .get("spec")
        .and_then(|value| value.get("valuesContent"))
        .and_then(|value| value.as_str());
    let Some(values_content) = values_content else {
        return Ok(None);
    };
    parse_gateway_tls_enabled_from_values(values_content).map(Some)
}

fn parse_gateway_tls_enabled_from_values(values_content: &str) -> Result<bool> {
    let values: serde_yaml::Value = serde_yaml::from_str(values_content)
        .into_diagnostic()
        .wrap_err("failed to parse Helm values")?;
    let enabled = values
        .get("gateway")
        .and_then(|value| value.get("tls"))
        .and_then(|value| value.get("enabled"));
    match enabled {
        Some(value) => parse_bool_value(value).wrap_err("failed to read gateway.tls.enabled"),
        None => Ok(false),
    }
}

fn parse_bool_value(value: &serde_yaml::Value) -> Result<bool> {
    if let Some(value) = value.as_bool() {
        return Ok(value);
    }
    let Some(value) = value.as_str() else {
        return Err(miette::miette!("expected a boolean"));
    };
    parse_bool_env(value)
}

fn parse_bool_env(value: &str) -> Result<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" => Ok(true),
        "false" | "0" | "no" => Ok(false),
        _ => Err(miette::miette!("expected a boolean")),
    }
}

async fn fetch_cli_mtls_bundle(docker: &Docker, name: &str) -> Result<Option<CliMtlsBundle>> {
    let container_name = container_name(name);
    let jsonpath = r#"{.data.ca\.crt}{"\n"}{.data.tls\.crt}{"\n"}{.data.tls\.key}"#;
    let output = exec_capture(
        docker,
        &container_name,
        vec![
            "kubectl".to_string(),
            "-n".to_string(),
            "navigator".to_string(),
            "get".to_string(),
            "secret".to_string(),
            CLI_SECRET_NAME.to_string(),
            "-o".to_string(),
            format!("jsonpath={jsonpath}"),
        ],
    )
    .await?;
    if output.trim().is_empty()
        || output.contains("NotFound")
        || output.contains("not found")
        || output.contains("Error from server")
    {
        return Ok(None);
    }

    let mut lines = output.lines();
    let ca_b64 = lines.next().unwrap_or("").trim();
    let cert_b64 = lines.next().unwrap_or("").trim();
    let key_b64 = lines.next().unwrap_or("").trim();
    if ca_b64.is_empty() || cert_b64.is_empty() || key_b64.is_empty() {
        return Ok(None);
    }

    let ca = STANDARD.decode(ca_b64).into_diagnostic()?;
    let cert = STANDARD.decode(cert_b64).into_diagnostic()?;
    let key = STANDARD.decode(key_b64).into_diagnostic()?;

    Ok(Some(CliMtlsBundle { ca, cert, key }))
}

fn cli_mtls_dir(name: &str) -> Result<PathBuf> {
    Ok(xdg_config_dir()?
        .join("navigator")
        .join("clusters")
        .join(name)
        .join("mtls"))
}

fn cli_mtls_temp_dir(name: &str) -> Result<PathBuf> {
    Ok(cli_mtls_dir(name)?.with_extension("tmp"))
}

fn cli_mtls_backup_dir(name: &str) -> Result<PathBuf> {
    Ok(cli_mtls_dir(name)?.with_extension("bak"))
}

fn store_cli_mtls_bundle(name: &str, bundle: CliMtlsBundle) -> Result<()> {
    let dir = cli_mtls_dir(name)?;
    let temp_dir = cli_mtls_temp_dir(name)?;
    let backup_dir = cli_mtls_backup_dir(name)?;

    if temp_dir.exists() {
        std::fs::remove_dir_all(&temp_dir)
            .into_diagnostic()
            .wrap_err_with(|| format!("failed to remove {}", temp_dir.display()))?;
    }

    std::fs::create_dir_all(&temp_dir)
        .into_diagnostic()
        .wrap_err_with(|| format!("failed to create {}", temp_dir.display()))?;

    std::fs::write(temp_dir.join("ca.crt"), bundle.ca)
        .into_diagnostic()
        .wrap_err("failed to write ca.crt")?;
    std::fs::write(temp_dir.join("tls.crt"), bundle.cert)
        .into_diagnostic()
        .wrap_err("failed to write tls.crt")?;
    std::fs::write(temp_dir.join("tls.key"), bundle.key)
        .into_diagnostic()
        .wrap_err("failed to write tls.key")?;

    validate_cli_mtls_bundle_dir(&temp_dir)?;

    let had_backup = if dir.exists() {
        if backup_dir.exists() {
            std::fs::remove_dir_all(&backup_dir)
                .into_diagnostic()
                .wrap_err_with(|| format!("failed to remove {}", backup_dir.display()))?;
        }
        std::fs::rename(&dir, &backup_dir)
            .into_diagnostic()
            .wrap_err_with(|| format!("failed to rename {}", dir.display()))?;
        true
    } else {
        false
    };

    if let Err(err) = std::fs::rename(&temp_dir, &dir)
        .into_diagnostic()
        .wrap_err_with(|| format!("failed to move {}", temp_dir.display()))
    {
        if had_backup {
            let _ = std::fs::rename(&backup_dir, &dir);
        }
        return Err(err);
    }

    if had_backup {
        std::fs::remove_dir_all(&backup_dir)
            .into_diagnostic()
            .wrap_err_with(|| format!("failed to remove {}", backup_dir.display()))?;
    }
    Ok(())
}

fn validate_cli_mtls_bundle_dir(dir: &Path) -> Result<()> {
    for name in ["ca.crt", "tls.crt", "tls.key"] {
        let path = dir.join(name);
        let metadata = std::fs::metadata(&path)
            .into_diagnostic()
            .wrap_err_with(|| format!("failed to read {}", path.display()))?;
        if metadata.len() == 0 {
            return Err(miette::miette!("{} is empty", path.display()));
        }
    }
    Ok(())
}

fn merge_kubeconfig(target: &mut Kubeconfig, incoming: Kubeconfig) {
    merge_named(&mut target.clusters, incoming.clusters);
    merge_named(&mut target.contexts, incoming.contexts);
    merge_named(&mut target.users, incoming.users);

    if incoming.current_context.is_some() {
        target.current_context = incoming.current_context;
    }
    if incoming.preferences.is_some() {
        target.preferences = incoming.preferences;
    }

    target
        .extra
        .extend(incoming.extra.into_iter().filter(|(k, _)| !k.is_empty()));
}

fn merge_named<T: NamedEntry>(target: &mut Vec<T>, incoming: Vec<T>) {
    for entry in incoming {
        if let Some(existing) = target.iter_mut().find(|item| item.name() == entry.name()) {
            *existing = entry;
        } else {
            target.push(entry);
        }
    }
}

trait NamedEntry {
    fn name(&self) -> &str;
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Kubeconfig {
    #[serde(rename = "apiVersion", default)]
    api_version: String,
    #[serde(default)]
    kind: String,
    #[serde(default)]
    clusters: Vec<NamedCluster>,
    #[serde(default)]
    contexts: Vec<NamedContext>,
    #[serde(default)]
    users: Vec<NamedUser>,
    #[serde(rename = "current-context", default)]
    current_context: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    preferences: Option<serde_yaml::Value>,
    #[serde(flatten, default)]
    extra: BTreeMap<String, serde_yaml::Value>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct NamedCluster {
    name: String,
    cluster: serde_yaml::Value,
}

impl NamedEntry for NamedCluster {
    fn name(&self) -> &str {
        &self.name
    }
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct NamedContext {
    name: String,
    context: serde_yaml::Value,
}

impl NamedEntry for NamedContext {
    fn name(&self) -> &str {
        &self.name
    }
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct NamedUser {
    name: String,
    user: serde_yaml::Value,
}

impl NamedEntry for NamedUser {
    fn name(&self) -> &str {
        &self.name
    }
}

fn is_not_found(err: &BollardError) -> bool {
    matches!(
        err,
        BollardError::DockerResponseServerError {
            status_code: 404,
            ..
        }
    )
}

fn is_conflict(err: &BollardError) -> bool {
    matches!(
        err,
        BollardError::DockerResponseServerError {
            status_code: 409,
            ..
        }
    )
}

#[cfg(test)]
mod tests {
    use super::rewrite_kubeconfig;

    #[test]
    fn rewrite_updates_server_address() {
        let input = "apiVersion: v1\nclusters:\n- cluster:\n    server: https://10.0.0.1:6443\n";
        let output = rewrite_kubeconfig(input, "test-cluster");
        assert!(output.contains("server: https://127.0.0.1:6443"));
    }

    #[test]
    fn rewrite_preserves_trailing_newline() {
        let input = "apiVersion: v1\nserver: https://10.0.0.1\n";
        let output = rewrite_kubeconfig(input, "test-cluster");
        assert!(output.ends_with('\n'));
    }

    #[test]
    fn rewrite_renames_default_entries() {
        let input = "apiVersion: v1
clusters:
- name: default
  cluster:
    server: https://10.0.0.1:6443
contexts:
- name: default
  context:
    cluster: default
    user: default
users:
- name: default
current-context: default
";
        let output = rewrite_kubeconfig(input, "my-cluster");
        assert!(
            output.contains("name: my-cluster"),
            "should contain 'name: my-cluster'"
        );
        assert!(
            output.contains("cluster: my-cluster"),
            "should contain 'cluster: my-cluster'"
        );
        assert!(
            output.contains("user: my-cluster"),
            "should contain 'user: my-cluster'"
        );
        assert!(
            output.contains("current-context: my-cluster"),
            "should contain 'current-context: my-cluster'"
        );
        assert!(
            !output.contains("name: default"),
            "should not contain 'name: default'"
        );
    }
}
