use anyhow::{Error, Result};
use async_trait::async_trait;
use reqwest::Client;
use serde_json::{json, Value};
use std::time::Duration;

use super::base::{ConfigKey, Provider, ProviderMetadata, ProviderUsage};
use super::errors::ProviderError;
use super::templates::{TemplateRenderer, TemplateContext, TemplatedToolConfig};
use super::utils::{emit_debug_trace, get_model, handle_response_openai_compat};
use crate::message::{Message, MessageContent};
use crate::model::ModelConfig;
use crate::providers::formats::openai::{create_request, get_usage, response_to_message};
use mcp_core::{role::Role, tool::Tool};

pub const OPENROUTER_DEFAULT_MODEL: &str = "anthropic/claude-3.5-sonnet";
pub const OPENROUTER_MODEL_PREFIX_ANTHROPIC: &str = "anthropic";
pub const OPENROUTER_MODEL_PREFIX_DEEPSEEK: &str = "deepseek";

// OpenRouter can run many models, we suggest the default
pub const OPENROUTER_KNOWN_MODELS: &[&str] = &[OPENROUTER_DEFAULT_MODEL];
pub const OPENROUTER_DOC_URL: &str = "https://openrouter.ai/models";

#[derive(serde::Serialize)]
pub struct OpenRouterProvider {
    #[serde(skip)]
    client: Client,
    host: String,
    api_key: String,
    model: ModelConfig,
    #[serde(skip)]
    template_renderer: Option<TemplateRenderer>,
}

impl Default for OpenRouterProvider {
    fn default() -> Self {
        let model = ModelConfig::new(OpenRouterProvider::metadata().default_model);
        OpenRouterProvider::from_env(model).expect("Failed to initialize OpenRouter provider")
    }
}

impl OpenRouterProvider {
    pub fn from_env(model: ModelConfig) -> Result<Self> {
        let config = crate::config::Config::global();
        let api_key: String = config.get_secret("OPENROUTER_API_KEY")?;
        let host: String = config
            .get("OPENROUTER_HOST")
            .unwrap_or_else(|_| "https://openrouter.ai".to_string());

        let client = Client::builder()
            .timeout(Duration::from_secs(600))
            .build()?;

        // Initialize template renderer for models that need it
        let template_renderer = if model.model_name.starts_with(OPENROUTER_MODEL_PREFIX_DEEPSEEK) {
            Some(TemplateRenderer::new(TemplatedToolConfig::deepseek_style()))
        } else {
            None
        };

        Ok(Self {
            client,
            host,
            api_key,
            model,
            template_renderer,
        })
    }

    async fn post(&self, payload: Value) -> Result<Value, ProviderError> {
        let url = format!(
            "{}/api/v1/chat/completions",
            self.host.trim_end_matches('/')
        );

        let response = self
            .client
            .post(&url)
            .header("Content-Type", "application/json")
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("HTTP-Referer", "https://github.com/block/goose")
            .header("X-Title", "Goose")
            .json(&payload)
            .send()
            .await?;

        handle_response_openai_compat(response).await
    }

    fn uses_templated_tools(&self) -> bool {
        self.model.model_name.starts_with(OPENROUTER_MODEL_PREFIX_DEEPSEEK)
    }
}

/// Update the request when using anthropic model.
/// For anthropic model, we can enable prompt caching to save cost. Since openrouter is the OpenAI compatible
/// endpoint, we need to modify the open ai request to have anthropic cache control field.
fn update_request_for_anthropic(original_payload: &Value) -> Value {
    let mut payload = original_payload.clone();
    if let Some(messages_spec) = payload
        .as_object_mut()
        .and_then(|obj| obj.get_mut("messages"))
        .and_then(|messages| messages.as_array_mut())
    {
        // Add "cache_control" to the last and second-to-last "user" messages.
        // During each turn, we mark the final message with cache_control so the conversation can be
        // incrementally cached. The second-to-last user message is also marked for caching with the
        // cache_control parameter, so that this checkpoint can read from the previous cache.
        let mut user_count = 0;
        for message in messages_spec.iter_mut().rev() {
            if message.get("role") == Some(&json!("user")) {
                if let Some(content) = message.get_mut("content") {
                    if let Some(content_str) = content.as_str() {
                        *content = json!([{
                            "type": "text",
                            "text": content_str,
                            "cache_control": { "type": "ephemeral" }
                        }]);
                    }
                }
                user_count += 1;
                if user_count >= 2 {
                    break;
                }
            }
        }

        // Update the system message to have cache_control field.
        if let Some(system_message) = messages_spec
            .iter_mut()
            .find(|msg| msg.get("role") == Some(&json!("system")))
        {
            if let Some(content) = system_message.get_mut("content") {
                if let Some(content_str) = content.as_str() {
                    *system_message = json!({
                        "role": "system",
                        "content": [{
                            "type": "text",
                            "text": content_str,
                            "cache_control": { "type": "ephemeral" }
                        }]
                    });
                }
            }
        }
    }
    payload
}

