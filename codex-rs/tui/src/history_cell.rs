use crate::diff_render::create_diff_summary;
use crate::exec_command::relativize_to_home;
use crate::exec_command::strip_bash_lc_and_escape;
use crate::markdown::append_markdown;
use crate::render::line_utils::line_to_static;
use crate::render::line_utils::prefix_lines;
use crate::render::line_utils::push_owned_lines;
pub(crate) use crate::status::RateLimitSnapshotDisplay;
pub(crate) use crate::status::new_status_output;
pub(crate) use crate::status::rate_limit_snapshot_display;
use crate::text_formatting::format_and_truncate_tool_result;
use crate::ui_consts::LIVE_PREFIX_COLS;
use crate::wrapping::RtOptions;
use crate::wrapping::word_wrap_line;
use crate::wrapping::word_wrap_lines;
use base64::Engine;
use codex_ansi_escape::ansi_escape_line;
use codex_common::elapsed::format_duration;
use codex_core::config::Config;
use codex_core::config_types::ReasoningSummaryFormat;
use codex_core::plan_tool::PlanItemArg;
use codex_core::plan_tool::StepStatus;
use codex_core::plan_tool::UpdatePlanArgs;
use codex_core::protocol::FileChange;
use codex_core::protocol::McpInvocation;
use codex_core::protocol::SessionConfiguredEvent;
use codex_core::protocol_config_types::ReasoningEffort as ReasoningEffortConfig;
use codex_protocol::parse_command::ParsedCommand;
use image::DynamicImage;
use image::ImageReader;
use itertools::Itertools;
use mcp_types::EmbeddedResourceResource;
use mcp_types::ResourceLink;
use ratatui::prelude::*;
use ratatui::style::Modifier;
use ratatui::style::Style;
use ratatui::style::Styled;
use ratatui::style::Stylize;
use ratatui::widgets::Paragraph;
use ratatui::widgets::WidgetRef;
use ratatui::widgets::Wrap;
use std::any::Any;
use std::collections::HashMap;
use std::io::Cursor;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;
use std::time::Instant;
use tracing::error;
use unicode_width::UnicodeWidthStr;

#[derive(Clone, Debug)]
pub(crate) struct CommandOutput {
    pub(crate) exit_code: i32,
    pub(crate) stdout: String,
    pub(crate) stderr: String,
    pub(crate) formatted_output: String,
}

#[derive(Clone, Debug)]
pub(crate) enum PatchEventType {
    ApprovalRequest,
    ApplyBegin { auto_approved: bool },
}

/// Represents an event to display in the conversation history. Returns its
/// `Vec<Line<'static>>` representation to make it easier to display in a
/// scrollable list.
pub(crate) trait HistoryCell: std::fmt::Debug + Send + Sync + Any {
    fn display_lines(&self, width: u16) -> Vec<Line<'static>>;

    fn transcript_lines(&self) -> Vec<Line<'static>> {
        self.display_lines(u16::MAX)
    }

    fn desired_height(&self, width: u16) -> u16 {
        Paragraph::new(Text::from(self.display_lines(width)))
            .wrap(Wrap { trim: false })
            .line_count(width)
            .try_into()
            .unwrap_or(0)
    }

    fn is_stream_continuation(&self) -> bool {
        false
    }
}

impl dyn HistoryCell {
    pub(crate) fn as_any(&self) -> &dyn Any {
        self
    }

    pub(crate) fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

#[derive(Debug)]
pub(crate) struct UserHistoryCell {
    pub message: String,
}

impl HistoryCell for UserHistoryCell {
    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        let mut lines: Vec<Line<'static>> = Vec::new();

        // Wrap the content first, then prefix each wrapped line with the marker.
        let wrap_width = width.saturating_sub(LIVE_PREFIX_COLS); // account for the ▌ prefix and trailing space
        let wrapped = textwrap::wrap(
            &self.message,
            textwrap::Options::new(wrap_width as usize)
                .wrap_algorithm(textwrap::WrapAlgorithm::FirstFit), // Match textarea wrap
        );

        for line in wrapped {
            lines.push(vec!["▌ ".cyan().dim(), line.to_string().dim()].into());
        }
        lines
    }

    fn transcript_lines(&self) -> Vec<Line<'static>> {
        let mut lines: Vec<Line<'static>> = Vec::new();
        lines.push("user".cyan().bold().into());
        lines.extend(self.message.lines().map(|l| l.to_string().into()));
        lines
    }
}

#[derive(Debug)]
pub(crate) struct ReasoningSummaryCell {
    _header: Vec<Line<'static>>,
    content: Vec<Line<'static>>,
}

impl ReasoningSummaryCell {
    pub(crate) fn new(header: Vec<Line<'static>>, content: Vec<Line<'static>>) -> Self {
        Self {
            _header: header,
            content,
        }
    }
}

impl HistoryCell for ReasoningSummaryCell {
    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        let summary_lines = self
            .content
            .iter()
            .map(|l| l.clone().dim().italic())
            .collect::<Vec<_>>();

        word_wrap_lines(
            &summary_lines,
            RtOptions::new(width as usize)
                .initial_indent("• ".into())
                .subsequent_indent("  ".into()),
        )
    }

    fn transcript_lines(&self) -> Vec<Line<'static>> {
        let mut out: Vec<Line<'static>> = Vec::new();
        out.push("thinking".magenta().bold().into());
        out.extend(self.content.clone());
        out
    }
}

#[derive(Debug)]
pub(crate) struct AgentMessageCell {
    lines: Vec<Line<'static>>,
    is_first_line: bool,
}

impl AgentMessageCell {
    pub(crate) fn new(lines: Vec<Line<'static>>, is_first_line: bool) -> Self {
        Self {
            lines,
            is_first_line,
        }
    }
}

impl HistoryCell for AgentMessageCell {
    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        word_wrap_lines(
            &self.lines,
            RtOptions::new(width as usize)
                .initial_indent(if self.is_first_line {
                    "> ".into()
                } else {
                    "  ".into()
                })
                .subsequent_indent("  ".into()),
        )
    }

    fn transcript_lines(&self) -> Vec<Line<'static>> {
        let mut out: Vec<Line<'static>> = Vec::new();
        if self.is_first_line {
            out.push("codex".magenta().bold().into());
        }
        out.extend(self.lines.clone());
        out
    }

    fn is_stream_continuation(&self) -> bool {
        !self.is_first_line
    }
}

#[derive(Debug)]
pub(crate) struct PlainHistoryCell {
    lines: Vec<Line<'static>>,
}

impl PlainHistoryCell {
    pub(crate) fn new(lines: Vec<Line<'static>>) -> Self {
        Self { lines }
    }
}

impl HistoryCell for PlainHistoryCell {
    fn display_lines(&self, _width: u16) -> Vec<Line<'static>> {
        self.lines.clone()
    }
}

#[derive(Debug)]
pub(crate) struct TranscriptOnlyHistoryCell {
    lines: Vec<Line<'static>>,
}

impl HistoryCell for TranscriptOnlyHistoryCell {
    fn display_lines(&self, _width: u16) -> Vec<Line<'static>> {
        Vec::new()
    }

    fn transcript_lines(&self) -> Vec<Line<'static>> {
        self.lines.clone()
    }
}

/// Cyan history cell line showing the current review status.
pub(crate) fn new_review_status_line(message: String) -> PlainHistoryCell {
    PlainHistoryCell {
        lines: vec![Line::from(message.cyan())],
    }
}

#[derive(Debug)]
pub(crate) struct PatchHistoryCell {
    event_type: PatchEventType,
    changes: HashMap<PathBuf, FileChange>,
    cwd: PathBuf,
}

impl HistoryCell for PatchHistoryCell {
    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        create_diff_summary(
            &self.changes,
            self.event_type.clone(),
            &self.cwd,
            width as usize,
        )
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ExecCall {
    pub(crate) call_id: String,
    pub(crate) command: Vec<String>,
    pub(crate) parsed: Vec<ParsedCommand>,
    pub(crate) output: Option<CommandOutput>,
    start_time: Option<Instant>,
    duration: Option<Duration>,
}

#[derive(Debug)]
pub(crate) struct ExecCell {
    calls: Vec<ExecCall>,
}
impl HistoryCell for ExecCell {
    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        if self.is_exploring_cell() {
            self.exploring_display_lines(width)
        } else {
            self.command_display_lines(width)
        }
    }

