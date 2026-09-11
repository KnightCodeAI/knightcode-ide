//! What the provider knows: the engine's accounts, login options and
//! models, and the login in progress. Refreshed from the engine on demand
//! and whenever the engine reports a change; never a credential.

use anyhow::anyhow;
use futures::{StreamExt as _, channel::mpsc};
use gpui::{AppContext as _, Context, Entity, Subscription, Task};
use knightcode_engine::{
    Engine, EngineEvent,
    client::{Account, EngineModel, LoginEvent, LoginOption, PendingPrompt},
    login::{Login, LoginOutcome},
};
use language_model::AuthenticateError;

pub enum LoginStep {
    Starting,
    Browser(String),
    DeviceCode { code: String, url: String },
    Prompt(PendingPrompt),
    Failed(String),
}

pub struct ActiveLogin {
    pub option: LoginOption,
    pub step: LoginStep,
    /// The login, parked while the user answers a prompt.
    parked: Option<Login>,
    _task: Option<Task<()>>,
}

pub struct State {
    engine: Entity<Engine>,
    pub accounts: Vec<Account>,
    pub login_options: Vec<LoginOption>,
    pub models: Vec<EngineModel>,
    pub login: Option<ActiveLogin>,
    refresh: Option<Task<()>>,
    _subscription: Subscription,
}

impl State {
    pub fn new(engine: Entity<Engine>, cx: &mut Context<Self>) -> Self {
        let subscription = cx.subscribe(&engine, |this, _, event, cx| match event {
            EngineEvent::Ready(_)
            | EngineEvent::AccountChanged { .. }
            | EngineEvent::ModelsChanged => {
                this.refresh_quietly(cx);
            }
            EngineEvent::Stopped => {
                this.models.clear();
                cx.notify();
            }
            EngineEvent::Failed(_) => {}
        });
        Self {
            engine,
            accounts: Vec::new(),
            login_options: Vec::new(),
            models: Vec::new(),
            login: None,
            refresh: None,
            _subscription: subscription,
        }
    }

    pub fn engine(&self) -> Entity<Engine> {
        self.engine.clone()
    }

    pub fn default_model(&self) -> Option<&EngineModel> {
        self.models.first()
    }

    /// Non-interactive. `CredentialsNotFound` when the engine has nothing
    /// to offer; `ConnectionRefused` while it is still starting.
    pub fn refresh(&mut self, cx: &mut Context<Self>) -> Task<Result<(), AuthenticateError>> {
        let Some(client) = self.engine.read(cx).client() else {
            return Task::ready(Err(AuthenticateError::ConnectionRefused));
        };
        cx.spawn(async move |this, cx| {
            let accounts = client
                .accounts()
                .await
                .map_err(|error| AuthenticateError::Other(anyhow!(error)))?;
            let models = client
                .models()
                .await
                .map_err(|error| AuthenticateError::Other(anyhow!(error)))?;
            let empty = models.is_empty();
            this.update(cx, |this, cx| {
                this.accounts = accounts.accounts;
                this.login_options = accounts.login_options;
                this.models = models;
                cx.notify();
            })
            .map_err(AuthenticateError::Other)?;
            if empty {
                Err(AuthenticateError::CredentialsNotFound)
            } else {
                Ok(())
            }
        })
    }

    /// The background form of `refresh`, for the event subscription and the
    /// post-login and post-sign-out paths, where nobody awaits the outcome.
    fn refresh_quietly(&mut self, cx: &mut Context<Self>) {
        let refresh = self.refresh(cx);
        self.refresh = Some(cx.spawn(async move |_, _| {
            refresh.await.ok();
        }));
    }

    pub fn start_login(&mut self, option: LoginOption, cx: &mut Context<Self>) {
        let Some(client) = self.engine.read(cx).client() else {
            self.login = Some(ActiveLogin {
                option,
                step: LoginStep::Failed("the engine is not running".into()),
                parked: None,
                _task: None,
            });
            cx.notify();
            return;
        };
        let (provider_id, kind) = (option.provider_id.clone(), option.kind);
        self.login = Some(ActiveLogin {
            option,
            step: LoginStep::Starting,
            parked: None,
            _task: None,
        });
        cx.notify();
        let task = cx.spawn(async move |this, cx| {
            match Login::start(client, &provider_id, kind).await {
                Ok(login) => this.update(cx, |this, cx| this.drive(login, cx)).ok(),
                Err(error) => this
                    .update(cx, |this, cx| this.fail(error.to_string(), cx))
                    .ok(),
            };
        });
        if let Some(login) = &mut self.login {
            login._task = Some(task);
        }
    }

