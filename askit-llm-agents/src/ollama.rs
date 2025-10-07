#![cfg(feature = "ollama")]

use std::sync::{Arc, Mutex};
use std::vec;

use agent_stream_kit::{
    ASKit, Agent, AgentConfig, AgentConfigEntry, AgentContext, AgentData, AgentDefinition,
    AgentError, AgentOutput, AgentValue, AsAgent, AsAgentData, async_trait, new_agent_boxed,
};

use ollama_rs::{
    Ollama,
    generation::{
        chat::{ChatMessage, MessageRole, request::ChatMessageRequest},
        completion::request::GenerationRequest,
        embeddings::request::GenerateEmbeddingsRequest,
    },
    history::ChatHistory,
    models::ModelOptions,
};
use tokio_stream::StreamExt;

use crate::message::{Message, MessageHistory};

// Shared client management for Ollama agents
struct OllamaManager {
    client: Arc<Mutex<Option<Ollama>>>,
}

impl OllamaManager {
    fn new() -> Self {
        Self {
            client: Arc::new(Mutex::new(None)),
        }
    }

    fn get_ollama_url(global_config: Option<AgentConfig>) -> String {
        if let Some(ollama_url) =
            global_config.and_then(|cfg| cfg.get_string(CONFIG_OLLAMA_URL).ok())
        {
            if !ollama_url.is_empty() {
                return ollama_url;
            }
        }
        if let Ok(ollama_api_base_url) = std::env::var("OLLAMA_API_BASE_URL") {
            return ollama_api_base_url;
        } else if let Ok(ollama_host) = std::env::var("OLLAMA_HOST") {
            return format!("http://{}:11434", ollama_host);
        }
        DEFAULT_OLLAMA_URL.to_string()
    }

    fn get_client(&self, askit: &ASKit) -> Result<Ollama, AgentError> {
        let mut client_guard = self.client.lock().unwrap();

        if let Some(client) = client_guard.as_ref() {
            return Ok(client.clone());
        }

        let global_config = askit.get_global_config("ollama_completion");
        let api_base_url = Self::get_ollama_url(global_config);
        let new_client = Ollama::try_new(api_base_url)
            .map_err(|e| AgentError::IoError(format!("Ollama Client Error: {}", e)))?;
        *client_guard = Some(new_client.clone());

        Ok(new_client)
    }
}

// Ollama Completion Agent
pub struct OllamaCompletionAgent {
    data: AsAgentData,
    manager: OllamaManager,
}

#[async_trait]
impl AsAgent for OllamaCompletionAgent {
    fn new(
        askit: ASKit,
        id: String,
        def_name: String,
        config: Option<AgentConfig>,
    ) -> Result<Self, AgentError> {
        Ok(Self {
            data: AsAgentData::new(askit, id, def_name, config),
            manager: OllamaManager::new(),
        })
    }

    fn data(&self) -> &AsAgentData {
        &self.data
    }

    fn mut_data(&mut self) -> &mut AsAgentData {
        &mut self.data
    }

    async fn process(&mut self, ctx: AgentContext, data: AgentData) -> Result<(), AgentError> {
        let config_model = &self.config()?.get_string_or_default(CONFIG_MODEL);
        if config_model.is_empty() {
            return Ok(());
        }

        let message = data.as_str().unwrap_or("");
        if message.is_empty() {
            return Ok(());
        }

        let mut request = GenerationRequest::new(config_model.to_string(), message);

        let config_system = self.config()?.get_string_or_default(CONFIG_SYSTEM);
        if !config_system.is_empty() {
            request = request.system(config_system);
        }

        let config_options = self.config()?.get_string_or_default(CONFIG_OPTIONS);
        if !config_options.is_empty() && config_options != "{}" {
            if let Ok(options_json) = serde_json::from_str::<ModelOptions>(&config_options) {
                request = request.options(options_json);
            } else {
                return Err(AgentError::InvalidValue(
                    "Invalid JSON in options".to_string(),
                ));
            }
        }

        let client = self.manager.get_client(self.askit())?;
        let res = client
            .generate(request)
            .await
            .map_err(|e| AgentError::IoError(format!("Ollama Error: {}", e)))?;

        let message = Message::assistant(res.response.clone());
        self.try_output(ctx.clone(), PORT_MESSAGE, message.into())?;

        let out_response = AgentData::from_serialize(&res)?;
        self.try_output(ctx, PORT_RESPONSE, out_response)?;

        Ok(())
    }
}