    fn transcript_lines(&self) -> Vec<Line<'static>> {
        let mut lines: Vec<Line<'static>> = vec![];
        for call in &self.calls {
            let cmd_display = strip_bash_lc_and_escape(&call.command);
            for (i, part) in cmd_display.lines().enumerate() {
                if i == 0 {
                    lines.push(vec!["$ ".magenta(), part.to_string().into()].into());
                } else {
                    lines.push(vec!["    ".into(), part.to_string().into()].into());
                }
            }

            if let Some(output) = call.output.as_ref() {
                lines.extend(output.formatted_output.lines().map(ansi_escape_line));
                let duration = call
                    .duration
                    .map(format_duration)
                    .unwrap_or_else(|| "unknown".to_string());
                let mut result: Line = if output.exit_code == 0 {
                    Line::from("✓".green().bold())
                } else {
                    Line::from(vec![
                        "✗".red().bold(),
                        format!(" ({})", output.exit_code).into(),
                    ])
                };
                result.push_span(format!(" • {duration}").dim());
                lines.push(result);
            }
            lines.push("".into());
        }
        lines
    }
}

impl ExecCell {
    fn is_active(&self) -> bool {
        self.calls.iter().any(|c| c.output.is_none())
    }

    fn exploring_display_lines(&self, width: u16) -> Vec<Line<'static>> {
        let mut out: Vec<Line<'static>> = Vec::new();
        let active_start_time = self
            .calls
            .iter()
            .find(|c| c.output.is_none())
            .and_then(|c| c.start_time);
        out.push(Line::from(vec![
            if self.is_active() {
                // Show an animated spinner while exploring
                spinner(active_start_time)
            } else {
                "•".bold()
            },
            " ".into(),
            if self.is_active() {
                "Exploring".bold()
            } else {
                "Explored".bold()
            },
        ]));
        let mut calls = self.calls.clone();
        let mut out_indented = Vec::new();
        while !calls.is_empty() {
            let mut call = calls.remove(0);
            if call
                .parsed
                .iter()
                .all(|c| matches!(c, ParsedCommand::Read { .. }))
            {
                while let Some(next) = calls.first() {
                    if next
                        .parsed
                        .iter()
                        .all(|c| matches!(c, ParsedCommand::Read { .. }))
                    {
                        call.parsed.extend(next.parsed.clone());
                        calls.remove(0);
                    } else {
                        break;
                    }
                }
            }
            let call_lines: Vec<(&str, Vec<Span<'static>>)> = if call
                .parsed
                .iter()
                .all(|c| matches!(c, ParsedCommand::Read { .. }))
            {
                let names = call
                    .parsed
                    .iter()
                    .map(|c| match c {
                        ParsedCommand::Read { name, .. } => name.clone(),
                        _ => unreachable!(),
                    })
                    .unique();
                vec![(
                    "Read",
                    itertools::Itertools::intersperse(
                        names.into_iter().map(Into::into),
                        ", ".dim(),
                    )
                    .collect(),
                )]
            } else {
                let mut lines = Vec::new();
                for p in call.parsed {
                    match p {
                        ParsedCommand::Read { name, .. } => {
                            lines.push(("Read", vec![name.into()]));
                        }
                        ParsedCommand::ListFiles { cmd, path } => {
                            lines.push(("List", vec![path.unwrap_or(cmd).into()]));
                        }
                        ParsedCommand::Search { cmd, query, path } => {
                            lines.push((
                                "Search",
                                match (query, path) {
                                    (Some(q), Some(p)) => {
                                        vec![q.into(), " in ".dim(), p.into()]
                                    }
                                    (Some(q), None) => vec![q.into()],
                                    _ => vec![cmd.into()],
                                },
                            ));
                        }
                        ParsedCommand::Unknown { cmd } => {
                            lines.push(("Run", vec![cmd.into()]));
                        }
                    }
                }
                lines
            };
            for (title, line) in call_lines {
                let line = Line::from(line);
                let initial_indent = Line::from(vec![title.cyan(), " ".into()]);
                let subsequent_indent = " ".repeat(initial_indent.width()).into();
                let wrapped = word_wrap_line(
                    &line,
                    RtOptions::new(width as usize)
                        .initial_indent(initial_indent)
                        .subsequent_indent(subsequent_indent),
                );
                push_owned_lines(&wrapped, &mut out_indented);
            }
        }
        out.extend(prefix_lines(out_indented, "  └ ".dim(), "    ".into()));
        out
    }

    fn command_display_lines(&self, width: u16) -> Vec<Line<'static>> {
        use textwrap::Options as TwOptions;

        let mut lines: Vec<Line<'static>> = Vec::new();
        let [call] = &self.calls.as_slice() else {
            panic!("Expected exactly one call in a command display cell");
        };
        let success = call.output.as_ref().map(|o| o.exit_code == 0);
        let bullet = match success {
            Some(true) => "•".green().bold(),
            Some(false) => "•".red().bold(),
            None => spinner(call.start_time),
        };
        let title = if self.is_active() { "Running" } else { "Ran" };
        let cmd_display = strip_bash_lc_and_escape(&call.command);

        // If the command fits on the same line as the header at the current width,
        // show a single compact line: "• Ran <command>". Use the width of
        // "• Running " (including trailing space) as the reserved prefix width.
        // If the command contains newlines, always use the multi-line variant.
        let reserved = "• Running ".width();

        let mut body_lines: Vec<Line<'static>> = Vec::new();

        let highlighted_lines = crate::render::highlight::highlight_bash_to_lines(&cmd_display);

        if highlighted_lines.len() == 1
            && highlighted_lines[0].width() < (width as usize).saturating_sub(reserved)
        {
            let mut line = Line::from(vec![bullet, " ".into(), title.bold(), " ".into()]);
            line.extend(highlighted_lines[0].clone());
            lines.push(line);
        } else {
            lines.push(vec![bullet, " ".into(), title.bold()].into());

            for hl_line in highlighted_lines.iter() {
                let opts = crate::wrapping::RtOptions::new((width as usize).saturating_sub(4))
                    .initial_indent("".into())
                    .subsequent_indent("    ".into())
                    // Hyphenation likes to break words on hyphens, which is bad for bash scripts --because-of-flags.
                    .word_splitter(textwrap::WordSplitter::NoHyphenation);
                let wrapped_borrowed = crate::wrapping::word_wrap_line(hl_line, opts);
                body_lines.extend(wrapped_borrowed.iter().map(|l| line_to_static(l)));
            }
        }
        if let Some(output) = call.output.as_ref()
            && output.exit_code != 0
        {
            let out = output_lines(
                Some(output),
                OutputLinesParams {
                    only_err: false,
                    include_angle_pipe: false,
                    include_prefix: false,
                },
            )
            .into_iter()
            .join("\n");
            if !out.trim().is_empty() {
                // Wrap the output.
                for line in out.lines() {
                    let wrapped = textwrap::wrap(line, TwOptions::new(width as usize - 4));
                    body_lines.extend(wrapped.into_iter().map(|l| Line::from(l.to_string().dim())));
                }
            }
        }
        lines.extend(prefix_lines(body_lines, "  └ ".dim(), "    ".into()));
        lines
    }
}

impl WidgetRef for &ExecCell {
    fn render_ref(&self, area: Rect, buf: &mut Buffer) {
        if area.height == 0 {
            return;
        }
        let content_area = Rect {
            x: area.x,
            y: area.y,
            width: area.width,
            height: area.height,
        };
        let lines = self.display_lines(area.width);
        let max_rows = area.height as usize;
        let rendered = if lines.len() > max_rows {
            // Keep the last `max_rows` lines in original order
            lines[lines.len() - max_rows..].to_vec()
        } else {
            lines
        };

        Paragraph::new(Text::from(rendered))
            .wrap(Wrap { trim: false })
            .render(content_area, buf);
    }
}

impl ExecCell {
    pub(crate) fn mark_failed(&mut self) {
        for call in self.calls.iter_mut() {
            if call.output.is_none() {
                let elapsed = call
                    .start_time
                    .map(|st| st.elapsed())
                    .unwrap_or_else(|| Duration::from_millis(0));
                call.start_time = None;
                call.duration = Some(elapsed);
                call.output = Some(CommandOutput {
                    exit_code: 1,
                    stdout: String::new(),
                    stderr: String::new(),
                    formatted_output: String::new(),
                });
            }
        }
    }

    pub(crate) fn new(call: ExecCall) -> Self {
        ExecCell { calls: vec![call] }
    }

    fn is_exploring_call(call: &ExecCall) -> bool {
        !call.parsed.is_empty()
            && call.parsed.iter().all(|p| {
                matches!(
                    p,
                    ParsedCommand::Read { .. }
                        | ParsedCommand::ListFiles { .. }
                        | ParsedCommand::Search { .. }
                )
            })
    }

    fn is_exploring_cell(&self) -> bool {
        self.calls.iter().all(Self::is_exploring_call)
    }

    pub(crate) fn with_added_call(
        &self,
        call_id: String,
        command: Vec<String>,
        parsed: Vec<ParsedCommand>,
    ) -> Option<Self> {
        let call = ExecCall {
            call_id,
            command,
            parsed,
            output: None,
            start_time: Some(Instant::now()),
            duration: None,
        };
        if self.is_exploring_cell() && Self::is_exploring_call(&call) {
            Some(Self {
                calls: [self.calls.clone(), vec![call]].concat(),
            })
        } else {
            None
        }
    }

