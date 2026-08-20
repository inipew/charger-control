use std::fmt::Display;
use std::io::{self, IsTerminal, Write};

#[derive(Debug, Clone, Copy)]
enum Color {
    Reset,
    BoldCyan,
    Blue,
    Green,
    Yellow,
    Red,
}

impl Color {
    const fn code(self) -> &'static str {
        match self {
            Self::Reset => "\x1b[0m",
            Self::BoldCyan => "\x1b[1;36m",
            Self::Blue => "\x1b[34m",
            Self::Green => "\x1b[32m",
            Self::Yellow => "\x1b[33m",
            Self::Red => "\x1b[31m",
        }
    }
}

fn colors_enabled_stdout() -> bool {
    std::env::var_os("NO_COLOR").is_none() && io::stdout().is_terminal()
}

fn colors_enabled_stderr() -> bool {
    std::env::var_os("NO_COLOR").is_none() && io::stderr().is_terminal()
}

fn print_stdout(prefix_color: Color, prefix: &str, text: &str) {
    let mut out = io::stdout().lock();

    if colors_enabled_stdout() {
        let _ = writeln!(
            out,
            "{}{}{} {}",
            prefix_color.code(),
            prefix,
            Color::Reset.code(),
            text
        );
    } else {
        let _ = writeln!(out, "{} {}", prefix, text);
    }
}

fn print_stderr(prefix_color: Color, prefix: &str, text: &str) {
    let mut out = io::stderr().lock();

    if colors_enabled_stderr() {
        let _ = writeln!(
            out,
            "{}{}{} {}",
            prefix_color.code(),
            prefix,
            Color::Reset.code(),
            text
        );
    } else {
        let _ = writeln!(out, "{} {}", prefix, text);
    }
}

pub fn title(text: &str) {
    let mut out = io::stdout().lock();

    if colors_enabled_stdout() {
        let _ = writeln!(
            out,
            "{}==> {}{}",
            Color::BoldCyan.code(),
            text,
            Color::Reset.code()
        );
    } else {
        let _ = writeln!(out, "==> {}", text);
    }
}

pub fn info(text: &str) {
    print_stdout(Color::Blue, "[INFO]", text);
}

pub fn success(text: &str) {
    print_stdout(Color::Green, "[OK]", text);
}

pub fn warn(text: &str) {
    print_stdout(Color::Yellow, "[WARN]", text);
}

pub fn error(text: &str) {
    print_stderr(Color::Red, "[ERROR]", text);
}

pub fn key_val<T: Display>(key: &str, val: T) {
    let mut out = io::stdout().lock();
    let _ = writeln!(out, "{:<20}: {}", key, val);
}
