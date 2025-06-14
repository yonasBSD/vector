#![allow(missing_docs)]
use std::{
    num::{NonZeroU64, NonZeroUsize},
    path::PathBuf,
    process::ExitStatus,
    sync::atomic::{AtomicUsize, Ordering},
    time::Duration,
};

use exitcode::ExitCode;
use futures::StreamExt;
use tokio::runtime::{self, Runtime};
use tokio::sync::{broadcast::error::RecvError, MutexGuard};
use tokio_stream::wrappers::UnboundedReceiverStream;

use crate::extra_context::ExtraContext;
#[cfg(feature = "api")]
use crate::{api, internal_events::ApiStarted};
use crate::{
    cli::{handle_config_errors, LogFormat, Opts, RootOpts, WatchConfigMethod},
    config::{self, ComponentConfig, Config, ConfigPath},
    heartbeat,
    internal_events::{VectorConfigLoadError, VectorQuit, VectorStarted, VectorStopped},
    signal::{SignalHandler, SignalPair, SignalRx, SignalTo},
    topology::{
        ReloadOutcome, RunningTopology, SharedTopologyController, ShutdownErrorReceiver,
        TopologyController,
    },
    trace,
};

#[cfg(unix)]
use std::os::unix::process::ExitStatusExt;
#[cfg(windows)]
use std::os::windows::process::ExitStatusExt;
use tokio::runtime::Handle;

static WORKER_THREADS: AtomicUsize = AtomicUsize::new(0);

pub fn worker_threads() -> Option<NonZeroUsize> {
    NonZeroUsize::new(WORKER_THREADS.load(Ordering::Relaxed))
}

pub struct ApplicationConfig {
    pub config_paths: Vec<config::ConfigPath>,
    pub topology: RunningTopology,
    pub graceful_crash_receiver: ShutdownErrorReceiver,
    pub internal_topologies: Vec<RunningTopology>,
    #[cfg(feature = "api")]
    pub api: config::api::Options,
    pub extra_context: ExtraContext,
}

pub struct Application {
    pub root_opts: RootOpts,
    pub config: ApplicationConfig,
    pub signals: SignalPair,
}

impl ApplicationConfig {
    pub async fn from_opts(
        opts: &RootOpts,
        signal_handler: &mut SignalHandler,
        extra_context: ExtraContext,
    ) -> Result<Self, ExitCode> {
        let config_paths = opts.config_paths_with_formats();

        let graceful_shutdown_duration = (!opts.no_graceful_shutdown_limit)
            .then(|| Duration::from_secs(u64::from(opts.graceful_shutdown_limit_secs)));

        let watcher_conf = if opts.watch_config {
            Some(watcher_config(
                opts.watch_config_method,
                opts.watch_config_poll_interval_seconds,
            ))
        } else {
            None
        };

        let config = load_configs(
            &config_paths,
            watcher_conf,
            opts.require_healthy,
            opts.allow_empty_config,
            graceful_shutdown_duration,
            signal_handler,
        )
        .await?;

        Self::from_config(config_paths, config, extra_context).await
    }

    pub async fn from_config(
        config_paths: Vec<ConfigPath>,
        config: Config,
        extra_context: ExtraContext,
    ) -> Result<Self, ExitCode> {
        #[cfg(feature = "api")]
        let api = config.api;

        let (topology, graceful_crash_receiver) =
            RunningTopology::start_init_validated(config, extra_context.clone())
                .await
                .ok_or(exitcode::CONFIG)?;

        Ok(Self {
            config_paths,
            topology,
            graceful_crash_receiver,
            internal_topologies: Vec::new(),
            #[cfg(feature = "api")]
            api,
            extra_context,
        })
    }

    pub async fn add_internal_config(
        &mut self,
        config: Config,
        extra_context: ExtraContext,
    ) -> Result<(), ExitCode> {
        let Some((topology, _)) =
            RunningTopology::start_init_validated(config, extra_context).await
        else {
            return Err(exitcode::CONFIG);
        };
        self.internal_topologies.push(topology);
        Ok(())
    }

