//! What the provider knows: the engine's accounts, login options and
//! models, and the login in progress. Refreshed from the engine on demand
//! and whenever the engine reports a change; never a credential.

use anyhow::anyhow;
use futures::{StreamExt as _, channel::mpsc};
use gpui::{AppContext as _, Context, Entity, Subscription, Task};
use knightcode_engine::{
    Engine, EngineEvent,
    client::{Account, EngineClient, EngineModel, LoginEvent, LoginOption, PendingPrompt},
    login::Login,
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
    /// The engine's id for this login, once it has started: what `submit` and
    /// `cancel` address. The login itself stays with the polling task.
    login_id: Option<String>,
    _task: Option<Task<()>>,
}

/// What the polling task sends the view: the engine's own events, plus each
/// prompt it wants answered.
enum DriveMessage {
    Event(LoginEvent),
    Prompt(PendingPrompt),
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
                login_id: None,
                _task: None,
            });
            cx.notify();
            return;
        };
        let (provider_id, kind) = (option.provider_id.clone(), option.kind);
        self.login = Some(ActiveLogin {
            option,
            step: LoginStep::Starting,
            login_id: None,
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
        let Some((client, login_id)) = self.pending(cx) else {
            return;
        };
        if let Some(active) = &mut self.login {
            active.step = LoginStep::Starting;
        }
        cx.notify();
        cx.background_spawn(async move {
            // The poll reports the outcome. A rejected submit only means the
            // engine has already moved on — a browser callback that beat the
            // paste box, say — so it is not the login's verdict.
            if let Err(error) = client.submit_login(&login_id, &value).await {
                log::warn!("knightcode: submitting the login value failed: {error}");
            }
        })
        .detach();
    }

    pub fn cancel_login(&mut self, cx: &mut Context<Self>) {
        // Aborting the flow on the engine is what closes an OAuth login's
        // loopback listener; dropping our side alone would leave it open.
        if let Some((client, login_id)) = self.pending(cx) {
            cx.background_spawn(async move {
                client.cancel_login(&login_id).await.ok();
            })
            .detach();
        }
        self.login = None;
        cx.notify();
    }

    /// The engine client and the id of the login in flight, if there is one.
    fn pending(&self, cx: &Context<Self>) -> Option<(EngineClient, String)> {
        let login_id = self.login.as_ref()?.login_id.clone()?;
        Some((self.engine.read(cx).client()?, login_id))
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
        }
        cx.notify();
    }

    /// Poll the login in the background, reflect each event and prompt in
    /// `step`, open the browser when the engine reports a URL, and close the
    /// view when the engine settles. Polling continues while a prompt is on
    /// screen: an OAuth flow's paste box races its own browser callback, and
    /// whichever wins, the engine is the one that says the login is done.
    fn drive(&mut self, mut login: Login, cx: &mut Context<Self>) {
        if let Some(active) = &mut self.login {
            active.login_id = Some(login.id.clone());
        }
        let (tx, mut rx) = mpsc::unbounded::<DriveMessage>();
        let prompts = tx.clone();
        let polling = cx.background_spawn(async move {
            login
                .advance(
                    |event| {
                        tx.unbounded_send(DriveMessage::Event(event.clone())).ok();
                    },
                    |prompt| {
                        prompts
                            .unbounded_send(DriveMessage::Prompt(prompt.clone()))
                            .ok();
                        true
                    },
                )
                .await
        });
        let task = cx.spawn(async move |this, cx| {
            while let Some(message) = rx.next().await {
                this.update(cx, |this, cx| {
                    let Some(active) = &mut this.login else {
                        return;
                    };
                    match message {
                        DriveMessage::Event(LoginEvent::AuthUrl { url, .. }) => {
                            cx.open_url(&url);
                            active.step = LoginStep::Browser(url);
                        }
                        DriveMessage::Event(LoginEvent::DeviceCode {
                            user_code,
                            verification_uri,
                        }) => {
                            cx.open_url(&verification_uri);
                            active.step = LoginStep::DeviceCode {
                                code: user_code,
                                url: verification_uri,
                            };
                        }
                        DriveMessage::Event(_) => return,
                        DriveMessage::Prompt(prompt) => active.step = LoginStep::Prompt(prompt),
                    }
                    cx.notify();
                })
                .ok();
            }
            let outcome = polling.await;
            this.update(cx, |this, cx| match outcome {
                // The prompt callback never stops the poll, so a login that
                // comes back at all came back complete.
                Ok(_) => {
                    this.login = None;
                    this.refresh_quietly(cx);
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
