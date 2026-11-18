// Не открывать консольное окно рядом с программой в релизной сборке.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    talkie_lib::run()
}
