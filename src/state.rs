use std::error::Error;
use std::io::Read;
use std::ops::DerefMut;
use std::process::Child;
use std::process::Command;
use std::process::Stdio;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::RwLock;

use arboard::Clipboard;
use crossterm::event;
use crossterm::event::Event;
use crossterm::event::KeyCode;
use crossterm::event::KeyEventKind;
use crossterm::event::KeyModifiers;
use git2::Repository;
use ratatui::layout::Rect;
use tui_prompts::State as _;
use tui_prompts::Status;

use crate::bindings::Bindings;
use crate::cli;
use crate::cmd_log::CmdLog;
use crate::cmd_log::CmdLogEntry;
use crate::config::Config;
use crate::menu::Menu;
use crate::menu::PendingMenu;
use crate::ops::Op;
use crate::prompt;
use crate::screen;
use crate::screen::Screen;
use crate::term;
use crate::term::Term;
use crate::ui;

use super::Res;

pub(crate) struct State {
    pub repo: Rc<Repository>,
    pub config: Rc<Config>,
    pub bindings: Bindings,
    pending_keys: Vec<(KeyModifiers, KeyCode)>,
    pub quit: bool,
    pub screens: Vec<Screen>,
    pub pending_menu: Option<PendingMenu>,
    pub pending_cmd: Option<(Child, Arc<RwLock<CmdLogEntry>>)>,
    enable_async_cmds: bool,
    pub current_cmd_log: CmdLog,
    pub prompt: prompt::Prompt,
    pub clipboard: Option<Clipboard>,
}

impl State {
    pub fn create(
        repo: Rc<Repository>,
        size: Rect,
        args: &cli::Args,
        config: Rc<Config>,
        enable_async_cmds: bool,
    ) -> Res<Self> {
        let screens = match args.command {
            Some(cli::Commands::Show { ref reference }) => {
                vec![screen::show::create(
                    Rc::clone(&config),
                    Rc::clone(&repo),
                    size,
                    reference.clone(),
                )?]
            }
            None => vec![screen::status::create(
                Rc::clone(&config),
                Rc::clone(&repo),
                size,
            )?],
        };

        let bindings = Bindings::from(&config.bindings);
        let pending_menu = root_menu(&config).map(PendingMenu::init);

        let clipboard = Clipboard::new()
            .inspect_err(|e| log::warn!("Couldn't initialize clipboard: {}", e))
            .ok();

        Ok(Self {
            repo,
            config,
            bindings,
            pending_keys: vec![],
            enable_async_cmds,
            quit: false,
            screens,
            pending_cmd: None,
            pending_menu,
            current_cmd_log: CmdLog::new(),
            prompt: prompt::Prompt::new(),
            clipboard,
        })
    }

    pub fn update(&mut self, term: &mut Term, events: &[Event]) -> Res<()> {
        for event in events {
            match *event {
                Event::Resize(w, h) => {
                    for screen in self.screens.iter_mut() {
                        screen.size = Rect::new(0, 0, w, h);
                    }
                }
                Event::Key(key) => {
                    if self.prompt.state.is_focused() {
                        self.prompt.state.handle_key_event(key)
                    } else if key.kind == KeyEventKind::Press {
                        if self.pending_cmd.is_none() {
                            self.current_cmd_log.clear();
                        }

                        self.handle_key_input(term, key)?;
                    }
                }
                _ => (),
            }

            self.update_prompt(term)?;
        }

        let handle_pending_cmd_result = self.handle_pending_cmd();
        let pending_cmd_done = self
            .handle_result(handle_pending_cmd_result)
            .unwrap_or(true);

        let needs_redraw = !events.is_empty() || pending_cmd_done;

        if needs_redraw && self.screens.last_mut().is_some() {
            term.draw(|frame| ui::ui(frame, self))?;
        }

        Ok(())
    }

    fn update_prompt(&mut self, term: &mut Term) -> Res<()> {
        if self.prompt.state.status() == Status::Aborted {
            self.prompt.reset(term)?;
        } else if let Some(mut prompt_data) = self.prompt.data.take() {
            let result = (Rc::get_mut(&mut prompt_data.update_fn).unwrap())(self, term);

            match result {
                Ok(()) => {
                    if self.prompt.state.is_focused() {
                        self.prompt.data = Some(prompt_data);
                    }
                }
                Err(error) => self
                    .current_cmd_log
                    .push(CmdLogEntry::Error(error.to_string())),
            }
        }

        Ok(())
    }

    fn handle_key_input(&mut self, term: &mut Term, key: event::KeyEvent) -> Res<()> {
        let menu = match &self.pending_menu {
            None => Menu::Root,
            Some(menu) if menu.menu == Menu::Help => Menu::Root,
            Some(menu) => menu.menu,
        };

        self.pending_keys.push((key.modifiers, key.code));
        let matching_bindings = self
            .bindings
            .match_bindings(&menu, &self.pending_keys)
            .collect::<Vec<_>>();

        match matching_bindings[..] {
            [binding] => {
                if binding.keys == self.pending_keys {
                    self.handle_op(binding.op.clone(), term)?;
                    self.pending_keys.clear();
                }
            }
            [] => self.pending_keys.clear(),
            [_, ..] => (),
        }

        Ok(())
    }

    pub(crate) fn handle_op(&mut self, op: Op, term: &mut Term) -> Res<()> {
        let target = self.screen().get_selected_item().target_data.as_ref();
        if let Some(mut action) = op.clone().implementation().get_action(target) {
            let result = Rc::get_mut(&mut action).unwrap()(self, term);
            self.handle_result(result);
        }

        Ok(())
    }

