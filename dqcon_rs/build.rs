use std::process::Command;

fn main() {
    let output = Command::new("date")
        .arg("+%Y-%m-%d")
        .output()
        .expect("Failed to execute date command");
    let date = String::from_utf8(output.stdout).unwrap();
    println!("cargo:rustc-env=BUILD_DATE={}", date.trim());
}
