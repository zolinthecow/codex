use crate::history_cell::CompositeHistoryCell;
use crate::history_cell::HistoryCell;
use crate::history_cell::PlainHistoryCell;
use crate::history_cell::with_border_with_inner_width;
use crate::version::CODEX_CLI_VERSION;
use codex_common::create_config_summary_entries;
use codex_core::config::Config;
use codex_core::protocol::SandboxPolicy;
use codex_core::protocol::TokenUsage;
use codex_protocol::mcp_protocol::ConversationId;
use ratatui::prelude::*;
use ratatui::style::Stylize;
use std::collections::BTreeSet;
use std::path::PathBuf;

use super::account::StatusAccountDisplay;
use super::format::FieldFormatter;
use super::format::line_display_width;
use super::format::push_label;
use super::format::truncate_line_to_width;
use super::helpers::compose_account_display;
use super::helpers::compose_agents_summary;
use super::helpers::compose_model_display;
use super::helpers::format_directory_display;
use super::helpers::format_tokens_compact;
use super::rate_limits::RESET_BULLET;
use super::rate_limits::RateLimitSnapshotDisplay;
use super::rate_limits::StatusRateLimitData;
use super::rate_limits::compose_rate_limit_data;
use super::rate_limits::format_status_limit_summary;
use super::rate_limits::render_status_limit_progress_bar;

#[derive(Debug, Clone)]
pub(crate) struct StatusTokenUsageData {
    total: u64,
    input: u64,
    output: u64,
}

#[derive(Debug)]
struct StatusHistoryCell {
    model_name: String,
    model_details: Vec<String>,
    directory: PathBuf,
    approval: String,
    sandbox: String,
    agents_summary: String,
    account: Option<StatusAccountDisplay>,
    session_id: Option<String>,
    token_usage: StatusTokenUsageData,
    rate_limits: StatusRateLimitData,
}

pub(crate) fn new_status_output(
    config: &Config,
    usage: &TokenUsage,
    session_id: &Option<ConversationId>,
    rate_limits: Option<&RateLimitSnapshotDisplay>,
) -> CompositeHistoryCell {
    let command = PlainHistoryCell::new(vec!["/status".magenta().into()]);
    let card = StatusHistoryCell::new(config, usage, session_id, rate_limits);

    CompositeHistoryCell::new(vec![Box::new(command), Box::new(card)])
}

impl StatusHistoryCell {
    fn new(
        config: &Config,
        usage: &TokenUsage,
        session_id: &Option<ConversationId>,
        rate_limits: Option<&RateLimitSnapshotDisplay>,
    ) -> Self {
        let config_entries = create_config_summary_entries(config);
        let (model_name, model_details) = compose_model_display(config, &config_entries);
        let approval = config_entries
            .iter()
            .find(|(k, _)| *k == "approval")
            .map(|(_, v)| v.clone())
            .unwrap_or_else(|| "<unknown>".to_string());
        let sandbox = match &config.sandbox_policy {
            SandboxPolicy::DangerFullAccess => "danger-full-access".to_string(),
            SandboxPolicy::ReadOnly => "read-only".to_string(),
            SandboxPolicy::WorkspaceWrite { .. } => "workspace-write".to_string(),
        };
        let agents_summary = compose_agents_summary(config);
        let account = compose_account_display(config);
        let session_id = session_id.as_ref().map(std::string::ToString::to_string);
        let token_usage = StatusTokenUsageData {
            total: usage.blended_total(),
            input: usage.non_cached_input(),
            output: usage.output_tokens,
        };
        let rate_limits = compose_rate_limit_data(rate_limits);

        Self {
            model_name,
            model_details,
            directory: config.cwd.clone(),
            approval,
            sandbox,
            agents_summary,
            account,
            session_id,
            token_usage,
            rate_limits,
        }
    }

    fn token_usage_spans(&self) -> Vec<Span<'static>> {
        let total_fmt = format_tokens_compact(self.token_usage.total);
        let input_fmt = format_tokens_compact(self.token_usage.input);
        let output_fmt = format_tokens_compact(self.token_usage.output);

