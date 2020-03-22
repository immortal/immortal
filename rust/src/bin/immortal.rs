use clap::{App, Arg, SubCommand};
use immortal::daemon;
use nix::unistd::{fork, ForkResult};
use std::env;

fn main() {
    let matches = App::new("immortal")
        .version(env!("CARGO_PKG_VERSION"))
        .about("Run a command forever")
        .arg(
            Arg::with_name("q")
                .short("q")
                .long("quiet")
                .help("Redirect standard input, output, error to /dev/null")
                .takes_value(false),
        )
        .arg(
            Arg::with_name("f")
                .short("f")
                .long("follow")
                .help("Follow pid")
                .takes_value(false),
        )
        .arg(
            Arg::with_name("u")
                .short("u")
                .long("user")
                .help("Execute command on behalf user")
                .takes_value(true),
        )
        .arg(
            Arg::with_name("command")
                .help("Command to daemonize")
                .required(true)
                .index(1),
        )
        .get_matches();

    if matches.is_present("u") {
        println!("user: {}", matches.value_of("u").unwrap());
    }

    match fork() {
        Ok(ForkResult::Parent { child, .. }) => {
            println!(
                "Continuing execution in parent process, new child has pid: {}",
                child
            );
        }
        Ok(ForkResult::Child) => println!("I'm a new child process"),
        Err(_) => println!("Fork failed"),
    }
}