    /// Configure the API server, if applicable
    #[cfg(feature = "api")]
    pub fn setup_api(&self, handle: &Handle) -> Option<api::Server> {
        if self.api.enabled {
            match api::Server::start(
                self.topology.config(),
                self.topology.watch(),
                std::sync::Arc::clone(&self.topology.running),
                handle,
            ) {
                Ok(api_server) => {
                    emit!(ApiStarted {
                        addr: self.api.address.unwrap(),
                        playground: self.api.playground,
                        graphql: self.api.graphql
                    });

                    Some(api_server)
                }
                Err(error) => {
                    let error = error.to_string();
                    error!("An error occurred that Vector couldn't handle: {}.", error);
                    _ = self
                        .topology
                        .abort_tx
                        .send(crate::signal::ShutdownError::ApiFailed { error });
                    None
                }
            }
        } else {
            info!(message="API is disabled, enable by setting `api.enabled` to `true` and use commands like `vector top`.");
            None
        }
    }
}

impl Application {
    pub fn run(extra_context: ExtraContext) -> ExitStatus {
        let (runtime, app) =
            Self::prepare_start(extra_context).unwrap_or_else(|code| std::process::exit(code));

        runtime.block_on(app.run())
    }

    pub fn prepare_start(
        extra_context: ExtraContext,
    ) -> Result<(Runtime, StartedApplication), ExitCode> {
        Self::prepare(extra_context)
            .and_then(|(runtime, app)| app.start(runtime.handle()).map(|app| (runtime, app)))
    }

    pub fn prepare(extra_context: ExtraContext) -> Result<(Runtime, Self), ExitCode> {
        let opts = Opts::get_matches().map_err(|error| {
            // Printing to stdout/err can itself fail; ignore it.
            _ = error.print();
            exitcode::USAGE
        })?;

        Self::prepare_from_opts(opts, extra_context)
    }

    pub fn prepare_from_opts(
        opts: Opts,
        extra_context: ExtraContext,
    ) -> Result<(Runtime, Self), ExitCode> {
        opts.root.init_global();

        let color = opts.root.color.use_color();

        init_logging(
            color,
            opts.root.log_format,
            opts.log_level(),
            opts.root.internal_log_rate_limit,
        );

        // Can only log this after initializing the logging subsystem
        if opts.root.openssl_no_probe {
            debug!(message = "Disabled probing and configuration of root certificate locations on the system for OpenSSL.");
        }

        let runtime = build_runtime(opts.root.threads, "vector-worker")?;

        // Signal handler for OS and provider messages.
        let mut signals = SignalPair::new(&runtime);

        if let Some(sub_command) = &opts.sub_command {
            return Err(runtime.block_on(sub_command.execute(signals, color)));
        }

        let config = runtime.block_on(ApplicationConfig::from_opts(
            &opts.root,
            &mut signals.handler,
            extra_context,
        ))?;

        Ok((
            runtime,
            Self {
                root_opts: opts.root,
                config,
                signals,
            },
        ))
    }

    pub fn start(self, handle: &Handle) -> Result<StartedApplication, ExitCode> {
        // Any internal_logs sources will have grabbed a copy of the
        // early buffer by this point and set up a subscriber.
        crate::trace::stop_early_buffering();

        emit!(VectorStarted);
        handle.spawn(heartbeat::heartbeat());

        let Self {
            root_opts,
            config,
            signals,
        } = self;

        let topology_controller = SharedTopologyController::new(TopologyController {
            #[cfg(feature = "api")]
            api_server: config.setup_api(handle),
            topology: config.topology,
            config_paths: config.config_paths.clone(),
            require_healthy: root_opts.require_healthy,
            extra_context: config.extra_context,
        });

        Ok(StartedApplication {
            config_paths: config.config_paths,
            internal_topologies: config.internal_topologies,
            graceful_crash_receiver: config.graceful_crash_receiver,
            signals,
            topology_controller,
            allow_empty_config: root_opts.allow_empty_config,
        })
    }
}

pub struct StartedApplication {
    pub config_paths: Vec<ConfigPath>,
    pub internal_topologies: Vec<RunningTopology>,
    pub graceful_crash_receiver: ShutdownErrorReceiver,
    pub signals: SignalPair,
    pub topology_controller: SharedTopologyController,
    pub allow_empty_config: bool,
}

impl StartedApplication {
    pub async fn run(self) -> ExitStatus {
        self.main().await.shutdown().await
    }