        vec![
            Span::from(total_fmt),
            Span::from(" total "),
            Span::from(" (").dim(),
            Span::from(input_fmt).dim(),
            Span::from(" input").dim(),
            Span::from(" + ").dim(),
            Span::from(output_fmt).dim(),
            Span::from(" output").dim(),
            Span::from(")").dim(),
        ]
    }

    fn rate_limit_lines(
        &self,
        available_inner_width: usize,
        formatter: &FieldFormatter,
    ) -> Vec<Line<'static>> {
        match &self.rate_limits {
            StatusRateLimitData::Available(rows_data) => {
                if rows_data.is_empty() {
                    return vec![
                        formatter.line("Limits", vec![Span::from("data not available yet").dim()]),
                    ];
                }

                let mut lines = Vec::with_capacity(rows_data.len() * 2);

                for row in rows_data {
                    let value_spans = vec![
                        Span::from(render_status_limit_progress_bar(row.percent_used)),
                        Span::from(" "),
                        Span::from(format_status_limit_summary(row.percent_used)),
                    ];
                    let base_spans = formatter.full_spans(row.label, value_spans);
                    let base_line = Line::from(base_spans.clone());

                    if let Some(resets_at) = row.resets_at.as_ref() {
                        let resets_span =
                            Span::from(format!("{RESET_BULLET} resets {resets_at}")).dim();
                        let mut inline_spans = base_spans.clone();
                        inline_spans.push(Span::from(" ").dim());
                        inline_spans.push(resets_span.clone());

                        if line_display_width(&Line::from(inline_spans.clone()))
                            <= available_inner_width
                        {
                            lines.push(Line::from(inline_spans));
                        } else {
                            lines.push(base_line);
                            lines.push(formatter.continuation(vec![resets_span]));
                        }
                    } else {
                        lines.push(base_line);
                    }
                }

                lines
            }
            StatusRateLimitData::Missing => {
                vec![formatter.line("Limits", vec![Span::from("data not available yet").dim()])]
            }
        }
    }

    fn collect_rate_limit_labels(
        &self,
        seen: &mut BTreeSet<&'static str>,
        labels: &mut Vec<&'static str>,
    ) {
        match &self.rate_limits {
            StatusRateLimitData::Available(rows) => {
                if rows.is_empty() {
                    push_label(labels, seen, "Limits");
                } else {
                    for row in rows {
                        push_label(labels, seen, row.label);
                    }
                }
            }
            StatusRateLimitData::Missing => push_label(labels, seen, "Limits"),
        }
    }
}

impl HistoryCell for StatusHistoryCell {
    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        let mut lines: Vec<Line<'static>> = Vec::new();
        lines.push(Line::from(vec![
            Span::from(format!("{}>_ ", FieldFormatter::INDENT)).dim(),
            Span::from("OpenAI Codex").bold(),
            Span::from(" ").dim(),
            Span::from(format!("(v{CODEX_CLI_VERSION})")).dim(),
        ]));
        lines.push(Line::from(Vec::<Span<'static>>::new()));

        let available_inner_width = usize::from(width.saturating_sub(4));
        if available_inner_width == 0 {
            return Vec::new();
        }

        let account_value = self.account.as_ref().map(|account| match account {
            StatusAccountDisplay::ChatGpt { email, plan } => match (email, plan) {
                (Some(email), Some(plan)) => format!("{email} ({plan})"),
                (Some(email), None) => email.clone(),
                (None, Some(plan)) => plan.clone(),
                (None, None) => "ChatGPT".to_string(),
            },
            StatusAccountDisplay::ApiKey => {
                "API key configured (run codex login to use ChatGPT)".to_string()
            }
        });

        let mut labels: Vec<&'static str> =
            vec!["Model", "Directory", "Approval", "Sandbox", "Agents.md"];
        let mut seen: BTreeSet<&'static str> = labels.iter().copied().collect();

        if account_value.is_some() {
            push_label(&mut labels, &mut seen, "Account");
        }
        if self.session_id.is_some() {
            push_label(&mut labels, &mut seen, "Session");
        }
        push_label(&mut labels, &mut seen, "Token Usage");
        self.collect_rate_limit_labels(&mut seen, &mut labels);

        let formatter = FieldFormatter::from_labels(labels.iter().copied());
        let value_width = formatter.value_width(available_inner_width);

        let mut model_spans = vec![Span::from(self.model_name.clone())];
        if !self.model_details.is_empty() {
            model_spans.push(Span::from(" (").dim());
            model_spans.push(Span::from(self.model_details.join(", ")).dim());
            model_spans.push(Span::from(")").dim());
        }

        let directory_value = format_directory_display(&self.directory, Some(value_width));

        lines.push(formatter.line("Model", model_spans));
        lines.push(formatter.line("Directory", vec![Span::from(directory_value)]));
        lines.push(formatter.line("Approval", vec![Span::from(self.approval.clone())]));
        lines.push(formatter.line("Sandbox", vec![Span::from(self.sandbox.clone())]));
        lines.push(formatter.line("Agents.md", vec![Span::from(self.agents_summary.clone())]));

        if let Some(account_value) = account_value {
            lines.push(formatter.line("Account", vec![Span::from(account_value)]));
        }

        if let Some(session) = self.session_id.as_ref() {
            lines.push(formatter.line("Session", vec![Span::from(session.clone())]));
        }

        lines.push(Line::from(Vec::<Span<'static>>::new()));
        lines.push(formatter.line("Token Usage", self.token_usage_spans()));

        lines.extend(self.rate_limit_lines(available_inner_width, &formatter));

        let content_width = lines.iter().map(line_display_width).max().unwrap_or(0);
        let inner_width = content_width.min(available_inner_width);
        let truncated_lines: Vec<Line<'static>> = lines
            .into_iter()
            .map(|line| truncate_line_to_width(line, inner_width))
            .collect();

        with_border_with_inner_width(truncated_lines, inner_width)
    }
}
