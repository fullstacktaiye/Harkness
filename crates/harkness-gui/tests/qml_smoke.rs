#![allow(dead_code, unused_imports)]

#[path = "../src/main.rs"]
mod gui;

fn main() {
    gui::hotreload::tests::module_prefix_is_the_root_url_directory();
    gui::tests::main_qml_loads();
}
