use anyhow::{Context, Result};
use clap::Parser;
use lore_git_md_helper::email_to_markdown;
use mail_parser::MessageParser;
use std::fs;
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(name = "mbox2md")]
#[command(about = "Convert email files (EML/MBOX) to Markdown", long_about = None)]
struct Args {
    /// Input file (.eml or .mbox)
    #[arg(value_name = "FILE")]
    input: PathBuf,

    /// Output file (default: input filename with .md extension)
    #[arg(short, long, value_name = "FILE")]
    output: Option<PathBuf>,
}

fn main() -> Result<()> {
    let args = Args::parse();

    let raw_message = fs::read(&args.input)
        .with_context(|| format!("Failed to read input file: {0:?}", args.input))?;

    let message = MessageParser::default()
        .parse(&raw_message)
        .context("Failed to parse email")?;

    let markdown = email_to_markdown(&message)?;

    let output_path = args.output.unwrap_or_else(|| {
        let mut path = args.input.clone();
        path.set_extension("md");
        path
    });

    fs::write(&output_path, markdown)
        .with_context(|| format!("Failed to write output file: {output_path:?}"))?;

    println!("Converted: {:?} -> {:?}", args.input, output_path);

    Ok(())
}
