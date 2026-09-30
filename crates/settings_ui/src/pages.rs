mod audio_input_output_setup;
// Only Zed calls used the audio test window, and Zaseo has none.
#[allow(dead_code, unused_imports)]
mod audio_test_window;
mod edit_prediction_provider_setup;
// Zed's own agent is off in Zaseo, so its settings pages are unreachable; they stay to keep
// upstream merges small.
#[allow(dead_code, unused_imports)]
mod external_agents_page;
mod feature_flags;
#[allow(dead_code, unused_imports)]
mod llm_providers_page;
#[allow(dead_code, unused_imports)]
mod mcp_servers_page;
#[allow(dead_code, unused_imports)]
mod sandbox_settings;
mod skill_creator;
#[allow(dead_code, unused_imports)]
mod skills_setup;
#[allow(dead_code, unused_imports)]
mod tool_permissions_setup;

pub(crate) use audio_input_output_setup::{
    render_input_audio_device_dropdown, render_output_audio_device_dropdown,
};
pub(crate) use edit_prediction_provider_setup::render_edit_prediction_setup_page;
pub(crate) use external_agents_page::{CustomAgentForm, render_add_agent_popover};
pub(crate) use feature_flags::render_feature_flags_page;
pub(crate) use llm_providers_page::{LlmProviderForm, render_add_llm_provider_popover};
pub(crate) use mcp_servers_page::{McpServerForm, render_add_server_popover};
pub use skill_creator::SkillCreatorOpenMode;
pub(crate) use skill_creator::{
    SkillCreatorEvent, SkillCreatorPage, render_skill_creator_page, skill_url_from_clipboard,
};

pub use tool_permissions_setup::{
    render_copy_path_tool_config, render_create_directory_tool_config,
    render_delete_path_tool_config, render_edit_file_tool_config, render_fetch_tool_config,
    render_move_path_tool_config, render_skill_tool_config, render_terminal_tool_config,
    render_web_search_tool_config, render_write_file_tool_config,
};
