use clap::Parser;

fn main() {
    if let Err(error) = rdedup_server::command::Arguments::parse().run() {
        eprintln!("Error: {error}");
        std::process::exit(1);
    }
}
