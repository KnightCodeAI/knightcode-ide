//! One engine model as a Zed `LanguageModel`. Requests go to the engine's
//! `/v1/chat/completions` with the launch token; the engine picks the
//! provider and the credential.

use anyhow::anyhow;
use futures::{FutureExt as _, StreamExt as _, future::BoxFuture, stream::BoxStream};
use gpui::{AsyncApp, Entity};
use http_client::{CustomHeaders, HttpClient};
use knightcode_engine::{Engine, client::EngineModel};
use language_model::{
    LanguageModel, LanguageModelCompletionError, LanguageModelCompletionEvent, LanguageModelId,
    LanguageModelName, LanguageModelProviderId, LanguageModelProviderName, LanguageModelRequest,
    LanguageModelToolChoice, RateLimiter, chat_completion::ChatCompletionEventMapper,
    stream_in_background,
};
use std::sync::Arc;

use crate::{provider_id, provider_name, request::to_request};

pub struct KnightCodeLanguageModel {
    pub(crate) model: EngineModel,
    pub(crate) engine: Entity<Engine>,
    pub(crate) http: Arc<dyn HttpClient>,
    pub(crate) request_limiter: RateLimiter,
}

impl LanguageModel for KnightCodeLanguageModel {
    fn id(&self) -> LanguageModelId {
        LanguageModelId::from(self.model.reference.clone())
    }

    fn name(&self) -> LanguageModelName {
        // Five providers may offer "Claude Opus 5"; the engine provider tells them apart.
        LanguageModelName::from(format!(
            "{} ({})",
            self.model.name, self.model.provider_name
        ))
    }

    fn provider_id(&self) -> LanguageModelProviderId {
        provider_id()
    }

    fn provider_name(&self) -> LanguageModelProviderName {
        provider_name()
    }

    fn telemetry_id(&self) -> String {
        format!("knightcode/{}", self.model.reference)
    }

    fn supports_images(&self) -> bool {
        self.model.input.iter().any(|input| input == "image")
    }

    fn supports_tools(&self) -> bool {
        false
    }

    fn supports_tool_choice(&self, choice: LanguageModelToolChoice) -> bool {
        matches!(choice, LanguageModelToolChoice::None)
    }

    fn max_token_count(&self) -> u64 {
        self.model.context_window
    }

    fn max_output_tokens(&self) -> Option<u64> {
        Some(self.model.max_tokens)
    }

    fn stream_completion(
        &self,
        request: LanguageModelRequest,
        cx: &AsyncApp,
    ) -> BoxFuture<
        'static,
        Result<
            BoxStream<'static, Result<LanguageModelCompletionEvent, LanguageModelCompletionError>>,
            LanguageModelCompletionError,
        >,
    > {
        let Some(endpoint) = self.engine.read_with(cx, |engine, _| engine.endpoint()) else {
            return async {
                Err(LanguageModelCompletionError::Other(anyhow!(
                    "the KnightCode engine is not running"
                )))
            }
            .boxed();
        };
        let request = to_request(request, &self.model.reference);
        let http = self.http.clone();
        let api_url = format!("{}/v1", endpoint.url);
        let token = endpoint.token.clone();
        let future = self.request_limiter.stream(async move {
            let events = open_ai::stream_completion(
                http.as_ref(),
                "KnightCode",
                &api_url,
                &token,
                request,
                &CustomHeaders::default(),
            )
            .await?;
            Ok(events)
        });
        let executor = cx.background_executor().clone();
        async move {
            let events = future.await?;
            Ok(stream_in_background(
                ChatCompletionEventMapper::new()
                    .map_stream(events.boxed())
                    .boxed(),
                executor,
            ))
        }
        .boxed()
    }
}