    pub async fn main(self) -> FinishedApplication {
        let Self {
            config_paths,
            graceful_crash_receiver,
            signals,
            topology_controller,
            internal_topologies,
            allow_empty_config,
        } = self;

        let mut graceful_crash = UnboundedReceiverStream::new(graceful_crash_receiver);

        let mut signal_handler = signals.handler;
        let mut signal_rx = signals.receiver;

        let signal = loop {
            let has_sources = !topology_controller.lock().await.topology.config.is_empty();
            tokio::select! {
                signal = signal_rx.recv() => if let Some(signal) = handle_signal(
                    signal,
                    &topology_controller,
                    &config_paths,
                    &mut signal_handler,
                    allow_empty_config,
                ).await {
                    break signal;
                },
                // Trigger graceful shutdown if a component crashed, or all sources have ended.
                error = graceful_crash.next() => break SignalTo::Shutdown(error),
                _ = TopologyController::sources_finished(topology_controller.clone()), if has_sources => {
                    info!("All sources have finished.");
                    break SignalTo::Shutdown(None)
                } ,
                else => unreachable!("Signal streams never end"),
            }
        };

        FinishedApplication {
            signal,
            signal_rx,
            topology_controller,
            internal_topologies,
        }
    }
}

async fn handle_signal(
    signal: Result<SignalTo, RecvError>,
    topology_controller: &SharedTopologyController,
    config_paths: &[ConfigPath],
    signal_handler: &mut SignalHandler,
    allow_empty_config: bool,
) -> Option<SignalTo> {
    match signal {
        Ok(SignalTo::ReloadComponents(components_to_reload)) => {
            let mut topology_controller = topology_controller.lock().await;
            topology_controller
                .topology
                .extend_reload_set(components_to_reload);

            // Reload paths
            if let Some(paths) = config::process_paths(config_paths) {
                topology_controller.config_paths = paths;
            }

            // Reload config
            let new_config = config::load_from_paths_with_provider_and_secrets(
                &topology_controller.config_paths,
                signal_handler,
                allow_empty_config,
            )
            .await;

            reload_config_from_result(topology_controller, new_config).await
        }
        Ok(SignalTo::ReloadFromConfigBuilder(config_builder)) => {
            let topology_controller = topology_controller.lock().await;
            reload_config_from_result(topology_controller, config_builder.build()).await
        }
        Ok(SignalTo::ReloadFromDisk) => {
            let mut topology_controller = topology_controller.lock().await;

            // Reload paths
            if let Some(paths) = config::process_paths(config_paths) {
                topology_controller.config_paths = paths;
            }

            // Reload config
            let new_config = config::load_from_paths_with_provider_and_secrets(
                &topology_controller.config_paths,
                signal_handler,
                allow_empty_config,
            )
            .await;

            reload_config_from_result(topology_controller, new_config).await
        }
        Err(RecvError::Lagged(amt)) => {
            warn!("Overflow, dropped {} signals.", amt);
            None
        }
        Err(RecvError::Closed) => Some(SignalTo::Shutdown(None)),
        Ok(signal) => Some(signal),
    }
}

async fn reload_config_from_result(
    mut topology_controller: MutexGuard<'_, TopologyController>,
    config: Result<Config, Vec<String>>,
) -> Option<SignalTo> {
    match config {
        Ok(new_config) => match topology_controller.reload(new_config).await {
            ReloadOutcome::FatalError(error) => Some(SignalTo::Shutdown(Some(error))),
            _ => None,
        },
        Err(errors) => {
            handle_config_errors(errors);
            emit!(VectorConfigLoadError);
            None
        }
    }
}

pub struct FinishedApplication {
    pub signal: SignalTo,
    pub signal_rx: SignalRx,
    pub topology_controller: SharedTopologyController,
    pub internal_topologies: Vec<RunningTopology>,
}

impl FinishedApplication {
    pub async fn shutdown(self) -> ExitStatus {
        let FinishedApplication {
            signal,
            signal_rx,
            topology_controller,
            internal_topologies,
        } = self;

        // At this point, we'll have the only reference to the shared topology controller and can
        // safely remove it from the wrapper to shut down the topology.
        let topology_controller = topology_controller
            .try_into_inner()
            .expect("fail to unwrap topology controller")
            .into_inner();

        let status = match signal {
            SignalTo::Shutdown(_) => Self::stop(topology_controller, signal_rx).await,
            SignalTo::Quit => Self::quit(),
            _ => unreachable!(),
        };

        for topology in internal_topologies {
            topology.stop().await;
        }

        status
    }

    async fn stop(topology_controller: TopologyController, mut signal_rx: SignalRx) -> ExitStatus {
        emit!(VectorStopped);
        tokio::select! {
            _ = topology_controller.stop() => ExitStatus::from_raw({
                #[cfg(windows)]
                {
                    exitcode::OK as u32
                }
                #[cfg(unix)]
                exitcode::OK
            }), // Graceful shutdown finished
            _ = signal_rx.recv() => Self::quit(),
        }
    }

