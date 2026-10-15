//! Remote catalog with explicit lifecycle actions; no local store import.
use super::{
    remote_sessions, session_tui::ShellEvents, session_tui_input, session_tui_layout as theme,
    session_tui_terminal::Terminal,
};
use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
use cutex::management::{connections::Connection, v2::host_sessions::HostSession};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Layout},
    style::Style,
    text::Line,
    widgets::{Block, Cell, Paragraph, Row, Table, TableState, Wrap},
};
use serde_json::Value;
use std::{io::Stdout, sync::mpsc};
use tui_input::Input;
pub(super) enum Outcome {
    Back,
    Exit,
}
pub(super) fn run(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    events: &mut ShellEvents,
    c: Connection,
) -> anyhow::Result<Outcome> {
    run_selected(terminal,events,c,None)
}
pub(super) fn run_selected(terminal:&mut Terminal<CrosstermBackend<Stdout>>,events:&mut ShellEvents,c:Connection,id:Option<String>)->anyhow::Result<Outcome>{
    let (tx, rx) = mpsc::channel::<(bool, Result<Value, String>)>();
    let mut rows: Vec<HostSession> = vec![];
    let mut selected = 0usize;
    let mut query = Input::new(id.clone().unwrap_or_default());
    let mut applied_query=id.unwrap_or_default();
    let mut filtering = false;
    let mut cursors = vec![None::<String>];
    let mut next = None;
    let mut due = true;
    let mut loading = false;
    let mut notice = remote_sessions::ssh_hint(&c);
    let mut close_confirm = false;
    let mut detail=false;let mut scroll=0u16;
    let mut refresh_at=std::time::Instant::now();
    loop {
        if let Ok((page, result)) = rx.try_recv() {
            loading = false;
            match result {
                Ok(value) if page => {
                    let prior = rows.get(selected).map(|r| r.id.clone());
                    rows = serde_json::from_value(value["data"].clone())?;
                    next = value["nextCursor"].as_str().map(str::to_owned);
                    selected = prior
                        .and_then(|id| rows.iter().position(|r| r.id == id))
                        .unwrap_or(0);
                }
                Ok(value) => {
                    notice = format!(
                        "Remote runtime: {}",
                        value
                            .pointer("/cutex/result/status")
                            .and_then(Value::as_str)
                            .unwrap_or("response received")
                    );
                    due = true;
                }
                Err(e) => notice = e,
            }
        }
        if !loading && !filtering && !close_confirm && std::time::Instant::now()>=refresh_at {due=true;}
        if due && !loading {
            refresh_at=std::time::Instant::now()+std::time::Duration::from_secs(5);
            due = false;
            loading = true;
            let c = c.clone();
            let tx = tx.clone();
            let q = applied_query.clone();
            let cursor = cursors.last().cloned().flatten();
            std::thread::spawn(move || {
                let _ = tx.send((
                    true,
                    remote_sessions::list(&c, &q, cursor.as_deref()).map_err(|e| format!("{e:#}")),
                ));
            });
        }
        terminal.draw(|f|{
   let areas=Layout::vertical([Constraint::Length(1),Constraint::Min(5),Constraint::Length(3),Constraint::Length(2)]).split(f.area());
   f.render_widget(Paragraph::new(format!("{} · Remote Agents / Sessions",c.name)).style(Style::default().fg(theme::focus())),areas[0]);
   let(left,right)=theme::inspector_panes(areas[1],true).map(|(l,r)|(l,Some(r))).unwrap_or((areas[1],None));
   let l=Layout::vertical([Constraint::Length(3),Constraint::Min(2)]).split(left);
   f.render_widget(Paragraph::new(query.value()).block(Block::bordered().title(" Filter [/] ").border_style(Style::default().fg(if filtering{theme::focus()}else{theme::muted()}))),l[0]);
   let table=Table::new(rows.iter().map(|r|Row::new(vec![Cell::from(r.name.clone()),Cell::from(r.state.clone()).style(Style::default().fg(match r.state.as_str(){"Online"=>theme::accent(),"Unobserved"=>theme::focus(),_=>theme::text()})),Cell::from(r.kind.clone())])),[Constraint::Min(18),Constraint::Length(12),Constraint::Length(8)])
    .header(Row::new(["NAME","STATUS","KIND"]).style(Style::default().fg(theme::muted()))).block(Block::bordered().title(" Sessions ")).row_highlight_style(Style::default().bg(theme::selection())).highlight_symbol("> ");
   f.render_stateful_widget(table,l[1],&mut TableState::default().with_selected((!rows.is_empty()).then_some(selected)));
   if let Some(right)=right.or_else(||detail.then_some(areas[1])){let detail=rows.get(selected).map(|r|vec![Line::from(r.name.clone()),Line::from(format!("Host: {} ({})",c.name,r.host_id)),Line::from(format!("ID: {}",r.id)),Line::from(format!("Native: {}",r.native_id.as_deref().unwrap_or("N/A"))),Line::from(format!("Profile: {}",r.profile.as_deref().unwrap_or("N/A"))),Line::from(format!("Generation: {}",r.generation)),Line::from(format!("Directory: {}",r.cwd)),Line::from(""),Line::from(remote_sessions::ssh_hint(&c)),Line::from("X closes only the remote runtime; history is retained.")]).unwrap_or_default();f.render_widget(Paragraph::new(detail).wrap(Wrap{trim:false}).scroll((scroll,0)).block(Block::bordered().title(" Details ")),right);}
   let status=if close_confirm{"Close selected remote runtime? Y confirms · Esc cancels".into()}else if loading{"Request running…".into()}else if !notice.is_empty(){notice.clone()}else{format!("Page {} · {} records · {}",cursors.len(),rows.len(),c.host_id)};
   f.render_widget(Paragraph::new(status).wrap(Wrap{trim:false}),areas[2]);
   let hints=if filtering{vec![("Enter","apply"),("Esc","cancel filter")]}else{vec![("↑/↓","select"),("Enter","SSH help"),("I","details"),("X","close"),("/","filter"),("N/P","pages"),("R","refresh"),("Esc","back")]};
   f.render_widget(Paragraph::new(Line::from(super::session_tui::footer_hints(&hints))).wrap(Wrap{trim:true}),areas[3]);
  })?;
        let Some(event) = events.next()? else {
            continue;
        };
        if let Event::Paste(text)=&event {
            if filtering {
                let mut remaining=256usize.saturating_sub(query.value().len());
                let paste:String=text.chars().filter(|c|!c.is_control()).take_while(|c|{if c.len_utf8()>remaining{return false;}remaining-=c.len_utf8();true}).collect();
                session_tui_input::paste(&mut query,&paste);
            }
            continue;
        }
        let Event::Key(key) = event else { continue };
        if key.kind == KeyEventKind::Release {
            continue;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return Ok(Outcome::Exit);
        }
        if filtering {
            match key.code {
                KeyCode::Esc => {filtering=false;query=Input::from(applied_query.clone());},
                KeyCode::Enter if !loading => {
                    filtering = false;
                    applied_query=query.value().to_owned();
                    cursors = vec![None];
                    next = None;
                    due = true;
                    notice.clear()
                }
                _ => {
                    let old = query.value().to_owned();
                    session_tui_input::edit(&mut query, key);
                    if query.value().len() > 256 {
                        query = Input::from(old)
                    }
                }
            }
            continue;
        }
        if close_confirm {
            match key.code {
                KeyCode::Esc => close_confirm = false,
                KeyCode::Char('y' | 'Y') => {
                    close_confirm = false;
                    let id = rows[selected].id.clone();
                    let c = c.clone();
                    let tx = tx.clone();
                    loading = true;
                    std::thread::spawn(move || {
                        let _ = tx.send((
                            false,
                            remote_sessions::lifecycle(&c, &id, true).map_err(|e| format!("{e:#}")),
                        ));
                    });
                }
                _ => {}
            }
            continue;
        }
        match key.code {
            KeyCode::Esc if detail=>{detail=false;scroll=0},
            KeyCode::Esc => return Ok(Outcome::Back),
            KeyCode::Char('i'|'I')=>{detail=!detail;scroll=0},
            KeyCode::Up if detail=>scroll=scroll.saturating_sub(1),
            KeyCode::Down if detail=>scroll=scroll.saturating_add(1).min(2048),
            KeyCode::Up => selected = selected.saturating_sub(1),
            KeyCode::Down => selected = (selected + 1).min(rows.len().saturating_sub(1)),
            KeyCode::Enter | KeyCode::Char('o' | 'O') if !rows.is_empty() => {
                notice = remote_sessions::ssh_hint(&c);
            }
            KeyCode::Char('x' | 'X') if !loading && !rows.is_empty() => close_confirm = true,
            KeyCode::Char('r' | 'R') => {
                due = true;
                notice.clear()
            }
            KeyCode::Char('/') if !loading => filtering = true,
            KeyCode::Char('n' | 'N') if !loading && next.is_some() => {
                cursors.push(next.clone());
                due = true;
                notice.clear()
            }
            KeyCode::Char('p' | 'P') if !loading && cursors.len() > 1 => {
                cursors.pop();
                due = true;
                notice.clear()
            }
            _ => {}
        }
    }
}
