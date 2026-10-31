//! The built-in plugins compiled into `ch`, and how the CLI hands options to the
//! frontends.

use std::sync::Arc;

use ch_kernel::{Plugin, Services};

/// Every built-in plugin compiled into this binary, one per cargo feature.
///
/// Order doesn't matter for correctness (the registry orders plugins by `requires`), but
/// it is the order `ch --list-plugins` uses for plugins that don't depend on each other.
// One `push` per feature keeps each plugin a one-line change.
#[allow(clippy::vec_init_then_push)]
pub fn builtin_plugins() -> Vec<Arc<dyn Plugin>> {
    #[allow(unused_mut)]
    let mut plugins: Vec<Arc<dyn Plugin>> = Vec::new();

    // Model providers.
    #[cfg(feature = "provider-anthropic")]
    plugins.push(ch_provider_anthropic::plugin());
    #[cfg(feature = "provider-openai")]
    plugins.push(ch_provider_openai::plugin());
    #[cfg(feature = "provider-scripted")]
    plugins.push(ch_provider_scripted::plugin());

    // Safety, executors and tools.
    #[cfg(feature = "safety")]
    plugins.push(ch_safety::plugin());
    #[cfg(feature = "executor-local")]
    plugins.push(ch_executor_local::plugin());
    // #[cfg(feature = "executor-docker")]
    // plugins.push(ch_executor_docker::plugin());
    #[cfg(feature = "tools-fs")]
    plugins.push(ch_tools_fs::plugin());
    #[cfg(feature = "tool-bash")]
    plugins.push(ch_tool_bash::plugin());
    #[cfg(feature = "tool-web-fetch")]
    plugins.push(ch_tool_web_fetch::plugin());

    // Harness components.
    // #[cfg(feature = "stuck")]
    // plugins.push(ch_stuck::plugin());
    #[cfg(feature = "diagnostics")]
    plugins.push(ch_diagnostics::plugin());
    #[cfg(feature = "planning")]
    plugins.push(ch_planning::plugin());
    // #[cfg(feature = "context")]
    // plugins.push(ch_context::plugin());

    // Prompts and commands.
    #[cfg(feature = "prompt-base")]
    plugins.push(ch_prompt_base::plugin());
    #[cfg(feature = "project-instructions")]
    plugins.push(ch_project_instructions::plugin());
    #[cfg(feature = "commands-core")]
    plugins.push(ch_commands_core::plugin());
    #[cfg(feature = "commands-markdown")]
    plugins.push(ch_commands_markdown::plugin());

    // Outside plugins.
    // #[cfg(feature = "mcp-bridge")]
    // plugins.push(ch_mcp_bridge::plugin());
    // #[cfg(feature = "external-host")]
    // plugins.push(ch_external_host::plugin());
    // #[cfg(feature = "hooks-shell")]
    // plugins.push(ch_hooks_shell::plugin());

    // Telemetry.
    // #[cfg(feature = "telemetry-jsonl")]
    // plugins.push(ch_telemetry_jsonl::plugin());
    // #[cfg(feature = "telemetry-langfuse")]
    // plugins.push(ch_telemetry_langfuse::plugin());

    // Frontends.
    #[cfg(feature = "frontend-repl")]
    plugins.push(ch_frontend_repl::plugin());
    #[cfg(feature = "frontend-print")]
    plugins.push(ch_frontend_print::plugin());

    plugins
}

/// Hands the print frontend its task, as a service the frontend reads from the registry.
/// Fails if this binary has no print frontend.
pub fn print_options(
    services: &mut Services,
    prompt: String,
    output_format: &str,
    verbose: bool,
) -> anyhow::Result<()> {
    #[cfg(feature = "frontend-print")]
    {
        services.provide(Arc::new(ch_frontend_print::PrintOptions {
            prompt,
            output_format: output_format.parse()?,
            verbose,
        }));
        Ok(())
    }
    #[cfg(not(feature = "frontend-print"))]
    {
        let _ = (services, prompt, output_format, verbose);
        anyhow::bail!("this ch was built without the frontend-print feature, so -p is unavailable")
    }
}

/// Called with the rule after the user saves an "always" rule from the REPL.
pub type OnRuleSaved = Box<dyn Fn(&str) + Send + Sync>;

/// Hands the REPL its first prompt, if any, and what to call after the user saves an
/// "always" rule. A no-op without the REPL frontend.
pub fn repl_options(
    services: &mut Services,
    initial_prompt: Option<String>,
    on_rule_saved: Option<OnRuleSaved>,
) {
    #[cfg(feature = "frontend-repl")]
    {
        services.provide(Arc::new(ch_frontend_repl::ReplOptions { initial_prompt }));
        if let Some(f) = on_rule_saved {
            services.provide(Arc::new(ch_frontend_repl::RuleSaved(f)));
        }
    }
    #[cfg(not(feature = "frontend-repl"))]
    let _ = (services, initial_prompt, on_rule_saved);
}