    pub(crate) fn complete_call(
        &mut self,
        call_id: &str,
        output: CommandOutput,
        duration: Duration,
    ) {
        if let Some(call) = self.calls.iter_mut().rev().find(|c| c.call_id == call_id) {
            call.output = Some(output);
            call.duration = Some(duration);
            call.start_time = None;
        }
    }

    pub(crate) fn should_flush(&self) -> bool {
        !self.is_exploring_cell() && self.calls.iter().all(|c| c.output.is_some())
    }
}

#[derive(Debug)]
struct CompletedMcpToolCallWithImageOutput {
    _image: DynamicImage,
}
impl HistoryCell for CompletedMcpToolCallWithImageOutput {
    fn display_lines(&self, _width: u16) -> Vec<Line<'static>> {
        vec!["tool result (image output omitted)".into()]
    }
}

const TOOL_CALL_MAX_LINES: usize = 5;
pub(crate) const SESSION_HEADER_MAX_INNER_WIDTH: usize = 56; // Just an eyeballed value

pub(crate) fn card_inner_width(width: u16, max_inner_width: usize) -> Option<usize> {
    if width < 4 {
        return None;
    }
    let inner_width = std::cmp::min(width.saturating_sub(4) as usize, max_inner_width);
    Some(inner_width)
}

/// Render `lines` inside a border sized to the widest span in the content.
pub(crate) fn with_border(lines: Vec<Line<'static>>) -> Vec<Line<'static>> {
    with_border_internal(lines, None)
}

/// Render `lines` inside a border whose inner width is at least `inner_width`.
///
/// This is useful when callers have already clamped their content to a
/// specific width and want the border math centralized here instead of
/// duplicating padding logic in the TUI widgets themselves.
pub(crate) fn with_border_with_inner_width(
    lines: Vec<Line<'static>>,
    inner_width: usize,
) -> Vec<Line<'static>> {
    with_border_internal(lines, Some(inner_width))
}

fn with_border_internal(
    lines: Vec<Line<'static>>,
    forced_inner_width: Option<usize>,
) -> Vec<Line<'static>> {
    let max_line_width = lines
        .iter()
        .map(|line| {
            line.iter()
                .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
                .sum::<usize>()
        })
        .max()
        .unwrap_or(0);
    let content_width = forced_inner_width
        .unwrap_or(max_line_width)
        .max(max_line_width);

    let mut out = Vec::with_capacity(lines.len() + 2);
    let border_inner_width = content_width + 2;
    out.push(vec![format!("╭{}╮", "─".repeat(border_inner_width)).dim()].into());

    for line in lines.into_iter() {
        let used_width: usize = line
            .iter()
            .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
            .sum();
        let span_count = line.spans.len();
        let mut spans: Vec<Span<'static>> = Vec::with_capacity(span_count + 4);
        spans.push(Span::from("│ ").dim());
        spans.extend(line.into_iter());
        if used_width < content_width {
            spans.push(Span::from(" ".repeat(content_width - used_width)).dim());
        }
        spans.push(Span::from(" │").dim());
        out.push(Line::from(spans));
    }

    out.push(vec![format!("╰{}╯", "─".repeat(border_inner_width)).dim()].into());

    out
}

/// Return the emoji followed by a hair space (U+200A).
/// Using only the hair space avoids excessive padding after the emoji while
/// still providing a small visual gap across terminals.
pub(crate) fn padded_emoji(emoji: &str) -> String {
    format!("{emoji}\u{200A}")
}

pub(crate) fn new_session_info(
    config: &Config,
    event: SessionConfiguredEvent,
    is_first_event: bool,
) -> CompositeHistoryCell {
    let SessionConfiguredEvent {
        model,
        reasoning_effort,
        session_id: _,
        history_log_id: _,
        history_entry_count: _,
        initial_messages: _,
        rollout_path: _,
    } = event;
    if is_first_event {
        // Header box rendered as history (so it appears at the very top)
        let header = SessionHeaderHistoryCell::new(
            model,
            reasoning_effort,
            config.cwd.clone(),
            crate::version::CODEX_CLI_VERSION,
        );

        // Help lines below the header (new copy and list)
        let help_lines: Vec<Line<'static>> = vec![
            "  To get started, describe a task or try one of these commands:"
                .dim()
                .into(),
            Line::from(""),
            Line::from(vec![
                "  ".into(),
                "/init".into(),
                " - create an AGENTS.md file with instructions for Codex".dim(),
            ]),
            Line::from(vec![
                "  ".into(),
                "/status".into(),
                " - show current session configuration".dim(),
            ]),
            Line::from(vec![
                "  ".into(),
                "/approvals".into(),
                " - choose what Codex can do without approval".dim(),
            ]),
            Line::from(vec![
                "  ".into(),
                "/model".into(),
                " - choose what model and reasoning effort to use".dim(),
            ]),
        ];

        CompositeHistoryCell {
            parts: vec![
                Box::new(header),
                Box::new(PlainHistoryCell { lines: help_lines }),
            ],
        }
    } else if config.model == model {
        CompositeHistoryCell { parts: vec![] }
    } else {
        let lines = vec![
            "model changed:".magenta().bold().into(),
            format!("requested: {}", config.model).into(),
            format!("used: {model}").into(),
        ];
        CompositeHistoryCell {
            parts: vec![Box::new(PlainHistoryCell { lines })],
        }
    }
}

pub(crate) fn new_user_prompt(message: String) -> UserHistoryCell {
    UserHistoryCell { message }
}

pub(crate) fn new_user_approval_decision(lines: Vec<Line<'static>>) -> PlainHistoryCell {
    PlainHistoryCell { lines }
}

pub(crate) fn new_active_exec_command(
    call_id: String,
    command: Vec<String>,
    parsed: Vec<ParsedCommand>,
) -> ExecCell {
    ExecCell::new(ExecCall {
        call_id,
        command,
        parsed,
        output: None,
        start_time: Some(Instant::now()),
        duration: None,
    })
}

#[derive(Debug)]
struct SessionHeaderHistoryCell {
    version: &'static str,
    model: String,
    reasoning_effort: Option<ReasoningEffortConfig>,
    directory: PathBuf,
}

impl SessionHeaderHistoryCell {
    fn new(
        model: String,
        reasoning_effort: Option<ReasoningEffortConfig>,
        directory: PathBuf,
        version: &'static str,
    ) -> Self {
        Self {
            version,
            model,
            reasoning_effort,
            directory,
        }
    }

    fn format_directory(&self, max_width: Option<usize>) -> String {
        Self::format_directory_inner(&self.directory, max_width)
    }

    fn format_directory_inner(directory: &Path, max_width: Option<usize>) -> String {
        let formatted = if let Some(rel) = relativize_to_home(directory) {
            if rel.as_os_str().is_empty() {
                "~".to_string()
            } else {
                format!("~{}{}", std::path::MAIN_SEPARATOR, rel.display())
            }
        } else {
            directory.display().to_string()
        };

        if let Some(max_width) = max_width {
            if max_width == 0 {
                return String::new();
            }
            if UnicodeWidthStr::width(formatted.as_str()) > max_width {
                return crate::text_formatting::center_truncate_path(&formatted, max_width);
            }
        }

        formatted
    }

    fn reasoning_label(&self) -> Option<&'static str> {
        self.reasoning_effort.map(|effort| match effort {
            ReasoningEffortConfig::Minimal => "minimal",
            ReasoningEffortConfig::Low => "low",
            ReasoningEffortConfig::Medium => "medium",
            ReasoningEffortConfig::High => "high",
        })
    }
}

impl HistoryCell for SessionHeaderHistoryCell {
    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        let Some(inner_width) = card_inner_width(width, SESSION_HEADER_MAX_INNER_WIDTH) else {
            return Vec::new();
        };

        let make_row = |spans: Vec<Span<'static>>| Line::from(spans);

