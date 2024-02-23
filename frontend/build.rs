use std::process::Command;

fn main() {
    Command::new("tailwindcss")
        .args(["-o", "assets/tailwind.css"])
        .args(["-i", "main.css"])
        .arg("-m")
        .spawn()
        .unwrap();
}
