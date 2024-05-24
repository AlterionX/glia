use std::fs::File;

use crate::{cfg::Configuration, ReportableError};

fn io_err_mapper(e: std::io::Error) -> ReportableError {
    match e.kind() {
        std::io::ErrorKind::NotFound => {
            ReportableError { message: "bad log path".to_owned() }
        },
        k => {
            ReportableError { message: k.to_string() }
        },
    }
}

pub fn setup(cfg: &Configuration) -> Result<(), ReportableError> {
    let log_file = File::create(cfg.log.path.as_str()).map_err(io_err_mapper)?;

    let builder_with_fmt = tracing_subscriber::fmt()
        .compact()
        .with_file(true)
        .with_line_number(true)
        .with_thread_ids(true)
        .with_target(false);

    let builder_with_fmt_output = builder_with_fmt
        .with_writer(log_file);

    let subscriber = builder_with_fmt_output.finish();
    trc::subscriber::set_global_default(subscriber).expect("no issues setting up logging");
    Ok(())
}