        // Title line rendered inside the box: ">_ OpenAI Codex (vX)"
        let title_spans: Vec<Span<'static>> = vec![
            Span::from(">_ ").dim(),
            Span::from("OpenAI Codex").bold(),
            Span::from(" ").dim(),
            Span::from(format!("(v{})", self.version)).dim(),
        ];

        const CHANGE_MODEL_HINT_COMMAND: &str = "/model";
        const CHANGE_MODEL_HINT_EXPLANATION: &str = " to change";
        const DIR_LABEL: &str = "directory:";
        let label_width = DIR_LABEL.len();
        let model_label = format!(
            "{model_label:<label_width$}",
            model_label = "model:",
            label_width = label_width
        );
        let reasoning_label = self.reasoning_label();
        let mut model_spans: Vec<Span<'static>> = vec![
            Span::from(format!("{model_label} ")).dim(),
            Span::from(self.model.clone()),
        ];
        if let Some(reasoning) = reasoning_label {
            model_spans.push(Span::from(" "));
            model_spans.push(Span::from(reasoning));
        }
        model_spans.push("   ".dim());
        model_spans.push(CHANGE_MODEL_HINT_COMMAND.cyan());
        model_spans.push(CHANGE_MODEL_HINT_EXPLANATION.dim());

        let dir_label = format!("{DIR_LABEL:<label_width$}");
        let dir_prefix = format!("{dir_label} ");
        let dir_prefix_width = UnicodeWidthStr::width(dir_prefix.as_str());
        let dir_max_width = inner_width.saturating_sub(dir_prefix_width);
        let dir = self.format_directory(Some(dir_max_width));
        let dir_spans = vec![Span::from(dir_prefix).dim(), Span::from(dir)];

        let lines = vec![
            make_row(title_spans),
            make_row(Vec::new()),
            make_row(model_spans),
            make_row(dir_spans),
        ];

        with_border(lines)
    }
}

#[derive(Debug)]
pub(crate) struct CompositeHistoryCell {
    parts: Vec<Box<dyn HistoryCell>>,
}

impl CompositeHistoryCell {
    pub(crate) fn new(parts: Vec<Box<dyn HistoryCell>>) -> Self {
        Self { parts }
    }
}

impl HistoryCell for CompositeHistoryCell {
    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        let mut out: Vec<Line<'static>> = Vec::new();
        let mut first = true;
        for part in &self.parts {
            let mut lines = part.display_lines(width);
            if !lines.is_empty() {
                if !first {
                    out.push(Line::from(""));
                }
                out.append(&mut lines);
                first = false;
            }
        }
        out
    }
}

#[derive(Debug)]
pub(crate) struct McpToolCallCell {
    call_id: String,
    invocation: McpInvocation,
    start_time: Instant,
    duration: Option<Duration>,
    result: Option<Result<mcp_types::CallToolResult, String>>,
}

impl McpToolCallCell {
    pub(crate) fn new(call_id: String, invocation: McpInvocation) -> Self {
        Self {
            call_id,
            invocation,
            start_time: Instant::now(),
            duration: None,
            result: None,
        }
    }

    pub(crate) fn call_id(&self) -> &str {
        &self.call_id
    }

    pub(crate) fn complete(
        &mut self,
        duration: Duration,
        result: Result<mcp_types::CallToolResult, String>,
    ) -> Option<Box<dyn HistoryCell>> {
        let image_cell = try_new_completed_mcp_tool_call_with_image_output(&result)
            .map(|cell| Box::new(cell) as Box<dyn HistoryCell>);
        self.duration = Some(duration);
        self.result = Some(result);
        image_cell
    }

    fn success(&self) -> Option<bool> {
        match self.result.as_ref() {
            Some(Ok(result)) => Some(!result.is_error.unwrap_or(false)),
            Some(Err(_)) => Some(false),
            None => None,
        }
    }

    pub(crate) fn mark_failed(&mut self) {
        let elapsed = self.start_time.elapsed();
        self.duration = Some(elapsed);
        self.result = Some(Err("interrupted".to_string()));
    }

    fn render_content_block(block: &mcp_types::ContentBlock, width: usize) -> String {
        match block {
            mcp_types::ContentBlock::TextContent(text) => {
                format_and_truncate_tool_result(&text.text, TOOL_CALL_MAX_LINES, width)
            }
            mcp_types::ContentBlock::ImageContent(_) => "<image content>".to_string(),
            mcp_types::ContentBlock::AudioContent(_) => "<audio content>".to_string(),
            mcp_types::ContentBlock::EmbeddedResource(resource) => {
                let uri = match &resource.resource {
                    EmbeddedResourceResource::TextResourceContents(text) => text.uri.clone(),
                    EmbeddedResourceResource::BlobResourceContents(blob) => blob.uri.clone(),
                };
                format!("embedded resource: {uri}")
            }
            mcp_types::ContentBlock::ResourceLink(ResourceLink { uri, .. }) => {
                format!("link: {uri}")
            }
        }
    }
}

impl HistoryCell for McpToolCallCell {
    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        let mut lines: Vec<Line<'static>> = Vec::new();
        let status = self.success();
        let bullet = match status {
            Some(true) => "•".green().bold(),
            Some(false) => "•".red().bold(),
            None => spinner(Some(self.start_time)),
        };
        let header_text = if status.is_some() {
            "Called"
        } else {
            "Calling"
        };

        let invocation_line = line_to_static(&format_mcp_invocation(self.invocation.clone()));
        let mut compact_spans = vec![bullet.clone(), " ".into(), header_text.bold(), " ".into()];
        let mut compact_header = Line::from(compact_spans.clone());
        let reserved = compact_header.width();

        let inline_invocation =
            invocation_line.width() <= (width as usize).saturating_sub(reserved);

        if inline_invocation {
            compact_header.extend(invocation_line.spans.clone());
            lines.push(compact_header);
        } else {
            compact_spans.pop(); // drop trailing space for standalone header
            lines.push(Line::from(compact_spans));

            let opts = RtOptions::new((width as usize).saturating_sub(4))
                .initial_indent("".into())
                .subsequent_indent("    ".into());
            let wrapped = word_wrap_line(&invocation_line, opts);
            let body_lines: Vec<Line<'static>> = wrapped.iter().map(line_to_static).collect();
            lines.extend(prefix_lines(body_lines, "  └ ".dim(), "    ".into()));
        }

        let mut detail_lines: Vec<Line<'static>> = Vec::new();

        if let Some(result) = &self.result {
            match result {
                Ok(mcp_types::CallToolResult { content, .. }) => {
                    if !content.is_empty() {
                        for block in content {
                            let text = Self::render_content_block(block, width as usize);
                            for segment in text.split('\n') {
                                let line = Line::from(segment.to_string().dim());
                                let wrapped = word_wrap_line(
                                    &line,
                                    RtOptions::new((width as usize).saturating_sub(4))
                                        .initial_indent("".into())
                                        .subsequent_indent("    ".into()),
                                );
                                detail_lines.extend(wrapped.iter().map(line_to_static));
                            }
                        }
                    }
                }
                Err(err) => {
                    let err_line = Line::from(format!("Error: {err}").dim());
                    let wrapped = word_wrap_line(
                        &err_line,
                        RtOptions::new((width as usize).saturating_sub(4))
                            .initial_indent("".into())
                            .subsequent_indent("    ".into()),
                    );
                    detail_lines.extend(wrapped.iter().map(line_to_static));
                }
            }
        }

        if !detail_lines.is_empty() {
            let initial_prefix: Span<'static> = if inline_invocation {
                "  └ ".dim()
            } else {
                "    ".into()
            };
            lines.extend(prefix_lines(detail_lines, initial_prefix, "    ".into()));
        }

        lines
    }
}

impl WidgetRef for &McpToolCallCell {
    fn render_ref(&self, area: Rect, buf: &mut Buffer) {
        if area.height == 0 {
            return;
        }
        let lines = self.display_lines(area.width);
        let max_rows = area.height as usize;
        let rendered = if lines.len() > max_rows {
            lines[lines.len() - max_rows..].to_vec()
        } else {
            lines
        };

        Text::from(rendered).render(area, buf);
    }
}

fn spinner(start_time: Option<Instant>) -> Span<'static> {
    const FRAMES: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
    let idx = start_time
        .map(|st| ((st.elapsed().as_millis() / 100) as usize) % FRAMES.len())
        .unwrap_or(0);
    let ch = FRAMES[idx];
    ch.to_string().into()
}

pub(crate) fn new_active_mcp_tool_call(
    call_id: String,
    invocation: McpInvocation,
) -> McpToolCallCell {
    McpToolCallCell::new(call_id, invocation)
}

pub(crate) fn new_web_search_call(query: String) -> PlainHistoryCell {
    let lines: Vec<Line<'static>> = vec![Line::from(vec![padded_emoji("🌐").into(), query.into()])];
    PlainHistoryCell { lines }
}

/// If the first content is an image, return a new cell with the image.
/// TODO(rgwood-dd): Handle images properly even if they're not the first result.
fn try_new_completed_mcp_tool_call_with_image_output(
    result: &Result<mcp_types::CallToolResult, String>,
) -> Option<CompletedMcpToolCallWithImageOutput> {
    match result {
        Ok(mcp_types::CallToolResult { content, .. }) => {
            if let Some(mcp_types::ContentBlock::ImageContent(image)) = content.first() {
                let raw_data = match base64::engine::general_purpose::STANDARD.decode(&image.data) {
                    Ok(data) => data,
                    Err(e) => {
                        error!("Failed to decode image data: {e}");
                        return None;
                    }
                };
                let reader = match ImageReader::new(Cursor::new(raw_data)).with_guessed_format() {
                    Ok(reader) => reader,
                    Err(e) => {
                        error!("Failed to guess image format: {e}");
                        return None;
                    }
                };

                let image = match reader.decode() {
                    Ok(image) => image,
                    Err(e) => {
                        error!("Image decoding failed: {e}");
                        return None;
                    }
                };

                Some(CompletedMcpToolCallWithImageOutput { _image: image })
            } else {
                None
            }
        }
        _ => None,
    }
}

