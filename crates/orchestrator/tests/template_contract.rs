#![cfg(target_os = "linux")]

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use orchestrator::builder::config::BuilderConfig;
use orchestrator::envd::ENVD_PORT;
use orchestrator::envd::process::{CommandContext, run_command};
use orchestrator::featureflags::FeatureFlags;
use orchestrator::network::settings::NetworkSettings;
use orchestrator::network::slot::Slot;
use orchestrator::proto::sandbox_service_client::SandboxServiceClient;
use orchestrator::proto::template_config::Source;
use orchestrator::proto::template_service_client::TemplateServiceClient;
use orchestrator::proto::{
    FromTemplateConfig, SandboxConfig, SandboxCreateRequest, SandboxDeleteRequest, TemplateBuildMetadata,
    TemplateBuildState, TemplateConfig, TemplateCreateRequest, TemplateStatusRequest, TemplateStep,
};
use orchestrator::server::app::{App, TemplateManagerSetup};
use orchestrator::server::config::NodeConfig;
use orchestrator::server::config::binaries::Binaries;
use orchestrator::server::config::runtime::RuntimeFlags;
use orchestrator::server::node::Node;
use orchestrator::server::services::Services;
use orchestrator::storage::spec::StorageSpec;
use orchestrator::tcpfirewall::proxy::TcpFirewall;
use uuid::Uuid;

const SLOTS: std::ops::RangeInclusive<u32> = 4120..=4135;
const BASE_IMAGE: &str = "debian:trixie-slim";
const LOG_PAGE: u32 = 100;
const BUILD_TIMEOUT: Duration = Duration::from_secs(20 * 60);
const V2_VERSION: &str = "v2.1.0";
const FILES_HASH: &str = "c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0";
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
const GROWN_DISK_MB: i32 = 4096;
const BASE_DISK_MB: i64 = 2048;
const START_COMMAND: &str = "echo \"Starting with TEST_VAR=$TEST_VAR\"; echo 'Initialization complete'; sleep infinity";

struct BuildRequest {
    source: Source,
    steps: Vec<TemplateStep>,
    version: &'static str,
    start_command: &'static str,
    ready_command: &'static str,
    free_disk_size_mb: Option<i32>,
}

impl BuildRequest {
    fn new(source: Source) -> Self {
        Self {
            source,
            steps: Vec::new(),
            version: "",
            start_command: "",
            ready_command: "",
            free_disk_size_mb: None,
        }
    }

    fn steps(self, steps: Vec<TemplateStep>) -> Self {
        Self { steps, ..self }
    }

    fn version(self, version: &'static str) -> Self {
        Self { version, ..self }
    }

    fn free_disk(self, free_disk_size_mb: i32) -> Self {
        Self {
            free_disk_size_mb: Some(free_disk_size_mb),
            ..self
        }
    }

    fn start(self, start_command: &'static str, ready_command: &'static str) -> Self {
        Self {
            start_command,
            ready_command,
            ..self
        }
    }
}

struct Built {
    build_id: String,
    state: i32,
    logs: Vec<String>,
    metadata: Option<TemplateBuildMetadata>,
    reason: String,
}

impl Built {
    fn has(&self, line: &str) -> bool {
        self.logs.iter().any(|log| log == line)
    }

    fn has_prefix(&self, prefix: &str) -> bool {
        self.logs.iter().any(|log| log.starts_with(prefix))
    }

    fn last(&self, prefix: &str) -> usize {
        self.logs
            .iter()
            .rposition(|log| log.starts_with(prefix))
            .unwrap_or_else(|| panic!("missing {prefix:?}"))
    }
}

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

