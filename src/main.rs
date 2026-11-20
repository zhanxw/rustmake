fn main() {
    let code = rustmake::cli::main_entry();
    if code != 0 {
        std::process::exit(code);
    }
}