#[allow(clippy::disallowed_methods)]
pub(crate) fn new_warning_event(message: String) -> PlainHistoryCell {
    PlainHistoryCell {
        lines: vec![vec![format!("⚠ {message}").yellow()].into()],
    }
}

/// Render a summary of configured MCP servers from the current `Config`.
pub(crate) fn empty_mcp_output() -> PlainHistoryCell {
    let lines: Vec<Line<'static>> = vec![
        "/mcp".magenta().into(),
        "".into(),
        vec!["🔌  ".into(), "MCP Tools".bold()].into(),
        "".into(),
        "  • No MCP servers configured.".italic().into(),
        Line::from(vec![
            "    See the ".into(),
            "\u{1b}]8;;https://github.com/openai/codex/blob/main/docs/config.md#mcp_servers\u{7}MCP docs\u{1b}]8;;\u{7}".underlined(),
            " to configure them.".into(),
        ])
        .style(Style::default().add_modifier(Modifier::DIM)),
    ];

    PlainHistoryCell { lines }
}

/// Render MCP tools grouped by connection using the fully-qualified tool names.
pub(crate) fn new_mcp_tools_output(
    config: &Config,
    tools: std::collections::HashMap<String, mcp_types::Tool>,
) -> PlainHistoryCell {
    let mut lines: Vec<Line<'static>> = vec![
        "/mcp".magenta().into(),
        "".into(),
        vec!["🔌  ".into(), "MCP Tools".bold()].into(),
        "".into(),
    ];

    if tools.is_empty() {
        lines.push("  • No MCP tools available.".italic().into());
        lines.push("".into());
        return PlainHistoryCell { lines };
    }

    for (server, cfg) in config.mcp_servers.iter() {
        let prefix = format!("{server}__");
        let mut names: Vec<String> = tools
            .keys()
            .filter(|k| k.starts_with(&prefix))
            .map(|k| k[prefix.len()..].to_string())
            .collect();
        names.sort();

        lines.push(vec!["  • Server: ".into(), server.clone().into()].into());

        if !cfg.command.is_empty() {
            let cmd_display = format!("{} {}", cfg.command, cfg.args.join(" "));

            lines.push(vec!["    • Command: ".into(), cmd_display.into()].into());
        }

        if names.is_empty() {
            lines.push("    • Tools: (none)".into());
        } else {
            lines.push(vec!["    • Tools: ".into(), names.join(", ").into()].into());
        }
        lines.push(Line::from(""));
    }

    PlainHistoryCell { lines }
}

pub(crate) fn new_info_event(message: String, hint: Option<String>) -> PlainHistoryCell {
    let mut line = vec!["> ".into(), message.into()];
    if let Some(hint) = hint {
        line.push(" ".into());
        line.push(hint.dark_gray());
    }
    let lines: Vec<Line<'static>> = vec![line.into()];
    PlainHistoryCell { lines }
}

pub(crate) fn new_error_event(message: String) -> PlainHistoryCell {
    // Use a hair space (U+200A) to create a subtle, near-invisible separation
    // before the text. VS16 is intentionally omitted to keep spacing tighter
    // in terminals like Ghostty.
    let lines: Vec<Line<'static>> = vec![vec![format!("■ {message}").red()].into()];
    PlainHistoryCell { lines }
}

pub(crate) fn new_stream_error_event(message: String) -> PlainHistoryCell {
    let lines: Vec<Line<'static>> = vec![vec![padded_emoji("⚠️").into(), message.dim()].into()];
    PlainHistoryCell { lines }
}

/// Render a user‑friendly plan update styled like a checkbox todo list.
pub(crate) fn new_plan_update(update: UpdatePlanArgs) -> PlanUpdateCell {
    let UpdatePlanArgs { explanation, plan } = update;
    PlanUpdateCell { explanation, plan }
}

#[derive(Debug)]
pub(crate) struct PlanUpdateCell {
    explanation: Option<String>,
    plan: Vec<PlanItemArg>,
}

impl HistoryCell for PlanUpdateCell {
    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        let render_note = |text: &str| -> Vec<Line<'static>> {
            let wrap_width = width.saturating_sub(4).max(1) as usize;
            textwrap::wrap(text, wrap_width)
                .into_iter()
                .map(|s| s.to_string().dim().italic().into())
                .collect()
        };

        let render_step = |status: &StepStatus, text: &str| -> Vec<Line<'static>> {
            let (box_str, step_style) = match status {
                StepStatus::Completed => ("✔ ", Style::default().crossed_out().dim()),
                StepStatus::InProgress => ("□ ", Style::default().cyan().bold()),
                StepStatus::Pending => ("□ ", Style::default().dim()),
            };
            let wrap_width = (width as usize)
                .saturating_sub(4)
                .saturating_sub(box_str.width())
                .max(1);
            let parts = textwrap::wrap(text, wrap_width);
            let step_text = parts
                .into_iter()
                .map(|s| s.to_string().set_style(step_style).into())
                .collect();
            prefix_lines(step_text, box_str.into(), "  ".into())
        };

        let mut lines: Vec<Line<'static>> = vec![];
        lines.push(vec!["• ".into(), "Updated Plan".bold()].into());

        let mut indented_lines = vec![];
        let note = self
            .explanation
            .as_ref()
            .map(|s| s.trim())
            .filter(|t| !t.is_empty());
        if let Some(expl) = note {
            indented_lines.extend(render_note(expl));
        };

        if self.plan.is_empty() {
            indented_lines.push(Line::from("(no steps provided)".dim().italic()));
        } else {
            for PlanItemArg { step, status } in self.plan.iter() {
                indented_lines.extend(render_step(status, step));
            }
        }
        lines.extend(prefix_lines(indented_lines, "  └ ".into(), "    ".into()));

        lines
    }
}

/// Create a new `PendingPatch` cell that lists the file‑level summary of
/// a proposed patch. The summary lines should already be formatted (e.g.
/// "A path/to/file.rs").
pub(crate) fn new_patch_event(
    event_type: PatchEventType,
    changes: HashMap<PathBuf, FileChange>,
    cwd: &Path,
) -> PatchHistoryCell {
    PatchHistoryCell {
        event_type,
        changes,
        cwd: cwd.to_path_buf(),
    }
}

pub(crate) fn new_patch_apply_failure(stderr: String) -> PlainHistoryCell {
    let mut lines: Vec<Line<'static>> = Vec::new();

    // Failure title
    lines.push(Line::from("✘ Failed to apply patch".magenta().bold()));

    if !stderr.trim().is_empty() {
        lines.extend(output_lines(
            Some(&CommandOutput {
                exit_code: 1,
                stdout: String::new(),
                stderr,
                formatted_output: String::new(),
            }),
            OutputLinesParams {
                only_err: true,
                include_angle_pipe: true,
                include_prefix: true,
            },
        ));
    }

    PlainHistoryCell { lines }
}

/// Create a new history cell for a proposed command approval.
/// Renders a header and the command preview similar to how proposed patches
/// show a header and summary.
pub(crate) fn new_proposed_command(command: &[String]) -> PlainHistoryCell {
    let cmd = strip_bash_lc_and_escape(command);

    let mut lines: Vec<Line<'static>> = Vec::new();
    lines.push(Line::from(vec!["• ".into(), "Proposed Command".bold()]));

    let highlighted_lines = crate::render::highlight::highlight_bash_to_lines(&cmd);
    let initial_prefix: Span<'static> = "  └ ".dim();
    let subsequent_prefix: Span<'static> = "    ".into();
    lines.extend(prefix_lines(
        highlighted_lines,
        initial_prefix,
        subsequent_prefix,
    ));

    PlainHistoryCell { lines }
}

pub(crate) fn new_reasoning_block(
    full_reasoning_buffer: String,
    config: &Config,
) -> TranscriptOnlyHistoryCell {
    let mut lines: Vec<Line<'static>> = Vec::new();
    lines.push(Line::from("thinking".magenta().italic()));
    append_markdown(&full_reasoning_buffer, &mut lines, config);
    TranscriptOnlyHistoryCell { lines }
}

