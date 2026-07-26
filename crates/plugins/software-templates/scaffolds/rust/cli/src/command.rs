//! What {{ values.name }} does. Each command is a variant, so adding one is adding a variant and a
//! match arm.

use anyhow::Result;
use clap::Subcommand;
use tracing::instrument;

use crate::platform::{Config, Flags};

#[derive(Subcommand)]
pub enum Command {
    /// Says hello, and shows what the platform is telling this service
    Hello {
        /// Who to greet
        #[arg(long, default_value = "world")]
        who: String,
    },
    /// Prints every flag and setting DOC holds for this service
    Flags,
}

impl Command {
    #[instrument(skip_all)]
    pub async fn run(&self, config: &Config, flags: &Flags) -> Result<()> {
        match self {
            Self::Hello { who } => {
                // A flag read here is DOC's, with the value this service falls back to beside it.
                let greeting = flags.string("greeting", "Hello");
                let greeting =
                    if flags.bool("shout", false) { greeting.to_uppercase() } else { greeting };
                println!(
                    "{greeting}, {who} — from {} in {}",
                    config.service, config.environment
                );
                Ok(())
            }
            Self::Flags => {
                println!("{} reads its flags from {}", config.service, config.flags_url);
                println!("  greeting = {:?}", flags.string("greeting", "Hello"));
                println!("  shout    = {}", flags.bool("shout", false));
                if config.flags_token.is_none() {
                    eprintln!("note: DOC_FLAGS_TOKEN is not set, so only public flags are read");
                }
                Ok(())
            }
        }
    }
}