// Ollama Chat Agent
pub struct OllamaChatAgent {
    data: AsAgentData,
    manager: OllamaManager,
    history: Arc<Mutex<MessageHistory>>,
}

#[async_trait]
impl AsAgent for OllamaChatAgent {
    fn new(
        askit: ASKit,
        id: String,
        def_name: String,
        config: Option<AgentConfig>,
    ) -> Result<Self, AgentError> {
        Ok(Self {
            data: AsAgentData::new(askit, id, def_name, config),
            manager: OllamaManager::new(),
            history: Arc::new(Mutex::new(MessageHistory::default())),
        })
    }

    fn data(&self) -> &AsAgentData {
        &self.data
    }

    fn mut_data(&mut self) -> &mut AsAgentData {
        &mut self.data
    }

    fn set_config(&mut self, config: AgentConfig) -> Result<(), AgentError> {
        let history_size = config.get_integer_or_default(CONFIG_HISTORY);
        if history_size != self.config()?.get_integer_or_default(CONFIG_HISTORY) {
            let mut history = self.history.lock().unwrap();
            *history = MessageHistory::new(history.messages.clone(), history_size);
        }
        Ok(())
    }

    async fn process(&mut self, ctx: AgentContext, data: AgentData) -> Result<(), AgentError> {
        let config_model = &self.config()?.get_string_or_default(CONFIG_MODEL);
        if config_model.is_empty() {
            return Ok(());
        }

        let message = data.as_str().unwrap_or("");
        if message.is_empty() {
            return Ok(());
        }

        let mut client = self.manager.get_client(self.askit())?;
        let mut request = ChatMessageRequest::new(
            config_model.to_string(),
            vec![ChatMessage::user(message.to_string())],
        );

        let config_options = self.config()?.get_string_or_default(CONFIG_OPTIONS);
        if !config_options.is_empty() && config_options != "{}" {
            if let Ok(options_json) = serde_json::from_str::<ModelOptions>(&config_options) {
                request = request.options(options_json);
            } else {
                return Err(AgentError::InvalidValue(
                    "Invalid JSON in options".to_string(),
                ));
            }
        }

        let history_size = self.config()?.get_integer_or_default(CONFIG_HISTORY);
        let use_stream = self.config()?.get_bool_or_default(CONFIG_STREAM);

        if use_stream {
            let mut stream = if history_size > 0 {
                client
                    .send_chat_messages_with_history_stream(self.history.clone(), request)
                    .await
            } else {
                client.send_chat_messages_stream(request).await
            }
            .map_err(|e| AgentError::IoError(format!("Ollama Error: {}", e)))?;

            let mut content = String::new();
            while let Some(res) = stream.next().await {
                let res = res.map_err(|_| AgentError::IoError(format!("Ollama Stream Error")))?;

                content.push_str(&res.message.content);

                let message = Message::assistant(content.clone());
                self.try_output(ctx.clone(), PORT_MESSAGE, message.into())?;

                let out_response = AgentData::from_serialize(&res)?;
                self.try_output(ctx.clone(), PORT_RESPONSE, out_response)?;

                if res.done {
                    break;
                }
            }
            if history_size > 0 {
                let messages = self.history.lock().unwrap();
                self.try_output(ctx.clone(), PORT_HISTORY, messages.clone().into())?;
            }
        } else {
            let res = if history_size > 0 {
                let mut history = self.history.lock().unwrap().clone();
                let res = client
                    .send_chat_messages_with_history(&mut history, request)
                    .await;
                *self.history.lock().unwrap() = history;
                res
            } else {
                client.send_chat_messages(request).await
            }
            .map_err(|e| AgentError::IoError(format!("Ollama Error: {}", e)))?;

            let message: Message = res.message.clone().into();
            self.try_output(ctx.clone(), PORT_MESSAGE, message.into())?;

            let out_response = AgentData::from_serialize(&res)?;
            self.try_output(ctx.clone(), PORT_RESPONSE, out_response)?;

            if history_size > 0 {
                let messages = self.history.lock().unwrap();
                self.try_output(ctx, PORT_HISTORY, messages.clone().into())?;
            }
        }

        Ok(())
    }
}

