use super::new_status_output;
use super::rate_limit_snapshot_display;
use crate::history_cell::HistoryCell;
use chrono::TimeZone;
use codex_core::config::Config;
use codex_core::config::ConfigOverrides;
use codex_core::config::ConfigToml;
use codex_core::protocol::RateLimitSnapshot;
use codex_core::protocol::RateLimitWindow;
use codex_core::protocol::SandboxPolicy;
use codex_core::protocol::TokenUsage;
use codex_protocol::config_types::ReasoningEffort;
use codex_protocol::config_types::ReasoningSummary;
use insta::assert_snapshot;
use ratatui::prelude::*;
use std::path::PathBuf;
use tempfile::TempDir;

fn test_config(temp_home: &TempDir) -> Config {
    Config::load_from_base_config_with_overrides(
        ConfigToml::default(),
        ConfigOverrides::default(),
        temp_home.path().to_path_buf(),
    )
    .expect("load config")
}

fn render_lines(lines: &[Line<'static>]) -> Vec<String> {
    lines
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        })
        .collect()
}

fn sanitize_directory(lines: Vec<String>) -> Vec<String> {
    lines
        .into_iter()
        .map(|line| {
            if let (Some(dir_pos), Some(pipe_idx)) = (line.find("Directory: "), line.rfind('│')) {
                let prefix = &line[..dir_pos + "Directory: ".len()];
                let suffix = &line[pipe_idx..];
                let content_width = pipe_idx.saturating_sub(dir_pos + "Directory: ".len());
                let replacement = "[[workspace]]";
                let mut rebuilt = prefix.to_string();
                rebuilt.push_str(replacement);
                if content_width > replacement.len() {
                    rebuilt.push_str(&" ".repeat(content_width - replacement.len()));
                }
                rebuilt.push_str(suffix);
                rebuilt
            } else {
                line
            }
        })
        .collect()
}

#[test]
fn status_snapshot_includes_reasoning_details() {
    let temp_home = TempDir::new().expect("temp home");
    let mut config = test_config(&temp_home);
    config.model = "gpt-5-codex".to_string();
    config.model_provider_id = "openai".to_string();
    config.model_reasoning_effort = Some(ReasoningEffort::High);
    config.model_reasoning_summary = ReasoningSummary::Detailed;
    config.sandbox_policy = SandboxPolicy::WorkspaceWrite {
        writable_roots: Vec::new(),
        network_access: false,
        exclude_tmpdir_env_var: false,
        exclude_slash_tmp: false,
    };

    config.cwd = PathBuf::from("/workspace/tests");

    let usage = TokenUsage {
        input_tokens: 1_200,
        cached_input_tokens: 200,
        output_tokens: 900,
        reasoning_output_tokens: 150,
        total_tokens: 2_250,
    };

    let snapshot = RateLimitSnapshot {
        primary: Some(RateLimitWindow {
            used_percent: 72.5,
            window_minutes: Some(300),
            resets_in_seconds: Some(600),
        }),
        secondary: Some(RateLimitWindow {
            used_percent: 45.0,
            window_minutes: Some(1_440),
            resets_in_seconds: Some(1_200),
        }),
    };
    let captured_at = chrono::Local
        .with_ymd_and_hms(2024, 1, 2, 3, 4, 5)
        .single()
        .expect("timestamp");
    let rate_display = rate_limit_snapshot_display(&snapshot, captured_at);

    let composite = new_status_output(&config, &usage, &None, Some(&rate_display));
    let mut rendered_lines = render_lines(&composite.display_lines(80));
    if cfg!(windows) {
        for line in &mut rendered_lines {
            *line = line.replace('\\', "/");
        }
    }
    let sanitized = sanitize_directory(rendered_lines).join("\n");
    assert_snapshot!(sanitized);
}

#[test]
fn status_card_token_usage_excludes_cached_tokens() {
    let temp_home = TempDir::new().expect("temp home");
    let mut config = test_config(&temp_home);
    config.model = "gpt-5-codex".to_string();
    config.cwd = PathBuf::from("/workspace/tests");

    let usage = TokenUsage {
        input_tokens: 1_200,
        cached_input_tokens: 200,
        output_tokens: 900,
        reasoning_output_tokens: 0,
        total_tokens: 2_100,
    };

    let composite = new_status_output(&config, &usage, &None, None);
    let rendered = render_lines(&composite.display_lines(120));

    assert!(
        rendered.iter().all(|line| !line.contains("cached")),
        "cached tokens should not be displayed, got: {rendered:?}"
    );
}

#[test]
fn status_snapshot_truncates_in_narrow_terminal() {
    let temp_home = TempDir::new().expect("temp home");
    let mut config = test_config(&temp_home);
    config.model = "gpt-5-codex".to_string();
    config.model_provider_id = "openai".to_string();
    config.model_reasoning_effort = Some(ReasoningEffort::High);
    config.model_reasoning_summary = ReasoningSummary::Detailed;
    config.cwd = PathBuf::from("/workspace/tests");

    let usage = TokenUsage {
        input_tokens: 1_200,
        cached_input_tokens: 200,
        output_tokens: 900,
        reasoning_output_tokens: 150,
        total_tokens: 2_250,
    };

    let snapshot = RateLimitSnapshot {
        primary: Some(RateLimitWindow {
            used_percent: 72.5,
            window_minutes: Some(300),
            resets_in_seconds: Some(600),
        }),
        secondary: None,
    };
    let captured_at = chrono::Local
        .with_ymd_and_hms(2024, 1, 2, 3, 4, 5)
        .single()
        .expect("timestamp");
    let rate_display = rate_limit_snapshot_display(&snapshot, captured_at);

    let composite = new_status_output(&config, &usage, &None, Some(&rate_display));
    let mut rendered_lines = render_lines(&composite.display_lines(46));
    if cfg!(windows) {
        for line in &mut rendered_lines {
            *line = line.replace('\\', "/");
        }
    }
    let sanitized = sanitize_directory(rendered_lines).join("\n");

    assert_snapshot!(sanitized);
}
