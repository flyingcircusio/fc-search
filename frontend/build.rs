use std::process::Command;

fn main() {
    Command::new("tailwindcss")
        .args(["-o", "assets/tailwind.css", "-m"])
        .spawn()
        .unwrap();
}
