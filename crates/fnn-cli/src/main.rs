mod colorize;
mod commands;
mod rpc_client;

use std::collections::HashMap;

use anyhow::Result;
use clap::{Arg, Command};
use colored::Colorize;
use rpc_client::RpcClient;
use rustyline::completion::Completer;
use rustyline::error::ReadlineError;
use rustyline::highlight::Highlighter;
use rustyline::hint::Hinter;
use rustyline::validate::Validator;
use rustyline::{Context, Editor, Helper};

const FNN_CLI_VERSION: &str = env!("CARGO_PKG_VERSION");
const DEFAULT_RPC_URL: &str = "http://127.0.0.1:8228";

const BANNER: &str = r#"
  ______ ___ ___  ______ _____
 |  ____|_ _| _ \|  ____|  __ \
 | |__   | ||___/| |__  | |__) |
 |  __|  | || _ \|  __| |  _  /
 | |    _| || |_\| |____| | \ \
 |_|   |___|____/|______|_|  \_\
"#;

fn build_cli() -> Command {
    Command::new("fnn-cli")
        .about("Fiber Network Node CLI - interactive command-line interface for FNN")
        .version(FNN_CLI_VERSION)
        .arg(
            Arg::new("url")
                .long("url")
                .short('u')
                .global(true)
                .default_value(DEFAULT_RPC_URL)
                .help("The RPC endpoint URL"),
        )
        .arg(
            Arg::new("raw_data")
                .long("raw-data")
                .global(true)
                .num_args(0)
                .help("Output raw JSON-RPC data"),
        )
        .arg(
            Arg::new("output_format")
                .long("output-format")
                .short('o')
                .global(true)
                .default_value("yaml")
                .help("Output format: json or yaml"),
        )
        .arg(
            Arg::new("no_banner")
                .long("no-banner")
                .global(true)
                .num_args(0)
                .help("Suppress the banner in interactive mode"),
        )
        .arg(
            Arg::new("color")
                .long("color")
                .global(true)
                .default_value("auto")
                .help("Color output: auto, always, or never"),
        )
        .subcommand(commands::info::command())
        .subcommand(commands::peer::command())
        .subcommand(commands::channel::command())
        .subcommand(commands::invoice::command())
        .subcommand(commands::payment::command())
        .subcommand(commands::graph::command())
        .subcommand(commands::cch::command())
        .subcommand(commands::dev::command())
        .subcommand(commands::watchtower::command())
        .subcommand(commands::prof::command())
}

fn build_interactive_cli() -> Command {
    Command::new("interactive")
        .multicall(true)
        .subcommand(commands::info::command())
        .subcommand(commands::peer::command())
        .subcommand(commands::channel::command())
        .subcommand(commands::invoice::command())
        .subcommand(commands::payment::command())
        .subcommand(commands::graph::command())
        .subcommand(commands::cch::command())
        .subcommand(commands::dev::command())
        .subcommand(commands::watchtower::command())
        .subcommand(commands::prof::command())
        .subcommand(Command::new("exit").about("Exit the interactive shell"))
        .subcommand(Command::new("quit").about("Exit the interactive shell"))
}

fn print_banner(url: &str, output_format: &str) {
    println!("{}", BANNER.bright_cyan());
    println!("[  fnn-cli version ]: {}", FNN_CLI_VERSION.bright_yellow());
    println!("[              url ]: {}", url.bright_green());
    println!("[    output format ]: {}", output_format.bright_white());
    println!();
}

/// Build a map of command name -> list of subcommand names from a clap Command.
fn build_completion_tree(cmd: &Command) -> HashMap<String, Vec<String>> {
    let mut tree = HashMap::new();

    // Top-level commands
    let top_level: Vec<String> = cmd
        .get_subcommands()
        .map(|sub| sub.get_name().to_string())
        .collect();
    tree.insert(String::new(), top_level);

    // Second-level subcommands for each top-level command
    for sub in cmd.get_subcommands() {
        let children: Vec<String> = sub
            .get_subcommands()
            .map(|s| s.get_name().to_string())
            .collect();
        // Also collect long option names
        let mut options: Vec<String> = sub
            .get_arguments()
            .filter_map(|a| a.get_long().map(|l| format!("--{}", l)))
            .collect();
        // Include options from subcommands too (will be handled per-subcommand)
        options.extend(children.iter().cloned());
        tree.insert(sub.get_name().to_string(), options);

        // Third-level: options for each subcommand
        for child in sub.get_subcommands() {
            let child_options: Vec<String> = child
                .get_arguments()
                .filter_map(|a| a.get_long().map(|l| format!("--{}", l)))
                .collect();
            let key = format!("{} {}", sub.get_name(), child.get_name());
            tree.insert(key, child_options);
        }
    }

    tree
}