    fn handle_result<T>(&mut self, result: Result<T, Box<dyn Error>>) -> Option<T> {
        match result {
            Ok(value) => Some(value),
            Err(error) => {
                self.current_cmd_log
                    .push(CmdLogEntry::Error(error.to_string()));

                None
            }
        }
    }

    pub fn close_menu(&mut self) {
        self.pending_menu = root_menu(&self.config).map(PendingMenu::init)
    }

    pub fn screen_mut(&mut self) -> &mut Screen {
        self.screens.last_mut().expect("No screen")
    }

    pub fn screen(&self) -> &Screen {
        self.screens.last().expect("No screen")
    }

    /// Displays an `Info` message to the CmdLog.
    pub fn display_info(&mut self, message: String) {
        self.current_cmd_log.push(CmdLogEntry::Info(message));
    }

    /// Displays an `Error` message to the CmdLog.
    pub fn display_error(&mut self, error: String) {
        self.current_cmd_log.push(CmdLogEntry::Error(error));
    }

    /// Runs a `Command` and handles its output.
    /// Will block awaiting its completion.
    pub fn run_cmd(&mut self, term: &mut Term, input: &[u8], cmd: Command) -> Res<()> {
        self.run_cmd_async(term, input, cmd)?;
        self.await_pending_cmd()?;
        self.handle_pending_cmd()?;
        Ok(())
    }

    /// Runs a `Command` and handles its output asynchronously (if async commands are enabled).
    /// Will return `Ok(())` if one is already running.
    pub fn run_cmd_async(&mut self, term: &mut Term, input: &[u8], mut cmd: Command) -> Res<()> {
        if self.pending_cmd.is_some() {
            return Err("A command is already running".into());
        }

        cmd.current_dir(self.repo.workdir().expect("No workdir"));

        cmd.stdin(Stdio::piped());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());

        let log_entry = self.current_cmd_log.push_cmd(&cmd);
        term.draw(|frame| ui::ui(frame, self))?;

        let mut child = cmd.spawn()?;

        use std::io::Write;
        child.stdin.take().unwrap().write_all(input)?;

        self.pending_cmd = Some((child, log_entry));

        if !self.enable_async_cmds {
            self.await_pending_cmd()?;
        }

        Ok(())
    }

    fn await_pending_cmd(&mut self) -> Res<()> {
        if let Some((child, _)) = &mut self.pending_cmd {
            child.wait()?;
        }
        Ok(())
    }

    /// Handles any pending_cmd in State without blocking. Returns `true` if a cmd was handled.
    pub fn handle_pending_cmd(&mut self) -> Res<bool> {
        let Some((ref mut child, ref mut log_rwlock)) = self.pending_cmd else {
            return Ok(false);
        };

        let Some(status) = child.try_wait()? else {
            return Ok(false);
        };

        log::debug!("pending cmd finished with {:?}", status);

        let result = write_child_output_to_log(log_rwlock, child, status);
        self.pending_cmd = None;
        self.screen_mut().update()?;
        result?;

        Ok(true)
    }

    pub fn run_cmd_interactive(&mut self, term: &mut Term, mut cmd: Command) -> Res<()> {
        if self.pending_cmd.is_some() {
            return Err("A command is already running".into());
        }

        cmd.current_dir(self.repo.workdir().expect("No workdir"));

        cmd.stdin(Stdio::piped());
        let child = cmd.spawn()?;

        let out = child.wait_with_output()?;
        let out_utf8 = String::from_utf8(out.stderr.clone())
            .expect("Error turning command output to String")
            .into();

        self.current_cmd_log.push_cmd_with_output(&cmd, out_utf8);

        // Prevents cursor flash when exiting editor
        term.hide_cursor()?;

        // In case the command left the alternate screen (editors would)
        term::enter_alternate_screen()?;

        term.clear()?;
        self.screen_mut().update()?;

        if !out.status.success() {
            return Err(format!(
                "exited with code: {}",
                out.status
                    .code()
                    .map(|c| c.to_string())
                    .unwrap_or("".to_string())
            )
            .into());
        }

        Ok(())
    }

    pub fn hide_menu(&mut self) {
        if let Some(ref mut menu) = self.pending_menu {
            menu.is_hidden = true;
        }
    }

    pub fn unhide_menu(&mut self) {
        if let Some(ref mut menu) = self.pending_menu {
            menu.is_hidden = false;
        }
    }
}

pub(crate) fn root_menu(config: &Config) -> Option<Menu> {
    if config.general.always_show_help.enabled {
        Some(Menu::Help)
    } else {
        None
    }
}

fn write_child_output_to_log(
    log_rwlock: &mut Arc<RwLock<CmdLogEntry>>,
    child: &mut Child,
    status: std::process::ExitStatus,
) -> Result<(), Box<dyn Error>> {
    let mut log = log_rwlock.write().unwrap();

    let CmdLogEntry::Cmd { args, out: out_log } = log.deref_mut() else {
        unreachable!("pending_cmd is always CmdLogEntry::Cmd variant");
    };

    drop(child.stdin.take());

    let mut out_bytes = vec![];
    log::debug!("Reading stderr");

    child
        .stderr
        .take()
        .unwrap()
        .read_to_end(&mut out_bytes)
        .map_err(|e| format!("Couldn't read cmd output: {}", e))?;

    child
        .stdout
        .take()
        .unwrap()
        .read_to_end(&mut out_bytes)
        .map_err(|e| format!("Couldn't read cmd output: {}", e))?;

    let out_string = String::from_utf8(out_bytes.clone())?;
    *out_log = Some(out_string.into());

    if !status.success() {
        return Err(format!(
            "'{}' exited with code: {}",
            args,
            status
                .code()
                .map(|c| c.to_string())
                .unwrap_or("".to_string())
        )
        .into());
    }

    Ok(())
}
