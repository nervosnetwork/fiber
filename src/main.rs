use ckb_pcn_node::{print_help_and_exit, Config};

fn main() {
    let config = Config::parse();
    start_program(config);
}

fn start_program(config: Config) {
    let ckb_config = config.ckb;
    println!("Hello, {}!", ckb_config.name);

    println!("Your age is {}!", ckb_config.age);
    if ckb_config.age == 0 {
        println!("Age must not be 0");
        print_help_and_exit(1);
    }

    println!("Executing the rest of the program logic");
    // Rest of the program logic goes here
    // ...
}
