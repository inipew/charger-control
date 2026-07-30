use std::fmt::Display;

pub fn title(text: &str) {
    println!("\x1b[1;36m==> {}\x1b[0m", text);
}

pub fn info(text: &str) {
    println!("\x1b[34m[INFO]\x1b[0m {}", text);
}

pub fn success(text: &str) {
    println!("\x1b[32m[OK]\x1b[0m {}", text);
}

pub fn warn(text: &str) {
    println!("\x1b[33m[WARN]\x1b[0m {}", text);
}

pub fn error(text: &str) {
    eprintln!("\x1b[31m[ERROR]\x1b[0m {}", text);
}

pub fn key_val<T: Display>(key: &str, val: T) {
    println!("{:<20}: {}", key, val);
}