/// Tab-completion helper for the FNN REPL.
struct FnnHelper {
    completion_tree: HashMap<String, Vec<String>>,
}

impl FnnHelper {
    fn new(cmd: &Command) -> Self {
        Self {
            completion_tree: build_completion_tree(cmd),
        }
    }
}

impl Completer for FnnHelper {
    type Candidate = String;

    fn complete(
        &self,
        line: &str,
        pos: usize,
        _ctx: &Context<'_>,
    ) -> rustyline::Result<(usize, Vec<String>)> {
        let line_to_cursor = &line[..pos];
        let words: Vec<&str> = line_to_cursor.split_whitespace().collect();

        // Determine what we're completing based on how many words are typed
        let (candidates_key, prefix, start_pos) = match words.len() {
            0 => {
                // Empty line - show top-level commands
                (String::new(), "", 0)
            }
            1 => {
                if line_to_cursor.ends_with(' ') {
                    // First word completed, space typed - show subcommands of first word
                    (words[0].to_string(), "", pos)
                } else {
                    // Typing first word - complete top-level commands
                    (String::new(), words[0], pos - words[0].len())
                }
            }
            2 => {
                if line_to_cursor.ends_with(' ') {
                    // Two words completed, space typed - show options
                    let key = format!("{} {}", words[0], words[1]);
                    // Try the two-word key first, fall back to one-word key
                    if self.completion_tree.contains_key(&key) {
                        (key, "", pos)
                    } else {
                        (words[0].to_string(), "", pos)
                    }
                } else {
                    // Typing second word - complete from first word's candidates
                    (words[0].to_string(), words[1], pos - words[1].len())
                }
            }
            _ => {
                let last = *words.last().unwrap();
                if line_to_cursor.ends_with(' ') {
                    // Completed a word, show options
                    let key = format!("{} {}", words[0], words[1]);
                    if self.completion_tree.contains_key(&key) {
                        (key, "", pos)
                    } else {
                        (words[0].to_string(), "", pos)
                    }
                } else {
                    // Typing an option flag - complete from the subcommand's options
                    let key = format!("{} {}", words[0], words[1]);
                    if self.completion_tree.contains_key(&key) {
                        (key, last, pos - last.len())
                    } else {
                        (words[0].to_string(), last, pos - last.len())
                    }
                }
            }
        };

        let candidates = match self.completion_tree.get(&candidates_key) {
            Some(candidates) => candidates
                .iter()
                .filter(|c| c.starts_with(prefix))
                .cloned()
                .collect(),
            None => Vec::new(),
        };

        Ok((start_pos, candidates))
    }
}

impl Hinter for FnnHelper {
    type Hint = String;
}

impl Highlighter for FnnHelper {}
impl Validator for FnnHelper {}
impl Helper for FnnHelper {}

