#![cfg(target_os = "linux")]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use orchestrator::builder::config::BuilderConfig;
use orchestrator::network::settings::NetworkSettings;
use orchestrator::proto::template_config::Source;
use orchestrator::proto::template_service_client::TemplateServiceClient;
use orchestrator::proto::{
    FromTemplateConfig, TemplateBuildState, TemplateConfig, TemplateCreateRequest, TemplateStatusRequest, TemplateStep,
};
use orchestrator::server::app::{App, TemplateManagerSetup};
use orchestrator::server::config::NodeConfig;
use orchestrator::server::node::Node;
use orchestrator::server::services::Services;
use orchestrator::storage::spec::StorageSpec;
use orchestrator::tcpfirewall::proxy::TcpFirewall;
use uuid::Uuid;

const SLOTS: std::ops::RangeInclusive<u32> = 4106..=4109;
const BASE_IMAGE: &str = "debian:trixie-slim";
const LOG_PAGE: u32 = 100;
const BUILD_TIMEOUT: Duration = Duration::from_secs(20 * 60);
const V2_VERSION: &str = "v2.1.0";
const FILES_HASH: &str = "contract-files";

struct Host {
    firecracker: PathBuf,
    kernel: PathBuf,
    builder_vars: HashMap<String, String>,
}

fn host() -> Option<Host> {
    let var = |name: &str| std::env::var(name).ok().filter(|value| !value.is_empty());
    let in_ci = var("CI").is_some_and(|value| value == "true");
    let ready = nix::unistd::geteuid().is_root() && Path::new("/dev/kvm").exists();
    let host = ready.then_some(()).and_then(|()| {
        Some(Host {
            firecracker: var("E2B_FIRECRACKER")?.into(),
            kernel: var("E2B_KERNEL")?.into(),
            builder_vars: ["HOST_ENVD_PATH", "HOST_BUSYBOX_DIR", "BUSYBOX_VERSION"]
                .iter()
                .map(|name| var(name).map(|value| (name.to_string(), value)))
                .collect::<Option<HashMap<_, _>>>()?,
        })
    });
    assert!(
        host.is_some() || !in_ci,
        "template contract needs root, KVM, E2B_FIRECRACKER, E2B_KERNEL, HOST_ENVD_PATH, HOST_BUSYBOX_DIR and BUSYBOX_VERSION in CI"
    );
    host
}

fn node_config(host: &Host, dir: &Path) -> NodeConfig {
    let mut builder_vars = host.builder_vars.clone();
    builder_vars.insert(
        "TEMPLATES_DIR".to_owned(),
        dir.join("build-templates").display().to_string(),
    );
    NodeConfig {
        grpc_port: 0,
        node_id: "template-node".to_owned(),
        firecracker: host.firecracker.clone(),
        kernel: host.kernel.clone(),
        template_storage: StorageSpec::local(&dir.join("store").display().to_string()),
        build_cache_storage: Some(StorageSpec::local(&dir.join("build-cache").display().to_string())),
        local_upload_base_url: String::new(),
        builder: BuilderConfig::from_vars(&builder_vars),
        work_dir: dir.join("sandboxes"),
        cache_dir: dir.join("cache"),
        template_cache_dir: dir.join("templates"),
        slots: SLOTS,
        local_expiry: false,
        envd_timeout: Duration::from_secs(10),
        labels: Vec::new(),
        max_sandboxes: 10,
        max_starting: 3,
        local: true,
        development: true,
        force_stop: false,
        build_cache_max_usage: 95.0,
        prefetch_from_trace: true,
        use_memfd: true,
        services: Services::parse(Some("orchestrator,template-manager")),
        orchestrator_ip: "192.0.2.1".to_owned(),
        hyperloop_port: 0,
        logs_collector: String::new(),
        node_ip: "localhost".to_owned(),
        redis: None,
        clickhouse: None,
        clickhouse_additional: Vec::new(),
    }
}

