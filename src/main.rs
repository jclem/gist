use clap::{Parser, Subcommand};

mod api;
mod commands;
mod error;

#[derive(Parser)]
#[command(name = "gist", version, about = "GitHub Gist CLI")]
struct Cli {
    /// Show full error details including source chains
    #[arg(short, long, global = true)]
    verbose: bool,

    /// Make the gist public
    #[arg(short, long, global = true)]
    public: bool,

    /// Filename for the gist
    #[arg(short, long, global = true)]
    name: Option<String>,

    /// Open EDITOR to write gist content
    #[arg(short, long, global = true)]
    editor: bool,

    #[command(subcommand)]
    command: Option<Command>,

    /// Gist URL or ID to show
    #[arg(value_name = "URL")]
    url: Option<String>,
}

#[derive(Subcommand)]
enum Command {
    /// List your gists
    #[command(visible_alias = "ls")]
    List,
    /// Delete a gist
    #[command(visible_alias = "rm")]
    Delete {
        /// Gist URL or ID
        url: String,
    },
    /// Interactive TUI for browsing gists
    Tui,
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let cli = Cli::parse();
    let client = api::new_client();

    let result = match cli.command {
        Some(Command::List) => commands::list::execute(&client).await,
        Some(Command::Delete { ref url }) => commands::delete::execute(&client, url).await,
        Some(Command::Tui) => commands::tui::execute(&client).await,
        None => {
            if let Some(ref url) = cli.url {
                commands::show::execute(&client, url).await
            } else {
                commands::create::execute(&client, cli.name, cli.public, cli.editor).await
            }
        }
    };

    if let Err(e) = result {
        error::print_error(&e, cli.verbose);
        std::process::exit(error::exit_code(&e));
    }
}
