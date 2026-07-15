use immortal::cli::{
    self,
    actions::{self, Action},
};

fn main() -> std::process::ExitCode {
    let action = match cli::start() {
        Ok(action) => action,
        Err(error) => return error.report(),
    };

    let result = match action {
        Action::CheckConfig(path) => actions::check_config::execute(&path),

        Action::SuperviseConfig {
            control_directory,
            path,
            foreground,
        } => actions::supervise_config::execute(&path, control_directory, foreground),

        Action::SuperviseCommand(service) => actions::supervise_command::execute(service),
    };

    cli::finish(result)
}
