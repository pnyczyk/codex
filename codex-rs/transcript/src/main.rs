use std::io::BufRead;
use std::io::IsTerminal;

use anyhow::Context;
use anyhow::Result;
use clap::Parser;
use codex_core::config::Config;
use codex_core::config::ConfigOverrides;
use codex_core::config::ConfigToml;
use codex_core::config::find_codex_home;
use codex_exec::Color;
use codex_exec::EventProcessor;
use codex_exec::EventProcessorWithHumanOutput;
use codex_protocol::config_types::SandboxMode;
use codex_protocol::protocol::AgentMessageEvent;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::RolloutItem;
use codex_protocol::protocol::RolloutLine;
use codex_protocol::protocol::SandboxPolicy;
use codex_protocol::protocol::SessionMetaLine;
use codex_protocol::protocol::TurnContextItem;

#[derive(Parser, Debug)]
#[command(version)]
struct Cli {
    /// Controls ANSI color output.
    #[arg(long = "color", value_enum, default_value_t = Color::Auto)]
    color: Color,

    /// Hide agent reasoning blocks in the transcript output.
    #[arg(long = "hide-reasoning", default_value_t = false)]
    hide_reasoning: bool,

    /// Include raw agent reasoning content when available.
    #[arg(long = "show-raw-reasoning", default_value_t = false)]
    show_raw_reasoning: bool,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    let stdout_with_ansi = match cli.color {
        Color::Always => true,
        Color::Never => false,
        Color::Auto => std::io::stdout().is_terminal(),
    };

    let stdin = std::io::stdin();
    let mut items = Vec::new();
    let mut first_session_meta: Option<SessionMetaLine> = None;
    let mut first_turn_context: Option<TurnContextItem> = None;
    let mut first_user_message: Option<String> = None;

    for line_result in stdin.lock().lines() {
        let line = line_result?;
        if line.trim().is_empty() {
            continue;
        }
        let rollout_line: RolloutLine = serde_json::from_str(&line)
            .with_context(|| format!("failed to parse rollout line as JSON: {line}"))?;
        let item = rollout_line.item;
        match &item {
            RolloutItem::SessionMeta(meta) => {
                if first_session_meta.is_none() {
                    first_session_meta = Some(meta.clone());
                }
            }
            RolloutItem::TurnContext(ctx) => {
                if first_turn_context.is_none() {
                    first_turn_context = Some(ctx.clone());
                }
            }
            RolloutItem::EventMsg(EventMsg::UserMessage(user)) => {
                if first_user_message.is_none() {
                    first_user_message = Some(user.message.clone());
                }
            }
            _ => {}
        }
        items.push(item);
    }

    let config = build_config(
        first_turn_context.as_ref(),
        first_session_meta.as_ref(),
        &cli,
    )?;

    let prompt = first_user_message
        .or_else(|| {
            first_session_meta
                .as_ref()
                .and_then(|meta| meta.meta.instructions.clone())
        })
        .unwrap_or_default();

    let mut processor =
        EventProcessorWithHumanOutput::create_with_ansi(stdout_with_ansi, &config, None);
    processor.print_config_summary(&config, &prompt);

    for item in items {
        match item {
            RolloutItem::EventMsg(msg) => {
                let event = Event {
                    id: String::new(),
                    msg,
                };
                let _ = processor.process_event(event);
            }
            RolloutItem::Compacted(compacted) => {
                let event = Event {
                    id: String::new(),
                    msg: EventMsg::AgentMessage(AgentMessageEvent {
                        message: compacted.message,
                    }),
                };
                let _ = processor.process_event(event);
            }
            RolloutItem::SessionMeta(_)
            | RolloutItem::ResponseItem(_)
            | RolloutItem::TurnContext(_) => {
                // Already handled via config/prompt extraction.
            }
        }
    }

    Ok(())
}

fn build_config(
    turn_context: Option<&TurnContextItem>,
    session_meta: Option<&SessionMetaLine>,
    cli: &Cli,
) -> Result<Config> {
    let sandbox_mode_override = turn_context.map(|ctx| match &ctx.sandbox_policy {
        SandboxPolicy::DangerFullAccess => SandboxMode::DangerFullAccess,
        SandboxPolicy::ReadOnly => SandboxMode::ReadOnly,
        SandboxPolicy::WorkspaceWrite { .. } => SandboxMode::WorkspaceWrite,
    });

    let overrides = ConfigOverrides {
        model: turn_context.map(|ctx| ctx.model.clone()),
        review_model: None,
        cwd: turn_context
            .map(|ctx| ctx.cwd.clone())
            .or_else(|| session_meta.map(|meta| meta.meta.cwd.clone())),
        approval_policy: turn_context.map(|ctx| ctx.approval_policy),
        sandbox_mode: sandbox_mode_override,
        model_provider: None,
        config_profile: None,
        codex_linux_sandbox_exe: None,
        base_instructions: None,
        include_plan_tool: None,
        include_apply_patch_tool: None,
        include_view_image_tool: None,
        show_raw_agent_reasoning: Some(cli.show_raw_reasoning),
        tools_web_search_request: None,
    };

    let codex_home = find_codex_home()?;
    let mut config =
        Config::load_from_base_config_with_overrides(ConfigToml::default(), overrides, codex_home)?;

    if let Some(meta) = session_meta
        && config.user_instructions.is_none() {
            config.user_instructions = meta.meta.instructions.clone();
        }

    if let Some(ctx) = turn_context {
        config.model = ctx.model.clone();
        config.cwd = ctx.cwd.clone();
        config.approval_policy = ctx.approval_policy;
        config.sandbox_policy = ctx.sandbox_policy.clone();
        config.model_reasoning_effort = ctx.effort;
        config.model_reasoning_summary = ctx.summary;
    }

    config.hide_agent_reasoning = cli.hide_reasoning;
    config.show_raw_agent_reasoning = cli.show_raw_reasoning;

    Ok(config)
}
