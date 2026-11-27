#[cfg(not(target_arch = "wasm32"))]
fn main() -> anyhow::Result<()> {
    use anyhow::{bail, Context};
    use fiber_dst::{run, RandomFaultPolicy, Scenario};

    const USAGE: &str = "usage:\n  fiber-dst random [seed] [--payments N] [--phases N] [--chaos|--chaos-payments]\n  fiber-dst lsp [seed] [--payments N] [--chaos]";

    let mut args = std::env::args().skip(1);
    let mode = match args.next().as_deref() {
        Some("random") => "random",
        Some("lsp") => "lsp",
        Some("--help" | "-h") => {
            println!("{USAGE}");
            return Ok(());
        }
        _ => bail!("{USAGE}"),
    };
    let mut seed = None;
    let mut payments = if mode == "lsp" { 16 } else { 128 };
    let mut phases = 8;
    let mut fault_policy = RandomFaultPolicy::Windows;
    let mut lsp_chaos = false;
    while let Some(arg) = args.next() {
        let option = match arg.as_str() {
            "--payments" => &mut payments,
            "--phases" if mode == "random" => &mut phases,
            "--chaos" if mode == "lsp" => {
                lsp_chaos = true;
                continue;
            }
            "--chaos" | "--chaos-payments" if mode == "random" => {
                anyhow::ensure!(
                    fault_policy == RandomFaultPolicy::Windows,
                    "specify one fault policy"
                );
                fault_policy = if arg == "--chaos" {
                    RandomFaultPolicy::Continuous
                } else {
                    RandomFaultPolicy::Payments
                };
                continue;
            }
            "--help" | "-h" => {
                println!("{USAGE}");
                return Ok(());
            }
            _ if !arg.starts_with('-') && seed.is_none() => {
                seed = Some(
                    arg.parse::<u64>()
                        .with_context(|| format!("invalid seed {arg}"))?,
                );
                continue;
            }
            _ => bail!("unexpected argument {arg}\n{USAGE}"),
        };
        *option = args
            .next()
            .with_context(|| format!("provide a value for {arg}"))?
            .parse()
            .with_context(|| format!("invalid value for {arg}"))?;
    }
    let seed = seed.unwrap_or_else(rand::random);
    if let Ok(filter) = tracing_subscriber::EnvFilter::try_from_default_env() {
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_writer(std::io::stderr)
            .init();
    }
    let scenario = if mode == "lsp" {
        let chaos_flag = if lsp_chaos { " --chaos" } else { "" };
        eprintln!("fiber-dst lsp {seed} --payments {payments}{chaos_flag}");
        Scenario::LspRandom {
            payments,
            chaos: lsp_chaos,
        }
    } else {
        let policy = match fault_policy {
            RandomFaultPolicy::Windows => "",
            RandomFaultPolicy::Continuous => " --chaos",
            RandomFaultPolicy::Payments => " --chaos-payments",
        };
        eprintln!("fiber-dst random {seed} --payments {payments} --phases {phases}{policy}");
        Scenario::Random {
            payments,
            phases,
            fault_policy,
        }
    };
    for event in run(scenario, seed)? {
        println!("{event}");
    }
    Ok(())
}

#[cfg(target_arch = "wasm32")]
fn main() {}
