//! `LanguageModelRequest` to the engine's chat-completions request. The
//! engine accepts system, user and assistant messages with string content
//! and nothing else, which is what every seam-2 surface sends.

use language_model::{LanguageModelRequest, Role};
use open_ai::{MessageContent, Request, RequestMessage, StreamOptions};

pub fn to_request(request: LanguageModelRequest, model: &str) -> Request {
    let messages = request
        .messages
        .iter()
        .filter_map(|message| {
            let text = message.string_contents();
            if text.is_empty() {
                return None;
            }
            let content = MessageContent::Plain(text);
            Some(match message.role {
                Role::System => RequestMessage::System { content },
                Role::User => RequestMessage::User { content },
                Role::Assistant => RequestMessage::Assistant {
                    content: Some(content),
                    tool_calls: Vec::new(),
                    reasoning_content: None,
                    reasoning_details: None,
                },
            })
        })
        .collect();
    Request {
        model: model.to_owned(),
        messages,
        stream: true,
        stream_options: Some(StreamOptions {
            include_usage: true,
        }),
        max_completion_tokens: None,
        max_tokens: None,
        stop: request.stop,
        temperature: request.temperature,
        tool_choice: None,
        parallel_tool_calls: None,
        tools: Vec::new(),
        prompt_cache_key: None,
        reasoning_effort: None,
        service_tier: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use language_model::{LanguageModelRequestMessage, MessageContent, Role};

    fn message(role: Role, content: Vec<MessageContent>) -> LanguageModelRequestMessage {
        LanguageModelRequestMessage {
            role,
            content,
            cache: false,
            reasoning_details: None,
        }
    }

    #[test]
    fn roles_map_text_flattens_and_non_text_is_dropped() {
        let request = LanguageModelRequest {
            messages: vec![
                message(
                    Role::System,
                    vec![MessageContent::Text("You edit code.".into())],
                ),
                message(
                    Role::User,
                    vec![
                        MessageContent::Text("Rename ".into()),
                        MessageContent::Text("greet".into()),
                    ],
                ),
                message(Role::Assistant, vec![MessageContent::Text("Done.".into())]),
                message(
                    Role::User,
                    vec![MessageContent::RedactedThinking("x".into())],
                ),
            ],
            temperature: Some(0.2),
            stop: vec!["END".into()],
            ..Default::default()
        };
        let converted = to_request(request, "anthropic/claude-opus-5");
        assert_eq!(converted.model, "anthropic/claude-opus-5");
        assert!(converted.stream);
        assert_eq!(converted.temperature, Some(0.2));
        assert_eq!(converted.stop, vec!["END".to_string()]);
        assert_eq!(
            converted.messages.len(),
            3,
            "a message with no text is not sent"
        );
        assert!(matches!(
            &converted.messages[0],
            open_ai::RequestMessage::System { content: open_ai::MessageContent::Plain(text) } if text == "You edit code."
        ));
        assert!(matches!(
            &converted.messages[1],
            open_ai::RequestMessage::User { content: open_ai::MessageContent::Plain(text) } if text == "Rename greet"
        ));
        assert!(matches!(
            &converted.messages[2],
            open_ai::RequestMessage::Assistant { content: Some(open_ai::MessageContent::Plain(text)), .. } if text == "Done."
        ));
        assert!(converted.tools.is_empty());
        assert_eq!(
            serde_json::to_value(&converted).unwrap().get("tools"),
            None,
            "no tools key on the wire"
        );
    }
}
