#[cfg(target_os = "linux")]
#[tokio::main]
async fn main() -> Result<(), String> {
    serve::run().await
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("orchestrator {} runs on Linux only", orchestrator::version());
}

#[cfg(target_os = "linux")]
mod serve {
    use std::net::SocketAddr;
    use std::sync::Arc;

    use axum::serve::ListenerExt;
    use tokio::net::TcpStream;

    use orchestrator::clickhouse::sinks::Clickhouse;
    use orchestrator::events::EventSink;
    use orchestrator::featureflags::SANDBOX_MAX_INCOMING_CONNECTIONS;
    use orchestrator::network::settings::NetworkSettings;
    use orchestrator::nfsproxy::NfsProxy;
    use orchestrator::proxy::sandbox::SandboxProxy;
    use orchestrator::routing::redis::Redis;
    use orchestrator::sandbox_observer::SandboxObserver;
    use orchestrator::sandbox_observer::sandboxes::running;
    use orchestrator::server::app::{App, TemplateManagerSetup};
    use orchestrator::server::config::NodeConfig;
    use orchestrator::server::info::service_instance_id;
    use orchestrator::server::node::Node;
    use orchestrator::server::shutdown::drain;
    use orchestrator::telemetry::Telemetry;
    use orchestrator::telemetry::identity::{Identity, service_name};
    use orchestrator::telemetry::server::gauges::{NodeGauges, NodeSource};

    const CONSUMER_GRACE: std::time::Duration = std::time::Duration::from_secs(15);
    use tokio::signal::unix::{Signal, SignalKind, signal};

