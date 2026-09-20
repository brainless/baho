use std::time::{Duration, Instant};

use akar_core::{InputState, Key, KeyEvent, Modifiers};
use akar_layout::Layout;

#[derive(Clone, Debug)]
pub(crate) enum Step {
    Hover(f32, f32),
    Click(f32, f32),
    Press(usize),
    Release(usize),
    Scroll(f32, f32),
    Key(KeyEvent),
    Delay(f64),
    Screenshot(String),
}

fn key(name: &str) -> Result<Key, String> {
    Ok(match name {
        "Backspace" => Key::Backspace,
        "Delete" => Key::Delete,
        "Left" => Key::Left,
        "Right" => Key::Right,
        "Up" => Key::Up,
        "Down" => Key::Down,
        "Home" => Key::Home,
        "End" => Key::End,
        "PageUp" => Key::PageUp,
        "PageDown" => Key::PageDown,
        "Enter" => Key::Enter,
        "Escape" => Key::Escape,
        "Tab" => Key::Tab,
        one if one.chars().count() == 1 => {
            Key::Character(one.chars().next().unwrap().to_ascii_lowercase())
        }
        other => return Err(format!("unknown key '{other}'")),
    })
}

pub(crate) fn parse_script(input: &str) -> Result<Vec<Step>, String> {
    let mut steps = Vec::new();
    for (line_number, raw) in input.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut parts = line.split_whitespace();
        let command = parts.next().unwrap();
        let number = |value: Option<&str>, name: &str| {
            value
                .ok_or_else(|| format!("line {}: {command} requires {name}", line_number + 1))
                .and_then(|v| {
                    v.parse::<f32>()
                        .map_err(|_| format!("line {}: invalid {name}", line_number + 1))
                })
        };
        let mut target =
            || number(parts.next(), "x").and_then(|x| number(parts.next(), "y").map(|y| (x, y)));
        match command {
            "hover" => {
                let (x, y) = target()?;
                steps.push(Step::Hover(x, y));
            }
            "click" => {
                let (x, y) = target()?;
                steps.push(Step::Click(x, y));
            }
            "press" | "release" => {
                let button = match parts.next().unwrap_or("left") {
                    "left" => 0,
                    "middle" => 1,
                    "right" => 2,
                    other => {
                        return Err(format!(
                            "line {}: unknown button '{other}'",
                            line_number + 1
                        ));
                    }
                };
                steps.push(if command == "press" {
                    Step::Press(button)
                } else {
                    Step::Release(button)
                });
            }
            "scroll" => steps.push(Step::Scroll(
                number(parts.next(), "dx")?,
                number(parts.next(), "dy")?,
            )),
            "key" => {
                let value = parts
                    .next()
                    .ok_or_else(|| format!("line {}: key requires a name", line_number + 1))?;
                steps.push(Step::Key(KeyEvent {
                    key: key(value)?,
                    modifiers: Modifiers::default(),
                    repeat: false,
                }));
            }
            "delay" => steps.push(Step::Delay(
                parts
                    .next()
                    .ok_or_else(|| format!("line {}: delay requires seconds", line_number + 1))?
                    .parse()
                    .map_err(|_| format!("line {}: invalid seconds", line_number + 1))?,
            )),
            "screenshot" => steps.push(Step::Screenshot(
                parts
                    .next()
                    .ok_or_else(|| format!("line {}: screenshot requires a path", line_number + 1))?
                    .to_string(),
            )),
            other => {
                return Err(format!(
                    "line {}: unknown command '{other}'",
                    line_number + 1
                ));
            }
        }
    }
    Ok(steps)
}

pub struct ScriptRunner {
    steps: Vec<Step>,
    cursor: usize,
    deadline: Option<Instant>,
}

impl ScriptRunner {
    pub(crate) fn new(steps: Vec<Step>) -> Self {
        Self {
            steps,
            cursor: 0,
            deadline: None,
        }
    }

    pub fn advance(
        &mut self,
        input: &mut InputState,
        _layout: &Layout,
        now: Instant,
    ) -> Option<String> {
        while let Some(Step::Delay(seconds)) = self.steps.get(self.cursor) {
            let deadline = *self
                .deadline
                .get_or_insert_with(|| now + Duration::from_secs_f64(*seconds));
            if deadline > now {
                return None;
            }
            self.deadline = None;
            self.cursor += 1;
        }
        let step = self.steps.get(self.cursor)?.clone();
        self.cursor += 1;
        match step {
            Step::Hover(x, y) => input.set_mouse_pos(x, y),
            Step::Click(x, y) => {
                input.set_mouse_pos(x, y);
                input.push_mouse_button(0, true);
                input.push_mouse_button(0, false);
            }
            Step::Press(button) => input.push_mouse_button(button, true),
            Step::Release(button) => input.push_mouse_button(button, false),
            Step::Scroll(x, y) => input.push_scroll(x, y),
            Step::Key(event) => input.push_key_event(event),
            Step::Screenshot(path) => return Some(path),
            Step::Delay(_) => unreachable!(),
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_grid_debug_script() {
        let steps = parse_script(
            "click 10 20\nscroll 0 -40\nkey Down\ndelay 0.1\nscreenshot /tmp/grid.png",
        )
        .unwrap();
        assert_eq!(steps.len(), 5);
    }
}
