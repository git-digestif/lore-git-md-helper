use anyhow::Result;
use clap::Parser;
use lore_git_md_helper::ai_backend::BackendArgs;

#[derive(Parser)]
#[command(about = "Send a prompt to the AI backend (for testing)")]
struct Args {
    /// The prompt text (or @file to read from file)
    prompt: String,

    /// Optional system message (or @file to read from file)
    #[arg(long, short)]
    system: Option<String>,

    /// Temperature (0.0–2.0)
    #[arg(long, short)]
    temperature: Option<f32>,

    #[command(flatten)]
    backend: BackendArgs,
}

fn read_arg(s: &str) -> Result<String> {
    if let Some(path) = s.strip_prefix('@') {
        Ok(std::fs::read_to_string(path)?)
    } else {
        Ok(s.to_string())
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    let backend = args.backend.resolve()?;

    let system = match &args.system {
        Some(s) => read_arg(s)?,
        None => "You are a helpful assistant.".to_string(),
    };
    let prompt = read_arg(&args.prompt)?;

    let reply = if let Some(temp) = args.temperature {
        backend
            .chat_with_options(&system, &prompt, Some(temp))
            .await?
    } else {
        backend.chat(&system, &prompt).await?
    };
    println!("{reply}");
    Ok(())
}
