#[cfg(not(target_arch = "wasm32"))]
fn main() -> anyhow::Result<()> {
    use anyhow::{bail, Context};
    use fiber_dst::{run, RandomFaultPolicy, Scenario};

    const USAGE: &str =
        "usage: fiber-dst random [seed] [--payments N] [--phases N] [--chaos|--chaos-payments]";

    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("random") => {}
        Some("--help" | "-h") => {
            println!("{USAGE}");
            return Ok(());
        }
        _ => bail!("{USAGE}"),
    }
    let mut seed = None;
    let mut payments = 128;
    let mut phases = 8;
    let mut fault_policy = RandomFaultPolicy::Windows;
    while let Some(arg) = args.next() {
        let option = match arg.as_str() {
            "--payments" => &mut payments,
            "--phases" => &mut phases,
            "--chaos" | "--chaos-payments" => {
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
    let policy = match fault_policy {
        RandomFaultPolicy::Windows => "",
        RandomFaultPolicy::Continuous => " --chaos",
        RandomFaultPolicy::Payments => " --chaos-payments",
    };
    eprintln!("fiber-dst random {seed} --payments {payments} --phases {phases}{policy}");
    if let Ok(filter) = tracing_subscriber::EnvFilter::try_from_default_env() {
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_writer(std::io::stderr)
            .init();
    }
    for event in run(
        Scenario::Random {
            payments,
            phases,
            fault_policy,
        },
        seed,
    )? {
        println!("{event}");
    }
    Ok(())
}

#[cfg(target_arch = "wasm32")]
fn main() {}
