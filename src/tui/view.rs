use std::collections::HashMap;

use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Clear, List, ListItem, ListState, Paragraph, Wrap},
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use orch::{
    events::AgentEvent,
    session::{Provider, Status, Turn},
};

use super::{App, Conversation, Create};

const ACCENT: Color = Color::Cyan;
const MESSAGE_BACKGROUND: Color = Color::Indexed(237);
const MESSAGE_FOREGROUND: Color = Color::Indexed(255);
pub(super) const SPINNER: [char; 8] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧'];

pub(super) fn draw(app: &mut App, frame: &mut Frame) {
    let area = frame.area();
    if area.width < 12 || area.height < 6 {
        frame.render_widget(Paragraph::new("Terminal pequeña"), area);
        return;
    }
    let [body, bar] = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(area);
    let small = body.width < 64;
    let [sessions, active] = if small {
        Layout::vertical([Constraint::Length(3), Constraint::Min(1)]).areas(body)
    } else {
        Layout::horizontal([
            Constraint::Length((body.width / 4).clamp(19, 32)),
            Constraint::Min(1),
        ])
        .areas(body)
    };
    let items: Vec<_> = app
        .views
        .iter()
        .map(|view| {
            let status = view.session.inspect(|s| s.status);
            ListItem::new(format!(
                "{} {}",
                indicator(status),
                sidebar_alias(
                    view.session.alias(),
                    sessions.width.saturating_sub(6).into()
                )
            ))
        })
        .collect();
    let block = Block::bordered().title(" Sesiones ");
    if items.is_empty() {
        frame.render_widget(Paragraph::new("Ctrl+n · Nueva").block(block), sessions);
    } else {
        let list = List::new(items)
            .block(block)
            .highlight_style(Style::default().fg(ACCENT).add_modifier(Modifier::BOLD))
            .highlight_symbol("› ");
        frame.render_stateful_widget(
            list,
            sessions,
            &mut ListState::default().with_selected(Some(app.selected)),
        );
    }

    let newline = if app.keyboard_enhanced {
        "Shift+Enter"
    } else {
        "Ctrl+j"
    };
    let mut status = if small {
        "Enter enviar · Alt+↑/↓ · Alt+q".to_owned()
    } else {
        format!("Enter enviar · {newline} salto · Alt+↑/↓ cambiar · Ctrl+n nueva · Alt+q salir")
    };
    let shared_cwd = app.views.get(app.selected).is_some_and(|selected| {
        app.views
            .iter()
            .filter(|other| other.session.cwd() == selected.session.cwd())
            .count()
            > 1
    });
    if let Some(view) = app.views.get_mut(app.selected) {
        draw_active(
            frame,
            active,
            view,
            app.create.is_none(),
            shared_cwd,
            SPINNER[app.spinner],
        );
        if !view.notice.is_empty() {
            status = format!("{} · {}", visible(&view.notice), status);
        }
    } else {
        frame.render_widget(
            Paragraph::new("Abrí una sesión con Ctrl+n.").block(Block::bordered().title(" Orch ")),
            active,
        );
    }
    frame.render_widget(
        Paragraph::new(status).style(Style::default().fg(ACCENT)),
        bar,
    );
    if let Some(create) = &app.create {
        draw_create(frame, area, create);
    }
}

fn sidebar_alias(alias: &str, width: usize) -> String {
    let alias = visible(alias);
    if alias.width() <= width {
        return alias;
    }
    if let Some((prefix, suffix)) = alias.rsplit_once('-')
        && suffix.len() == 8
        && suffix.bytes().all(|byte| byte.is_ascii_hexdigit())
        && width > suffix.len()
    {
        let mut room = width - suffix.len() - 1;
        let prefix: String = prefix
            .chars()
            .take_while(|ch| {
                let cells = ch.width().unwrap_or(0);
                let fits = cells <= room;
                room = room.saturating_sub(cells);
                fits
            })
            .collect();
        return format!("{prefix}…{suffix}");
    }
    alias
}

fn indicator(status: Status) -> &'static str {
    match status {
        Status::Idle => "·",
        Status::Running => "▶",
        Status::Cancelling => "…",
        Status::Error => "!",
        Status::Closed => "×",
    }
}

