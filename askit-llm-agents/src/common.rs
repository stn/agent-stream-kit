use std::vec;

use agent_stream_kit::{
    ASKit, Agent, AgentConfigEntry, AgentConfigs, AgentContext, AgentData, AgentDefinition,
    AgentError, AgentOutput, AsAgent, AsAgentData, async_trait, new_agent_boxed,
};

use crate::message::Message;

// User Message Agent
pub struct UserMessageAgent {
    data: AsAgentData,
}

#[async_trait]
impl AsAgent for UserMessageAgent {
    fn new(
        askit: ASKit,
        id: String,
        def_name: String,
        config: Option<AgentConfigs>,
    ) -> Result<Self, AgentError> {
        Ok(Self {
            data: AsAgentData::new(askit, id, def_name, config),
        })
    }

    fn data(&self) -> &AsAgentData {
        &self.data
    }

    fn mut_data(&mut self) -> &mut AsAgentData {
        &mut self.data
    }

    async fn process(
        &mut self,
        ctx: AgentContext,
        _pin: String,
        data: AgentData,
    ) -> Result<(), AgentError> {
        let value = self.configs()?.get_string(CONFIG_MESSAGE)?;
        let message = Message::user(value);
        let messages = add_message(data, message);
        self.try_output(ctx, PORT_MESSAGES, messages)?;
        Ok(())
    }
}

fn add_message(data: AgentData, message: Message) -> AgentData {
    if data.is_array() {
        let mut arr = data.as_array().unwrap_or(&vec![]).to_owned();
        arr.push(message.into());
        return AgentData::array("message", arr);
    }

    let value = data.as_str().unwrap_or("");
    if !value.is_empty() {
        let in_message = Message::user(value.to_string());
        return AgentData::array("message", vec![message.into(), in_message.into()]);
    }

    AgentData::array("message", vec![message.into()])
}

static AGENT_KIND: &str = "agent";
static CATEGORY: &str = "LLM";

static PORT_MESSAGES: &str = "messages";

static CONFIG_MESSAGE: &str = "message";

pub fn register_agents(askit: &ASKit) {
    askit.register_agent(
        AgentDefinition::new(
            AGENT_KIND,
            "llm_user_message",
            Some(new_agent_boxed::<UserMessageAgent>),
        )
        // .use_native_thread()
        .with_title("User Message")
        .with_category(CATEGORY)
        .with_inputs(vec![PORT_MESSAGES])
        .with_outputs(vec![PORT_MESSAGES])
        .with_default_configs(vec![(CONFIG_MESSAGE, AgentConfigEntry::new("", "string"))]),
    );
}