    fn quit() -> ExitStatus {
        // It is highly unlikely that this event will exit from topology.
        emit!(VectorQuit);
        ExitStatus::from_raw({
            #[cfg(windows)]
            {
                exitcode::UNAVAILABLE as u32
            }
            #[cfg(unix)]
            exitcode::OK
        })
    }
}

fn get_log_levels(default: &str) -> String {
    std::env::var("VECTOR_LOG")
        .or_else(|_| {
            std::env::var("LOG").inspect(|_log| {
                warn!(
                    message =
                        "DEPRECATED: Use of $LOG is deprecated. Please use $VECTOR_LOG instead."
                );
            })
        })
        .unwrap_or_else(|_| default.into())
}

pub fn build_runtime(threads: Option<usize>, thread_name: &str) -> Result<Runtime, ExitCode> {
    let mut rt_builder = runtime::Builder::new_multi_thread();
    rt_builder.max_blocking_threads(20_000);
    rt_builder.enable_all().thread_name(thread_name);

    let threads = threads.unwrap_or_else(crate::num_threads);
    if threads == 0 {
        error!("The `threads` argument must be greater or equal to 1.");
        return Err(exitcode::CONFIG);
    }
    WORKER_THREADS
        .compare_exchange(0, threads, Ordering::Acquire, Ordering::Relaxed)
        .unwrap_or_else(|_| panic!("double thread initialization"));
    rt_builder.worker_threads(threads);

    debug!(messaged = "Building runtime.", worker_threads = threads);
    Ok(rt_builder.build().expect("Unable to create async runtime"))
}

pub async fn load_configs(
    config_paths: &[ConfigPath],
    watcher_conf: Option<config::watcher::WatcherConfig>,
    require_healthy: Option<bool>,
    allow_empty_config: bool,
    graceful_shutdown_duration: Option<Duration>,
    signal_handler: &mut SignalHandler,
) -> Result<Config, ExitCode> {
    let config_paths = config::process_paths(config_paths).ok_or(exitcode::CONFIG)?;

    let watched_paths = config_paths
        .iter()
        .map(<&PathBuf>::from)
        .collect::<Vec<_>>();

    info!(
        message = "Loading configs.",
        paths = ?watched_paths
    );

    let mut config = config::load_from_paths_with_provider_and_secrets(
        &config_paths,
        signal_handler,
        allow_empty_config,
    )
    .await
    .map_err(handle_config_errors)?;

    let mut watched_component_paths = Vec::new();

    if let Some(watcher_conf) = watcher_conf {
        for (name, transform) in config.transforms() {
            let files = transform.inner.files_to_watch();
            let component_config =
                ComponentConfig::new(files.into_iter().cloned().collect(), name.clone());
            watched_component_paths.push(component_config);
        }

        for (name, sink) in config.sinks() {
            let files = sink.inner.files_to_watch();
            let component_config =
                ComponentConfig::new(files.into_iter().cloned().collect(), name.clone());
            watched_component_paths.push(component_config);
        }

        info!(
            message = "Starting watcher.",
            paths = ?watched_paths
        );
        info!(
            message = "Components to watch.",
            paths = ?watched_component_paths
        );

        // Start listening for config changes.
        config::watcher::spawn_thread(
            watcher_conf,
            signal_handler.clone_tx(),
            watched_paths,
            watched_component_paths,
            None,
        )
        .map_err(|error| {
            error!(message = "Unable to start config watcher.", %error);
            exitcode::CONFIG
        })?;
    }

    config::init_log_schema(config.global.log_schema.clone(), true);
    config::init_telemetry(config.global.telemetry.clone(), true);

    if !config.healthchecks.enabled {
        info!("Health checks are disabled.");
    }
    config.healthchecks.set_require_healthy(require_healthy);
    config.graceful_shutdown_duration = graceful_shutdown_duration;

    Ok(config)
}

pub fn init_logging(color: bool, format: LogFormat, log_level: &str, rate: u64) {
    let level = get_log_levels(log_level);
    let json = match format {
        LogFormat::Text => false,
        LogFormat::Json => true,
    };

    trace::init(color, json, &level, rate);
    debug!(message = "Internal log rate limit configured.",);
    info!(message = "Log level is enabled.", level = ?level);
}

pub fn watcher_config(
    method: WatchConfigMethod,
    interval: NonZeroU64,
) -> config::watcher::WatcherConfig {
    match method {
        WatchConfigMethod::Recommended => config::watcher::WatcherConfig::RecommendedWatcher,
        WatchConfigMethod::Poll => config::watcher::WatcherConfig::PollWatcher(interval.into()),
    }
}
