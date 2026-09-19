//! Binary entry point for `stabilizatores`.
//!
//! Handles the command line arguments, self-elevates when needed and starts
//! the interactive application. All logic lives in the library crate.

use stabilizatores::{app, cli, config, win};

const USAGE: &str = "stabilizatores - Windows internet optimization

usage: stabilizatores [-h | --help] [-V | --version]
                      [--apply | --revert | --status | --plan | --report | --restore-point | --undo]
                      [--dns=on|off] [--mtu=off|1280..9000] [--power=on|off]
                      [--profile save|apply|list|delete <name>]

runs interactively when started without flags.
--apply          apply every enabled tweak and exit
--revert         restore the archived machine state and exit
--undo           revert the last apply and restore its archive snapshot
--status         print the current alignment state and exit (read-only)
--plan           dry-run: print what --apply would change (read-only)
--report         write a diagnostic JSON snapshot and print its path (read-only)
--restore-point  create a System Restore point before tweaking
--profile        save the config as a named profile, apply/list/delete one
the --dns / --mtu / --power overrides change the config for this run only.
requires administrator rights for the network tweaks.";

fn main() {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    let options = match cli::parse(&args) {
        Ok(options) => options,
        Err(error) => {
            eprintln!("stabilizatores: {error}");
            println!("\n{USAGE}");
            std::process::exit(cli::EXIT_ERROR);
        }
    };
    let code = match options.command {
        cli::Command::Help => {
            println!("{USAGE}");
            cli::EXIT_OK
        }
        cli::Command::Version => {
            println!("stabilizatores {}", env!("CARGO_PKG_VERSION"));
            cli::EXIT_OK
        }
        cli::Command::Apply => {
            ensure_elevated();
            cli::run_apply(&options)
        }
        cli::Command::Revert => {
            ensure_elevated();
            cli::run_revert()
        }
        cli::Command::Status => cli::run_status(&options),
        cli::Command::Plan => cli::run_plan(&options),
        cli::Command::Report => cli::run_report(),
        cli::Command::RestorePoint => {
            ensure_elevated();
            cli::run_restore_point()
        }
        cli::Command::Undo => {
            ensure_elevated();
            cli::run_undo()
        }
        cli::Command::Profile => {
            if matches!(options.profile, Some(cli::ProfileAction::Apply { .. })) {
                ensure_elevated();
            }
            cli::run_profile(&options)
        }
        cli::Command::Interactive => {
            ensure_elevated();
            let cfg = config::load();
            let archive = config::load_archive();
            let mut app = app::App::new(cfg, archive);
            app.run();
            cli::EXIT_OK
        }
    };
    std::process::exit(code);
}

/// Self-elevates for the actions that touch the system. Read-only commands
/// never reach this.
fn ensure_elevated() {
    if win::is_elevated() {
        return;
    }
    if win::relaunch_elevated() {
        std::process::exit(cli::EXIT_OK);
    }
    println!("stabilizatores requires administrator rights. run the program as administrator.");
    std::thread::sleep(std::time::Duration::from_secs(4));
    std::process::exit(cli::EXIT_OK);
}
