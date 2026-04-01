use clap::Parser;

fn main() {
    let cli = mars_agents::cli::Cli::parse();
    let result = mars_agents::cli::dispatch(cli);
    match result {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(3);
        }
    }
}
