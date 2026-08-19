use std::error::Error;
use std::io;

use libppm::{App as Package, PackageManager};
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::{prelude::*, widgets::*, DefaultTerminal};

const RESULT_LIMIT: usize = 5;

struct Model {
    manager: PackageManager,
    query: String,
    hits: Vec<Package>,
    selected: usize,
    error: Option<String>,
    confirmation: Option<Package>,
    chosen: Option<Package>,
}

impl Model {
    fn new(query: String) -> Result<Self, Box<dyn Error>> {
        let mut model = Self {
            manager: PackageManager::init()?,
            query,
            hits: Vec::new(),
            selected: 0,
            error: None,
            confirmation: None,
            chosen: None,
        };
        model.search();
        Ok(model)
    }

    fn search(&mut self) {
        match self.manager.search(&self.query, RESULT_LIMIT) {
            Ok(hits) => {
                self.hits = hits;
                self.selected = 0;
                self.error = None;
            }
            Err(error) => {
                self.hits.clear();
                self.error = Some(format!("{error:#}"));
            }
        }
    }

    fn run(&mut self, terminal: &mut DefaultTerminal) -> io::Result<()> {
        loop {
            terminal.draw(|frame| self.render(frame))?;

            if let Event::Key(key) = event::read()? {
                if key.kind != KeyEventKind::Release && self.handle_key(key) {
                    return Ok(());
                }
            }
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> bool {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return true;
        }

        if self.confirmation.is_some() {
            return match key.code {
                KeyCode::Enter | KeyCode::Char('y' | 'Y') => {
                    self.chosen = self.confirmation.take();
                    true
                }
                KeyCode::Esc | KeyCode::Char('n' | 'N') => {
                    self.confirmation = None;
                    false
                }
                _ => false,
            };
        }

        match key.code {
            KeyCode::Esc => return true,
            KeyCode::Up => self.selected = self.selected.saturating_sub(1),
            KeyCode::Down => {
                self.selected = (self.selected + 1).min(self.hits.len().saturating_sub(1));
            }
            KeyCode::Enter => self.confirmation = self.hits.get(self.selected).cloned(),
            KeyCode::Backspace => {
                if self.query.pop().is_some() {
                    self.search();
                }
            }
            KeyCode::Char(character)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                self.query.push(character);
                // currently doing synchronously so it blocks keyboard input
                // generally sub-millisecond tho so prob not worth async unless it becomes a thing
                self.search();
            }
            _ => {}
        }

        false
    }

    fn render(&self, frame: &mut Frame) {
        let [search_area, results_area, status_area] = frame.area().layout(&Layout::vertical([
            Constraint::Length(3),
            Constraint::Fill(1),
            Constraint::Length(1),
        ]));

        frame.render_widget(
            Paragraph::new(format!("{}▌", self.query)).block(Block::bordered().title("Search")),
            search_area,
        );

        let items = self.hits.iter().enumerate().map(|(index, package)| {
            let selected = index == self.selected;
            ListItem::new(vec![
                Line::from(vec![
                    Span::styled(if selected { "› " } else { "  " }, Color::Cyan),
                    Span::styled(
                        package.name.clone(),
                        if selected {
                            Style::new().cyan().bold()
                        } else {
                            Style::new().bold()
                        },
                    ),
                ]),
                Line::styled(format!("  {}", package.description), Color::Gray),
            ])
        });
        frame.render_widget(
            List::new(items).block(Block::bordered().title("Packages")),
            results_area,
        );

        let status = if let Some(error) = &self.error {
            Line::styled(error.clone(), Color::Red)
        } else if self.hits.is_empty() {
            Line::styled("No packages found", Color::DarkGray)
        } else {
            Line::styled("↑/↓ select  •  Enter choose  •  Esc quit", Color::DarkGray)
        };
        frame.render_widget(status, status_area);

        if let Some(package) = &self.confirmation {
            let area = frame
                .area()
                .centered(Constraint::Percentage(70), Constraint::Length(5));
            let dialog = Paragraph::new(vec![
                Line::from(format!("Choose {}?", package.name)),
                Line::from(""),
                Line::from("Enter/y confirm  •  Esc/n cancel"),
            ])
            .alignment(Alignment::Center)
            .block(Block::bordered().title("Confirm"));
            frame.render_widget(Clear, area);
            frame.render_widget(dialog, area);
        }
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    let query = std::env::args().skip(1).collect::<Vec<_>>().join(" ");
    let mut model = Model::new(query)?;
    ratatui::run(|terminal| model.run(terminal))?;

    if let Some(package) = model.chosen {
        println!(
            "{}:{}",
            package.package_source.manager, package.package_source.package
        );
    }

    Ok(())
}