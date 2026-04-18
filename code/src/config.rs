use clap::Parser;
use std::path::PathBuf;

#[derive(Parser, Debug, Clone)]
#[command(
    name = "mmm",
    about = "Organise images and videos: deduplicate, rename by date/location, sort into directories",
    version
)]
pub struct Config {
    /// One or more directories to scan for media files
    #[arg(required = true)]
    pub directories: Vec<PathBuf>,

    /// Output directory for organised files (default: first input directory)
    #[arg(short, long)]
    pub output: Option<PathBuf>,

    /// Dry run — show what would happen without making changes
    #[arg(short, long, default_value_t = false)]
    pub dry_run: bool,

    /// Number of files to process per chunk before prompting to continue
    #[arg(short, long, default_value_t = 100)]
    pub chunk_size: usize,

    /// Skip user confirmation prompts between chunks
    #[arg(long, default_value_t = false)]
    pub no_prompt: bool,

    /// Increase verbosity (can be repeated: -v, -vv, -vvv)
    #[arg(short, long, action = clap::ArgAction::Count)]
    pub verbose: u8,
}

impl Config {
    pub fn output_dir(&self) -> &PathBuf {
        self.output.as_ref().unwrap_or_else(|| &self.directories[0])
    }
}