    pub fn submit_prompt(&mut self, value: String, cx: &mut Context<Self>) {
        let Some(login) = self.login.as_mut().and_then(|active| active.parked.take()) else {
            return;
        };
        if let Some(active) = &mut self.login {
            active.step = LoginStep::Starting;
        }
        cx.notify();
        let task = cx.spawn(async move |this, cx| {
            match login.submit(&value).await {
                Ok(()) => this.update(cx, |this, cx| this.drive(login, cx)).ok(),
                Err(error) => this
                    .update(cx, |this, cx| this.fail(error.to_string(), cx))
                    .ok(),
            };
        });
        if let Some(active) = &mut self.login {
            active._task = Some(task);
        }
    }

    pub fn cancel_login(&mut self, cx: &mut Context<Self>) {
        if let Some(active) = self.login.take()
            && let Some(login) = active.parked
        {
            cx.background_spawn(async move {
                login.cancel().await.ok();
            })
            .detach();
        }
        cx.notify();
    }

    pub fn sign_out(&mut self, provider_id: String, cx: &mut Context<Self>) {
        let Some(client) = self.engine.read(cx).client() else {
            return;
        };
        cx.spawn(async move |this, cx| {
            if let Err(error) = client.sign_out(&provider_id).await {
                log::warn!("knightcode: sign out of {provider_id} failed: {error}");
            }
            // The engine also emits account.changed; refreshing here makes the
            // view current even if that event is lost.
            this.update(cx, |this, cx| {
                this.refresh_quietly(cx);
            })
            .ok();
        })
        .detach();
    }

    fn fail(&mut self, reason: String, cx: &mut Context<Self>) {
        if let Some(active) = &mut self.login {
            active.step = LoginStep::Failed(reason);
            active.parked = None;
        }
        cx.notify();
    }

    /// Poll the login in the background, reflect each event in `step`, open
    /// the browser when the engine reports a URL, and end complete (refresh)
    /// or parked on a prompt.
    fn drive(&mut self, mut login: Login, cx: &mut Context<Self>) {
        let (tx, mut rx) = mpsc::unbounded::<LoginEvent>();
        let polling = cx.background_spawn(async move {
            let outcome = login
                .advance(|event| {
                    tx.unbounded_send(event.clone()).ok();
                })
                .await;
            (login, outcome)
        });
        let task = cx.spawn(async move |this, cx| {
            while let Some(event) = rx.next().await {
                this.update(cx, |this, cx| {
                    let Some(active) = &mut this.login else {
                        return;
                    };
                    match event {
                        LoginEvent::AuthUrl { url, .. } => {
                            cx.open_url(&url);
                            active.step = LoginStep::Browser(url);
                        }
                        LoginEvent::DeviceCode {
                            user_code,
                            verification_uri,
                        } => {
                            cx.open_url(&verification_uri);
                            active.step = LoginStep::DeviceCode {
                                code: user_code,
                                url: verification_uri,
                            };
                        }
                        LoginEvent::Info { .. } | LoginEvent::Progress { .. } => {}
                    }
                    cx.notify();
                })
                .ok();
            }
            let (login, outcome) = polling.await;
            this.update(cx, |this, cx| match outcome {
                Ok(LoginOutcome::Complete) => {
                    this.login = None;
                    this.refresh_quietly(cx);
                    cx.notify();
                }
                Ok(LoginOutcome::Prompt(prompt)) => {
                    if let Some(active) = &mut this.login {
                        active.step = LoginStep::Prompt(prompt);
                        active.parked = Some(login);
                    }
                    cx.notify();
                }
                Err(error) => this.fail(error.to_string(), cx),
            })
            .ok();
        });
        if let Some(active) = &mut self.login {
            active._task = Some(task);
        }
    }
}
