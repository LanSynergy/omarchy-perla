//! Tool layer for perla-voice: definitions the realtime model sees, the
//! dispatcher trait hosts implement (or compose), and the built-in fast tools.
//!
//! Port of `Perla/Realtime/ToolDefs.swift` + `ToolDispatcher.swift`.

pub mod assist;
pub mod dispatcher;
pub mod fast;
pub mod omarchy;
pub mod prompt;
pub mod registry;
pub mod types;

pub use assist::{assist_tools, AssistDispatcher, AssistLayer, ASSIST_TOOL_NAMES};
pub use dispatcher::{ToolCallContext, ToolDispatcher};
pub use omarchy::{omarchy_tools, LayeredDispatcher, OmarchyDispatcher, OMARCHY_TOOL_NAMES};
pub use prompt::build_desktop_instructions;
pub use registry::{builder_tools, hands_tools, herdr_tools};
pub use types::{ToolDef, ToolResult};