fn draw_active(
    frame: &mut Frame,
    area: Rect,
    view: &mut Conversation,
    editing: bool,
    shared_cwd: bool,
    spinner: char,
) {
    let [detail, history, input] = Layout::vertical([
        Constraint::Length(2),
        Constraint::Min(1),
        Constraint::Length(if area.height > 10 { 4 } else { 3 }),
    ])
    .areas(area);
    let (provider, model) = match view.session.provider() {
        Provider::Codex { model } => ("Codex", model),
        Provider::Agy { model } => ("agy", model),
    };
    let (status, id) = view
        .session
        .inspect(|s| (s.status, s.provider_session_id.is_some()));
    let state = match status {
        Status::Idle if view.pending.is_some() => "",
        Status::Idle if id => " · Lista · reanuda",
        Status::Idle => " · Lista",
        Status::Running | Status::Cancelling => "",
        Status::Error => " · Error",
        Status::Closed => " · Cerrada",
    };
    let model = visible(
        view.model
            .reported()
            .or(model.as_deref())
            .unwrap_or("modelo automático"),
    );
    let detail_text = format!(
        "{provider} · {model}{state}\n{}{}",
        visible(&view.session.cwd().display().to_string()),
        if shared_cwd { " · cwd compartido" } else { "" },
    );
    frame.render_widget(
        Paragraph::new(detail_text).style(Style::default().fg(ACCENT)),
        detail,
    );

    let title = if status == Status::Cancelling {
        " Cancelando ".into()
    } else if view.is_waiting() {
        format!(" {spinner} Respondiendo ")
    } else {
        " Conversación ".into()
    };
    let block = Block::bordered().title(title);
    let inside = block.inner(history);
    if inside.width != view.width {
        view.width = inside.width;
        view.completed_turns = 0;
        view.completed_lines = 0;
        view.transcript_dirty = true;
    }
    if view.transcript_dirty {
        view.transcript_dirty = false;
        view.transcript.truncate(view.completed_lines);
        let width = usize::from(inside.width.max(1));
        view.session.inspect(|snapshot| {
            for turn in &snapshot.history[view.completed_turns..] {
                render_turn(&mut view.transcript, turn, width);
                if matches!(
                    turn.events.last(),
                    Some(
                        AgentEvent::TurnCompleted { .. }
                            | AgentEvent::TurnFailed { .. }
                            | AgentEvent::TurnCancelled
                    )
                ) {
                    view.completed_turns += 1;
                    view.completed_lines = view.transcript.len();
                }
            }
        });
    }
    let height = usize::from(inside.height);
    let bottom = view.transcript.len().saturating_sub(height);
    let mut offset = view.offset.unwrap_or(bottom).min(bottom);
    if view.scroll_request != 0 {
        let page = (height / 2).max(1) * view.scroll_request.unsigned_abs();
        offset = if view.scroll_request < 0 {
            offset.saturating_sub(page)
        } else {
            (offset + page).min(bottom)
        };
        view.offset = (offset < bottom).then_some(offset);
        view.scroll_request = 0;
    } else if let Some(saved) = &mut view.offset {
        *saved = offset;
    }
    let lines = view
        .transcript
        .iter()
        .skip(offset)
        .take(height)
        .cloned()
        .collect::<Vec<_>>();
    frame.render_widget(Paragraph::new(lines).block(block), history);

    let prompt = Block::bordered().title(" Mensaje ");
    let inside = prompt.inner(input);
    let lines = wrap(&view.draft, usize::from(inside.width.max(1)));
    let end = lines.len().saturating_sub(usize::from(inside.height));
    let last_width = lines
        .last()
        .map_or(0, |line| UnicodeWidthStr::width(line.as_str()));
    let lines: Vec<Line<'static>> = lines.into_iter().skip(end).map(Line::from).collect();
    if editing && inside.width > 0 && inside.height > 0 {
        let column = (last_width as u16).min(inside.width - 1);
        let row = (lines.len().saturating_sub(1) as u16).min(inside.height - 1);
        frame.set_cursor_position((inside.x + column, inside.y + row));
    }
    frame.render_widget(Paragraph::new(lines).block(prompt), input);
}