// Ollama Embeddings Agent
pub struct OllamaEmbeddingsAgent {
    data: AsAgentData,
    manager: OllamaManager,
}

#[async_trait]
impl AsAgent for OllamaEmbeddingsAgent {
    fn new(
        askit: ASKit,
        id: String,
        def_name: String,
        config: Option<AgentConfig>,
    ) -> Result<Self, AgentError> {
        Ok(Self {
            data: AsAgentData::new(askit, id, def_name, config),
            manager: OllamaManager::new(),
        })
    }

    fn data(&self) -> &AsAgentData {
        &self.data
    }

    fn mut_data(&mut self) -> &mut AsAgentData {
        &mut self.data
    }

    async fn process(&mut self, ctx: AgentContext, data: AgentData) -> Result<(), AgentError> {
        let config_model = &self.config()?.get_string_or_default(CONFIG_MODEL);
        if config_model.is_empty() {
            return Ok(());
        }

        let input = data.as_str().unwrap_or(""); // TODO: other types
        if input.is_empty() {
            return Ok(());
        }

        let client = self.manager.get_client(self.askit())?;
        let mut request = GenerateEmbeddingsRequest::new(config_model.to_string(), input.into());

        let config_options = self.config()?.get_string_or_default(CONFIG_OPTIONS);
        if !config_options.is_empty() && config_options != "{}" {
            if let Ok(options_json) = serde_json::from_str::<ModelOptions>(&config_options) {
                request = request.options(options_json);
            } else {
                return Err(AgentError::InvalidValue(
                    "Invalid JSON in options".to_string(),
                ));
            }
        }

        let res = client
            .generate_embeddings(request)
            .await
            .map_err(|e| AgentError::IoError(format!("Ollama Error: {}", e)))?;

        let embeddings = AgentData::from_serialize(&res.embeddings)?;
        self.try_output(ctx.clone(), PORT_EMBEDDINGS, embeddings)?;

        Ok(())
    }
}

impl From<ChatMessage> for Message {
    fn from(msg: ChatMessage) -> Self {
        let role = match msg.role {
            MessageRole::User => "user",
            MessageRole::Assistant => "assistant",
            MessageRole::System => "system",
            MessageRole::Tool => "tool",
        };
        Self {
            role: role.to_string(),
            content: msg.content,
        }
    }
}

impl From<Message> for ChatMessage {
    fn from(msg: Message) -> Self {
        match msg.role.as_str() {
            "user" => ChatMessage::user(msg.content),
            "assistant" => ChatMessage::assistant(msg.content),
            "system" => ChatMessage::system(msg.content),
            "tool" => ChatMessage::tool(msg.content),
            _ => ChatMessage::user(msg.content), // Default to user if unknown role
        }
    }
}

impl ChatHistory for MessageHistory {
    fn push(&mut self, message: ChatMessage) {
        self.push(message.into());
    }

    fn messages(&self) -> std::borrow::Cow<'_, [ChatMessage]> {
        let messages: Vec<ChatMessage> =
            self.messages.iter().map(|msg| msg.clone().into()).collect();
        std::borrow::Cow::Owned(messages)
    }
}

static AGENT_KIND: &str = "agent";
static CATEGORY: &str = "LLM";