pub(crate) fn new_reasoning_summary_block(
    full_reasoning_buffer: String,
    config: &Config,
) -> Box<dyn HistoryCell> {
    if config.model_family.reasoning_summary_format == ReasoningSummaryFormat::Experimental {
        // Experimental format is following:
        // ** header **
        //
        // reasoning summary
        //
        // So we need to strip header from reasoning summary
        let full_reasoning_buffer = full_reasoning_buffer.trim();
        if let Some(open) = full_reasoning_buffer.find("**") {
            let after_open = &full_reasoning_buffer[(open + 2)..];
            if let Some(close) = after_open.find("**") {
                let after_close_idx = open + 2 + close + 2;
                // if we don't have anything beyond `after_close_idx`
                // then we don't have a summary to inject into history
                if after_close_idx < full_reasoning_buffer.len() {
                    let header_buffer = full_reasoning_buffer[..after_close_idx].to_string();
                    let mut header_lines = Vec::new();
                    append_markdown(&header_buffer, &mut header_lines, config);

                    let summary_buffer = full_reasoning_buffer[after_close_idx..].to_string();
                    let mut summary_lines = Vec::new();
                    append_markdown(&summary_buffer, &mut summary_lines, config);

                    return Box::new(ReasoningSummaryCell::new(header_lines, summary_lines));
                }
            }
        }
    }
    Box::new(new_reasoning_block(full_reasoning_buffer, config))
}

struct OutputLinesParams {
    only_err: bool,
    include_angle_pipe: bool,
    include_prefix: bool,
}

fn output_lines(output: Option<&CommandOutput>, params: OutputLinesParams) -> Vec<Line<'static>> {
    let OutputLinesParams {
        only_err,
        include_angle_pipe,
        include_prefix,
    } = params;
    let CommandOutput {
        exit_code,
        stdout,
        stderr,
        ..
    } = match output {
        Some(output) if only_err && output.exit_code == 0 => return vec![],
        Some(output) => output,
        None => return vec![],
    };

    let src = if *exit_code == 0 { stdout } else { stderr };
    let lines: Vec<&str> = src.lines().collect();
    let total = lines.len();
    let limit = TOOL_CALL_MAX_LINES;

    let mut out = Vec::new();

    let head_end = total.min(limit);
    for (i, raw) in lines[..head_end].iter().enumerate() {
        let mut line = ansi_escape_line(raw);
        let prefix = if !include_prefix {
            ""
        } else if i == 0 && include_angle_pipe {
            "  └ "
        } else {
            "    "
        };
        line.spans.insert(0, prefix.into());
        line.spans.iter_mut().for_each(|span| {
            span.style = span.style.add_modifier(Modifier::DIM);
        });
        out.push(line);
    }

    // If we will ellipsize less than the limit, just show it.
    let show_ellipsis = total > 2 * limit;
    if show_ellipsis {
        let omitted = total - 2 * limit;
        out.push(format!("… +{omitted} lines").into());
    }

    let tail_start = if show_ellipsis {
        total - limit
    } else {
        head_end
    };
    for raw in lines[tail_start..].iter() {
        let mut line = ansi_escape_line(raw);
        if include_prefix {
            line.spans.insert(0, "    ".into());
        }
        line.spans.iter_mut().for_each(|span| {
            span.style = span.style.add_modifier(Modifier::DIM);
        });
        out.push(line);
    }

    out
}

