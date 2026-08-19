#![allow(dead_code, unused_imports)]

#[path = "../src/main.rs"]
mod gui;

fn main() {
    gui::tests::run_surfaces();
}
