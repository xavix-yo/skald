/// Application name, sent as `X-Title` HTTP header to LLM/image/audio providers.
pub const APP_NAME: &str = "Skald";

pub mod approval;
pub mod bus;
pub mod system_bus;
pub mod chatbot;
pub mod chat_hub;
pub mod events;
pub mod image_generate;
pub mod interface_tool;
pub mod location;
pub mod memory;
pub mod plugin;
pub mod provider;
pub mod remote;
pub mod tool;
pub mod secrets;
pub mod transcribe;
pub mod tts;