fn render_turn(lines: &mut Vec<Line<'static>>, turn: &Turn, width: usize) {
    let inset = usize::from(width > 2);
    let content_width = width.saturating_sub(inset * 2).max(1);
    let style = if crossterm::style::Colored::ansi_color_disabled_memoized() {
        Style::default().add_modifier(Modifier::REVERSED)
    } else {
        Style::default()
            .fg(MESSAGE_FOREGROUND)
            .bg(MESSAGE_BACKGROUND)
    };
    lines.extend(wrap(&turn.prompt, content_width).into_iter().map(|text| {
        let padding = width.saturating_sub(UnicodeWidthStr::width(text.as_str()) + inset);
        Line::styled(
            format!("{}{text}{}", " ".repeat(inset), " ".repeat(padding)),
            style,
        )
    }));
    let mut messages = Vec::<String>::new();
    let mut indices = HashMap::new();
    for event in &turn.events {
        match event {
            AgentEvent::TextDelta { message_id, text }
            | AgentEvent::MessageCompleted { message_id, text } => {
                let index = *indices.entry(message_id).or_insert_with(|| {
                    messages.push(String::new());
                    messages.len() - 1
                });
                let message = &mut messages[index];
                if matches!(event, AgentEvent::MessageCompleted { .. }) {
                    message.clear();
                }
                message.push_str(text);
            }
            _ => {}
        }
    }
    let terminal = turn.events.last();
    if !messages.is_empty()
        || matches!(
            terminal,
            Some(
                AgentEvent::TurnFailed { .. }
                    | AgentEvent::TurnCancelled
                    | AgentEvent::TurnCompleted { .. }
            )
        )
    {
        lines.push(Line::default());
    }
    for message in &messages {
        lines.extend(
            wrap(message.trim_end_matches('\n'), width)
                .into_iter()
                .map(Line::from),
        );
    }
    match terminal {
        Some(AgentEvent::TurnFailed { message, .. }) => {
            lines.push(Line::styled(
                "Error",
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            ));
            lines.extend(wrap(message, width).into_iter().map(Line::from));
        }
        Some(AgentEvent::TurnCancelled) => {
            lines.push(Line::styled("Turno cancelado", Color::Yellow))
        }
        Some(AgentEvent::TurnCompleted { final_text })
            if messages.is_empty() && !final_text.is_empty() =>
        {
            lines.extend(
                wrap(final_text.trim_end_matches('\n'), width)
                    .into_iter()
                    .map(Line::from),
            );
        }
        _ => {}
    }
    lines.push(Line::default());
}

fn visible(text: &str) -> String {
    text.chars().filter(|ch| !ch.is_control()).collect()
}

fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut column = 0;
    let mut line = String::new();
    let text = text.replace('\t', "    ");
    for ch in text.chars() {
        if ch == '\n' {
            lines.push(std::mem::take(&mut line));
            column = 0;
            continue;
        }
        let Some(size) = ch.width() else {
            continue;
        };
        if column > 0 && column + size > width {
            lines.push(std::mem::take(&mut line));
            column = 0;
        }
        line.push(ch);
        column += size;
    }
    lines.push(line);
    lines
}

fn draw_create(frame: &mut Frame, area: Rect, create: &Create) {
    let width = area.width.min(72);
    let height = area.height.min(12);
    let modal = Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    );
    frame.render_widget(Clear, modal);
    let values = [
        (if create.agy { "agy" } else { "Codex" }).to_owned(),
        if create.fields[0].is_empty() {
            "predeterminado".to_owned()
        } else {
            visible(&create.fields[0])
        },
        visible(&create.fields[1]),
        visible(&create.fields[2]),
    ];
    let mut text: Vec<_> = ["Proveedor", "Modelo opcional", "Alias opcional", "Cwd"]
        .into_iter()
        .zip(values)
        .enumerate()
        .map(|(index, (label, value))| {
            let style = if create.field == index {
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            Line::from(Span::styled(
                format!(
                    "{} {label}  {value}",
                    if create.field == index { "›" } else { " " }
                ),
                style,
            ))
        })
        .collect();
    if let Some(error) = &create.error {
        text.push(Line::styled(visible(error), Color::Red));
    }
    text.push(Line::from(
        "↑/↓ o Tab campo · ←/→ proveedor · Enter crear · Esc cerrar",
    ));
    frame.render_widget(
        Paragraph::new(text)
            .block(Block::bordered().title(" Nueva sesión "))
            .wrap(Wrap { trim: true }),
        modal,
    );
}