async fn run_interactive(
    client: &RpcClient,
    output_format: &str,
    no_banner: bool,
    use_color: bool,
) -> Result<()> {
    if !no_banner {
        print_banner(client.url(), output_format);
    }

    // Health check: try to reach the RPC endpoint
    match client.call_no_params("node_info").await {
        Ok(_) => {
            println!("[           status ]: {}", "Connected".bright_green());
        }
        Err(e) => {
            eprintln!(
                "[           status ]: {} ({})",
                "Not connected".bright_red(),
                e
            );
            eprintln!(
                "{}",
                "Warning: Could not reach the RPC endpoint. Commands may fail.".bright_yellow()
            );
        }
    }
    println!();

    let interactive_cli = build_interactive_cli();
    let helper = FnnHelper::new(&interactive_cli);

    let config = rustyline::Config::builder()
        .completion_type(rustyline::CompletionType::List)
        .build();
    let mut rl = Editor::with_config(config)?;
    rl.set_helper(Some(helper));
    let history_path = dirs_home().join(".fnn_cli_history");
    let _ = rl.load_history(&history_path);

    loop {
        let readline = rl.readline(&format!("{} ", "FNN>".bright_cyan()));
        match readline {
            Ok(line) => {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }

                let _ = rl.add_history_entry(line);

                if line == "exit" || line == "quit" {
                    break;
                }

                // Parse the line as shell-like arguments
                let args = match shell_words(line) {
                    Ok(args) => args,
                    Err(e) => {
                        eprintln!("{}: {}", "error".bright_red(), e);
                        continue;
                    }
                };

                let cli = build_interactive_cli();
                match cli.try_get_matches_from(args) {
                    Ok(matches) => {
                        if let Err(e) =
                            commands::execute_command(client, &matches, output_format, use_color)
                                .await
                        {
                            eprintln!("{}: {}", "error".bright_red(), e);
                        }
                    }
                    Err(e) => {
                        // Colorize help output from clap
                        let rendered = e.render().to_string();
                        if use_color {
                            eprintln!("{}", colorize::colorize_help(&rendered));
                        } else {
                            eprintln!("{}", rendered);
                        }
                    }
                }
            }
            Err(ReadlineError::Interrupted) => {
                println!("Use 'exit' or 'quit' to leave the shell.");
            }
            Err(ReadlineError::Eof) => {
                break;
            }
            Err(err) => {
                eprintln!("{}: {:?}", "readline error".bright_red(), err);
                break;
            }
        }
    }

    let _ = rl.save_history(&history_path);
    println!("Bye!");
    Ok(())
}

/// Simple shell word splitting (handles basic quoting).
fn shell_words(input: &str) -> Result<Vec<String>> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut in_single_quote = false;
    let mut in_double_quote = false;
    let mut escape_next = false;

    for ch in input.chars() {
        if escape_next {
            current.push(ch);
            escape_next = false;
            continue;
        }

        match ch {
            '\\' if !in_single_quote => {
                escape_next = true;
            }
            '\'' if !in_double_quote => {
                in_single_quote = !in_single_quote;
            }
            '"' if !in_single_quote => {
                in_double_quote = !in_double_quote;
            }
            ' ' | '\t' if !in_single_quote && !in_double_quote => {
                if !current.is_empty() {
                    words.push(current.clone());
                    current.clear();
                }
            }
            _ => {
                current.push(ch);
            }
        }
    }

    if in_single_quote || in_double_quote {
        return Err(anyhow::anyhow!("Unclosed quote in input"));
    }

    if !current.is_empty() {
        words.push(current);
    }

    Ok(words)
}

fn dirs_home() -> std::path::PathBuf {
    dirs_home_inner().unwrap_or_else(|| std::path::PathBuf::from("."))
}

fn dirs_home_inner() -> Option<std::path::PathBuf> {
    #[cfg(target_os = "windows")]
    {
        std::env::var_os("USERPROFILE").map(std::path::PathBuf::from)
    }
    #[cfg(not(target_os = "windows"))]
    {
        std::env::var_os("HOME").map(std::path::PathBuf::from)
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = build_cli();
    let matches = cli.get_matches();

    let url = matches.get_one::<String>("url").unwrap().clone();
    let raw_data = matches.get_flag("raw_data");
    let output_format = matches.get_one::<String>("output_format").unwrap().clone();
    let no_banner = matches.get_flag("no_banner");
    let color_mode = matches.get_one::<String>("color").unwrap().clone();

    // Configure color output based on --color flag
    let use_color = match color_mode.as_str() {
        "always" => true,
        "never" => false,
        _ => {
            // "auto": enable color only if stdout is a TTY
            std::io::IsTerminal::is_terminal(&std::io::stdout())
        }
    };
    if use_color && color_mode == "always" {
        colored::control::set_override(true);
    } else if !use_color {
        colored::control::set_override(false);
    }

    let client = RpcClient::new(&url, raw_data);

    // If a subcommand was given, run it directly (one-shot mode)
    if matches.subcommand().is_some() {
        commands::execute_command(&client, &matches, &output_format, use_color).await?;
    } else {
        // Otherwise, enter interactive REPL mode
        run_interactive(&client, &output_format, no_banner, use_color).await?;
    }

    Ok(())
}