fn format_mcp_invocation<'a>(invocation: McpInvocation) -> Line<'a> {
    let args_str = invocation
        .arguments
        .as_ref()
        .map(|v| {
            // Use compact form to keep things short but readable.
            serde_json::to_string(v).unwrap_or_else(|_| v.to_string())
        })
        .unwrap_or_default();

    let invocation_spans = vec![
        invocation.server.clone().cyan(),
        ".".into(),
        invocation.tool.cyan(),
        "(".into(),
        args_str.dim(),
        ")".into(),
    ];
    invocation_spans.into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_core::config::Config;
    use codex_core::config::ConfigOverrides;
    use codex_core::config::ConfigToml;
    use dirs::home_dir;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    use mcp_types::CallToolResult;
    use mcp_types::ContentBlock;
    use mcp_types::TextContent;

    fn test_config() -> Config {
        Config::load_from_base_config_with_overrides(
            ConfigToml::default(),
            ConfigOverrides::default(),
            std::env::temp_dir(),
        )
        .expect("config")
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

    fn render_transcript(cell: &dyn HistoryCell) -> Vec<String> {
        render_lines(&cell.transcript_lines())
    }

    #[test]
    fn active_mcp_tool_call_snapshot() {
        let invocation = McpInvocation {
            server: "search".into(),
            tool: "find_docs".into(),
            arguments: Some(json!({
                "query": "ratatui styling",
                "limit": 3,
            })),
        };

        let cell = new_active_mcp_tool_call("call-1".into(), invocation);
        let rendered = render_lines(&cell.display_lines(80)).join("\n");

        insta::assert_snapshot!(rendered);
    }

    #[test]
    fn completed_mcp_tool_call_success_snapshot() {
        let invocation = McpInvocation {
            server: "search".into(),
            tool: "find_docs".into(),
            arguments: Some(json!({
                "query": "ratatui styling",
                "limit": 3,
            })),
        };

        let result = CallToolResult {
            content: vec![ContentBlock::TextContent(TextContent {
                annotations: None,
                text: "Found styling guidance in styles.md".into(),
                r#type: "text".into(),
            })],
            is_error: None,
            structured_content: None,
        };

        let mut cell = new_active_mcp_tool_call("call-2".into(), invocation);
        assert!(
            cell.complete(Duration::from_millis(1420), Ok(result))
                .is_none()
        );

        let rendered = render_lines(&cell.display_lines(80)).join("\n");

        insta::assert_snapshot!(rendered);
    }

    #[test]
    fn completed_mcp_tool_call_error_snapshot() {
        let invocation = McpInvocation {
            server: "search".into(),
            tool: "find_docs".into(),
            arguments: Some(json!({
                "query": "ratatui styling",
                "limit": 3,
            })),
        };

        let mut cell = new_active_mcp_tool_call("call-3".into(), invocation);
        assert!(
            cell.complete(Duration::from_secs(2), Err("network timeout".into()))
                .is_none()
        );

        let rendered = render_lines(&cell.display_lines(80)).join("\n");

        insta::assert_snapshot!(rendered);
    }

    #[test]
    fn completed_mcp_tool_call_multiple_outputs_snapshot() {
        let invocation = McpInvocation {
            server: "search".into(),
            tool: "find_docs".into(),
            arguments: Some(json!({
                "query": "ratatui styling",
                "limit": 3,
            })),
        };

        let result = CallToolResult {
            content: vec![
                ContentBlock::TextContent(TextContent {
                    annotations: None,
                    text: "Found styling guidance in styles.md and additional notes in CONTRIBUTING.md.".into(),
                    r#type: "text".into(),
                }),
                ContentBlock::ResourceLink(ResourceLink {
                    annotations: None,
                    description: Some("Link to styles documentation".into()),
                    mime_type: None,
                    name: "styles.md".into(),
                    size: None,
                    title: Some("Styles".into()),
                    r#type: "resource_link".into(),
                    uri: "file:///docs/styles.md".into(),
                }),
            ],
            is_error: None,
            structured_content: None,
        };

        let mut cell = new_active_mcp_tool_call("call-4".into(), invocation);
        assert!(
            cell.complete(Duration::from_millis(640), Ok(result))
                .is_none()
        );

        let rendered = render_lines(&cell.display_lines(48)).join("\n");

        insta::assert_snapshot!(rendered);
    }

    #[test]
    fn completed_mcp_tool_call_wrapped_outputs_snapshot() {
        let invocation = McpInvocation {
            server: "metrics".into(),
            tool: "get_nearby_metric".into(),
            arguments: Some(json!({
                "query": "very_long_query_that_needs_wrapping_to_display_properly_in_the_history",
                "limit": 1,
            })),
        };

        let result = CallToolResult {
            content: vec![ContentBlock::TextContent(TextContent {
                annotations: None,
                text: "Line one of the response, which is quite long and needs wrapping.\nLine two continues the response with more detail.".into(),
                r#type: "text".into(),
            })],
            is_error: None,
            structured_content: None,
        };

        let mut cell = new_active_mcp_tool_call("call-5".into(), invocation);
        assert!(
            cell.complete(Duration::from_millis(1280), Ok(result))
                .is_none()
        );

        let rendered = render_lines(&cell.display_lines(40)).join("\n");

        insta::assert_snapshot!(rendered);
    }

    #[test]
    fn completed_mcp_tool_call_multiple_outputs_inline_snapshot() {
        let invocation = McpInvocation {
            server: "metrics".into(),
            tool: "summary".into(),
            arguments: Some(json!({
                "metric": "trace.latency",
                "window": "15m",
            })),
        };

        let result = CallToolResult {
            content: vec![
                ContentBlock::TextContent(TextContent {
                    annotations: None,
                    text: "Latency summary: p50=120ms, p95=480ms.".into(),
                    r#type: "text".into(),
                }),
                ContentBlock::TextContent(TextContent {
                    annotations: None,
                    text: "No anomalies detected.".into(),
                    r#type: "text".into(),
                }),
            ],
            is_error: None,
            structured_content: None,
        };

        let mut cell = new_active_mcp_tool_call("call-6".into(), invocation);
        assert!(
            cell.complete(Duration::from_millis(320), Ok(result))
                .is_none()
        );

        let rendered = render_lines(&cell.display_lines(120)).join("\n");

        insta::assert_snapshot!(rendered);
    }

    #[test]
    fn session_header_includes_reasoning_level_when_present() {
        let cell = SessionHeaderHistoryCell::new(
            "gpt-4o".to_string(),
            Some(ReasoningEffortConfig::High),
            std::env::temp_dir(),
            "test",
        );

        let lines = render_lines(&cell.display_lines(80));
        let model_line = lines
            .into_iter()
            .find(|line| line.contains("model:"))
            .expect("model line");

        assert!(model_line.contains("gpt-4o high"));
        assert!(model_line.contains("/model to change"));
    }

    #[test]
    fn session_header_directory_center_truncates() {
        let mut dir = home_dir().expect("home directory");
        for part in ["hello", "the", "fox", "is", "very", "fast"] {
            dir.push(part);
        }

        let formatted = SessionHeaderHistoryCell::format_directory_inner(&dir, Some(24));
        let sep = std::path::MAIN_SEPARATOR;
        let expected = format!("~{sep}hello{sep}the{sep}…{sep}very{sep}fast");
        assert_eq!(formatted, expected);
    }

    #[test]
    fn session_header_directory_front_truncates_long_segment() {
        let mut dir = home_dir().expect("home directory");
        dir.push("supercalifragilisticexpialidocious");

        let formatted = SessionHeaderHistoryCell::format_directory_inner(&dir, Some(18));
        let sep = std::path::MAIN_SEPARATOR;
        let expected = format!("~{sep}…cexpialidocious");
        assert_eq!(formatted, expected);
    }

    #[test]
    fn coalesces_sequential_reads_within_one_call() {
        // Build one exec cell with a Search followed by two Reads
        let call_id = "c1".to_string();
        let mut cell = ExecCell::new(ExecCall {
            call_id: call_id.clone(),
            command: vec!["bash".into(), "-lc".into(), "echo".into()],
            parsed: vec![
                ParsedCommand::Search {
                    query: Some("shimmer_spans".into()),
                    path: None,
                    cmd: "rg shimmer_spans".into(),
                },
                ParsedCommand::Read {
                    name: "shimmer.rs".into(),
                    cmd: "cat shimmer.rs".into(),
                },
                ParsedCommand::Read {
                    name: "status_indicator_widget.rs".into(),
                    cmd: "cat status_indicator_widget.rs".into(),
                },
            ],
            output: None,
            start_time: Some(Instant::now()),
            duration: None,
        });
        // Mark call complete so markers are ✓
        cell.complete_call(
            &call_id,
            CommandOutput {
                exit_code: 0,
                stdout: String::new(),
                stderr: String::new(),
                formatted_output: String::new(),
            },
            Duration::from_millis(1),
        );

        let lines = cell.display_lines(80);
        let rendered = render_lines(&lines).join("\n");
        insta::assert_snapshot!(rendered);
    }

    #[test]
    fn coalesces_reads_across_multiple_calls() {
        let mut cell = ExecCell::new(ExecCall {
            call_id: "c1".to_string(),
            command: vec!["bash".into(), "-lc".into(), "echo".into()],
            parsed: vec![ParsedCommand::Search {
                query: Some("shimmer_spans".into()),
                path: None,
                cmd: "rg shimmer_spans".into(),
            }],
            output: None,
            start_time: Some(Instant::now()),
            duration: None,
        });
        // Call 1: Search only
        cell.complete_call(
            "c1",
            CommandOutput {
                exit_code: 0,
                stdout: String::new(),
                stderr: String::new(),
                formatted_output: String::new(),
            },
            Duration::from_millis(1),
        );
        // Call 2: Read A
        cell = cell
            .with_added_call(
                "c2".into(),
                vec!["bash".into(), "-lc".into(), "echo".into()],
                vec![ParsedCommand::Read {
                    name: "shimmer.rs".into(),
                    cmd: "cat shimmer.rs".into(),
                }],
            )
            .unwrap();
        cell.complete_call(
            "c2",
            CommandOutput {
                exit_code: 0,
                stdout: String::new(),
                stderr: String::new(),
                formatted_output: String::new(),
            },
            Duration::from_millis(1),
        );
        // Call 3: Read B
        cell = cell
            .with_added_call(
                "c3".into(),
                vec!["bash".into(), "-lc".into(), "echo".into()],
                vec![ParsedCommand::Read {
                    name: "status_indicator_widget.rs".into(),
                    cmd: "cat status_indicator_widget.rs".into(),
                }],
            )
            .unwrap();
        cell.complete_call(
            "c3",
            CommandOutput {
                exit_code: 0,
                stdout: String::new(),
                stderr: String::new(),
                formatted_output: String::new(),
            },
            Duration::from_millis(1),
        );

        let lines = cell.display_lines(80);
        let rendered = render_lines(&lines).join("\n");
        insta::assert_snapshot!(rendered);
    }

    #[test]
    fn coalesced_reads_dedupe_names() {
        let mut cell = ExecCell::new(ExecCall {
            call_id: "c1".to_string(),
            command: vec!["bash".into(), "-lc".into(), "echo".into()],
            parsed: vec![
                ParsedCommand::Read {
                    name: "auth.rs".into(),
                    cmd: "cat auth.rs".into(),
                },
                ParsedCommand::Read {
                    name: "auth.rs".into(),
                    cmd: "cat auth.rs".into(),
                },
                ParsedCommand::Read {
                    name: "shimmer.rs".into(),
                    cmd: "cat shimmer.rs".into(),
                },
            ],
            output: None,
            start_time: Some(Instant::now()),
            duration: None,
        });
        cell.complete_call(
            "c1",
            CommandOutput {
                exit_code: 0,
                stdout: String::new(),
                stderr: String::new(),
                formatted_output: String::new(),
            },
            Duration::from_millis(1),
        );
        let lines = cell.display_lines(80);
        let rendered = render_lines(&lines).join("\n");
        insta::assert_snapshot!(rendered);
    }

    #[test]
    fn multiline_command_wraps_with_extra_indent_on_subsequent_lines() {
        // Create a completed exec cell with a multiline command
        let cmd = "set -o pipefail\ncargo test --all-features --quiet".to_string();
        let call_id = "c1".to_string();
        let mut cell = ExecCell::new(ExecCall {
            call_id: call_id.clone(),
            command: vec!["bash".into(), "-lc".into(), cmd],
            parsed: Vec::new(),
            output: None,
            start_time: Some(Instant::now()),
            duration: None,
        });
        // Mark call complete so it renders as "Ran"
        cell.complete_call(
            &call_id,
            CommandOutput {
                exit_code: 0,
                stdout: String::new(),
                stderr: String::new(),
                formatted_output: String::new(),
            },
            Duration::from_millis(1),
        );

        // Small width to force wrapping on both lines
        let width: u16 = 28;
        let lines = cell.display_lines(width);
        let rendered = render_lines(&lines).join("\n");
        insta::assert_snapshot!(rendered);
    }

    #[test]
    fn single_line_command_compact_when_fits() {
        let call_id = "c1".to_string();
        let mut cell = ExecCell::new(ExecCall {
            call_id: call_id.clone(),
            command: vec!["echo".into(), "ok".into()],
            parsed: Vec::new(),
            output: None,
            start_time: Some(Instant::now()),
            duration: None,
        });
        cell.complete_call(
            &call_id,
            CommandOutput {
                exit_code: 0,
                stdout: String::new(),
                stderr: String::new(),
                formatted_output: String::new(),
            },
            Duration::from_millis(1),
        );
        // Wide enough that it fits inline
        let lines = cell.display_lines(80);
        let rendered = render_lines(&lines).join("\n");
        insta::assert_snapshot!(rendered);
    }

    #[test]
    fn single_line_command_wraps_with_four_space_continuation() {
        let call_id = "c1".to_string();
        let long = "a_very_long_token_without_spaces_to_force_wrapping".to_string();
        let mut cell = ExecCell::new(ExecCall {
            call_id: call_id.clone(),
            command: vec!["bash".into(), "-lc".into(), long],
            parsed: Vec::new(),
            output: None,
            start_time: Some(Instant::now()),
            duration: None,
        });
        cell.complete_call(
            &call_id,
            CommandOutput {
                exit_code: 0,
                stdout: String::new(),
                stderr: String::new(),
                formatted_output: String::new(),
            },
            Duration::from_millis(1),
        );
        let lines = cell.display_lines(24);
        let rendered = render_lines(&lines).join("\n");
        insta::assert_snapshot!(rendered);
    }

    #[test]
    fn multiline_command_without_wrap_uses_branch_then_eight_spaces() {
        let call_id = "c1".to_string();
        let cmd = "echo one\necho two".to_string();
        let mut cell = ExecCell::new(ExecCall {
            call_id: call_id.clone(),
            command: vec!["bash".into(), "-lc".into(), cmd],
            parsed: Vec::new(),
            output: None,
            start_time: Some(Instant::now()),
            duration: None,
        });
        cell.complete_call(
            &call_id,
            CommandOutput {
                exit_code: 0,
                stdout: String::new(),
                stderr: String::new(),
                formatted_output: String::new(),
            },
            Duration::from_millis(1),
        );
        let lines = cell.display_lines(80);
        let rendered = render_lines(&lines).join("\n");
        insta::assert_snapshot!(rendered);
    }

    #[test]
    fn multiline_command_both_lines_wrap_with_correct_prefixes() {
        let call_id = "c1".to_string();
        let cmd = "first_token_is_long_enough_to_wrap\nsecond_token_is_also_long_enough_to_wrap"
            .to_string();
        let mut cell = ExecCell::new(ExecCall {
            call_id: call_id.clone(),
            command: vec!["bash".into(), "-lc".into(), cmd],
            parsed: Vec::new(),
            output: None,
            start_time: Some(Instant::now()),
            duration: None,
        });
        cell.complete_call(
            &call_id,
            CommandOutput {
                exit_code: 0,
                stdout: String::new(),
                stderr: String::new(),
                formatted_output: String::new(),
            },
            Duration::from_millis(1),
        );
        let lines = cell.display_lines(28);
        let rendered = render_lines(&lines).join("\n");
        insta::assert_snapshot!(rendered);
    }

    #[test]
    fn stderr_tail_more_than_five_lines_snapshot() {
        // Build an exec cell with a non-zero exit and 10 lines on stderr to exercise
        // the head/tail rendering and gutter prefixes.
        let call_id = "c_err".to_string();
        let mut cell = ExecCell::new(ExecCall {
            call_id: call_id.clone(),
            command: vec!["bash".into(), "-lc".into(), "seq 1 10 1>&2 && false".into()],
            parsed: Vec::new(),
            output: None,
            start_time: Some(Instant::now()),
            duration: None,
        });
        let stderr: String = (1..=10)
            .map(|n| n.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        cell.complete_call(
            &call_id,
            CommandOutput {
                exit_code: 1,
                stdout: String::new(),
                stderr,
                formatted_output: String::new(),
            },
            Duration::from_millis(1),
        );

        let rendered = cell
            .display_lines(80)
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        insta::assert_snapshot!(rendered);
    }

    #[test]
    fn ran_cell_multiline_with_stderr_snapshot() {
        // Build an exec cell that completes (so it renders as "Ran") with a
        // command long enough that it must render on its own line under the
        // header, and include a couple of stderr lines to verify the output
        // block prefixes and wrapping.
        let call_id = "c_wrap_err".to_string();
        let long_cmd =
            "echo this_is_a_very_long_single_token_that_will_wrap_across_the_available_width";
        let mut cell = ExecCell::new(ExecCall {
            call_id: call_id.clone(),
            command: vec!["bash".into(), "-lc".into(), long_cmd.to_string()],
            parsed: Vec::new(),
            output: None,
            start_time: Some(Instant::now()),
            duration: None,
        });

        let stderr = "error: first line on stderr\nerror: second line on stderr".to_string();
        cell.complete_call(
            &call_id,
            CommandOutput {
                exit_code: 1,
                stdout: String::new(),
                stderr,
                formatted_output: String::new(),
            },
            Duration::from_millis(5),
        );

        // Narrow width to force the command to render under the header line.
        let width: u16 = 28;
        let rendered = cell
            .display_lines(width)
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        insta::assert_snapshot!(rendered);
    }
    #[test]
    fn user_history_cell_wraps_and_prefixes_each_line_snapshot() {
        let msg = "one two three four five six seven";
        let cell = UserHistoryCell {
            message: msg.to_string(),
        };

        // Small width to force wrapping more clearly. Effective wrap width is width-2 due to the ▌ prefix and trailing space.
        let width: u16 = 12;
        let lines = cell.display_lines(width);
        let rendered = render_lines(&lines).join("\n");

        insta::assert_snapshot!(rendered);
    }

    #[test]
    fn plan_update_with_note_and_wrapping_snapshot() {
        // Long explanation forces wrapping; include long step text to verify step wrapping and alignment.
        let update = UpdatePlanArgs {
            explanation: Some(
                "I’ll update Grafana call error handling by adding retries and clearer messages when the backend is unreachable."
                    .to_string(),
            ),
            plan: vec![
                PlanItemArg {
                    step: "Investigate existing error paths and logging around HTTP timeouts".into(),
                    status: StepStatus::Completed,
                },
                PlanItemArg {
                    step: "Harden Grafana client error handling with retry/backoff and user‑friendly messages".into(),
                    status: StepStatus::InProgress,
                },
                PlanItemArg {
                    step: "Add tests for transient failure scenarios and surfacing to the UI".into(),
                    status: StepStatus::Pending,
                },
            ],
        };

        let cell = new_plan_update(update);
        // Narrow width to force wrapping for both the note and steps
        let lines = cell.display_lines(32);
        let rendered = render_lines(&lines).join("\n");
        insta::assert_snapshot!(rendered);
    }

    #[test]
    fn plan_update_without_note_snapshot() {
        let update = UpdatePlanArgs {
            explanation: None,
            plan: vec![
                PlanItemArg {
                    step: "Define error taxonomy".into(),
                    status: StepStatus::InProgress,
                },
                PlanItemArg {
                    step: "Implement mapping to user messages".into(),
                    status: StepStatus::Pending,
                },
            ],
        };

        let cell = new_plan_update(update);
        let lines = cell.display_lines(40);
        let rendered = render_lines(&lines).join("\n");
        insta::assert_snapshot!(rendered);
    }
    #[test]
    fn reasoning_summary_block() {
        let mut config = test_config();
        config.model_family.reasoning_summary_format = ReasoningSummaryFormat::Experimental;

        let cell = new_reasoning_summary_block(
            "**High level reasoning**\n\nDetailed reasoning goes here.".to_string(),
            &config,
        );

        let rendered_display = render_lines(&cell.display_lines(80));
        assert_eq!(rendered_display, vec!["• Detailed reasoning goes here."]);

        let rendered_transcript = render_transcript(cell.as_ref());
        assert_eq!(
            rendered_transcript,
            vec!["thinking", "Detailed reasoning goes here."]
        );
    }

    #[test]
    fn reasoning_summary_block_returns_reasoning_cell_when_feature_disabled() {
        let mut config = test_config();
        config.model_family.reasoning_summary_format = ReasoningSummaryFormat::Experimental;

        let cell =
            new_reasoning_summary_block("Detailed reasoning goes here.".to_string(), &config);

        let rendered = render_transcript(cell.as_ref());
        assert_eq!(rendered, vec!["thinking", "Detailed reasoning goes here."]);
    }

    #[test]
    fn reasoning_summary_block_falls_back_when_header_is_missing() {
        let mut config = test_config();
        config.model_family.reasoning_summary_format = ReasoningSummaryFormat::Experimental;

        let cell = new_reasoning_summary_block(
            "**High level reasoning without closing".to_string(),
            &config,
        );

        let rendered = render_transcript(cell.as_ref());
        assert_eq!(
            rendered,
            vec!["thinking", "**High level reasoning without closing"]
        );
    }

    #[test]
    fn reasoning_summary_block_falls_back_when_summary_is_missing() {
        let mut config = test_config();
        config.model_family.reasoning_summary_format = ReasoningSummaryFormat::Experimental;

        let cell = new_reasoning_summary_block(
            "**High level reasoning without closing**".to_string(),
            &config,
        );

        let rendered = render_transcript(cell.as_ref());
        assert_eq!(
            rendered,
            vec!["thinking", "High level reasoning without closing"]
        );

        let cell = new_reasoning_summary_block(
            "**High level reasoning without closing**\n\n  ".to_string(),
            &config,
        );

        let rendered = render_transcript(cell.as_ref());
        assert_eq!(
            rendered,
            vec!["thinking", "High level reasoning without closing"]
        );
    }

    #[test]
    fn reasoning_summary_block_splits_header_and_summary_when_present() {
        let mut config = test_config();
        config.model_family.reasoning_summary_format = ReasoningSummaryFormat::Experimental;

        let cell = new_reasoning_summary_block(
            "**High level plan**\n\nWe should fix the bug next.".to_string(),
            &config,
        );

        let rendered_display = render_lines(&cell.display_lines(80));
        assert_eq!(rendered_display, vec!["• We should fix the bug next."]);

        let rendered_transcript = render_transcript(cell.as_ref());
        assert_eq!(
            rendered_transcript,
            vec!["thinking", "We should fix the bug next."]
        );
    }
}
