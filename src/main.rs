use ckb_pcn_node::{start_ldk, Config};

#[tokio::main]
pub async fn main() {
    #[cfg(not(target_os = "windows"))]
    {
        // Catch Ctrl-C with a dummy signal handler.
        unsafe {
            let mut new_action: libc::sigaction = core::mem::zeroed();
            let mut old_action: libc::sigaction = core::mem::zeroed();

            extern "C" fn dummy_handler(
                _: libc::c_int,
                _: *const libc::siginfo_t,
                _: *const libc::c_void,
            ) {
            }

            new_action.sa_sigaction = dummy_handler as libc::sighandler_t;
            new_action.sa_flags = libc::SA_SIGINFO;

            libc::sigaction(
                libc::SIGINT,
                &new_action as *const libc::sigaction,
                &mut old_action as *mut libc::sigaction,
            );
        }
    }

    println!("Parsing config");
    let config = Config::parse();

    println!("Starting ldk");
    start_ldk(config.ldk).await;
}
