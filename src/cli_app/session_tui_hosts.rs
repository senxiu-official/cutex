//! Host connection editor. Testing is read-only and stays off the UI thread.
use super::{
    session_tui::ShellEvents, session_tui_input, session_tui_layout as theme,
    session_tui_terminal::Terminal,
};
use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
use cutex::management::connections::{Connection, Hosts};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Layout},
    style::Style,
    text::{Line, Span},
    widgets::{Block, Paragraph, Wrap},
};
use std::{io::Stdout, sync::mpsc};
use tui_input::Input;

struct Editor {
    original: Option<usize>,
    fields: Vec<(&'static str, Input)>,
    selected: usize,
}
impl Editor {
    fn new(c: Option<&Connection>, original: Option<usize>) -> Self {
        let values = match c {
            Some(c) => vec![
                c.id.clone(),
                c.name.clone(),
                c.host_id.clone(),
                c.ssh_target.clone(),
                c.local_port.to_string(),
                c.remote_port.to_string(),
                c.token_file.to_string_lossy().into_owned(),
            ],
            None => vec![
                "".into(),
                "".into(),
                "".into(),
                "".into(),
                "24671".into(),
                "24270".into(),
                "".into(),
            ],
        };
        Self {
            original,
            fields: [
                "Connection ID",
                "Display name",
                "Host identity",
                "SSH target",
                "Local tunnel port",
                "Remote service port",
                "Credential file",
            ]
            .into_iter()
            .zip(values)
            .map(|(k, v)| (k, Input::from(v)))
            .collect(),
            selected: 0,
        }
    }
    fn connection(&self) -> anyhow::Result<Connection> {
        let v = |i: usize| self.fields[i].1.value().trim().to_owned();
        Ok(Connection {
            id: v(0),
            name: v(1),
            host_id: v(2),
            ssh_target: v(3),
            local_port: v(4).parse()?,
            remote_port: v(5).parse()?,
            token_file: v(6).into(),
            enabled: true,
        })
    }
}
pub(super) fn run(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    events: &mut ShellEvents,
) -> anyhow::Result<super::session_tui_remote::Outcome> {
    let mut hosts = Hosts::load()?;
    let mut selected = 0usize;
    let mut editor: Option<Editor> = None;
    let mut rename: Option<Input> = None;
    let mut deleting = false;
    let mut notice = String::new();
    let (tx, rx) = mpsc::channel();
    let mut testing = false;
    loop {
        if let Ok(result) = rx.try_recv() {
            testing = false;
            notice = result;
        }
        terminal.draw(|f|{
   let area=Layout::vertical([Constraint::Length(1),Constraint::Min(5),Constraint::Length(3),Constraint::Length(2)]).split(f.area());
   f.render_widget(Paragraph::new("Hosts / Connections").style(Style::default().fg(theme::focus())),area[0]);
   let panes=Layout::horizontal([Constraint::Percentage(35),Constraint::Percentage(65)]).split(area[1]);
   let mut names=vec![hosts.local_name.clone().unwrap_or_else(cutex::platform::host::current_host_name)+" · Local"];
   names.extend(hosts.connections.iter().map(|c|format!("{} · {}",c.name,if c.enabled{"Remote"}else{"Disabled"})));
   let lines:Vec<_>=names.iter().enumerate().map(|(i,n)|Line::styled(format!("{} {}",if i==selected{">"}else{" "},n),Style::default().fg(theme::text()).bg(if i==selected{theme::selection()}else{ratatui::style::Color::Reset}))).collect();
   f.render_widget(Paragraph::new(lines).block(Block::bordered().title(" Hosts ")),panes[0]);
   let detail:Vec<Line>=if let Some(e)=&editor {
    e.fields.iter().enumerate().map(|(i,(name,input))|Line::from(vec![Span::styled(format!("{name}: "),Style::default().fg(if i==e.selected{theme::focus()}else{theme::muted()})),Span::raw(input.value().to_owned())])).collect()
   } else if let Some(input)=&rename {vec![Line::from(format!("Local display name: {}",input.value()))]}
   else if selected==0 {vec![Line::from(format!("Identity: {}",cutex::platform::host::current_host_name())),Line::from("Local identity is unchanged when renaming this display."),Line::from(format!("Configuration: {}",cutex::management::connections::path().unwrap_or_default().display()))]}
   else {let c=&hosts.connections[selected-1];vec![Line::from(format!("Connection: {}",c.id)),Line::from(format!("Host identity: {}",c.host_id)),Line::from(format!("SSH target: {}",c.ssh_target)),Line::from(format!("Management: {}",c.base_url())),Line::from(format!("Credential file: {}",c.token_file.display())),Line::from(""),Line::from("Start this tunnel in another terminal:"),Line::from(c.tunnel_command()),Line::from(""),Line::from("Enable/disable changes Cutex routing only. It does not stop the tunnel or any runtime.")]};
   f.render_widget(Paragraph::new(detail).wrap(Wrap{trim:false}).block(Block::bordered().title(" Details ")),panes[1]);
   f.render_widget(Paragraph::new(if deleting{"Remove selected connection? Y confirms · Esc cancels"}else if testing{"Testing host identity…"}else{&notice}).wrap(Wrap{trim:false}),area[2]);
   let hints=if editor.is_some()||rename.is_some(){"↑/↓ Tab fields · Ctrl+S save · Esc discard"}else{"↑/↓ select · Enter edit · B browse · N add · T test · Space enable/disable · Delete remove · Esc back"};
   f.render_widget(Paragraph::new(hints).style(Style::default().fg(theme::focus())).wrap(Wrap{trim:true}),area[3]);
  })?;
        let Some(event) = events.next()? else {
            continue;
        };
        if let Event::Paste(paste) = &event {
            let input = if let Some(e) = editor.as_mut() {
                Some(&mut e.fields[e.selected].1)
            } else {
                rename.as_mut()
            };
            if let Some(input) = input {
                let mut s = input.value().to_owned();
                for ch in paste.chars().filter(|c| !c.is_control()) {
                    if s.len() + ch.len_utf8() > 4096 {
                        break;
                    }
                    s.push(ch);
                }
                *input = Input::from(s);
            }
            continue;
        }
        let Event::Key(key) = event else { continue };
        if key.kind == KeyEventKind::Release {
            continue;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return Ok(super::session_tui_remote::Outcome::Exit);
        }
        if deleting {
            match key.code {
                KeyCode::Char('y' | 'Y') => {
                    let mut updated = hosts.clone();
                    updated.connections.remove(selected - 1);
                    match updated.save() {
                        Ok(()) => {
                            hosts = updated;
                            selected = selected.min(hosts.connections.len());
                            notice = "Connection removed".into()
                        }
                        Err(e) => notice = e.to_string(),
                    };
                    deleting = false
                }
                KeyCode::Esc => deleting = false,
                _ => {}
            }
            continue;
        }
        if key.code == KeyCode::Esc {
            if editor.take().is_none() && rename.take().is_none() {
                return Ok(super::session_tui_remote::Outcome::Back);
            }
            continue;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('s') {
            let save = (|| -> anyhow::Result<Hosts> {
                let mut updated = hosts.clone();
                if let Some(e) = &editor {
                    let mut c = e.connection()?;
                    if let Some(i) = e.original {
                        c.enabled = updated.connections[i].enabled;
                        updated.connections[i] = c
                    } else {
                        updated.connections.push(c)
                    }
                } else if let Some(name) = &rename {
                    updated.local_name = Some(name.value().trim().into())
                } else {
                    return Ok(updated);
                }
                updated.save()?;
                Ok(updated)
            })();
            match save {
                Ok(updated) => {
                    hosts = updated;
                    editor = None;
                    rename = None;
                    notice = "Saved".into()
                }
                Err(e) => notice = format!("Not saved: {e:#}"),
            };
            continue;
        }
        if let Some(e) = editor.as_mut() {
            match key.code {
                KeyCode::Tab | KeyCode::Down => e.selected = (e.selected + 1) % e.fields.len(),
                KeyCode::BackTab | KeyCode::Up => {
                    e.selected = (e.selected + e.fields.len() - 1) % e.fields.len()
                }
                _ => {
                    session_tui_input::edit(&mut e.fields[e.selected].1, key);
                }
            }
            continue;
        }
        if let Some(input) = rename.as_mut() {
            session_tui_input::edit(input, key);
            continue;
        }
        match key.code {
            KeyCode::Up => selected = selected.saturating_sub(1),
            KeyCode::Down => selected = (selected + 1).min(hosts.connections.len()),
            KeyCode::Enter => {
                if selected == 0 {
                    rename = Some(Input::from(
                        hosts
                            .local_name
                            .clone()
                            .unwrap_or_else(cutex::platform::host::current_host_name),
                    ))
                } else {
                    editor = Some(Editor::new(
                        Some(&hosts.connections[selected - 1]),
                        Some(selected - 1),
                    ))
                }
            }
            KeyCode::Char('b' | 'B') if selected>0 => {
                match super::session_tui_remote::run(terminal,events,hosts.connections[selected-1].clone())? {
                    super::session_tui_remote::Outcome::Back=>{}, outcome=>return Ok(outcome),
                }
            }
            KeyCode::Char('n' | 'N') => editor = Some(Editor::new(None, None)),
            KeyCode::Delete if selected > 0 => deleting = true,
            KeyCode::Char(' ') if selected > 0 => {
                let mut updated = hosts.clone();
                updated.connections[selected - 1].enabled =
                    !updated.connections[selected - 1].enabled;
                match updated.save() {
                    Ok(()) => {
                        hosts = updated;
                        notice = "Routing preference saved".into()
                    }
                    Err(e) => notice = e.to_string(),
                }
            }
            KeyCode::Char('t' | 'T') if selected > 0 && !testing => {
                let c = hosts.connections[selected - 1].clone();
                let tx = tx.clone();
                testing = true;
                std::thread::spawn(move || {
                    let result = match c.verified_endpoint() {
                        Ok(_) => format!("{}: connected; host identity matches", c.name),
                        Err(e) => format!("{}: {e:#}", c.name),
                    };
                    let _ = tx.send(result);
                });
            }
            _ => {}
        }
    }
}