fn node_config(host: &Host, dir: &Path, ensure_free_disk: bool) -> NodeConfig {
    let mut builder_vars = host.builder_vars.clone();
    builder_vars.insert(
        "TEMPLATES_DIR".to_owned(),
        dir.join("build-templates").display().to_string(),
    );
    let feature_flags = FeatureFlags::builder()
        .override_flag("max-sandboxes-per-node", 10)
        .override_flag("build-ensure-free-disk-space", ensure_free_disk)
        .offline();
    let builder = BuilderConfig {
        feature_flags: feature_flags.clone(),
        ..BuilderConfig::from_vars(&builder_vars)
    };
    NodeConfig {
        grpc_port: 0,
        node_id: "template-node".to_owned(),
        firecracker: host.firecracker.clone(),
        kernel: host.kernel.clone(),
        binaries: Binaries::from_vars(
            &Default::default(),
            &builder.target_arch,
            &host.firecracker,
            &host.kernel,
        ),
        network_version: 1,
        template_storage: StorageSpec::local(&dir.join("store").display().to_string()),
        build_cache_storage: Some(StorageSpec::local(&dir.join("build-cache").display().to_string())),
        local_upload_base_url: String::new(),
        builder,
        work_dir: dir.join("sandboxes"),
        cache_dir: dir.join("cache"),
        template_cache_dir: dir.join("templates"),
        slots: SLOTS,
        local_expiry: false,
        labels: Vec::new(),
        deployment_environment: String::new(),
        instance_group_name: String::new(),
        runtime_flags: RuntimeFlags::new(
            feature_flags.clone(),
            Services::parse(Some("orchestrator,template-manager")),
        ),
        feature_flags,
        local: true,
        development: true,
        force_stop: false,
        services: Services::parse(Some("orchestrator,template-manager")),
        orchestrator_ip: "192.0.2.1".to_owned(),
        hyperloop_port: 0,
        proxy_port: 0,
        logs_collector: String::new(),
        node_ip: "localhost".to_owned(),
        redis: None,
        clickhouse: None,
        clickhouse_additional: Vec::new(),
        volume_mounts: Default::default(),
        nfs_proxy: Default::default(),
        rootfs_exposure: Default::default(),
    }
}

