use anyhow::Result;
use serde_json::{Value, json};

use crate::core::tools::{Tool, ToolDescriptionLength};

pub struct Restart;

impl Tool for Restart {
    fn name(&self) -> &str { crate::core::tools::tool_names::RESTART }
    fn category(&self) -> crate::core::tools::ToolCategory { crate::core::tools::ToolCategory::Shell }

    fn description(&self) -> &str {
        "Restart the skald process. \
         Exits with code -1, signalling the supervisor (run.sh) to rebuild and relaunch. \
         Use this after editing the source code to load the new version. \
         Requires user approval."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {}
        })
    }

    fn describe(&self, _args: &Value, _length: ToolDescriptionLength) -> String {
        "restart skald".to_string()
    }

    fn execute(&self, _args: Value) -> Result<String> {
        // Use _exit() instead of exit() to skip C atexit handlers (e.g. Metal GPU cleanup
        // in whisper-rs which crashes with SIGABRT and produces exit code 134 instead of 255).
        unsafe { libc::_exit(-1) }
    }
}
