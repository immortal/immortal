use immortalctl::cli::{
    self,
    actions::{self, Action},
};

fn main() -> std::process::ExitCode {
    let action = match cli::start() {
        Ok(action) => action,
        Err(error) => return error.report(),
    };

    let result = match action {
        Action::Status(action) => actions::status::execute(&action),

        Action::Start(action) => actions::start::execute(&action),

        Action::Stop(action) => actions::stop::execute(&action),

        Action::Restart(action) => actions::restart::execute(&action),

        Action::Once(action) => actions::once::execute(&action),

        Action::Exit(action) => actions::exit::execute(&action),

        Action::Halt(action) => actions::halt::execute(&action),

        Action::Signal(action) => actions::signal::execute(&action),
    };

    cli::finish(result)
}
