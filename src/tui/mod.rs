mod view;

use std::{
    io::{self, IsTerminal},
    path::PathBuf,
    time::Duration,
};

use anyhow::{Result, ensure};
use crossterm::{
    cursor::{Hide, Show},
    event::{
        self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyEventKind,
        KeyModifiers,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use orch::{
    agy::Agy,
    codex::Codex,
    session::{CommandError, Runner, Session, Sessions, Status},
};
use ratatui::{Terminal, backend::CrosstermBackend, text::Line};
use tokio::{sync::watch, task::JoinHandle, time};

pub struct Options {
    pub cwd: PathBuf,
    pub codex: PathBuf,
    pub agy: PathBuf,
    pub timeout: Duration,
}

struct RestoreTerminal;

impl RestoreTerminal {
    fn enter() -> Result<Self> {
        ensure!(
            io::stdin().is_terminal() && io::stdout().is_terminal(),
            "la TUI requiere una terminal. Para la CLI usá --smoke-test"
        );
        enable_raw_mode()?;
        let restore = Self;
        execute!(
            io::stdout(),
            EnterAlternateScreen,
            Hide,
            EnableBracketedPaste
        )?;
        Ok(restore)
    }
}

impl Drop for RestoreTerminal {
    fn drop(&mut self) {
        if let Err(error) = execute!(
            io::stdout(),
            DisableBracketedPaste,
            LeaveAlternateScreen,
            Show
        ) {
            tracing::error!(%error, "could not restore terminal screen");
        }
        if let Err(error) = disable_raw_mode() {
            tracing::error!(%error, "could not restore terminal input");
        }
    }
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum Mode {
    #[default]
    Normal,
    Insert,
}

struct Create {
    agy: bool,
    field: usize,
    fields: [String; 3],
    error: Option<String>,
}

impl Create {
    fn new(cwd: &std::path::Path) -> Self {
        Self {
            agy: false,
            field: 0,
            fields: [String::new(), String::new(), cwd.display().to_string()],
            error: None,
        }
    }

    fn runner(&self, options: &Options) -> Runner {
        let model = self.fields[0].trim();
        let model = (!model.is_empty()).then(|| model.to_owned());
        let cwd = PathBuf::from(&self.fields[2]);
        if self.agy {
            Runner::Agy(Agy {
                executable: options.agy.clone(),
                cwd,
                timeout: options.timeout,
                model,
            })
        } else {
            Runner::Codex(Codex {
                executable: options.codex.clone(),
                cwd,
                timeout: options.timeout,
                model,
            })
        }
    }
}

struct Conversation {
    session: Session,
    draft: String,
    pending: Option<JoinHandle<Result<u64, CommandError>>>,
    notice: String,
    offset: Option<usize>,
    scroll_request: isize,
    width: u16,
    transcript_dirty: bool,
    completed_turns: usize,
    completed_lines: usize,
    transcript: Vec<Line<'static>>,
}

impl Conversation {
    fn new(session: Session) -> Self {
        Self {
            session,
            draft: String::new(),
            pending: None,
            notice: String::new(),
            offset: None,
            scroll_request: 0,
            width: 0,
            transcript: Vec::new(),
            transcript_dirty: true,
            completed_turns: 0,
            completed_lines: 0,
        }
    }

    async fn refresh(&mut self) -> bool {
        let updated = self.session.take_update();
        if updated {
            self.transcript_dirty = true;
        }
        if self.pending.as_ref().is_some_and(JoinHandle::is_finished)
            && let Some(task) = self.pending.take()
        {
            match task.await {
                Ok(Ok(_)) => self.draft.clear(),
                Ok(Err(error)) => self.notice = error.to_string(),
                Err(error) => self.notice = error.to_string(),
            }
            return true;
        }
        updated
    }

    fn submit(&mut self) -> bool {
        if self.pending.is_some() || self.draft.trim().is_empty() {
            return false;
        }
        self.notice.clear();
        let session = self.session.clone();
        let prompt = self.draft.clone();
        self.pending = Some(tokio::spawn(async move { session.send_turn(prompt).await }));
        true
    }
}

struct App {
    options: Options,
    sessions: Sessions,
    views: Vec<Conversation>,
    selected: usize,
    mode: Mode,
    create: Option<Create>,
}

impl App {
    fn handle(&mut self, input: Event) -> bool {
        let key = match &input {
            Event::Key(key) if key.kind != KeyEventKind::Release => Some(*key),
            Event::Paste(_) => None,
            _ => return true,
        };

        if let Some(create) = &mut self.create {
            match key.map(|key| key.code) {
                Some(KeyCode::Esc) => self.create = None,
                Some(KeyCode::Tab) => create.field = (create.field + 1) % 4,
                Some(KeyCode::BackTab) => create.field = (create.field + 3) % 4,
                Some(KeyCode::Enter) => match self
                    .sessions
                    .create(&create.fields[1], create.runner(&self.options))
                {
                    Ok(session) => {
                        self.selected = self.views.len();
                        self.views.push(Conversation::new(session));
                        self.mode = Mode::Normal;
                        self.create = None;
                    }
                    Err(error) => create.error = Some(error.to_string()),
                },
                Some(KeyCode::Left | KeyCode::Right | KeyCode::Char(' ')) if create.field == 0 => {
                    create.agy = !create.agy;
                }
                _ => {
                    if let Some(field) = create.field.checked_sub(1) {
                        edit(&mut create.fields[field], input, false);
                    }
                }
            }
            return true;
        }
        if key.is_some_and(|key| control(key, 'n')) {
            self.create = Some(Create::new(&self.options.cwd));
            return true;
        }
        if self.mode == Mode::Normal
            && key.is_some_and(|key| {
                key.code == KeyCode::Char('q')
                    && !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
            })
        {
            return false;
        }
        let Some(active) = self.views.get_mut(self.selected) else {
            return true;
        };

        if self.mode == Mode::Insert {
            match key.map(|key| key.code) {
                Some(KeyCode::Esc) => self.mode = Mode::Normal,
                Some(KeyCode::Enter) => {
                    if active.submit() {
                        self.mode = Mode::Normal;
                    }
                }
                _ if active.pending.is_none() => edit(&mut active.draft, input, true),
                _ => {}
            }
            return true;
        }
        if let Some(key) = key {
            match key.code {
                KeyCode::Char('d') if control(key, 'd') => active.scroll_request += 1,
                KeyCode::Char('u') if control(key, 'u') => active.scroll_request -= 1,
                KeyCode::Char('i') | KeyCode::Enter => self.mode = Mode::Insert,
                KeyCode::Esc => {
                    if active.session.inspect(|s| s.status) == Status::Running {
                        let session = active.session.clone();
                        tokio::spawn(async move {
                            if let Err(error) = session.cancel().await {
                                tracing::warn!(%error, "could not request turn cancellation");
                            }
                        });
                    }
                }
                KeyCode::Char('j') | KeyCode::Down
                    if !key.modifiers.contains(KeyModifiers::CONTROL) =>
                {
                    self.selected = (self.selected + 1) % self.views.len();
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    self.selected = (self.selected + self.views.len() - 1) % self.views.len();
                }
                _ => {}
            }
        }
        true
    }
}

fn control(key: KeyEvent, letter: char) -> bool {
    key.code == KeyCode::Char(letter) && key.modifiers.contains(KeyModifiers::CONTROL)
}

fn edit(value: &mut String, event: Event, multiline: bool) {
    let suffix = match event {
        Event::Key(key) if control(key, 'u') => {
            value.clear();
            return;
        }
        Event::Key(key) if multiline && control(key, 'j') => "\n".into(),
        Event::Key(key) if key.code == KeyCode::Backspace => {
            value.pop();
            return;
        }
        Event::Key(KeyEvent {
            code: KeyCode::Char(ch),
            modifiers,
            ..
        }) if !ch.is_control()
            && !modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
        {
            ch.to_string()
        }
        Event::Paste(text) => text
            .chars()
            .filter(|ch| !ch.is_control() || (multiline && *ch == '\n'))
            .collect(),
        _ => return,
    };
    if value.len() + suffix.len() <= 64 * 1024 {
        value.push_str(&suffix);
    }
}

pub async fn run(options: Options, mut signal: watch::Receiver<bool>) -> Result<bool> {
    // The default hook writes to stderr before unwinding. Keep diagnostics in the file.
    std::panic::set_hook(Box::new(|info| {
        tracing::error!(location = ?info.location(), "terminal UI panicked");
    }));
    let restore = RestoreTerminal::enter()?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    let size = terminal.size()?;
    ensure!(
        size.width > 0 && size.height > 0,
        "la terminal informó un tamaño vacío"
    );
    let mut app = App {
        options,
        sessions: Sessions::default(),
        views: Vec::new(),
        selected: 0,
        mode: Mode::Normal,
        create: None,
    };
    let mut ticks = time::interval(Duration::from_millis(40));
    ticks.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
    let result = async {
        let mut dirty = true;
        loop {
            tokio::select! {
                _ = signal.wait_for(|value| *value) => return Ok(true),
                _ = ticks.tick() => {}
            }
            for conversation in &mut app.views {
                dirty |= conversation.refresh().await;
            }
            for _ in 0..32 {
                // The Unix poll backend preserves keyboard readiness alongside SIGWINCH.
                if !event::poll(Duration::from_millis(1))? {
                    break;
                }
                dirty = true;
                let input = event::read()?;
                if matches!(&input, Event::Key(key) if key.kind != KeyEventKind::Release && control(*key, 'c')) {
                    return Ok(true);
                }
                if !app.handle(input) {
                    return Ok(false);
                }
            }
            if dirty {
                terminal.draw(|frame| view::draw(&mut app, frame))?;
                dirty = false;
            }
        }
    }
    .await;
    drop(terminal);
    drop(restore);
    let shutdown = app.sessions.shutdown().await;
    shutdown?;
    result
}
