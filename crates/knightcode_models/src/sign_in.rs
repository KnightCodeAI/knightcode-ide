//! The provider's settings view: who is signed in, a button per login
//! option, and the login in progress — a URL the browser has been sent to,
//! a device code to type, or one input for the value the engine asked for.
//! Phase E turns this into the first-run screen; the state machine stays.

use gpui::{Context, Entity, Render, Subscription, Window};
use knightcode_engine::client::{LoginKind, PromptKind};
use ui::{
    Button, ButtonStyle, Label, LabelSize, ParentElement as _, Styled as _, h_flex, prelude::*,
    v_flex,
};
use ui_input::InputField;

use crate::state::{LoginStep, State};

pub struct SignInView {
    state: Entity<State>,
    input: Entity<InputField>,
    /// What the input is currently masking, so a render only touches the
    /// editor when the prompt kind changes.
    masked: bool,
    _subscription: Subscription,
}

impl SignInView {
    pub fn new(state: Entity<State>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input = cx.new(|cx| InputField::new(window, cx, "").masked(true));
        let subscription = cx.observe(&state, |_, _, cx| cx.notify());
        Self {
            state,
            input,
            masked: true,
            _subscription: subscription,
        }
    }
}

impl Render for SignInView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let state = self.state.read(cx);
        let mut root = v_flex().gap_2();

        for account in &state.accounts {
            let provider_id = account.provider_id.clone();
            let kind = match account.kind {
                LoginKind::Oauth if account.is_subscription => "subscription",
                LoginKind::Oauth => "account",
                LoginKind::ApiKey => "API key",
            };
            root = root.child(
                h_flex()
                    .justify_between()
                    .child(Label::new(format!(
                        "Signed in to {} ({kind})",
                        account.provider_name
                    )))
                    .child(
                        Button::new(format!("sign-out-{provider_id}"), "Sign out")
                            .style(ButtonStyle::Outlined)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.state.update(cx, |state, cx| {
                                    state.sign_out(provider_id.clone(), cx)
                                });
                            })),
                    ),
            );
        }

        if let Some(active) = &state.login {
            let title = format!("Signing in to {}", active.option.provider_name);
            root = root.child(Label::new(title).size(LabelSize::Small));
            root = match &active.step {
                LoginStep::Starting => root.child(Label::new("Contacting the engine…")),
                LoginStep::Browser(url) => root.child(Label::new(format!(
                    "Finish signing in in your browser. If it did not open: {url}"
                ))),
                LoginStep::DeviceCode { code, url } => {
                    root.child(Label::new(format!("Enter {code} at {url}")))
                }
                LoginStep::Prompt(prompt) => {
                    let masked = matches!(prompt.prompt.kind, PromptKind::Secret);
                    let message = prompt.prompt.message.clone();
                    if masked != self.masked {
                        self.masked = masked;
                        let input = self.input.clone();
                        input.update(cx, |input, cx| input.set_masked(masked, window, cx));
                    }
                    root.child(Label::new(message))
                        .child(self.input.clone())
                        .child(Button::new("submit", "Continue").on_click(cx.listener(
                            |this, _, _, cx| {
                                let value = this.input.read(cx).text(cx).trim().to_string();
                                if !value.is_empty() {
                                    this.state
                                        .update(cx, |state, cx| state.submit_prompt(value, cx));
                                }
                            },
                        )))
                }
                LoginStep::Failed(reason) => {
                    root.child(Label::new(format!("Sign-in failed: {reason}")))
                }
            };
            root = root.child(
                Button::new("cancel", "Cancel")
                    .style(ButtonStyle::Outlined)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.state.update(cx, |state, cx| state.cancel_login(cx));
                    })),
            );
            return root;
        }

        let mut buttons = h_flex().flex_wrap().gap_1();
        for option in &state.login_options {
            let option = option.clone();
            buttons = buttons.child(
                Button::new(
                    format!("login-{}-{:?}", option.provider_id, option.kind),
                    option.label.clone(),
                )
                .style(if option.is_subscription {
                    ButtonStyle::Filled
                } else {
                    ButtonStyle::Outlined
                })
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.state
                        .update(cx, |state, cx| state.start_login(option.clone(), cx));
                })),
            );
        }
        root.child(buttons)
    }
}