static PORT_EMBEDDINGS: &str = "embeddings";
static PORT_HISTORY: &str = "history";
static PORT_INPUT: &str = "input";
static PORT_MESSAGE: &str = "message";
static PORT_RESPONSE: &str = "response";

static CONFIG_HISTORY: &str = "history";
static CONFIG_MODEL: &str = "model";
static CONFIG_OLLAMA_URL: &str = "ollama_url";
static CONFIG_OPTIONS: &str = "options";
static CONFIG_STREAM: &str = "stream";
static CONFIG_SYSTEM: &str = "system";

const DEFAULT_CONFIG_MODEL: &str = "gemma3:4b";
const DEFAULT_OLLAMA_URL: &str = "http://localhost:11434";

pub fn register_agents(askit: &ASKit) {
    askit.register_agent(
        AgentDefinition::new(
            AGENT_KIND,
            "ollama_completion",
            Some(new_agent_boxed::<OllamaCompletionAgent>),
        )
        // .use_native_thread()
        .with_title("Ollama Completion")
        .with_category(CATEGORY)
        .with_inputs(vec![PORT_MESSAGE])
        .with_outputs(vec![PORT_MESSAGE, PORT_RESPONSE])
        .with_global_config(vec![(
            CONFIG_OLLAMA_URL.into(),
            AgentConfigEntry::new(AgentValue::string(DEFAULT_OLLAMA_URL), "string")
                .with_title("Ollama URL"),
        )])
        .with_default_config(vec![
            (
                CONFIG_MODEL.into(),
                AgentConfigEntry::new(AgentValue::string(DEFAULT_CONFIG_MODEL), "string")
                    .with_title("Model"),
            ),
            (
                CONFIG_SYSTEM.into(),
                AgentConfigEntry::new(AgentValue::string(""), "text").with_title("System"),
            ),
            (
                CONFIG_OPTIONS.into(),
                AgentConfigEntry::new(AgentValue::string("{}"), "text").with_title("Options"),
            ),
        ]),
    );

    askit.register_agent(
        AgentDefinition::new(
            AGENT_KIND,
            "ollama_chat",
            Some(new_agent_boxed::<OllamaChatAgent>),
        )
        // .use_native_thread()
        .with_title("Ollama Chat")
        .with_category(CATEGORY)
        .with_inputs(vec![PORT_MESSAGE])
        .with_outputs(vec![PORT_MESSAGE, PORT_RESPONSE, PORT_HISTORY])
        .with_default_config(vec![
            (
                CONFIG_MODEL.into(),
                AgentConfigEntry::new(AgentValue::string(DEFAULT_CONFIG_MODEL), "string")
                    .with_title("Model"),
            ),
            (
                CONFIG_HISTORY.into(),
                AgentConfigEntry::new(AgentValue::integer(0), "integer").with_title("History Size"),
            ),
            (
                CONFIG_STREAM.into(),
                AgentConfigEntry::new(AgentValue::boolean(false), "boolean").with_title("Stream"),
            ),
            (
                CONFIG_OPTIONS.into(),
                AgentConfigEntry::new(AgentValue::string("{}"), "text").with_title("Options"),
            ),
        ]),
    );

    askit.register_agent(
        AgentDefinition::new(
            AGENT_KIND,
            "ollama_embeddings",
            Some(new_agent_boxed::<OllamaEmbeddingsAgent>),
        )
        // .use_native_thread()
        .with_title("Ollama Embeddings")
        .with_category(CATEGORY)
        .with_inputs(vec![PORT_INPUT])
        .with_outputs(vec![PORT_EMBEDDINGS])
        .with_default_config(vec![
            (
                CONFIG_MODEL.into(),
                AgentConfigEntry::new(AgentValue::string(DEFAULT_CONFIG_MODEL), "string")
                    .with_title("Model"),
            ),
            (
                CONFIG_OPTIONS.into(),
                AgentConfigEntry::new(AgentValue::string("{}"), "text").with_title("Options"),
            ),
        ]),
    );
}
