//! Terminal output helpers. Colour is stripped automatically when not writing to a terminal
//! (and honours `NO_COLOR`), courtesy of `anstream`.

use anstyle::{AnsiColor, Style};

fn paint(style: Style, s: &str) -> String {
    format!("{style}{s}{style:#}")
}

pub fn green(s: &str) -> String {
    paint(Style::new().fg_color(Some(AnsiColor::Green.into())), s)
}
pub fn red(s: &str) -> String {
    paint(Style::new().fg_color(Some(AnsiColor::Red.into())), s)
}
pub fn yellow(s: &str) -> String {
    paint(Style::new().fg_color(Some(AnsiColor::Yellow.into())), s)
}
pub fn cyan(s: &str) -> String {
    paint(Style::new().fg_color(Some(AnsiColor::Cyan.into())), s)
}
pub fn bold(s: &str) -> String {
    paint(Style::new().bold(), s)
}
pub fn dim(s: &str) -> String {
    paint(Style::new().dimmed(), s)
}

/// Print a value as pretty JSON.
pub fn json(value: &serde_json::Value) {
    anstream::println!("{}", serde_json::to_string_pretty(value).unwrap_or_default());
}

/// A warning on stderr.
pub fn warn(msg: &str) {
    anstream::eprintln!("{} {msg}", yellow("warning:"));
}

/// A hint on stderr.
pub fn hint(msg: &str) {
    anstream::eprintln!("{} {msg}", dim("hint:"));
}

/// Colourise a rendered unified diff.
pub fn colour_diff(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 64);
    for line in text.split_inclusive('\n') {
        let (body, nl) = match line.strip_suffix('\n') {
            Some(b) => (b, "\n"),
            None => (line, ""),
        };
        let styled = if body.starts_with("diff --git")
            || body.starts_with("index ")
            || body.starts_with("--- ")
            || body.starts_with("+++ ")
            || body.starts_with("new file mode")
            || body.starts_with("deleted file mode")
            || body.starts_with("old mode")
            || body.starts_with("new mode")
        {
            bold(body)
        } else if body.starts_with("@@") {
            cyan(body)
        } else if body.starts_with('+') {
            green(body)
        } else if body.starts_with('-') {
            red(body)
        } else {
            body.to_owned()
        };
        out.push_str(&styled);
        out.push_str(nl);
    }
    out
}