fn create_request_based_on_model(
    provider: &OpenRouterProvider,
    system: &str,
    messages: &[Message],
    tools: &[Tool],
) -> anyhow::Result<Value, Error> {
    if provider.uses_templated_tools() {

        let renderer = provider.template_renderer.as_ref().unwrap();
        
        // Render system prompt with tools
        let system_prompt = renderer.render(TemplateContext {
            system: Some(system),
            tools: Some(tools),
        });

        // Create initial messages list with system prompt as first user message
        let mut mapped_messages: Vec<Message> = vec![Message::user().with_text(system_prompt)];
        
        // Add remaining messages, converting Tool role to User role
        mapped_messages.extend(messages.iter().map(|msg| {
            if msg.role == Role::Tool {
                // Create new message with same content but User role
                let mut new_msg = Message::user();
                new_msg.content = msg.content.clone();
                new_msg.created = msg.created;
                new_msg
            } else {
                msg.clone()
            }
        }));

        // Check if last message is a user message with text/image content
        if let Some(last_msg) = mapped_messages.last() {
            tracing::debug!("Last message: {:?}", last_msg);
            if last_msg.role == Role::User && 
            matches!(last_msg.content.last(), Some(MessageContent::Text(_) | MessageContent::Image(_) | MessageContent::ToolResponse(_))) 
            {
                let mut last_msg = mapped_messages.pop().unwrap();
                let existing_text = last_msg.as_concat_text();
                if !existing_text.is_empty() {
                    let tool_instructions = "\nTo use a tool, respond with a JSON object with only the fields 'name' (tool name) and 'parameters' (tool parameters based on input schemas)";
                    last_msg = Message::user().with_text(format!("{}\n{}", existing_text, tool_instructions));
                    mapped_messages.push(last_msg);
                }
            }
            tracing::debug!("last message after: {:?}", mapped_messages.last());
        }




        // Create normal OpenAI format request with mapped messages
        let mut payload = create_request(
            &provider.model,
            "",      // No system prompt since it's in the messages
            &mapped_messages,    // Use messages with Tool->User mapping
            &[],                // Don't pass tools since they're in system prompt
            &super::utils::ImageFormat::OpenAi,
        )?;

        // Add stop tokens
        payload["stop"] = json!(renderer.get_stop_tokens());

        if let Some(temp) = provider.model.temperature {
            payload["temperature"] = json!(temp);
        }
        if let Some(tokens) = provider.model.max_tokens {
            payload["max_tokens"] = json!(tokens);
        }

        Ok(payload)
    } else {
        // For models with native tool support, use the normal OpenAI format
        let mut payload = create_request(
            &provider.model,
            system,
            messages,
            tools,
            &super::utils::ImageFormat::OpenAi,
        )?;

        if provider.model.model_name.starts_with(OPENROUTER_MODEL_PREFIX_ANTHROPIC) {
            payload = update_request_for_anthropic(&payload);
        }

        Ok(payload)
    }
}

#[async_trait]
impl Provider for OpenRouterProvider {
    fn metadata() -> ProviderMetadata {
        ProviderMetadata::new(
            "openrouter",
            "OpenRouter",
            "Router for many model providers",
            OPENROUTER_DEFAULT_MODEL,
            OPENROUTER_KNOWN_MODELS
                .iter()
                .map(|&s| s.to_string())
                .collect(),
            OPENROUTER_DOC_URL,
            vec![
                ConfigKey::new("OPENROUTER_API_KEY", true, true, None),
                ConfigKey::new(
                    "OPENROUTER_HOST",
                    false,
                    false,
                    Some("https://openrouter.ai"),
                ),
            ],
        )
    }

    fn get_model_config(&self) -> ModelConfig {
        self.model.clone()
    }

    #[tracing::instrument(
        skip(self, system, messages, tools),
        fields(model_config, input, output, input_tokens, output_tokens, total_tokens)
    )]
    async fn complete(
        &self,
        system: &str,
        messages: &[Message],
        tools: &[Tool],
    ) -> Result<(Message, ProviderUsage), ProviderError> {
        // Create the request payload
        let mut payload = create_request_based_on_model(self, system, messages, tools)?;
        // payload["providers"] = json!({"order": ["Novita, Fireworks"]});

        tracing::debug!(payload_after_processing=?payload);

        // Make request
        let response = self.post(payload.clone()).await?;
        tracing::debug!(response=?response);

        // Parse response
        let message = if self.uses_templated_tools() {
            // For templated tools, we need to parse the response differently
            let response_text = response["choices"][0]["message"]["content"]
                .as_str()
                .ok_or_else(|| ProviderError::ResponseParsing("No content in response".to_string()))?;

            if let Some(renderer) = &self.template_renderer {
                let tool_calls = renderer.parse_tool_calls(response_text);
                tracing::debug!("GOT TOOL CALLS: {:?}", tool_calls);
                if !tool_calls.is_empty() {
                    let mut msg = Message::assistant();
                    for tool_call in tool_calls {
                        msg = msg.with_tool_request(nanoid::nanoid!(), Ok(tool_call));
                    }
                    msg
                } else {
                    Message::assistant().with_text(response_text)
                }
            } else {
                Message::assistant().with_text(response_text)
            }
        } else {
            // For native tool support, use normal parsing
            response_to_message(response.clone())?
        };

        let usage = get_usage(&response)?;
        let model = get_model(&response);
        emit_debug_trace(self, &payload, &response, &usage);
        Ok((message, ProviderUsage::new(model, usage)))
    }
}