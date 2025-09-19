use crate::frames::ALL_VARIANTS as FRAME_VARIANTS;
use crate::frames::FRAME_TICK_DEFAULT;
use crate::tui::FrameRequester;
use crate::tui::Tui;
use crate::tui::TuiEvent;
use color_eyre::eyre::Result;
use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use crossterm::event::KeyModifiers;
use rand::Rng as _;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::prelude::Widget;
use ratatui::style::Stylize;
use ratatui::text::Line;
use ratatui::widgets::Clear;
use ratatui::widgets::Paragraph;
use ratatui::widgets::WidgetRef;
use ratatui::widgets::Wrap;
use std::time::Duration;
use tokio_stream::StreamExt;

const FRAME_TICK: Duration = FRAME_TICK_DEFAULT;
const MIN_ANIMATION_HEIGHT: u16 = 24;
const MIN_ANIMATION_WIDTH: u16 = 60;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ModelUpgradeDecision {
    Switch,
    KeepCurrent,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ModelUpgradeOption {
    TryNewModel,
    KeepCurrent,
}

struct ModelUpgradePopup {
    highlighted: ModelUpgradeOption,
    decision: Option<ModelUpgradeDecision>,
    request_frame: FrameRequester,
    frame_idx: usize,
    variant_idx: usize,
}

impl ModelUpgradePopup {
    fn new(request_frame: FrameRequester) -> Self {
        Self {
            highlighted: ModelUpgradeOption::TryNewModel,
            decision: None,
            request_frame,
            frame_idx: 0,
            variant_idx: 0,
        }
    }

    fn handle_key_event(&mut self, key_event: KeyEvent) {
        match key_event.code {
            KeyCode::Up | KeyCode::Char('k') => self.highlight(ModelUpgradeOption::TryNewModel),
            KeyCode::Down | KeyCode::Char('j') => self.highlight(ModelUpgradeOption::KeepCurrent),
            KeyCode::Char('1') => self.select(ModelUpgradeOption::TryNewModel),
            KeyCode::Char('2') => self.select(ModelUpgradeOption::KeepCurrent),
            KeyCode::Enter => self.select(self.highlighted),
            KeyCode::Esc => self.select(ModelUpgradeOption::KeepCurrent),
            KeyCode::Char('.') => {
                if key_event.modifiers.contains(KeyModifiers::CONTROL) {
                    self.pick_random_variant();
                }
            }
            _ => {}
        }
    }

    fn highlight(&mut self, option: ModelUpgradeOption) {
        if self.highlighted != option {
            self.highlighted = option;
            self.request_frame.schedule_frame();
        }
    }

    fn select(&mut self, option: ModelUpgradeOption) {
        self.decision = Some(option.into());
        self.request_frame.schedule_frame();
    }

    fn advance_animation(&mut self) {
        let len = self.frames().len();
        self.frame_idx = (self.frame_idx + 1) % len;
        self.request_frame.schedule_frame_in(FRAME_TICK);
    }

    fn frames(&self) -> &'static [&'static str] {
        FRAME_VARIANTS[self.variant_idx]
    }

    fn pick_random_variant(&mut self) {
        let total = FRAME_VARIANTS.len();
        if total <= 1 {
            return;
        }
        let mut rng = rand::rng();
        let mut next = self.variant_idx;
        while next == self.variant_idx {
            next = rng.random_range(0..total);
        }
        self.variant_idx = next;
        self.request_frame.schedule_frame();
    }
}

impl From<ModelUpgradeOption> for ModelUpgradeDecision {
    fn from(option: ModelUpgradeOption) -> Self {
        match option {
            ModelUpgradeOption::TryNewModel => ModelUpgradeDecision::Switch,
            ModelUpgradeOption::KeepCurrent => ModelUpgradeDecision::KeepCurrent,
        }
    }
}

impl WidgetRef for &ModelUpgradePopup {
    fn render_ref(&self, area: Rect, buf: &mut Buffer) {
        Clear.render(area, buf);

        // Skip the animation entirely when the viewport is too small so we don't clip frames.
        let show_animation =
            area.height >= MIN_ANIMATION_HEIGHT && area.width >= MIN_ANIMATION_WIDTH;

        let mut lines: Vec<Line> = Vec::new();
        if show_animation {
            let frame = self.frames()[self.frame_idx];
            lines.extend(frame.lines().map(|l| l.into()));
            // Spacer between animation and text content.
            lines.push("".into());
        }

        lines.push(Line::from(vec![
            "  ".into(),
            "Introducing GPT-5-Codex".bold(),
        ]));
        lines.push("".into());
        lines.push(
            "  GPT-5-Codex works faster through easy tasks and harder on complex tasks,".into(),
        );
        lines.push("  improves on code quality, and is more steerable with AGENTS.md.".into());
        lines.push("".into());

        let create_option =
            |index: usize, option: ModelUpgradeOption, text: &str| -> Line<'static> {
                if self.highlighted == option {
                    Line::from(vec![
                        format!("> {}. ", index + 1).cyan(),
                        text.to_owned().cyan(),
                    ])
                } else {
                    format!("  {}. {text}", index + 1).into()
                }
            };

        lines.push(create_option(
            0,
            ModelUpgradeOption::TryNewModel,
            "Try the new GPT-5-Codex model",
        ));
        lines.push("".into());
        lines.push(create_option(
            1,
            ModelUpgradeOption::KeepCurrent,
            "Continue using current model",
        ));
        lines.push("".into());
        lines.push(
            "  Press Enter to confirm or Esc to keep your current model"
                .dim()
                .into(),
        );

        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .render(area, buf);
    }
}

pub(crate) async fn run_model_upgrade_popup(tui: &mut Tui) -> Result<ModelUpgradeDecision> {
    let mut popup = ModelUpgradePopup::new(tui.frame_requester());

    tui.draw(u16::MAX, |frame| {
        frame.render_widget_ref(&popup, frame.area());
    })?;

    popup.advance_animation();

    let events = tui.event_stream();
    tokio::pin!(events);
    while popup.decision.is_none() {
        if let Some(event) = events.next().await {
            match event {
                TuiEvent::Key(key_event) => popup.handle_key_event(key_event),
                TuiEvent::Draw => {
                    popup.advance_animation();
                    let _ = tui.draw(u16::MAX, |frame| {
                        frame.render_widget_ref(&popup, frame.area());
                    });
                }
                _ => {}
            }
        } else {
            break;
        }
    }

    Ok(popup.decision.unwrap_or(ModelUpgradeDecision::KeepCurrent))
}
