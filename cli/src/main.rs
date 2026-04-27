use std::env;
use std::error::Error;
use std::process::exit;
use structopt::StructOpt;

use stax_core::{args, cmd_record_mac, cmd_setup_mac};

fn main_impl() -> Result<(), Box<dyn Error>> {
    if env::var("RUST_LOG").is_err() {
        // cranelift_jit/cranelift_codegen log every JIT'd function at info,
        // which floods the terminal once we start the live RPC server.
        env::set_var("RUST_LOG", "info,cranelift_jit=warn,cranelift_codegen=warn");
    }

    env_logger::init();

    let opt = args::Opt::from_args();
    match opt {
        args::Opt::Record(args) => run_record(args)?,
        args::Opt::Setup(args) => cmd_setup_mac::main(args)?,
    }

    Ok(())
}

fn main() {
    if let Err(error) = main_impl() {
        eprintln!("error: {}", error);
        exit(1);
    }
}

fn run_record(args: args::RecordArgs) -> Result<(), Box<dyn Error>> {
    let (live_sink, _runtime): (Option<Box<dyn stax_core::live_sink::LiveSink>>, _) =
        if let Some(ref addr) = args.serve {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?;
            let (sink, _server_handle) = runtime.block_on(stax_live::start(addr))?;
            (Some(Box::new(sink)), Some(runtime))
        } else {
            (None, None)
        };

    cmd_record_mac::main_with_live_sink(args, live_sink)
}
