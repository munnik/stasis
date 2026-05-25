// Author: Dustin Pilgrim
// License: GPL-3.0-only

use crate::cli::{Args, Command};

type AnyError = Box<dyn std::error::Error + Send + Sync>;

pub async fn run(args: Args) -> Result<(), AnyError> {
    // command mode: args.command is Some
    let cmd = args.command.as_ref().expect("command mode");

    match cmd {
        Command::Reload => match crate::ipc::client::send_raw("reload").await {
            Ok(resp) => {
                let out = resp.trim_end();
                if out.is_empty() {
                    println!("Configuration reloaded");
                } else {
                    println!("{out}");
                }
                Ok(())
            }
            Err(e) => {
                eprintln!("stasis: {e}");
                Ok(())
            }
        },

        Command::Pause { args: pause_args } => {
            let mut msg = String::from("pause");
            if !pause_args.is_empty() {
                msg.push(' ');
                msg.push_str(&pause_args.join(" "));
            }

            match crate::ipc::client::send_raw(&msg).await {
                Ok(resp) => {
                    let out = resp.trim_end();
                    if out.is_empty() {
                        println!("Idle timers paused");
                    } else {
                        println!("{out}");
                    }
                    Ok(())
                }
                Err(e) => {
                    eprintln!("stasis: {e}");
                    Ok(())
                }
            }
        }

        Command::Resume => match crate::ipc::client::send_raw("resume").await {
            Ok(resp) => {
                let out = resp.trim_end();
                if out.is_empty() {
                    println!("Idle timers resumed");
                } else {
                    println!("{out}");
                }
                Ok(())
            }
            Err(e) => {
                eprintln!("stasis: {e}");
                Ok(())
            }
        },

        Command::ToggleInhibit => match crate::ipc::client::send_raw("toggle-inhibit").await {
            Ok(resp) => {
                let out = resp.trim_end();
                if out.is_empty() {
                    println!("Toggled idle inhibition");
                } else {
                    println!("{out}");
                }
                Ok(())
            }
            Err(e) => {
                eprintln!("stasis: {e}");
                Ok(())
            }
        },

        Command::Tray => crate::app::tray::run().await,

        Command::Trigger { step } => {
            let msg = format!("trigger {}", step);

            match crate::ipc::client::send_raw(&msg).await {
                Ok(resp) => {
                    let out = resp.trim_end();
                    if out.is_empty() {
                        println!("Triggered '{}'", step);
                    } else {
                        println!("{out}");
                    }
                    Ok(())
                }
                Err(e) => {
                    eprintln!("stasis: {e}");
                    Ok(())
                }
            }
        }

        Command::Info { json } => {
            let msg = if *json { "info --json" } else { "info" };

            match crate::ipc::client::send_raw(msg).await {
                Ok(resp) => {
                    if !resp.is_empty() {
                        println!("{resp}");
                    }
                    Ok(())
                }
                Err(e) => {
                    if *json {
                        // Waybar needs valid JSON on stdout even when daemon isn't running.
                        println!(
                            "{}",
                            r#"{"text":"","alt":"not_running","class":"not_running","tooltip":"Stasis not running","profile":null}"#
                        );
                    } else {
                        eprintln!("stasis: {e}");
                    }
                    Ok(())
                }
            }
        }

        Command::Dump { args } => {
            let mut msg = String::from("dump");
            if !args.is_empty() {
                msg.push(' ');
                msg.push_str(&args.join(" "));
            }

            match crate::ipc::client::send_raw(&msg).await {
                Ok(resp) => {
                    if !resp.is_empty() {
                        print!("{resp}");
                    }
                    Ok(())
                }
                Err(e) => {
                    eprintln!("stasis: {e}");
                    Ok(())
                }
            }
        }

        Command::Profile { args } => {
            let mut msg = String::from("profile");
            if !args.is_empty() {
                msg.push(' ');
                msg.push_str(&args.join(" "));
            }

            match crate::ipc::client::send_raw(&msg).await {
                Ok(resp) => {
                    let out = resp.trim_end();
                    if !out.is_empty() {
                        println!("{out}");
                    }
                    Ok(())
                }
                Err(e) => {
                    eprintln!("stasis: {e}");
                    Ok(())
                }
            }
        }

        Command::List { args } => {
            let mut msg = String::from("list");
            if !args.is_empty() {
                msg.push(' ');
                msg.push_str(&args.join(" "));
            }

            match crate::ipc::client::send_raw(&msg).await {
                Ok(resp) => {
                    if !resp.is_empty() {
                        print!("{resp}");
                    }
                    Ok(())
                }
                Err(e) => {
                    eprintln!("stasis: {e}");
                    Ok(())
                }
            }
        }

        Command::Stop => match crate::ipc::client::send_raw("stop").await {
            Ok(resp) => {
                let out = resp.trim_end();
                if out.is_empty() {
                    println!("Stopping Stasis daemon");
                } else {
                    println!("{out}");
                }
                Ok(())
            }
            Err(e) => {
                eprintln!("stasis: {e}");
                Ok(())
            }
        },
    }
}
