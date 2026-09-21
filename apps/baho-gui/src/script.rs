use std::time::{Duration, Instant};

use akar_core::{InputState, Key, KeyEvent, Modifiers};
use akar_layout::Layout;

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Target {
    Coordinates(f32, f32),
    Label(String),
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Step {
    Hover(Target),
    Click(Target),
    Press(usize),
    Release(usize),
    Scroll(f32, f32),
    Key(KeyEvent),
    Type(String),
    Delay(f64),
    Screenshot(String),
}

fn quoted_text(line: &str, line_number: usize) -> Result<String, String> {
    let start = line
        .find('"')
        .ok_or_else(|| format!("line {line_number}: type requires a quoted string"))?;
    let rest = &line[start + 1..];
    let mut parsed = String::new();
    let mut characters = rest.chars();
    while let Some(character) = characters.next() {
        match character {
            '"' => {
                if characters.as_str().trim().is_empty() {
                    return Ok(parsed);
                }
                return Err(format!(
                    "line {line_number}: unexpected content after quoted string"
                ));
            }
            '\\' => {
                let escaped = characters.next().ok_or_else(|| {
                    format!("line {line_number}: quoted string ends with an escape")
                })?;
                parsed.push(match escaped {
                    'n' => '\n',
                    'r' => '\r',
                    't' => '\t',
                    '\\' => '\\',
                    '"' => '"',
                    other => {
                        return Err(format!(
                            "line {line_number}: unsupported escape '\\{other}'"
                        ));
                    }
                });
            }
            other => parsed.push(other),
        }
    }
    Err(format!("line {line_number}: type requires a closing quote"))
}

fn target<'a>(
    command: &str,
    line_number: usize,
    parts: &mut impl Iterator<Item = &'a str>,
) -> Result<Target, String> {
    let first = parts
        .next()
        .ok_or_else(|| format!("line {line_number}: {command} requires a target"))?;
    if let Some(label) = first.strip_prefix('@') {
        if label.is_empty() {
            return Err(format!("line {line_number}: label must not be empty"));
        }
        if parts.next().is_some() {
            return Err(format!(
                "line {line_number}: unexpected token after label target"
            ));
        }
        return Ok(Target::Label(label.to_owned()));
    }
    let x = first
        .parse::<f32>()
        .map_err(|_| format!("line {line_number}: invalid x"))?;
    let y = parts
        .next()
        .ok_or_else(|| format!("line {line_number}: {command} requires y"))?
        .parse::<f32>()
        .map_err(|_| format!("line {line_number}: invalid y"))?;
    Ok(Target::Coordinates(x, y))
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
        if line.starts_with("type") {
            steps.push(Step::Type(quoted_text(line, line_number + 1)?));
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
        match command {
            "hover" => steps.push(Step::Hover(target(command, line_number + 1, &mut parts)?)),
            "click" => steps.push(Step::Click(target(command, line_number + 1, &mut parts)?)),
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

fn apply_target(input: &mut InputState, layout: &Layout, target: &Target) {
    match target {
        Target::Coordinates(x, y) => input.set_mouse_pos(*x, *y),
        Target::Label(label) => {
            if let Some(node) = layout.resolve_label(label) {
                let [x, y, width, height] = layout.rect(node);
                input.set_mouse_pos(x + width / 2.0, y + height / 2.0);
            }
        }
    }
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
        layout: &Layout,
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
            Step::Hover(target) => apply_target(input, layout, &target),
            Step::Click(target) => {
                apply_target(input, layout, &target);
                input.push_mouse_button(0, true);
                input.push_mouse_button(0, false);
            }
            Step::Press(button) => input.push_mouse_button(button, true),
            Step::Release(button) => input.push_mouse_button(button, false),
            Step::Scroll(x, y) => input.push_scroll(x, y),
            Step::Key(event) => input.push_key_event(event),
            Step::Type(text) => {
                for character in text.chars() {
                    input.push_char(character);
                }
            }
            Step::Screenshot(path) => return Some(path),
            Step::Delay(_) => unreachable!(),
        }
        None
    }

    pub(crate) fn next_deadline(&self) -> Option<Instant> {
        self.deadline
    }

    pub(crate) fn is_exhausted(&self) -> bool {
        self.cursor >= self.steps.len()
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

    #[test]
    fn parses_prompt_focus_typing_and_labeled_submit() {
        assert_eq!(
            parse_script("click @prompt\ntype \"List Name\"\nclick @submit").unwrap(),
            vec![
                Step::Click(Target::Label("prompt".to_owned())),
                Step::Type("List Name".to_owned()),
                Step::Click(Target::Label("submit".to_owned())),
            ]
        );
    }

    #[test]
    fn injects_typing_and_resolves_label_to_its_center() {
        use akar_layout::{Dimension, Size, Style};

        let mut layout = Layout::new();
        let node = layout.new_leaf(Style {
            size: Size {
                width: Dimension::length(100.0),
                height: Dimension::length(40.0),
            },
            ..Default::default()
        });
        layout.register_label("prompt", node);
        layout.compute(node, (Some(100.0), Some(40.0)), |_, _, _, _, _| Size::ZERO);
        let mut input = InputState::default();
        let mut runner = ScriptRunner::new(vec![
            Step::Click(Target::Label("prompt".to_owned())),
            Step::Type("Name".to_owned()),
        ]);

        runner.advance(&mut input, &layout, Instant::now());
        assert_eq!([input.mouse_pos.x, input.mouse_pos.y], [50.0, 20.0]);
        runner.advance(&mut input, &layout, Instant::now());
        assert_eq!(input.chars, vec!['N', 'a', 'm', 'e']);
    }

    #[test]
    fn exposes_delay_deadline_without_advancing_early() {
        let now = Instant::now();
        let mut runner = ScriptRunner::new(vec![Step::Delay(0.1), Step::Screenshot("x".into())]);
        let mut input = InputState::default();
        let layout = Layout::new();

        assert_eq!(runner.advance(&mut input, &layout, now), None);
        let deadline = runner.next_deadline().unwrap();
        assert_eq!(deadline, now + Duration::from_millis(100));
        assert!(!runner.is_exhausted());
        assert_eq!(
            runner.advance(&mut input, &layout, deadline),
            Some("x".into())
        );
        assert!(runner.is_exhausted());
        assert_eq!(runner.next_deadline(), None);
    }
}
