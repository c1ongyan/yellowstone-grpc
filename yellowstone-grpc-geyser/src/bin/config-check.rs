//!
//! A simple utility to check the validity of the plugin's configuration file.
//!
use {clap::Parser, yellowstone_grpc_geyser::config::Config};

/// Defines the command-line arguments for the config-check utility.
#[derive(Debug, Parser)]
#[clap(author, version, about)]
struct Args {
    /// The path to the configuration file to check.
    #[clap(short, long, default_value_t = String::from("config.json"))]
    config: String,
}

/// The main entry point for the config-check utility.
/// It parses command-line arguments, attempts to load the specified config file,
/// and reports whether the configuration is valid.
fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let _config = Config::load_from_file(args.config)?;
    println!("Config is OK!");
    Ok(())
}