async fn wait_for_build(
    templates: &mut TemplateServiceClient<tonic::transport::Channel>,
    template_id: &str,
    build_id: &str,
) -> Built {
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
            return Built {
                build_id: build_id.to_owned(),
                state: status.status,
                logs: all_logs(templates, template_id, build_id).await,
                metadata: status.metadata,
                reason: status.reason.map(|reason| reason.message).unwrap_or_default(),
            };
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

struct Running {
    address: SocketAddr,
    templates: TemplateServiceClient<tonic::transport::Channel>,
}

async fn start_builder(host: &Host, dir: &Path, ensure_free_disk: bool) -> Running {
    drop(
        tracing_subscriber::fmt()
            .with_test_writer()
            .with_max_level(tracing_subscriber::filter::LevelFilter::WARN)
            .try_init(),
    );
    SLOTS.for_each(|index| {
        drop(
            Command::new("ip")
                .args(["netns", "delete", &format!("ns-{index}")])
                .status(),
        )
    });
    let config = node_config(host, dir, ensure_free_disk);
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
    let templates = TemplateServiceClient::connect(format!("http://{address}"))
        .await
        .unwrap();
    Running { address, templates }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_debian_image_becomes_a_provisioned_base_layer() {
    let _serial = SERIAL.lock().await;
    let Some(host) = host() else {
        eprintln!("skipped: needs root, KVM and the build assets");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let Running { address, mut templates } = start_builder(&host, dir.path(), false).await;
    let template_id = format!("t{}", Uuid::new_v4().simple());
    let image = Source::FromImage(BASE_IMAGE.to_owned());

    let first = build(&mut templates, &template_id, BuildRequest::new(image.clone())).await;

    eprintln!("build logs:\n{}", first.logs.join("\n"));
    assert_eq!(
        first.state,
        TemplateBuildState::Completed as i32,
        "build failed: {}",
        first.reason
    );
    for line in [
        "Requesting Docker Image",
        "Provisioning sandbox template",
        "Provisioning was successful, cleaning up",
        "Sandbox template provisioned",
        "Creating base sandbox template layer",
    ] {
        assert!(first.has(line), "missing log line {line:?}");
    }
    for prefix in [
        "[finalize] Finalizing template build [",
        "[optimize] Optimizing template [",
    ] {
        assert!(first.has_prefix(prefix), "missing {prefix:?}");
    }
    assert!(!first.has_prefix("Waiting for template to be ready"));
    let metadata = first.metadata.clone().unwrap();
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
    assert!(metadata.scheduling_metadata.is_some());
    let store = dir.path().join("store");
    let indexed = indexed_builds(&dir.path().join("build-cache").join(&template_id).join("index"));
    assert_eq!(indexed.len(), 2);
    assert!(indexed.contains(&first.build_id));
    let base_build_id = indexed.into_iter().find(|id| *id != first.build_id).unwrap();
    for file in ["metadata.json", "rootfs.ext4.header", "memfile.header", "snapfile"] {
        assert!(store.join(&base_build_id).join(file).is_file(), "missing {file}");
    }
    let final_metadata = read_json(&store.join(&first.build_id).join("metadata.json"));
    assert_eq!(final_metadata["template"]["build_id"], first.build_id.as_str());
    assert_eq!(
        final_metadata["balloon"],
        serde_json::json!({"reporting": false, "hinting": false})
    );
    assert!(
        final_metadata["prefetch"].is_object(),
        "no prefetch in {final_metadata}"
    );

    let cached = build(&mut templates, &template_id, BuildRequest::new(image)).await;
    let from_template = || {
        Source::FromTemplate(FromTemplateConfig {
            alias: "base".to_owned(),
            build_id: base_build_id.clone(),
        })
    };
    let derived = build(&mut templates, &template_id, BuildRequest::new(from_template())).await;

    assert_eq!(cached.state, TemplateBuildState::Completed as i32);
    assert!(cached.has_prefix(&format!("CACHED [base] FROM {BASE_IMAGE} [")));
    assert!(!cached.has("Provisioning sandbox template"));
    assert_eq!(derived.state, TemplateBuildState::Completed as i32);
    assert!(derived.has_prefix("CACHED [base] FROM TEMPLATE base ["));

    write_layer_files(&dir.path().join("build-cache").join(&template_id).join("files"));
    let with_steps = || BuildRequest::new(from_template()).steps(steps()).version(V2_VERSION);
    let stepped = build(&mut templates, &template_id, with_steps()).await;
    eprintln!("steps build logs:\n{}", stepped.logs.join("\n"));
    assert_eq!(
        stepped.state,
        TemplateBuildState::Completed as i32,
        "steps build failed: {}",
        stepped.reason
    );
    let total = steps().len();
    for prefix in [
        "[base] DEFAULT USER user [".to_owned(),
        format!("[builder 1/{total}] ENV GREETING Hello, World! ["),
    ] {
        assert!(stepped.has_prefix(&prefix), "missing {prefix:?}");
    }
    assert!(stepped.has(&format!("[builder 3/{total}] [stdout]: Hello, World!")));

    let rebuilt = build(&mut templates, &template_id, with_steps()).await;
    assert_eq!(rebuilt.state, TemplateBuildState::Completed as i32);
    assert!(rebuilt.has_prefix(&format!("CACHED [builder 1/{total}] ENV GREETING Hello, World! [")));

    let started = build(
        &mut templates,
        &template_id,
        BuildRequest::new(from_template())
            .steps(start_steps())
            .version(V2_VERSION)
            .start(START_COMMAND, "sleep 2"),
    )
    .await;
    eprintln!("start build logs:\n{}", started.logs.join("\n"));
    assert_eq!(
        started.state,
        TemplateBuildState::Completed as i32,
        "start build failed: {}",
        started.reason
    );
    let running = format!("Running start command: {START_COMMAND}");
    for line in [
        running.as_str(),
        "[start] [stdout]: Starting with TEST_VAR=test_value",
        "[start] [stdout]: Initialization complete",
        "Waiting for template to be ready: sleep 2",
        "Template is ready",
    ] {
        assert!(started.has(line), "missing log line {line:?}");
    }
    assert!(started.last("Running start command") < started.last("Waiting for template to be ready"));
    assert!(started.last("Waiting for template to be ready") < started.last("Template is ready"));
    check_final_sandbox(address, &template_id, &started).await;

    let failed = build(
        &mut templates,
        &template_id,
        BuildRequest::new(from_template())
            .version(V2_VERSION)
            .start("exit 3", "sleep 5"),
    )
    .await;
    assert_eq!(failed.state, TemplateBuildState::Failed as i32);
    assert_eq!(
        failed.reason,
        "start command failed: error running start command: exit status 3"
    );
    assert!(failed.has("Template is ready"));
}

#[tokio::test(flavor = "multi_thread")]
async fn the_ensure_free_disk_phase_grows_the_rootfs_before_finalize() {
    let _serial = SERIAL.lock().await;
    let Some(host) = host() else {
        eprintln!("skipped: needs root, KVM and the build assets");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let Running { address, mut templates } = start_builder(&host, dir.path(), true).await;
    let template_id = format!("t{}", Uuid::new_v4().simple());
    let index = dir.path().join("build-cache").join(&template_id).join("index");
    let store = dir.path().join("store");
    let grown_request = || {
        BuildRequest::new(Source::FromImage(BASE_IMAGE.to_owned()))
            .version(V2_VERSION)
            .start(START_COMMAND, "sleep 2")
            .free_disk(GROWN_DISK_MB)
    };

    let grown = build(&mut templates, &template_id, grown_request()).await;

    eprintln!("grown build logs:\n{}", grown.logs.join("\n"));
    assert_eq!(
        grown.state,
        TemplateBuildState::Completed as i32,
        "grown build failed: {}",
        grown.reason
    );
    assert!(grown.has_prefix("[resize-disk] Resizing disk ["));
    assert!(!grown.has_prefix("CACHED [resize-disk]"));
    assert!(grown.last("[resize-disk] Resizing disk [") < grown.last("[finalize] Finalizing template build ["));
    let (free_before, free_after) = resized_free(&grown);
    assert!(free_before < i64::from(GROWN_DISK_MB), "{free_before} MiB free already");
    assert!(free_after > free_before, "free went {free_before} -> {free_after} MiB");
    let grown_size = i64::from(grown.metadata.clone().unwrap().rootfs_size_key);
    assert!(
        grown_size >= BASE_DISK_MB + i64::from(GROWN_DISK_MB) - free_before - 1,
        "rootfs is {grown_size} MB after {free_before} -> {free_after} MiB free"
    );
    let layers = filesystem_only_layers(&store, &indexed_builds(&index));
    assert_eq!(layers.len(), 1, "filesystem-only layers: {layers:?}");
    let grown_layer = store.join(&layers[0]);
    for file in ["metadata.json", "rootfs.ext4.header", "rootfs.ext4"] {
        assert!(grown_layer.join(file).is_file(), "missing {file}");
    }
    for file in ["snapfile", "memfile", "memfile.header"] {
        assert!(!grown_layer.join(file).exists(), "unexpected {file}");
    }
    let layer_metadata = read_json(&grown_layer.join("metadata.json"));
    assert_eq!(layer_metadata["filesystem_only"], true);
    assert!(layer_metadata["version"].as_u64().unwrap() >= 2);
    check_final_sandbox(address, &template_id, &grown).await;

    let cached = build(&mut templates, &template_id, grown_request()).await;
    assert_eq!(cached.state, TemplateBuildState::Completed as i32, "{}", cached.reason);
    assert!(cached.has_prefix("CACHED [resize-disk] Resizing disk ["));
    assert!(!cached.has_prefix("resize-disk: "));

    let unchanged = build(&mut templates, &template_id, grown_request().free_disk(1)).await;
    eprintln!("unchanged build logs:\n{}", unchanged.logs.join("\n"));
    assert_eq!(
        unchanged.state,
        TemplateBuildState::Completed as i32,
        "unchanged build failed: {}",
        unchanged.reason
    );
    assert!(unchanged.has_prefix("[resize-disk] Resizing disk ["));
    assert!(
        unchanged.logs.iter().any(|log| log.starts_with("resize-disk: ")
            && log.ends_with(" MiB free already >= target 1 MiB, no resize needed")),
        "no unchanged line in {:?}",
        unchanged.logs
    );
    let unchanged_size = i64::from(unchanged.metadata.clone().unwrap().rootfs_size_key);
    assert!(
        unchanged_size < grown_size,
        "{unchanged_size} MB is not below {grown_size} MB"
    );
    let layers_after = filesystem_only_layers(&store, &indexed_builds(&index));
    let unchanged_layers: Vec<&String> = layers_after.iter().filter(|id| !layers.contains(id)).collect();
    assert_eq!(unchanged_layers.len(), 1, "filesystem-only layers: {layers_after:?}");
    let unchanged_layer = store.join(unchanged_layers[0]);
    for file in ["metadata.json", "rootfs.ext4.header"] {
        assert!(unchanged_layer.join(file).is_file(), "missing {file}");
    }
    for file in ["rootfs.ext4", "snapfile", "memfile", "memfile.header"] {
        assert!(!unchanged_layer.join(file).exists(), "unexpected {file}");
    }
}

fn resized_free(built: &Built) -> (i64, i64) {
    let line = built
        .logs
        .iter()
        .find_map(|log| log.strip_prefix("resize-disk: free "))
        .unwrap_or_else(|| panic!("no resize line in {:?}", built.logs));
    let (before, after) = line
        .strip_suffix(" MiB")
        .and_then(|numbers| numbers.split_once(" -> "))
        .unwrap_or_else(|| panic!("unexpected resize line {line:?}"));
    (before.parse().unwrap(), after.parse().unwrap())
}

fn filesystem_only_layers(store: &Path, builds: &[String]) -> Vec<String> {
    builds
        .iter()
        .filter(|build| read_json(&store.join(build).join("metadata.json"))["filesystem_only"] == true)
        .cloned()
        .collect()
}

fn indexed_builds(index: &Path) -> Vec<String> {
    std::fs::read_dir(index)
        .unwrap()
        .map(|entry| {
            read_json(&entry.unwrap().path())["template"]["build_id"]
                .as_str()
                .unwrap()
                .to_owned()
        })
        .collect()
}

fn read_json(path: &Path) -> serde_json::Value {
    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
}

async fn check_final_sandbox(address: SocketAddr, template_id: &str, built: &Built) {
    let mut sandboxes = SandboxServiceClient::connect(format!("http://{address}"))
        .await
        .unwrap();
    let metadata = built.metadata.clone().unwrap();
    let now = SystemTime::now();
    sandboxes
        .create(SandboxCreateRequest {
            sandbox: Some(SandboxConfig {
                sandbox_id: "final-sbx".to_owned(),
                template_id: template_id.to_owned(),
                build_id: built.build_id.clone(),
                team_id: "team".to_owned(),
                execution_id: "final-execution".to_owned(),
                kernel_version: metadata.kernel_version,
                firecracker_version: metadata.firecracker_version,
                envd_version: metadata.envd_version_key,
                vcpu: 2,
                ram_mb: 1024,
                ..SandboxConfig::default()
            }),
            start_time: Some(now.into()),
            end_time: Some((now + Duration::from_secs(600)).into()),
            filesystem_boot: None,
        })
        .await
        .unwrap();
    let envd = running_envd().await;
    assert_eq!(
        guest_output(envd, "cat /.e2b").await,
        Ok(format!(
            "ENV_ID={template_id}\nTEMPLATE_ID={template_id}\nBUILD_ID={}\n",
            built.build_id
        ))
    );
    assert!(guest_output(envd, "pgrep -x sleep").await.is_ok());
    assert!(
        guest_output(envd, "test -s /usr/local/share/e2b/ssl-certs.tar")
            .await
            .is_ok()
    );
    sandboxes
        .delete(SandboxDeleteRequest {
            sandbox_id: "final-sbx".to_owned(),
            kill_reason: None,
        })
        .await
        .unwrap();
}

async fn running_envd() -> SocketAddr {
    let client = reqwest::Client::new();
    for _ in 0..20 {
        for index in SLOTS {
            let envd = SocketAddr::from((Slot::new(index).unwrap().host_ip(), ENVD_PORT));
            let health = client
                .get(format!("http://{envd}/health"))
                .timeout(Duration::from_millis(500))
                .send()
                .await;
            if health.is_ok_and(|response| response.status() == 204) {
                return envd;
            }
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    panic!("no envd answers in slots {SLOTS:?}")
}

async fn guest_output(envd: SocketAddr, command: &str) -> Result<String, String> {
    let root = CommandContext {
        user: "root".to_owned(),
        ..CommandContext::default()
    };
    let mut output = String::new();
    run_command(envd, command, &root, |stdout, _| output.push_str(stdout))
        .await
        .map(|()| output)
}

fn start_steps() -> Vec<TemplateStep> {
    vec![
        step("RUN", &["echo 'Setting up template'"]),
        step("ENV", &["TEST_VAR", "test_value"]),
    ]
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
    request: BuildRequest,
) -> Built {
    let build_id = Uuid::new_v4().to_string();
    templates
        .template_create(TemplateCreateRequest {
            template: Some(TemplateConfig {
                template_id: template_id.to_owned(),
                build_id: build_id.clone(),
                memory_mb: 1024,
                v_cpu_count: 2,
                disk_size_mb: 2048,
                source: Some(request.source),
                team_id: "team".to_owned(),
                steps: request.steps,
                start_command: request.start_command.to_owned(),
                ready_command: request.ready_command.to_owned(),
                free_disk_size_mb: request.free_disk_size_mb,
                ..TemplateConfig::default()
            }),
            version: Some(request.version.to_owned()).filter(|version| !version.is_empty()),
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
