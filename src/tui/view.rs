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

use super::{App, Conversation, Create, Mode};

const ACCENT: Color = Color::Cyan;

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
                visible(view.session.alias())
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

    let mut status = match (app.mode, small) {
        (Mode::Insert, false) => "INSERT · Enter enviar · Ctrl+j salto · Esc volver",
        (Mode::Insert, true) => "INSERT · Enter enviar · Esc",
        (Mode::Normal, false) => "NORMAL · Ctrl+n nueva · q salir · i editar · j/k sesiones · Ctrl+u/d historial · Esc cancelar",
        (Mode::Normal, true) => "NORMAL · Ctrl+n nueva · q salir",
    }.to_owned();
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
            app.mode == Mode::Insert && app.create.is_none(),
            shared_cwd,
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
        Status::Idle if id => "Lista · reanuda",
        Status::Idle => "Lista",
        Status::Running => "Ejecutando",
        Status::Cancelling => "Cancelando",
        Status::Error => "Error",
        Status::Closed => "Cerrada",
    };
    let detail_text = format!(
        "{} · {provider} · {} · {state}\n{}{}",
        visible(view.session.alias()),
        visible(model.as_deref().unwrap_or("predeterminado")),
        visible(&view.session.cwd().display().to_string()),
        if shared_cwd { " · cwd compartido" } else { "" },
    );
    frame.render_widget(
        Paragraph::new(detail_text).style(Style::default().fg(ACCENT)),
        detail,
    );

    let block = Block::bordered().title(" Conversación ");
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

    let prompt = Block::bordered().title(if editing {
        " Prompt · INSERT "
    } else {
        " Prompt "
    });
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
    push(lines, "Vos", &turn.prompt, width, ACCENT);
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
    for message in &messages {
        push(lines, "Agente", message, width, Color::Green);
    }
    match turn.events.last() {
        Some(AgentEvent::TurnFailed { message, .. }) => {
            push(lines, "Error", message, width, Color::Red)
        }
        Some(AgentEvent::TurnCancelled) => {
            lines.push(Line::styled("Turno cancelado", Color::Yellow))
        }
        Some(AgentEvent::TurnCompleted { final_text })
            if messages.is_empty() && !final_text.is_empty() =>
        {
            push(lines, "Agente", final_text, width, Color::Green);
        }
        _ => {}
    }
    lines.push(Line::default());
}

fn push(lines: &mut Vec<Line<'static>>, title: &str, text: &str, width: usize, color: Color) {
    lines.push(Line::styled(
        title.to_owned(),
        Style::default().fg(color).add_modifier(Modifier::BOLD),
    ));
    lines.extend(wrap(text, width).into_iter().map(Line::from));
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
    let mut text: Vec<_> = ["Proveedor", "Modelo opcional", "Alias", "Cwd"]
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
        "Tab campo · ←/→ proveedor · Enter crear · Esc cerrar",
    ));
    frame.render_widget(
        Paragraph::new(text)
            .block(Block::bordered().title(" Nueva sesión "))
            .wrap(Wrap { trim: true }),
        modal,
    );
}
