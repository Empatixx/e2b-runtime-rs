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

    use orchestrator::clickhouse::sinks::Clickhouse;
    use orchestrator::events::EventSink;
    use orchestrator::routing::redis::Redis;
    use orchestrator::server::app::{App, TemplateManagerSetup};
    use orchestrator::server::config::NodeConfig;
    use orchestrator::server::info::service_instance_id;
    use orchestrator::server::node::Node;
    use orchestrator::server::shutdown::drain;
    use orchestrator::telemetry::Telemetry;
    use orchestrator::telemetry::identity::{Identity, service_name};
    use orchestrator::telemetry::server::{NodeGauges, NodeSource};

    const CONSUMER_GRACE: std::time::Duration = std::time::Duration::from_secs(15);
    use tokio::signal::unix::{Signal, SignalKind, signal};

    pub async fn run() -> Result<(), String> {
        tracing_subscriber::fmt().init();
        let config = NodeConfig::from_env()?;
        let feature_flags = config
            .feature_flags_builder(orchestrator::server::app::COMMIT)
            .connect()
            .await
            .map_err(|e| format!("failed to create feature flags client: {e}"))?;
        let config = config.with_feature_flags(feature_flags.clone());
        let telemetry = Telemetry::start(&Identity {
            node_id: config.node_id.clone(),
            service_name: service_name(&config.services),
            service_commit: orchestrator::server::app::COMMIT.to_owned(),
            service_version: orchestrator::version().to_owned(),
            service_instance_id: service_instance_id(),
            node_labels: config.labels.clone(),
        })?;
        if config.network_version == 2 {
            orchestrator::network::prerequisites::require_v2_prerequisites()?;
        }
        let node = Arc::new(Node::new(&config)?);
        let _gauges = NodeGauges::register(NodeSource {
            running: Arc::new({
                let node = node.clone();
                move || node.running_configs()
            }),
            max_sandboxes: node.max_sandboxes.clone(),
            status: node.status.clone(),
            version: orchestrator::version().to_owned(),
            commit: orchestrator::server::app::COMMIT.to_owned(),
        });
        let mut event_sinks: Vec<Arc<dyn EventSink>> = Vec::new();
        match &config.redis {
            Some(settings) => {
                let redis = Redis::connect(settings).await?;
                node.routing.install(Arc::new(redis.routing_catalog()));
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
        let server = tokio::spawn(
            axum::serve(listener, app.router())
                .with_graceful_shutdown(async move { drop(stopped.await) })
                .into_future(),
        );
        let lookup_node = node.clone();
        let hyperloop = orchestrator::hyperloop::server::router(
            Arc::new(move |ip| lookup_node.source_of(ip)),
            config.logs_collector.clone(),
        );
        let hyperloop_address = SocketAddr::from(([0, 0, 0, 0], config.hyperloop_port));
        let hyperloop_listener = tokio::net::TcpListener::bind(hyperloop_address)
            .await
            .map_err(|e| format!("listen on {hyperloop_address}: {e}"))?;
        tokio::spawn(
            axum::serve(
                hyperloop_listener,
                hyperloop.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .into_future(),
        );
        let firewall_node = node.clone();
        let settings = orchestrator::network::settings::NetworkSettings::current();
        orchestrator::tcpfirewall::proxy::TcpFirewall::new(
            Arc::new(move |ip| firewall_node.firewall_source(ip)),
            settings,
        )
        .start(settings)
        .await?;
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
            drain(|| node.registry.is_empty(), std::future::pending()).await;
            node.shutdown().await;
        }
        let _ = stop.send(());
        let served = server.await.map_err(|e| e.to_string())?.map_err(|e| e.to_string());
        match config.force_stop {
            true => node.close_forced().await,
            false => node.close().await,
        }
        feature_flags.close().await;
        if let Some((events, host_stats)) = &clickhouse {
            tokio::join!(events.stop(), host_stats.stop());
        }
        if let Err(e) = telemetry.shutdown() {
            tracing::error!(error = %e, "error while shutting down telemetry");
        }
        served
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
