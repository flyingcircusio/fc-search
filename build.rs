use std::process::Command;

fn main() {
    std::fs::create_dir_all("assets").expect("could not create assets dir");
    Command::new("tailwindcss")
        .args(["-o", "assets/tailwind.css"])
        .args(["-i", "main.css"])
        .arg("-m")
        .spawn()
        .unwrap()
        .wait()
        .unwrap();
}