async fn wait_for_build(
    templates: &mut TemplateServiceClient<tonic::transport::Channel>,
    template_id: &str,
    build_id: &str,
) -> (
    i32,
    Vec<String>,
    Option<orchestrator::proto::TemplateBuildMetadata>,
    String,
) {
    let deadline = tokio::time::Instant::now() + BUILD_TIMEOUT;
    loop {
        let status = templates
            .template_build_status(TemplateStatusRequest {
                template_id: template_id.to_owned(),
                build_id: build_id.to_owned(),
                limit: Some(100),
                ..TemplateStatusRequest::default()
            })
            .await
            .unwrap()
            .into_inner();
        let finished = status.status != TemplateBuildState::Building as i32;
        if finished || tokio::time::Instant::now() > deadline {
            let logs = all_logs(templates, template_id, build_id).await;
            let reason = status.reason.map(|reason| reason.message).unwrap_or_default();
            return (status.status, logs, status.metadata, reason);
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_debian_image_becomes_a_provisioned_base_layer() {
    let Some(host) = host() else {
        eprintln!("skipped: needs root, KVM and the build assets");
        return;
    };
    SLOTS.for_each(|index| {
        drop(
            Command::new("ip")
                .args(["netns", "delete", &format!("ns-{index}")])
                .status(),
        )
    });
    let dir = tempfile::tempdir().unwrap();
    let config = node_config(&host, dir.path());
    let node = Arc::new(Node::new(&config).unwrap());
    let setup = config.open_build_storage().unwrap().map(|build| TemplateManagerSetup {
        build,
        builder: config.builder.clone(),
        development: true,
    });
    let app = App::new(node.clone(), setup).unwrap();
    app.start_background();
    let firewall_node = node.clone();
    let settings = NetworkSettings::current();
    TcpFirewall::new(Arc::new(move |ip| firewall_node.firewall_source(ip)), settings)
        .start(settings)
        .await
        .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(axum::serve(listener, app.router()).into_future());
    let mut templates = TemplateServiceClient::connect(format!("http://{address}"))
        .await
        .unwrap();
    let template_id = format!("t{}", Uuid::new_v4().simple());
    let image = Source::FromImage(BASE_IMAGE.to_owned());

    let (state, logs, metadata, reason) = build(&mut templates, &template_id, image.clone()).await;

    eprintln!("build logs:\n{}", logs.join("\n"));
    assert_eq!(state, TemplateBuildState::Completed as i32, "build failed: {reason}");
    for line in [
        "Requesting Docker Image",
        "Provisioning sandbox template",
        "Provisioning was successful, cleaning up",
        "Sandbox template provisioned",
        "Creating base sandbox template layer",
    ] {
        assert!(logs.iter().any(|log| log == line), "missing log line {line:?}");
    }
    let metadata = metadata.unwrap();
    assert!(!metadata.envd_version_key.is_empty());
    assert_eq!(
        (metadata.kernel_version.as_str(), metadata.firecracker_version.as_str()),
        ("vmlinux-6.1.158", "v1.14-0.2.0")
    );
    assert!(
        metadata.rootfs_size_key >= 2048,
        "rootfs is {} MB",
        metadata.rootfs_size_key
    );
    let index_dir = dir.path().join("build-cache").join(&template_id).join("index");
    let entries: Vec<_> = std::fs::read_dir(&index_dir).unwrap().collect();
    assert_eq!(entries.len(), 1);
    let base_build = std::fs::read_to_string(entries[0].as_ref().unwrap().path()).unwrap();
    let base_build: serde_json::Value = serde_json::from_str(&base_build).unwrap();
    let base_dir = dir
        .path()
        .join("store")
        .join(base_build["template"]["build_id"].as_str().unwrap());
    for file in ["metadata.json", "rootfs.ext4.header", "memfile.header", "snapfile"] {
        assert!(base_dir.join(file).is_file(), "missing {file}");
    }

    let (cached_state, cached_logs, _, _) = build(&mut templates, &template_id, image).await;
    let base_build_id = base_build["template"]["build_id"].as_str().unwrap().to_owned();
    let from_template = Source::FromTemplate(FromTemplateConfig {
        alias: "base".to_owned(),
        build_id: base_build_id,
    });
    let (derived_state, derived_logs, _, _) = build(&mut templates, &template_id, from_template).await;

    assert_eq!(cached_state, TemplateBuildState::Completed as i32);
    assert!(
        cached_logs
            .iter()
            .any(|log| log.starts_with(&format!("CACHED [base] FROM {BASE_IMAGE} [")))
    );
    assert!(!cached_logs.iter().any(|log| log == "Provisioning sandbox template"));
    assert_eq!(derived_state, TemplateBuildState::Completed as i32);
    assert!(
        derived_logs
            .iter()
            .any(|log| log.starts_with("CACHED [base] FROM TEMPLATE base ["))
    );

    write_layer_files(&dir.path().join("build-cache").join(&template_id).join("files"));
    let derived = Source::FromTemplate(FromTemplateConfig {
        alias: "base".to_owned(),
        build_id: base_build["template"]["build_id"].as_str().unwrap().to_owned(),
    });
    let (steps_state, steps_logs, _, steps_reason) =
        build_with(&mut templates, &template_id, derived.clone(), steps(), V2_VERSION).await;
    eprintln!("steps build logs:\n{}", steps_logs.join("\n"));
    assert_eq!(
        steps_state,
        TemplateBuildState::Completed as i32,
        "steps build failed: {steps_reason}"
    );
    let total = steps().len();
    for prefix in [
        "[base] DEFAULT USER user [".to_owned(),
        format!("[builder 1/{total}] ENV GREETING Hello, World! ["),
    ] {
        assert!(
            steps_logs.iter().any(|log| log.starts_with(&prefix)),
            "missing {prefix:?}"
        );
    }
    assert!(
        steps_logs
            .iter()
            .any(|log| log == &format!("[builder 3/{total}] [stdout]: Hello, World!"))
    );

    let (rebuilt_state, rebuilt_logs, _, _) =
        build_with(&mut templates, &template_id, derived, steps(), V2_VERSION).await;
    assert_eq!(rebuilt_state, TemplateBuildState::Completed as i32);
    assert!(
        rebuilt_logs
            .iter()
            .any(|log| log.starts_with(&format!("CACHED [builder 1/{total}] ENV GREETING Hello, World! [")))
    );
}

fn step(kind: &str, args: &[&str]) -> TemplateStep {
    TemplateStep {
        r#type: kind.to_owned(),
        args: args.iter().map(|arg| (*arg).to_owned()).collect(),
        ..TemplateStep::default()
    }
}

fn steps() -> Vec<TemplateStep> {
    vec![
        step("ENV", &["GREETING", "Hello, World!"]),
        step(
            "ENV",
            &[
                "QUOTED",
                r#"say "hello""#,
                "LITERAL",
                "`echo pwned` $(echo pwned)",
                "BACKSLASH",
                r"path\to\file",
                "MY_PATH",
                "${PATH}:/my/path",
                "LINES",
                "line1\nline2\nline3",
            ],
        ),
        step("RUN", &["echo 'Hello, World!'"]),
        step(
            "RUN",
            &[concat!(
                r#"[[ "$GREETING" == "Hello, World!" ]] && [[ "$QUOTED" == 'say "hello"' ]]"#,
                r#" && [[ "$LITERAL" == '`echo pwned` $(echo pwned)' ]] && [[ "$BACKSLASH" == 'path\to\file' ]]"#,
                r#" && [[ "$MY_PATH" == *:/my/path ]] && [[ "$(printf '%s' "$LINES" | wc -l)" == 2 ]]"#,
            )],
        ),
        step("WORKDIR", &["app"]),
        step("RUN", &[r#"[[ "$(pwd)" == "/app" ]]"#]),
        step("USER", &["root"]),
        step("RUN", &[r#"[[ "$(whoami)" == "root" ]]"#]),
        TemplateStep {
            files_hash: Some(FILES_HASH.to_owned()),
            ..step("COPY", &["hello.txt", "/app/"])
        },
        step("USER", &["user"]),
        step(
            "RUN",
            &[
                r#"[[ "$(whoami)" == "user" ]] && [[ "$(cat /app/hello.txt)" == "hello" ]] && [[ "$(stat -c %U /app/hello.txt)" == "root" ]]"#,
            ],
        ),
    ]
}

fn write_layer_files(dir: &Path) {
    std::fs::create_dir_all(dir).unwrap();
    let file = std::fs::File::create(dir.join(format!("{FILES_HASH}.tar"))).unwrap();
    let mut archive = tar::Builder::new(flate2::write::GzEncoder::new(file, flate2::Compression::default()));
    let mut header = tar::Header::new_gnu();
    header.set_size(5);
    header.set_mode(0o644);
    header.set_cksum();
    archive.append_data(&mut header, "hello.txt", &b"hello"[..]).unwrap();
    archive.into_inner().unwrap().finish().unwrap();
}

async fn build(
    templates: &mut TemplateServiceClient<tonic::transport::Channel>,
    template_id: &str,
    source: Source,
) -> (
    i32,
    Vec<String>,
    Option<orchestrator::proto::TemplateBuildMetadata>,
    String,
) {
    build_with(templates, template_id, source, Vec::new(), "").await
}

async fn build_with(
    templates: &mut TemplateServiceClient<tonic::transport::Channel>,
    template_id: &str,
    source: Source,
    steps: Vec<TemplateStep>,
    version: &str,
) -> (
    i32,
    Vec<String>,
    Option<orchestrator::proto::TemplateBuildMetadata>,
    String,
) {
    let build_id = Uuid::new_v4().to_string();
    templates
        .template_create(TemplateCreateRequest {
            template: Some(TemplateConfig {
                template_id: template_id.to_owned(),
                build_id: build_id.clone(),
                memory_mb: 1024,
                v_cpu_count: 2,
                disk_size_mb: 2048,
                source: Some(source),
                team_id: "team".to_owned(),
                steps,
                ..TemplateConfig::default()
            }),
            version: Some(version.to_owned()).filter(|version| !version.is_empty()),
            ..TemplateCreateRequest::default()
        })
        .await
        .unwrap();
    wait_for_build(templates, template_id, &build_id).await
}

async fn all_logs(
    templates: &mut TemplateServiceClient<tonic::transport::Channel>,
    template_id: &str,
    build_id: &str,
) -> Vec<String> {
    let mut logs = Vec::new();
    loop {
        let page = templates
            .template_build_status(TemplateStatusRequest {
                template_id: template_id.to_owned(),
                build_id: build_id.to_owned(),
                offset: Some(logs.len() as i32),
                limit: Some(LOG_PAGE),
                ..TemplateStatusRequest::default()
            })
            .await
            .unwrap()
            .into_inner()
            .log_entries;
        let full = page.len() as u32 == LOG_PAGE;
        logs.extend(page.into_iter().map(|entry| entry.message));
        if !full {
            return logs;
        }
    }
}