    pub async fn run() -> Result<(), String> {
        let config = NodeConfig::from_env()?;
        let telemetry = Telemetry::start(&Identity {
            node_id: config.node_id.clone(),
            service_name: service_name(&config.services),
            service_commit: orchestrator::server::app::COMMIT.to_owned(),
            service_version: orchestrator::version().to_owned(),
            service_instance_id: service_instance_id(),
            node_labels: config.labels.clone(),
        })?;
        telemetry.install_subscriber();
        let feature_flags = config
            .feature_flags_builder(orchestrator::server::app::COMMIT)
            .connect()
            .await
            .map_err(|e| format!("failed to create feature flags client: {e}"))?;
        orchestrator::featureflags::apply_test_overrides(&feature_flags, |name| std::env::var(name).ok());
        let config = config.with_feature_flags(feature_flags.clone());
        orchestrator::sandbox_logs::install_external(
            &config.logs_collector,
            feature_flags.clone(),
            &service_name(&config.services),
        );
        if config.network_version == 2 {
            orchestrator::network::prerequisites::require_v2_prerequisites()?;
        }
        orchestrator::exposure::use_transport(config.rootfs_exposure)?;
        orchestrator::volumes::check_support().map_err(|e| format!("volume confinement prerequisites not met: {e}"))?;
        let node = Arc::new(Node::new(&config)?);
        let _gauges = NodeGauges::register(NodeSource {
            sandboxes: {
                let node = node.clone();
                Arc::new(move || node.registry.len())
            },
            allocated: {
                let node = node.clone();
                Arc::new(move || node.allocated())
            },
            max_sandboxes: node.max_sandboxes.clone(),
            status: node.status.clone(),
            version: orchestrator::version().to_owned(),
            commit: orchestrator::server::app::COMMIT.to_owned(),
        });
        let _sandbox_observer = SandboxObserver::start(
            &telemetry.sandbox_meter(),
            {
                let node = node.clone();
                Arc::new(move || running(&node))
            },
            telemetry.sandbox_export(),
        );
        let mut event_sinks: Vec<Arc<dyn EventSink>> = Vec::new();
        match &config.redis {
            Some(settings) => {
                let redis = Redis::connect(settings).await?;
                node.routing.install(Arc::new(redis.routing_catalog()));
                if let Some(address) = config.node_address() {
                    node.peers
                        .install(Arc::new(redis.peer_registry(address.clone())), address);
                }
                node.uploads.signal_through(Arc::new(redis.upload_done()));
                event_sinks.push(Arc::new(redis.event_stream()));
            }
            None => tracing::warn!("redis disabled; orchestrator sandbox routing records are not published"),
        }
        let clickhouse = config
            .clickhouse
            .as_ref()
            .map(|primary| Clickhouse::open(primary, &config.clickhouse_additional))
            .transpose()?
            .map(|clickhouse| (Arc::new(clickhouse.events), Arc::new(clickhouse.host_stats)));
        if let Some((events, host_stats)) = &clickhouse {
            event_sinks.push(events.clone());
            let _ = node.host_stats.set(host_stats.clone());
        }
        node.events.install(event_sinks);
        node.sweep().await;
        let setup = config.open_build_storage()?.map(|build| TemplateManagerSetup {
            build,
            builder: config.builder.clone(),
            development: config.development,
        });
        let app = App::new(node.clone(), setup)?;
        app.start_background();
        let address = SocketAddr::from(([0, 0, 0, 0], config.grpc_port));
        let listener = tokio::net::TcpListener::bind(address)
            .await
            .map_err(|e| format!("listen on {address}: {e}"))?;
        tracing::info!(%address, node = %config.node_id, "orchestrator listening");
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let server = tokio::spawn(orchestrator::server::grpc_serve::serve(
            listener,
            app.router(),
            async move { drop(stopped.await) },
        ));
        let lookup_node = node.clone();
        let hyperloop = orchestrator::hyperloop::server::router(
            Arc::new(move |ip| lookup_node.source_of(ip)),
            feature_flags.clone(),
            config.logs_collector.clone(),
        );
        let hyperloop_address = SocketAddr::from(([0, 0, 0, 0], config.hyperloop_port));
        let hyperloop_listener = tokio::net::TcpListener::bind(hyperloop_address)
            .await
            .map_err(|e| format!("listen on {hyperloop_address}: {e}"))?
            .tap_io(no_delay);
        tokio::spawn(
            axum::serve(
                hyperloop_listener,
                hyperloop.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .into_future(),
        );
        let settings = NetworkSettings::current();
        if !config.volume_mounts.is_empty() {
            start_nfs_proxy(&node, &config, settings).await?;
        }
        let firewall_node = node.clone();
        orchestrator::tcpfirewall::proxy::TcpFirewall::new(
            Arc::new(move |ip| firewall_node.firewall_source(ip)),
            settings,
        )
        .start(settings)
        .await?;
        let flags = feature_flags.clone();
        let sandbox_proxy = Arc::new(SandboxProxy::new(
            orchestrator::proxy::sandbox::node::lookup_in(node.clone()),
            Arc::new(move || flags.get(&SANDBOX_MAX_INCOMING_CONNECTIONS, &[])),
        ));
        let proxy_address = SocketAddr::from(([0, 0, 0, 0], config.proxy_port));
        let proxy_listener = tokio::net::TcpListener::bind(proxy_address)
            .await
            .map_err(|e| format!("listen on {proxy_address}: {e}"))?;
        drop(node.proxy.set(sandbox_proxy.clone()));
        let serving_proxy = sandbox_proxy.clone();
        tokio::spawn(async move {
            if let Err(error) = serving_proxy.serve(proxy_listener).await {
                tracing::error!(%error, "sandbox proxy stopped");
            }
        });
        let mut signals = Signals::new()?;
        signals.next().await;
        tracing::info!("Shutdown signal received");
        node.status.shut_down();
        if !config.local {
            tokio::time::sleep(CONSUMER_GRACE).await;
        }
        if let Err(e) = app.wait_for_templates(config.force_stop, config.local).await {
            tracing::error!(error = %e, "error while waiting for template manager to drain");
        }
        if !config.force_stop {
            tracing::info!(
                sandbox_count = node.registry.running().len(),
                "Starting sandbox drain phase"
            );
            drain(|| node.registry.len(), std::future::pending()).await;
            node.shutdown().await;
        }
        let _ = stop.send(());
        let served = server.await.map_err(|e| e.to_string());
        if let Err(error) = sandbox_proxy.close(config.force_stop).await {
            tracing::error!(%error, "sandbox proxy shutdown failed");
        }
        match config.force_stop {
            true => node.close_forced().await,
            false => node.close().await,
        }
        feature_flags.close().await;
        if let Some((events, host_stats)) = &clickhouse {
            tokio::join!(events.stop(), host_stats.stop());
        }
        served
    }

    async fn start_nfs_proxy(node: &Arc<Node>, config: &NodeConfig, settings: &NetworkSettings) -> Result<(), String> {
        let portmap = listen(settings.portmapper_port)
            .await
            .map_err(|e| format!("failed to listen on portmapper port: {e}"))?;
        let nfs = listen(settings.nfs_proxy_port)
            .await
            .map_err(|e| format!("failed to listen on nfs port: {e}"))?;
        let mounting = node.clone();
        let live = node.clone();
        let proxy = NfsProxy::new(
            Arc::new(move |ip| mounting.mounting_sandbox(ip)),
            Arc::new(move || live.live_lifecycles()),
            config.volume_mounts.clone(),
        );
        tokio::spawn(proxy.clone().serve_portmap(portmap));
        tokio::spawn(proxy.serve(nfs));
        Ok(())
    }

    async fn listen(port: u16) -> std::io::Result<tokio::net::TcpListener> {
        tokio::net::TcpListener::bind(SocketAddr::from(([0, 0, 0, 0], port))).await
    }

    fn no_delay(tcp: &mut TcpStream) {
        if let Err(e) = tcp.set_nodelay(true) {
            tracing::warn!(error = %e, "failed to set TCP_NODELAY on an incoming connection");
        }
    }

    struct Signals {
        terminate: Signal,
    }

    impl Signals {
        fn new() -> Result<Self, String> {
            signal(SignalKind::terminate())
                .map(|terminate| Self { terminate })
                .map_err(|e| format!("listen for SIGTERM: {e}"))
        }

        async fn next(&mut self) {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = self.terminate.recv() => {}
            }
        }
    }
}
