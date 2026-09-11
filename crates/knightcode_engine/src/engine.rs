//! The engine's lifetime as a gpui entity: one per application, started at
//! launch, restarted with backoff, released at quit. Everything that needs
//! the engine asks this entity for an `Endpoint`; nothing else spawns it.

use crate::client::{Endpoint, EngineClient, EngineEvent as WireEvent};
use crate::environment::{self, TOKEN_ENV};
use crate::process::{self, EngineCommand, EngineProcess, ExitOutcome, STALL_TIMEOUT, StartError};
use crate::settings::EngineSettings;
use anyhow::{Result, anyhow};
use futures::{
    StreamExt as _,
    channel::{mpsc, oneshot},
};
use gpui::{App, AppContext as _, Context, Entity, EventEmitter, Global, SharedString, Task};
use http_client::HttpClient;
use settings::{Settings as _, SettingsStore};
use std::{
    collections::VecDeque,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

const RESTART_WINDOW: Duration = Duration::from_secs(60);
const RESTART_LIMIT: usize = 3;
const EVENTS_RETRY: Duration = Duration::from_secs(1);

#[derive(Clone, Debug)]
pub enum EngineStatus {
    Starting,
    Ready(Endpoint),
    Failed(SharedString),
}

#[derive(Clone, Debug)]
pub enum EngineEvent {
    Ready(Endpoint),
    Failed(SharedString),
    Stopped,
    AccountChanged {
        provider_id: String,
        authenticated: bool,
    },
    ModelsChanged,
}

struct GlobalEngine(Entity<Engine>);

impl Global for GlobalEngine {}

pub struct Engine {
    status: EngineStatus,
    process: Option<EngineProcess>,
    binary: Option<PathBuf>,
    token: Arc<str>,
    pinned_port: Option<u16>,
    exits: VecDeque<Instant>,
    proxy: Vec<(String, String)>,
    http: Arc<dyn HttpClient>,
    settings: EngineSettings,
    /// Incremented on every start; a task from an older generation must not act.
    generation: usize,
    waiters: Vec<oneshot::Sender<Result<Endpoint, SharedString>>>,
    _tasks: Vec<Task<()>>,
}

impl EventEmitter<EngineEvent> for Engine {}

/// The delay before the next restart, or `None` once `RESTART_LIMIT` exits
/// fall inside `RESTART_WINDOW`. Exits older than the window are forgotten.
pub fn restart_delay(exits: &VecDeque<Instant>, now: Instant) -> Option<Duration> {
    let recent = exits
        .iter()
        .filter(|at| now.duration_since(**at) < RESTART_WINDOW)
        .count();
    (recent < RESTART_LIMIT).then(|| Duration::from_secs(1u64 << recent))
}

/// `shell_env_loaded` is `main`'s signal that the login shell's environment
/// has been applied to the process; the engine inherits `PATH` from it, so
/// the first start waits. `None` when there is nothing to wait for.
pub fn init(
    proxy: Vec<(String, String)>,
    shell_env_loaded: Option<oneshot::Receiver<()>>,
    cx: &mut App,
) -> Entity<Engine> {
    EngineSettings::register(cx);
    let engine = cx.new(|cx| Engine::new(proxy, cx));
    cx.set_global(GlobalEngine(engine.clone()));
    cx.on_app_quit({
        let engine = engine.clone();
        move |cx| {
            engine.update(cx, |engine, _| engine.release());
            async {}
        }
    })
    .detach();
    match shell_env_loaded {
        Some(loaded) => cx
            .spawn({
                let engine = engine.clone();
                async move |cx| {
                    loaded.await.ok();
                    engine.update(cx, |engine, cx| engine.start(cx));
                }
            })
            .detach(),
        None => engine.update(cx, |engine, cx| engine.start(cx)),
    }
    engine
}

pub fn global(cx: &App) -> Entity<Engine> {
    cx.global::<GlobalEngine>().0.clone()
}

pub fn try_global(cx: &App) -> Option<Entity<Engine>> {
    cx.try_global::<GlobalEngine>()
        .map(|global| global.0.clone())
}

impl Engine {
    fn new(proxy: Vec<(String, String)>, cx: &mut Context<Self>) -> Self {
        let settings = EngineSettings::get_global(cx).clone();
        cx.observe_global::<SettingsStore>(|this, cx| {
            let settings = EngineSettings::get_global(cx).clone();
            if settings.engine_path != this.settings.engine_path
                || settings.engine_url != this.settings.engine_url
            {
                this.settings = settings;
                this.restart(cx);
            } else {
                this.settings = settings;
            }
        })
        .detach();
        Self {
            status: EngineStatus::Starting,
            process: None,
            binary: None,
            token: environment::generate_token().into(),
            pinned_port: None,
            exits: VecDeque::new(),
            proxy,
            http: cx.http_client(),
            settings,
            generation: 0,
            waiters: Vec::new(),
            _tasks: Vec::new(),
        }
    }

    pub fn status(&self) -> &EngineStatus {
        &self.status
    }

    pub fn endpoint(&self) -> Option<Endpoint> {
        match &self.status {
            EngineStatus::Ready(endpoint) => Some(endpoint.clone()),
            _ => None,
        }
    }

    pub fn client(&self) -> Option<EngineClient> {
        self.endpoint()
            .map(|endpoint| EngineClient::new(self.http.clone(), endpoint))
    }

    /// The binary the running engine was started from; what the ACP adapter
    /// is spawned from too.
    pub fn binary(&self) -> Option<&Path> {
        self.binary.as_deref()
    }

    /// Resolves once the engine is `Ready`, or fails with the `Failed` message.
    pub fn ready(&mut self, cx: &mut Context<Self>) -> Task<Result<Endpoint>> {
        match &self.status {
            EngineStatus::Ready(endpoint) => Task::ready(Ok(endpoint.clone())),
            EngineStatus::Failed(message) => Task::ready(Err(anyhow!("{message}"))),
            EngineStatus::Starting => {
                let (tx, rx) = oneshot::channel();
                self.waiters.push(tx);
                cx.background_spawn(async move {
                    rx.await
                        .map_err(|_| anyhow!("the engine was dropped"))?
                        .map_err(|message| anyhow!("{message}"))
                })
            }
        }
    }

    /// Close the engine's stdin. It shuts down on its own; this is what a
    /// quit does within gpui's 200 ms.
    pub fn release(&mut self) {
        if let Some(process) = &mut self.process {
            process.release();
        }
    }

    pub fn restart(&mut self, cx: &mut Context<Self>) {
        self.generation += 1;
        self.exits.clear();
        if let Some(process) = self.process.take() {
            self.pinned_port = Some(process.port);
            cx.background_spawn(async move {
                process.stop().await;
            })
            .detach();
        }
        self.start(cx);
    }

    fn set_status(&mut self, status: EngineStatus, cx: &mut Context<Self>) {
        self.status = status.clone();
        let event = match &status {
            EngineStatus::Ready(endpoint) => Some(EngineEvent::Ready(endpoint.clone())),
            EngineStatus::Failed(message) => Some(EngineEvent::Failed(message.clone())),
            EngineStatus::Starting => None,
        };
        let outcome = match &status {
            EngineStatus::Ready(endpoint) => Some(Ok(endpoint.clone())),
            EngineStatus::Failed(message) => Some(Err(message.clone())),
            EngineStatus::Starting => None,
        };
        if let Some(outcome) = outcome {
            for waiter in self.waiters.drain(..) {
                waiter.send(outcome.clone()).ok();
            }
        }
        if let Some(event) = event {
            cx.emit(event);
        }
        cx.notify();
    }

    fn start(&mut self, cx: &mut Context<Self>) {
        self.generation += 1;
        let generation = self.generation;
        self.set_status(EngineStatus::Starting, cx);

        if let Some(url) = self.settings.engine_url.clone() {
            // Attach: no process, no restart; the token must already be in the
            // environment. Readiness is the token check alone.
            let Ok(token) = std::env::var(TOKEN_ENV) else {
                self.set_status(
                    EngineStatus::Failed(
                        format!(
                            "knightcode.engine_url is set but {TOKEN_ENV} is not in the environment"
                        )
                        .into(),
                    ),
                    cx,
                );
                return;
            };
            let endpoint = Endpoint {
                url: url.trim_end_matches('/').to_owned(),
                token: token.into(),
            };
            let client = EngineClient::new(self.http.clone(), endpoint.clone());
            self._tasks.push(cx.spawn(async move |this, cx| {
                let outcome = client.accounts().await;
                this.update(cx, |this, cx| {
                    if this.generation != generation {
                        return;
                    }
                    match outcome {
                        Ok(_) => {
                            this.set_status(EngineStatus::Ready(endpoint), cx);
                            this.watch_events(generation, cx);
                        }
                        Err(error) => this.set_status(
                            EngineStatus::Failed(
                                format!("could not reach the engine at {url}: {error}").into(),
                            ),
                            cx,
                        ),
                    }
                })
                .ok();
            }));
            return;
        }

        let binary = match environment::locate_binary(
            self.settings.engine_path.as_deref(),
            std::env::var_os(environment::PATH_ENV)
                .map(PathBuf::from)
                .as_deref(),
            std::env::current_exe()
                .ok()
                .and_then(|exe| exe.parent().map(Path::to_path_buf))
                .as_deref(),
        ) {
            Ok(binary) => binary,
            Err(error) => {
                self.set_status(EngineStatus::Failed(error.to_string().into()), cx);
                return;
            }
        };
        self.binary = Some(binary.clone());
        let (set, remove) =
            environment::engine_environment(&self.token, self.pinned_port, &self.proxy);
        let command = EngineCommand {
            program: binary,
            args: Vec::new(),
            set,
            remove,
        };
        let token = self.token.clone();
        let http = self.http.clone();
        let pinned = self.pinned_port;

        self._tasks.push(cx.spawn(async move |this, cx| {
            let outcome = process::start(command, &token, http, STALL_TIMEOUT).await;
            this.update(cx, |this, cx| {
                if this.generation != generation {
                    // A restart superseded this start; its process is not ours to keep.
                    if let Ok(process) = outcome {
                        cx.background_spawn(async move {
                            process.stop().await;
                        })
                        .detach();
                    }
                    return;
                }
                match outcome {
                    Ok(process) => {
                        let endpoint = Endpoint {
                            url: format!("http://127.0.0.1:{}", process.port),
                            token: token.clone(),
                        };
                        let exit = process.exit();
                        this.process = Some(process);
                        this.set_status(EngineStatus::Ready(endpoint), cx);
                        this.watch_exit(generation, exit, cx);
                        this.watch_events(generation, cx);
                    }
                    Err(StartError::ExitedBeforeReady { stderr, .. })
                        if pinned.is_some() && stderr.contains("EADDRINUSE") =>
                    {
                        // The old port is gone for good; the adapters will not find us, but the IDE will.
                        log::warn!(
                            "knightcode-engine: pinned port in use; starting on an ephemeral port"
                        );
                        this.pinned_port = None;
                        this.start(cx);
                    }
                    Err(error) => {
                        this.set_status(EngineStatus::Failed(error.to_string().into()), cx)
                    }
                }
            })
            .ok();
        }));
    }

    fn watch_exit(
        &mut self,
        generation: usize,
        exit: impl Future<Output = ExitOutcome> + 'static,
        cx: &mut Context<Self>,
    ) {
        self._tasks.push(cx.spawn(async move |this, cx| {
            let outcome = exit.await;
            this.update(cx, |this, cx| this.on_exit(generation, outcome, cx))
                .ok();
        }));
    }

    fn on_exit(&mut self, generation: usize, outcome: ExitOutcome, cx: &mut Context<Self>) {
        if generation != self.generation {
            return;
        }
        let port = self.process.take().map(|process| process.port);
        cx.emit(EngineEvent::Stopped);
        let now = Instant::now();
        let status = outcome
            .status
            .map(|status| status.to_string())
            .unwrap_or_else(|| "unknown status".into());
        match restart_delay(&self.exits, now) {
            Some(delay) => {
                log::warn!("knightcode-engine exited ({status}); restarting in {delay:?}");
                self.exits.push_back(now);
                self.pinned_port = port;
                self.set_status(EngineStatus::Starting, cx);
                self._tasks.push(cx.spawn(async move |this, cx| {
                    cx.background_executor().timer(delay).await;
                    this.update(cx, |this, cx| {
                        if this.generation == generation {
                            this.start(cx);
                        }
                    })
                    .ok();
                }));
            }
            None => {
                let tail = if outcome.stderr.is_empty() {
                    String::new()
                } else {
                    format!(":\n{}", outcome.stderr)
                };
                self.set_status(
                    EngineStatus::Failed(
                        format!(
                            "knightcode-engine exited {RESTART_LIMIT} times within a minute; last exit {status}{tail}"
                        )
                        .into(),
                    ),
                    cx,
                );
            }
        }
    }

    /// Relay `account.changed` and `models.changed` from `/events` as entity
    /// events, reconnecting after a drop for as long as this generation runs.
    fn watch_events(&mut self, generation: usize, cx: &mut Context<Self>) {
        let Some(client) = self.client() else {
            return;
        };
        let (tx, mut rx) = mpsc::unbounded::<WireEvent>();
        let executor = cx.background_executor().clone();
        let reader = cx.background_spawn(async move {
            loop {
                let outcome = client
                    .events(|event| {
                        tx.unbounded_send(event).ok();
                    })
                    .await;
                if tx.is_closed() {
                    return;
                }
                if let Err(error) = outcome {
                    log::warn!("knightcode-engine: event stream dropped: {error}");
                }
                executor.timer(EVENTS_RETRY).await;
            }
        });
        self._tasks.push(cx.spawn(async move |this, cx| {
            let _reader = reader;
            while let Some(event) = rx.next().await {
                let stop = this
                    .update(cx, |this, cx| {
                        if this.generation != generation {
                            return true;
                        }
                        match event {
                            WireEvent::AccountChanged {
                                provider_id,
                                authenticated,
                            } => cx.emit(EngineEvent::AccountChanged {
                                provider_id,
                                authenticated,
                            }),
                            WireEvent::ModelsChanged => cx.emit(EngineEvent::ModelsChanged),
                            WireEvent::Other => {}
                        }
                        false
                    })
                    .unwrap_or(true);
                if stop {
                    break;
                }
            }
        }));
    }
}

#[cfg(any(test, feature = "test-support"))]
impl Engine {
    /// Every test constructor also installs the global, because `connect`
    /// and the provider reach the engine through `global(cx)`.
    fn for_tests(status: EngineStatus, cx: &mut App) -> Entity<Engine> {
        let http = cx.http_client();
        let engine = cx.new(|_| Engine {
            status,
            process: None,
            binary: None,
            token: "test-token".into(),
            pinned_port: None,
            exits: VecDeque::new(),
            proxy: Vec::new(),
            http,
            settings: EngineSettings::default(),
            generation: 0,
            waiters: Vec::new(),
            _tasks: Vec::new(),
        });
        cx.set_global(GlobalEngine(engine.clone()));
        engine
    }

    pub fn failed_for_tests(message: &str, cx: &mut App) -> Entity<Engine> {
        Self::for_tests(EngineStatus::Failed(message.to_owned().into()), cx)
    }

    pub fn starting_for_tests(cx: &mut App) -> Entity<Engine> {
        Self::for_tests(EngineStatus::Starting, cx)
    }

    pub fn ready_for_tests(
        endpoint: Endpoint,
        http: Arc<dyn HttpClient>,
        cx: &mut App,
    ) -> Entity<Engine> {
        let engine = Self::for_tests(EngineStatus::Ready(endpoint), cx);
        engine.update(cx, |engine, _| engine.http = http);
        engine
    }

    pub fn set_ready_for_tests(&mut self, endpoint: Endpoint, cx: &mut Context<Self>) {
        self.set_status(EngineStatus::Ready(endpoint), cx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[test]
    fn three_exits_in_a_minute_stop_the_restarts() {
        let now = Instant::now();
        let mut exits = VecDeque::new();
        assert_eq!(restart_delay(&exits, now), Some(Duration::from_secs(1)));
        exits.push_back(now - Duration::from_secs(50));
        assert_eq!(restart_delay(&exits, now), Some(Duration::from_secs(2)));
        exits.push_back(now - Duration::from_secs(20));
        assert_eq!(restart_delay(&exits, now), Some(Duration::from_secs(4)));
        exits.push_back(now - Duration::from_secs(5));
        assert_eq!(restart_delay(&exits, now), None);
        // Older than a minute no longer counts.
        let mut old = VecDeque::from([
            now - Duration::from_secs(120),
            now - Duration::from_secs(90),
            now - Duration::from_secs(61),
        ]);
        assert_eq!(restart_delay(&old, now), Some(Duration::from_secs(1)));
        old.push_back(now);
        assert_eq!(restart_delay(&old, now), Some(Duration::from_secs(2)));
    }

    #[gpui::test]
    async fn ready_surfaces_a_failed_engine_verbatim(cx: &mut TestAppContext) {
        let engine = cx.update(|cx| {
            Engine::failed_for_tests("knightcode-engine was not found at C:/nope", cx)
        });
        let error = cx
            .update(|cx| engine.update(cx, |engine, cx| engine.ready(cx)))
            .await
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "knightcode-engine was not found at C:/nope"
        );
        assert!(cx.read(|cx| engine.read(cx).endpoint()).is_none());
    }

    #[gpui::test]
    async fn ready_resolves_when_the_engine_becomes_ready(cx: &mut TestAppContext) {
        let engine = cx.update(|cx| Engine::starting_for_tests(cx));
        let waiting = cx.update(|cx| engine.update(cx, |engine, cx| engine.ready(cx)));
        let endpoint = Endpoint {
            url: "http://127.0.0.1:4545".into(),
            token: "t".into(),
        };
        cx.update(|cx| {
            engine.update(cx, |engine, cx| {
                engine.set_ready_for_tests(endpoint.clone(), cx)
            })
        });
        assert_eq!(waiting.await.unwrap().url, endpoint.url);
        assert_eq!(
            cx.read(|cx| engine.read(cx).endpoint()).unwrap().url,
            endpoint.url
        );
    }
}
