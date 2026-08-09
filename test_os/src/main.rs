fn main() { #[cfg(target_os="linux")] println!("Linux"); #[cfg(target_os="android")] println!("Android"); }
